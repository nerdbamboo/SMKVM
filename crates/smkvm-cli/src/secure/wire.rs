//! What the service and the worker say to each other.
//!
//! A private protocol between two processes of the same build on one
//! machine, so it is deliberately not the protocol that goes over the
//! network: nothing here is ever parsed from anything but the pipe, and
//! the pipe admits LocalSystem alone. Keeping it separate also keeps
//! `PROTO_VERSION` about the wire between machines, where a mismatch means
//! "update the other machine" -- a mismatch here can only mean two halves
//! of one installation have got out of step, which is answered by the
//! handshake below rather than by a number every machine has to agree on.

use serde::{Deserialize, Serialize};
use smkvm_layout::Monitor;
use smkvm_proto::{Key, MouseButton, Scroll};

/// Bumped whenever anything in this file changes shape.
///
/// The worker says it first; a service that hears a different one kills the
/// worker rather than guessing, because the alternative -- misreading a
/// frame and injecting whatever the bytes happen to decode as -- is a
/// program typing at random into a consent prompt.
pub const WORKER_PROTOCOL: u32 = 1;

/// Longest frame either side will send or accept.
///
/// Every message here is a handful of integers except the monitor list,
/// which is a few hundred bytes. A length that says otherwise is a fault or
/// an attack, and answering it by allocating what it asks for is how a
/// length prefix becomes a denial of service.
pub const LONGEST_FRAME: usize = 64 * 1024;

/// What the service tells the worker to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToWorker {
    MoveTo {
        x: i32,
        y: i32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Wheel(Scroll),
    Key {
        key: Key,
        down: bool,
    },
    /// Push what has been buffered at the system.
    Flush,
    HideCursor,
    ShowCursor,
    /// Say what displays this machine has.
    TellMonitors,
    /// Stop letting the local keyboard and mouse through to this machine,
    /// or start again. Only meaningful on the machine that owns them.
    Swallow(bool),
    /// Let go and exit. Sent before the worker is replaced, so a worker
    /// leaving a desktop does not leave a key held down on it.
    Stop,
}

/// What the worker says back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromWorker {
    /// First frame, always. Names the desktop the worker actually ended up
    /// on, which is not necessarily the one it was asked for: the input can
    /// move again between the service deciding and the worker starting.
    Ready {
        protocol: u32,
        desktop: String,
    },
    /// Something the local keyboard or mouse did.
    Saw(Saw),
    Monitors(Vec<Monitor>),
    /// An injection the system turned down, with whatever it said. The
    /// service treats this exactly as the daemon treats a local refusal.
    Refused(String),
}

/// The events the worker captures, mirrored here rather than shared.
///
/// `smkvm_core::Event` has this shape already, but it belongs to the state
/// machine crate and is not serialised anywhere; giving it derives to suit
/// a pipe would make the state machine's vocabulary answerable to a wire
/// format, which is the arrangement `smkvm-core` exists to avoid. Two small
/// types and one conversion is the cheaper of the two prices.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Saw {
    PointerAt { x: i32, y: i32 },
    PointerBy { dx: i32, dy: i32 },
    Button { button: MouseButton, down: bool },
    Wheel(Scroll),
    Key { key: Key, down: bool, repeat: bool },
}

