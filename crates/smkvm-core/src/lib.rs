//! Deciding where the cursor is and who should be told about it.
//!
//! The server owns one authoritative position on the global desktop. Every
//! movement of the physical pointer runs through it, and it answers with what
//! should happen: inject here, send there, start or stop swallowing local
//! input.
//!
//! Nothing in this crate talks to a socket, a display server, or a clock of its
//! own. Events go in with a timestamp, actions come out, so the whole of the
//! switching behaviour — including the parts that only show up at a screen
//! edge, mid-chord, or when a machine vanishes — is exercised in tests without
//! any of it running for real.

#![forbid(unsafe_code)]

mod server;

pub use server::{Action, Event, LocalAction, PointerMode, Server, Settings};
