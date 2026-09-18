//! Framing behaviour, including what happens when the bytes are hostile.
//!
//! Everything a peer sends arrives here before anything has authenticated it,
//! so the decoder's job is to be boring: never panic, never trust a declared
//! length, never allocate on a stranger's say-so.

use proptest::prelude::*;
use smkvm_layout::{DeviceId, Monitor, Point, Rect};
use smkvm_proto::keys::{Key, MouseButton, Scroll};
use smkvm_proto::msg::{ClipEntry, ClipFormat, ClipOffer, ClipSeq, Hello, Role, SuspendReason};
use smkvm_proto::{
    decode, encode, encode_with_limit, Bulk, ClientControl, FrameDecoder, ProtoError,
    ServerControl, HEADER_LEN, MAX_CHUNK_DATA, MAX_FRAME_LEN, PROTO_VERSION,
};

fn sample_server_messages() -> Vec<ServerControl> {
    vec![
        ServerControl::Hello(Hello {
            proto: PROTO_VERSION,
            device: DeviceId::from_bytes([7; 32]),
            name: "desk".into(),
            role: Role::Server,
        }),
        ServerControl::Enter {
            at: Point::new(1234, -56),
            pressed: vec![Key::LEFT_CTRL, Key::LEFT_SHIFT],
            buttons: vec![MouseButton::Left, MouseButton::Other(9)],
        },
        ServerControl::Leave,
        ServerControl::MoveTo {
            x: i32::MIN,
            y: i32::MAX,
        },
        ServerControl::Wheel(Scroll::new(-120, 15)),
        ServerControl::KeyEvent {
            key: Key(0x004F),
            down: true,
            repeat: true,
        },
        ServerControl::ReleaseAll,
        ServerControl::Ping { id: u32::MAX },
    ]
}

#[test]
fn every_message_survives_a_round_trip() {
    for msg in sample_server_messages() {
        let bytes = encode(&msg).unwrap();
        let mut dec = FrameDecoder::new();
        dec.extend(&bytes);
        let frame = dec.next_frame().unwrap().expect("one whole frame");
        assert_eq!(decode::<ServerControl>(frame).unwrap(), msg);
        assert!(dec.next_frame().unwrap().is_none());
    }
}

#[test]
fn client_and_bulk_messages_round_trip_too() {
    let client = ClientControl::Monitors {
        monitors: vec![Monitor::new("DP-2", Rect::new(0, 0, 2560, 1440))],
    };
    let bytes = encode(&client).unwrap();
    let mut dec = FrameDecoder::new();
    dec.extend(&bytes);
    let frame = dec.next_frame().unwrap().unwrap();
    assert_eq!(decode::<ClientControl>(frame).unwrap(), client);

    let bulk = Bulk::ClipOffer(ClipOffer {
        seq: ClipSeq {
            device: DeviceId::from_bytes([3; 32]),
            counter: 42,
        },
        entries: vec![
            ClipEntry {
                format: ClipFormat::Text,
                bytes: Some(11),
                hash: Some([9; 32]),
            },
            ClipEntry {
                format: ClipFormat::Png,
                bytes: None,
                hash: None,
            },
            ClipEntry {
                format: ClipFormat::Other("application/x-thing".into()),
                bytes: Some(u64::MAX),
                hash: None,
            },
        ],
    });
    let bytes = encode(&bulk).unwrap();
    let mut dec = FrameDecoder::new();
    dec.extend(&bytes);
    let frame = dec.next_frame().unwrap().unwrap();
    assert_eq!(decode::<Bulk>(frame).unwrap(), bulk);
}

#[test]
fn a_frame_split_at_every_possible_point_still_arrives() {
    let msg = ServerControl::MoveTo { x: 7, y: -9 };
    let bytes = encode(&msg).unwrap();

    for split in 0..=bytes.len() {
        let mut dec = FrameDecoder::new();
        dec.extend(&bytes[..split]);
        if split < bytes.len() {
            assert!(
                dec.next_frame().unwrap().is_none(),
                "claimed a frame from {split} of {} bytes",
                bytes.len()
            );
        }
        dec.extend(&bytes[split..]);
        let frame = dec.next_frame().unwrap().expect("frame after the rest");
        assert_eq!(decode::<ServerControl>(frame).unwrap(), msg);
    }
}

#[test]
fn many_frames_in_one_read_all_come_out_in_order() {
    let msgs = sample_server_messages();
    let mut stream = Vec::new();
    for m in &msgs {
        stream.extend_from_slice(&encode(m).unwrap());
    }

    let mut dec = FrameDecoder::new();
    dec.extend(&stream);
    let mut got = Vec::new();
    while let Some(frame) = dec.next_frame().unwrap() {
        got.push(decode::<ServerControl>(frame).unwrap());
    }
    assert_eq!(got, msgs);
}

