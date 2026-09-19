//! The SMKVM wire protocol: what two machines say to each other, and how those
//! messages become bytes.
//!
//! The crate holds no I/O and no state beyond a stream reassembly buffer, which
//! keeps the code that interprets bytes from an unauthenticated peer small,
//! pure, and testable on its own.
//!
//! ```
//! use smkvm_proto::{decode, encode, FrameDecoder, ServerControl};
//! use smkvm_proto::keys::{Key, MouseButton};
//!
//! let sent = ServerControl::KeyEvent { key: Key::LEFT_ALT, down: true, repeat: false };
//! let bytes = encode(&sent).unwrap();
//!
//! // Arriving split across two reads, as a real socket would deliver it.
//! let mut dec = FrameDecoder::new();
//! dec.extend(&bytes[..3]);
//! assert!(dec.next_frame().unwrap().is_none());
//! dec.extend(&bytes[3..]);
//!
//! let frame = dec.next_frame().unwrap().unwrap();
//! assert_eq!(decode::<ServerControl>(frame).unwrap(), sent);
//! # let _ = MouseButton::Left;
//! ```

#![forbid(unsafe_code)]

pub mod codec;
pub mod keys;
pub mod msg;

pub use codec::{
    decode, encode, encode_bare, encode_with_limit, FrameDecoder, ProtoError, HEADER_LEN,
    MAX_CHUNK_DATA, MAX_FRAME_LEN,
};
pub use keys::{Key, MouseButton, Scroll};
pub use msg::{
    Bulk, ClientControl, ClipEntry, ClipError, ClipFormat, ClipOffer, ClipSeq, FileEntry,
    FileOffer, Hello, Reject, Role, ServerControl, SuspendReason, TransferId, PROTO_VERSION,
};
