//! The messages two SMKVM peers exchange.
//!
//! Everything travels on one authenticated link. [`ClientControl`] and
//! [`ServerControl`] are the small messages that decide where the cursor is;
//! [`Bulk`] carries clipboard contents and file data, and rides inside them.
//!
//! One link, deliberately. A second connection for bulk data was the first
//! design, and it buys nothing that pacing does not: contents move one chunk
//! at a time, each requested only once the last has arrived, so however large
//! a paste is, at most one chunk is ever queued ahead of a pointer movement. A
//! chunk is [`crate::MAX_CHUNK_DATA`] bytes, well under a millisecond on any
//! network these machines share, and a second link would have meant a second
//! handshake, a second thing to fail, and a way to tell the two apart on
//! arrival.

use serde::{Deserialize, Serialize};
use smkvm_layout::{DeviceId, Monitor, Point};

use crate::keys::{Key, MouseButton, Scroll};

/// Wire format version. Bumped whenever a change would confuse an older peer.
pub const PROTO_VERSION: u16 = 3;

/// Which side of the session a peer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Owns the physical keyboard and mouse and the authoritative cursor.
    Server,
    /// Receives input and reports its monitors.
    Client,
}

/// Opening message, sent by both sides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u16,
    pub device: DeviceId,
    pub name: String,
    pub role: Role,
}

/// Why a peer refused the session.
///
/// Every rejection is explicit. A peer that cannot be understood is told so
/// rather than left to misbehave, which is what makes a partially updated set
/// of machines fail loudly instead of strangely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reject {
    /// The peer speaks a version this build cannot talk to.
    ProtocolVersion {
        theirs: u16,
        ours: u16,
    },
    /// The device is not in this peer's paired set.
    NotPaired,
    /// Another session already holds this device's slot.
    AlreadyConnected,
    /// Both peers claim the same role.
    RoleConflict,
    ShuttingDown,
}

/// Why a client cannot accept input right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspendReason {
    /// Windows switched to the secure desktop, for a UAC prompt or
    /// Ctrl+Alt+Del. Injection cannot reach it from a user session, so the
    /// client says so instead of fighting the cursor.
    SecureDesktop,
    /// The session is locked.
    Locked,
    /// The display went to sleep.
    DisplayAsleep,
    Other,
    /// The window in front runs with higher privileges than the client, and
    /// Windows refuses input injected from below. Nothing sent lands until
    /// the foreground changes, or the client is run elevated.
    Elevated,
}

/// Messages a client sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientControl {
    Hello(Hello),
    /// The client's monitors, sent on connect and whenever displays change.
    Monitors {
        monitors: Vec<Monitor>,
    },
    /// Input cannot be delivered. The server keeps the cursor on its own
    /// screen until [`ClientControl::Resumed`].
    Suspended {
        reason: SuspendReason,
    },
    Resumed,
    /// Acknowledges the keys and buttons the client believes are held. Lets
    /// the server detect drift rather than trust its own bookkeeping.
    KeyStateReport {
        pressed: Vec<Key>,
        buttons: Vec<MouseButton>,
    },
    Ping {
        id: u32,
    },
    Pong {
        id: u32,
    },
    Goodbye,
    /// Clipboard or file traffic.
    Bulk(Bulk),
}

/// Messages a server sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerControl {
    Hello(Hello),
    Rejected {
        reason: Reject,
    },

    /// The cursor has arrived. `at` is in the client's own coordinate space.
    ///
    /// `pressed` and `buttons` are the authoritative input state at the moment
    /// of the switch: the client presses what is listed and releases anything
    /// else, which is what stops a modifier held during the crossing from
    /// getting stuck down on the machine being left behind.
    Enter {
        at: Point,
        pressed: Vec<Key>,
        buttons: Vec<MouseButton>,
    },
    /// The cursor has gone elsewhere. The client releases everything.
    Leave,

    /// Absolute pointer position in the client's coordinate space.
    MoveTo {
        x: i32,
        y: i32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Wheel(Scroll),
    KeyEvent {
        key: Key,
        down: bool,
        repeat: bool,
    },

    /// Force the client's input state to exactly this set.
    SyncKeys {
        pressed: Vec<Key>,
        buttons: Vec<MouseButton>,
    },
    /// Release every key and button. Sent on disconnect and on any doubt.
    ReleaseAll,

    Ping {
        id: u32,
    },
    Pong {
        id: u32,
    },
    Goodbye,
    /// Clipboard or file traffic.
    Bulk(Bulk),
}

