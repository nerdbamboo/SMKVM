//! Watching what is on the clipboard, and offering it to another machine.
//!
//! Two ideas shape this, both of them departures from how Barrier does it.
//!
//! The first is that copying, not switching screens, is what makes a clipboard
//! travel. Barrier sends the clipboard when the cursor leaves a machine, so
//! copying something after you have already moved away simply does not
//! propagate. Here every change is noticed as it happens.
//!
//! The second is that a copy announces what is available rather than sending
//! it. A description is cheap and fixed in size; the contents might be a
//! screenshot. Sending everything eagerly is what forces a size limit, and a
//! size limit is what makes sharing fail silently once the thing being copied
//! is large enough. Nothing here is ever dropped for being too big -- it is
//! fetched when it is wanted, and until then it costs nothing.

#![deny(unsafe_code)]

pub mod platform;

use smkvm_proto::ClipFormat;

pub type Result<T> = std::result::Result<T, ClipboardError>;

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("the clipboard is held by something else")]
    Busy,
    #[error("the owning application would not hand over {0:?}")]
    Refused(ClipFormat),
    #[error("nothing on the clipboard is in a form this can carry")]
    NothingUsable,
    #[error("display server: {0}")]
    Display(String),
    #[error("{0} is not available on this platform")]
    Unsupported(&'static str),
}

/// What is on the clipboard right now, described rather than fetched.
///
/// Building one must stay cheap: it happens on every copy, and reading the
/// contents of a large image just to describe it would make copying slow.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Available {
    pub formats: Vec<ClipFormat>,
}

impl Available {
    pub fn is_empty(&self) -> bool {
        self.formats.is_empty()
    }

    pub fn has(&self, format: &ClipFormat) -> bool {
        self.formats.contains(format)
    }
}

/// Notices when the local clipboard changes.
pub trait Watch {
    /// Wait for the next change, and say what is now available.
    ///
    /// Returns `None` once the clipboard can no longer be watched, which for a
    /// display server means it has gone away.
    fn next_change(&mut self) -> Option<Available>;
}

/// Reads what is on the local clipboard.
pub trait Read {
    fn read(&mut self, format: &ClipFormat) -> Result<Vec<u8>>;
}

/// Puts another machine's clipboard onto this one.
pub trait Write {
    /// Offer these formats, to be fetched from `source` when something pastes.
    ///
    /// The contents are not supplied here. What the far machine copied may be
    /// large and may never be pasted at all, and reading it eagerly is what
    /// would put a ceiling on what can be shared.
    fn offer(&mut self, formats: &[ClipFormat], source: Box<dyn Fetch>) -> Result<()>;

    /// Give up ownership, so the local clipboard goes back to whatever had it.
    fn release(&mut self) -> Result<()>;
}

/// Fetches clipboard contents from wherever they actually are.
///
/// Called when something on this machine pastes, which may be a long time
/// after the copy, or never.
pub trait Fetch: Send {
    fn fetch(&self, format: &ClipFormat) -> Result<Vec<u8>>;
}