#[test]
fn an_absurd_declared_length_is_refused_outright() {
    // Four bytes claiming the next frame is 4 GiB. Nothing may be reserved on
    // the strength of that claim.
    let mut dec = FrameDecoder::new();
    dec.extend(&u32::MAX.to_le_bytes());
    match dec.next_frame() {
        Err(ProtoError::FrameTooLarge { len, max }) => {
            assert_eq!(len, u32::MAX as usize);
            assert_eq!(max, MAX_FRAME_LEN);
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    assert!(dec.buffered() <= HEADER_LEN, "buffered a stranger's claim");
}

#[test]
fn a_length_one_byte_over_the_limit_is_still_refused() {
    let mut dec = FrameDecoder::with_limit(64);
    dec.extend(&65u32.to_le_bytes());
    assert!(matches!(
        dec.next_frame(),
        Err(ProtoError::FrameTooLarge { len: 65, max: 64 })
    ));

    let mut dec = FrameDecoder::with_limit(64);
    dec.extend(&64u32.to_le_bytes());
    assert!(
        dec.next_frame().unwrap().is_none(),
        "64 is within the limit"
    );
}

#[test]
fn encoding_refuses_to_produce_an_over_limit_frame() {
    let big = ServerControl::SyncKeys {
        pressed: (0..5000).map(Key).collect(),
        buttons: vec![],
    };
    assert!(matches!(
        encode_with_limit(&big, 128),
        Err(ProtoError::FrameTooLarge { .. })
    ));
}

#[test]
fn a_full_chunk_fits_inside_the_frame_limit() {
    // The chunk cap exists so framing overhead can never push a bulk frame
    // past the frame cap. Check that with the largest plausible chunk.
    let chunk = Bulk::ClipChunk {
        seq: ClipSeq {
            device: DeviceId::from_bytes([1; 32]),
            counter: u64::MAX,
        },
        format: ClipFormat::Other("x".repeat(255)),
        offset: u64::MAX,
        data: vec![0xAB; MAX_CHUNK_DATA],
        last: false,
    };
    let bytes = encode(&chunk).unwrap();
    assert!(bytes.len() <= MAX_FRAME_LEN);

    let mut dec = FrameDecoder::new();
    dec.extend(&bytes);
    let frame = dec.next_frame().unwrap().unwrap();
    assert_eq!(decode::<Bulk>(frame).unwrap(), chunk);
}

#[test]
fn a_long_lived_stream_does_not_grow_without_bound() {
    let msg = ClientControl::Suspended {
        reason: SuspendReason::SecureDesktop,
    };
    let bytes = encode(&msg).unwrap();

    let mut dec = FrameDecoder::new();
    for _ in 0..200_000 {
        dec.extend(&bytes);
        let frame = dec.next_frame().unwrap().expect("a frame per write");
        let _ = decode::<ClientControl>(frame).unwrap();
    }
    assert!(
        dec.buffered() < 1024 * 1024,
        "buffer reached {} bytes after 200k frames",
        dec.buffered()
    );
    assert_eq!(dec.pending(), 0);
}

#[test]
fn a_well_framed_but_meaningless_payload_is_an_error_not_a_panic() {
    let junk = [0xFFu8; 64];
    let mut framed = (junk.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(&junk);

    let mut dec = FrameDecoder::new();
    dec.extend(&framed);
    let frame = dec.next_frame().unwrap().unwrap();
    assert!(decode::<ServerControl>(frame).is_err());
}

proptest! {
    /// Arbitrary bytes, delivered in arbitrary pieces, must never panic and
    /// never yield a frame longer than the limit.
    #[test]
    fn arbitrary_bytes_never_panic(
        chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..32),
    ) {
        let mut dec = FrameDecoder::with_limit(4096);
        for chunk in &chunks {
            dec.extend(chunk);
            loop {
                match dec.next_frame() {
                    Ok(Some(frame)) => {
                        prop_assert!(frame.len() <= 4096);
                        let _ = decode::<ServerControl>(frame);
                        let _ = decode::<Bulk>(frame);
                    }
                    Ok(None) => break,
                    Err(_) => return Ok(()),
                }
            }
        }
    }

    /// Any real message, delivered one byte at a time, comes back intact.
    #[test]
    fn dribbled_messages_reassemble(
        x in any::<i32>(),
        y in any::<i32>(),
        key in any::<u16>(),
        down in any::<bool>(),
    ) {
        for msg in [
            ServerControl::MoveTo { x, y },
            ServerControl::KeyEvent { key: Key(key), down, repeat: false },
        ] {
            let bytes = encode(&msg).unwrap();
            let mut dec = FrameDecoder::new();
            let mut out = None;
            for b in &bytes {
                dec.extend(std::slice::from_ref(b));
                if let Some(frame) = dec.next_frame().unwrap() {
                    out = Some(decode::<ServerControl>(frame).unwrap());
                }
            }
            prop_assert_eq!(out, Some(msg));
        }
    }
}

/// Postcard is not self-describing: a field omitted on write is still expected
/// on read, so `skip_serializing_if` on any type that crosses the wire
/// desynchronises the stream. Types reused between config files and the
/// protocol are the easy place for that to slip in, so pin it here.
#[test]
fn wire_types_with_optional_fields_round_trip() {
    let filled = Monitor {
        id: "DP-2".into(),
        local: Rect::new(0, 0, 2560, 1440),
        scale: 1.5,
        primary: true,
        label: Some("DELL U2720Q".into()),
    };
    let bare = Monitor {
        id: "HDMI-1".into(),
        local: Rect::new(2560, 0, 1920, 1080),
        scale: 1.0,
        primary: false,
        label: None,
    };

    for monitors in [vec![], vec![filled.clone()], vec![filled, bare]] {
        let msg = ClientControl::Monitors { monitors };
        let bytes = encode(&msg).unwrap();
        let mut dec = FrameDecoder::new();
        dec.extend(&bytes);
        let frame = dec.next_frame().unwrap().unwrap();
        assert_eq!(decode::<ClientControl>(frame).unwrap(), msg);
    }
}