impl ServerControl {
    /// May this be thrown away when a machine has stopped keeping up?
    ///
    /// Pointer motion may, because the next one supersedes it: a machine that
    /// misses a position is a few pixels behind for a moment and then right
    /// again. A wheel notch may, for the same reason a scroll that stutters is
    /// better than one that arrives a second late.
    ///
    /// Nothing else may. The rest either change what a machine believes about
    /// itself or move something that stays moved, and no later message repeats
    /// them. A lost [`ServerControl::Enter`] is the worst of them: the machine
    /// goes on thinking the cursor is elsewhere and silently discards
    /// everything sent afterwards, while the server swallows the keyboard and
    /// mouse on its behalf. From the outside that is a cursor that crossed and
    /// then vanished, and nothing short of another crossing puts it right.
    pub fn may_be_dropped(&self) -> bool {
        matches!(self, Self::MoveTo { .. } | Self::Wheel(_))
    }
}

/// Monotonic clipboard generation, scoped to the device that produced it.
///
/// Ordering is by counter within a device. Nothing is ever dropped for being
/// "out of sequence" — a stale offer simply loses to a newer one, which avoids
/// the failure where a clipboard update vanishes after a reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClipSeq {
    pub device: DeviceId,
    pub counter: u64,
}

/// A clipboard flavour.
///
/// PNG is the interchange form for images: Windows holds device-independent
/// bitmaps and X11 clients overwhelmingly ask for `image/png`, so converting
/// at the edges is what makes an image copied on one machine paste on another.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipFormat {
    /// UTF-8 text.
    Text,
    Html,
    Png,
    /// A list of files, as `text/uri-list` on the wire.
    Uris,
    /// Anything else, named by its MIME type.
    Other(String),
}

/// One flavour on offer.
///
/// Size and hash are best-effort: an owner that would have to read a large
/// payload just to describe it leaves them out. Describing the clipboard must
/// stay cheap, because it happens on every copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipEntry {
    pub format: ClipFormat,
    pub bytes: Option<u64>,
    /// BLAKE3 of the contents, when known. Lets a receiver skip fetching data
    /// it already holds.
    pub hash: Option<[u8; 32]>,
}

/// An announcement that the clipboard changed.
///
/// Only the description travels on a copy. The contents are fetched when a
/// paste actually happens, so a large image costs nothing until it is wanted
/// and there is no size at which sharing silently stops working.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipOffer {
    pub seq: ClipSeq,
    pub entries: Vec<ClipEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipError {
    /// The offer has been superseded; the data no longer exists.
    Stale,
    /// The owning application would not hand the data over.
    OwnerRefused,
    TooLarge {
        bytes: u64,
        limit: u64,
    },
    Cancelled,
}

/// A file within a transfer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the transfer root, using `/` separators. Absolute
    /// paths and `..` components are rejected on receipt.
    pub path: String,
    pub bytes: u64,
    #[serde(default)]
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TransferId {
    pub device: DeviceId,
    pub counter: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileOffer {
    pub id: TransferId,
    pub files: Vec<FileEntry>,
    pub total_bytes: u64,
}

/// Clipboard and file traffic. Symmetric: either peer may send any of these.
///
/// Contents move one chunk per request. A [`Bulk::ClipRequest`] names an
/// offset and is answered by exactly one [`Bulk::ClipChunk`] starting there,
/// of at most [`crate::MAX_CHUNK_DATA`] bytes; the requester asks for the next
/// once it has that one. That single rule is what keeps a large paste from
/// ever queuing more than one chunk ahead of the pointer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Bulk {
    ClipOffer(ClipOffer),
    /// Ask for the chunk of `format` beginning at `offset`.
    ClipRequest {
        seq: ClipSeq,
        format: ClipFormat,
        offset: u64,
    },
    ClipChunk {
        seq: ClipSeq,
        format: ClipFormat,
        offset: u64,
        data: Vec<u8>,
        last: bool,
    },
    ClipUnavailable {
        seq: ClipSeq,
        format: ClipFormat,
        reason: ClipError,
    },
    ClipCancel {
        seq: ClipSeq,
        format: ClipFormat,
    },

    FileOffer(FileOffer),
    FileRequest {
        id: TransferId,
        index: u32,
        offset: u64,
    },
    FileChunk {
        id: TransferId,
        index: u32,
        offset: u64,
        data: Vec<u8>,
        last: bool,
    },
    FileDone {
        id: TransferId,
    },
    FileAbort {
        id: TransferId,
        reason: String,
    },

    /// The offer `seq` is what the cursor is carrying: it was picked up from
    /// a drag in progress on the machine the cursor just left. Sent by that
    /// machine after the offer itself, and passed by the server to whichever
    /// machine the cursor arrived on, which pulls the files and drops them.
    Dragging {
        seq: ClipSeq,
    },
}