impl From<Saw> for smkvm_core::Event {
    fn from(saw: Saw) -> Self {
        match saw {
            Saw::PointerAt { x, y } => smkvm_core::Event::PointerAt { x, y },
            Saw::PointerBy { dx, dy } => smkvm_core::Event::PointerBy { dx, dy },
            Saw::Button { button, down } => smkvm_core::Event::Button { button, down },
            Saw::Wheel(scroll) => smkvm_core::Event::Wheel(scroll),
            Saw::Key { key, down, repeat } => smkvm_core::Event::Key { key, down, repeat },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("the pipe: {0}")]
    Pipe(#[from] std::io::Error),
    #[error("a frame of {0} bytes, which is more than anything here ever is")]
    TooLong(usize),
    #[error("a frame that did not decode; the two halves are not the same build")]
    Garbled,
}

/// One message, ready to go down the pipe: four bytes of length, then the
/// message.
///
/// A pipe in byte mode delivers whatever arrived, which is not necessarily
/// what was written in one call, so the length has to be on the wire. (The
/// pipe is byte mode rather than message mode on purpose: message mode
/// bounds a read by the *reader's* buffer as well, and a short buffer there
/// loses the rest of the message rather than returning it next time.)
pub fn frame<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_stdvec(message).map_err(|_| FrameError::Garbled)?;
    if body.len() > LONGEST_FRAME {
        return Err(FrameError::TooLong(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Read exactly one message, or say why not.
pub fn read_frame<T: serde::de::DeserializeOwned, R: std::io::Read>(
    from: &mut R,
) -> Result<T, FrameError> {
    let mut length = [0u8; 4];
    from.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    // Checked before anything is allocated. A length prefix trusted far
    // enough to reserve against is a length prefix that can exhaust memory.
    if length > LONGEST_FRAME {
        return Err(FrameError::TooLong(length));
    }
    let mut body = vec![0u8; length];
    from.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(|_| FrameError::Garbled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(m: T) {
        let bytes = frame(&m).expect("it fits");
        let mut cursor = std::io::Cursor::new(bytes);
        let back: T = read_frame(&mut cursor).expect("it reads back");
        assert_eq!(m, back);
        assert_eq!(
            cursor.position() as usize,
            cursor.get_ref().len(),
            "the frame said its own length exactly"
        );
    }

    #[test]
    fn every_instruction_survives_the_pipe() {
        round_trip(ToWorker::MoveTo { x: -1920, y: 43 });
        round_trip(ToWorker::Button {
            button: MouseButton::Left,
            down: true,
        });
        round_trip(ToWorker::Key {
            key: Key(0x04),
            down: false,
        });
        round_trip(ToWorker::Flush);
        round_trip(ToWorker::Swallow(true));
        round_trip(ToWorker::Stop);
    }

    #[test]
    fn every_report_survives_the_pipe() {
        round_trip(FromWorker::Ready {
            protocol: WORKER_PROTOCOL,
            desktop: "Winlogon".into(),
        });
        round_trip(FromWorker::Saw(Saw::PointerBy { dx: 3, dy: -4 }));
        round_trip(FromWorker::Monitors(Vec::new()));
        round_trip(FromWorker::Refused("SendInput returned 0".into()));
    }

    #[test]
    fn two_frames_in_one_read_do_not_run_together() {
        // The whole reason the length is on the wire: a byte-mode pipe is
        // free to hand both of these over in one go.
        let mut bytes = frame(&ToWorker::Flush).unwrap();
        bytes.extend(frame(&ToWorker::Stop).unwrap());
        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(
            read_frame::<ToWorker, _>(&mut cursor).unwrap(),
            ToWorker::Flush
        );
        assert_eq!(
            read_frame::<ToWorker, _>(&mut cursor).unwrap(),
            ToWorker::Stop
        );
    }

    #[test]
    fn a_length_larger_than_anything_real_is_refused_before_it_is_believed() {
        let mut bytes = (LONGEST_FRAME as u32 + 1).to_le_bytes().to_vec();
        // Deliberately no body: if the length were acted on, this would try
        // to read 64 KiB that is not there rather than refusing the length.
        bytes.extend_from_slice(b"..");
        let mut cursor = std::io::Cursor::new(bytes);
        assert!(matches!(
            read_frame::<ToWorker, _>(&mut cursor),
            Err(FrameError::TooLong(_))
        ));
    }

    #[test]
    fn nonsense_is_an_error_rather_than_whatever_it_decodes_as() {
        let mut bytes = 3u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0xff, 0xff, 0xff]);
        let mut cursor = std::io::Cursor::new(bytes);
        assert!(matches!(
            read_frame::<ToWorker, _>(&mut cursor),
            Err(FrameError::Garbled)
        ));
    }

    #[test]
    fn what_the_worker_saw_becomes_what_the_state_machine_knows() {
        assert_eq!(
            smkvm_core::Event::from(Saw::PointerAt { x: 7, y: 9 }),
            smkvm_core::Event::PointerAt { x: 7, y: 9 }
        );
        assert_eq!(
            smkvm_core::Event::from(Saw::Wheel(Scroll::new(0, Scroll::NOTCH))),
            smkvm_core::Event::Wheel(Scroll::new(0, Scroll::NOTCH))
        );
    }
}
