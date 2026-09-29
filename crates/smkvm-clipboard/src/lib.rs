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

// Reaching a platform's clipboard means calling into it, which is the one
// place unsafe is unavoidable. It is confined to the platform backends; the
// format conversions and everything above them are held to the original rule.
#![deny(unsafe_code)]

pub mod files;
pub mod html;
pub mod image;
pub mod platform;

use smkvm_proto::ClipFormat;

/// How long a paste may take before what it produces is worthless.
///
/// A paste is served while the pasting application waits, and on
/// Windows the clipboard is open only for the length of that. Take
/// longer than the requester's patience and the data is handed to a
/// clipboard that closed while it was being fetched -- which is worse
/// than handing over nothing, because the person gets an empty paste
/// long after pressing the keys and a warning they will never see.
/// That happened on a real machine: sixty seconds, then a successful
/// fetch into `ERROR_CLIPBOARD_NOT_OPEN`.
///
/// Here rather than in the Windows backend because it is not a fact
/// about Windows -- it is the outermost of a set of nested deadlines,
/// and every wait inside a paste has to expire before it does. Putting
/// it where every build can see it is what lets that ordering be
/// tested on a machine that cannot run any of the code it governs.
pub const RENDER_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

/// Somewhere to say things that must not be lost.
///
/// This crate logs through `tracing` like everything else, and in the
/// worker that has been unreliable in a way nobody has pinned down --
/// three rounds of diagnosis have been spent unable to tell "the code
/// did not run" from "the code ran and said nothing". The caller can
/// set this to a second, independent way of speaking: in the worker it
/// relays down the pipe to the service, which is the one channel known
/// to work because it carries every keystroke.
///
/// Set once, by whoever knows where words should go. Never read for
/// anything but saying something.
type Saying = Box<dyn Fn(&str) + Send + Sync>;
static WITNESS: std::sync::OnceLock<Saying> = std::sync::OnceLock::new();

/// Say where this crate's most important lines should also go.
pub fn witness_through(say: Saying) {
    let _ = WITNESS.set(say);
}

/// Say something twice: once into the log, once wherever
/// [`witness_through`] points.
///
/// For the handful of things whose absence is itself the diagnosis --
/// a window being made, a promise being taken, a render being entered.
/// A paste is a human action, so none of these is frequent enough to
/// be worth being quiet about.
// Only the Windows backend has anything whose absence is itself the
// diagnosis; on X11 the clipboard runs in the person's own session and
// none of this arises.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn witness(text: &str) {
    // Prefixed here rather than by whoever relays it, so that one
    // search finds these lines whichever way they arrived. Under the
    // scheduled task there is no worker and no relay and they reach
    // the log straight from here; under the service they arrive again
    // through the pipe, where the worker's own prefix makes them
    // `worker: clipboard: ...`. Somebody reading a log at a machine
    // with a consent prompt in the way should not have to know which
    // arrangement produced it in order to grep for the thing they
    // need.
    let said = format!("clipboard: {text}");
    tracing::info!("{said}");
    if let Some(say) = WITNESS.get() {
        say(&said);
    }
}

/// How a render ended.
///
/// An enum rather than a `bool` threaded through the handler, because
/// the renewal was added for the path where the contents do not
/// arrive, and there was nothing stopping it running on the path where
/// they do. Renewing a promise that was just kept is not a small
/// mistake -- it re-takes the clipboard, which empties it, which
/// throws away the very data that was handed over a moment earlier.
///
/// So the decision lives in one place, [`Rendered::needs_renewing`],
/// and is tested. A success path that shares code with a failure path
/// needs a test that it does not do the failure path's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rendered {
    /// Handed over, with this many bytes.
    Served(usize),
    /// Nothing is on offer, so there was nothing to render. Whoever
    /// asked will get nothing, and that is correct.
    NothingOffered,
    /// A format that was never promised. Windows asks for the ones it
    /// synthesises too, and those are not ours to supply.
    NotOurFormat,
    /// This thread holds no clipboard state at all, which should not
    /// happen.
    NoState,
    /// The contents could not be got in time.
    CouldNotFetch(String),
    /// They arrived, but too late to be worth handing over.
    TooLate(u64),
    /// They were there and the system would not take them.
    CouldNotHandOver(String),
}

impl Rendered {
    /// Should the promise be made again?
    ///
    /// Only when the promise was *not* kept and might be next time.
    /// Not when it was kept -- that would empty the clipboard we just
    /// filled. Not when there was nothing to keep, because renewing an
    /// offer that does not exist is churn with no end to it.
    pub fn needs_renewing(&self) -> bool {
        match self {
            Rendered::CouldNotFetch(_) | Rendered::TooLate(_) | Rendered::CouldNotHandOver(_) => {
                true
            }
            Rendered::Served(_)
            | Rendered::NothingOffered
            | Rendered::NotOurFormat
            | Rendered::NoState => false,
        }
    }

    /// What to put in the log, every time, whichever way it went.
    ///
    /// Including the success: the absence of a line was doing the work
    /// of evidence, and that only worked because somebody could see
    /// the log continue with other traffic.
    pub fn said(&self) -> String {
        match self {
            Rendered::Served(bytes) => format!("served it, {bytes} bytes"),
            Rendered::NothingOffered => "nothing is on offer, so nothing was served".into(),
            Rendered::NotOurFormat => {
                "this format was never promised, so it is not ours to serve".into()
            }
            Rendered::NoState => "this thread holds no clipboard state at all".into(),
            Rendered::CouldNotFetch(why) => format!("could not get the contents: {why}"),
            Rendered::TooLate(ms) => {
                format!("the contents took {ms} ms, too late to hand over")
            }
            Rendered::CouldNotHandOver(why) => {
                format!("the system would not take the contents: {why}")
            }
        }
    }
}

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

/// Something a drag catcher needs done to the pointer while it works.
///
/// Catching a drag means standing a window under the pointer and letting the
/// application doing the dragging notice it, which takes a movement, and then
/// letting go of the button on that application's behalf. The catcher owns no
/// way to move a pointer; the caller does, and drives it through this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drive {
    /// Put the pointer here, in this machine's own coordinates.
    MoveTo(i32, i32),
    /// Let go of the left button.
    ReleaseLeft,
}

/// Picks up the files of a drag in progress, as the cursor leaves.
///
/// An application dragging files tells only the window under the pointer
/// what it carries. So the moment the cursor leaves this screen with the
/// button held, a window of this program's is put under the pointer, the
/// pointer nudged so the application notices it, and the button released so
/// the drag ends there -- with the file list in hand and nothing moved or
/// copied on this machine. If nothing was being dragged, no window is told
/// anything and the button is left alone.
pub trait CatchDrag: Send {
    /// Returns the files being dragged, or `None` if no drag was in progress
    /// or it could not be read in time. Blocks for a fraction of a second.
    fn catch(&mut self, drive: &mut dyn FnMut(Drive)) -> Option<Vec<std::path::PathBuf>>;
}

#[cfg(test)]
mod rendered_tests {
    use super::Rendered;

    #[test]
    fn a_promise_that_was_kept_is_never_made_again() {
        // The whole reason this is an enum. Renewing after a success
        // re-takes the clipboard, which empties it, which throws away
        // the data handed over a moment before -- the opposite of what
        // the renewal was added to do.
        assert!(!Rendered::Served(17).needs_renewing());
    }

    #[test]
    fn a_promise_that_was_not_kept_is_made_again() {
        assert!(Rendered::CouldNotFetch("timed out".into()).needs_renewing());
        assert!(Rendered::TooLate(9_000).needs_renewing());
        assert!(Rendered::CouldNotHandOver("clipboard not open".into()).needs_renewing());
    }

    #[test]
    fn nothing_to_keep_is_not_a_promise_to_renew() {
        // Windows asks for the formats it synthesises from ours, and
        // those were never promised. Renewing for them would re-take
        // the clipboard on every paste, for ever.
        assert!(!Rendered::NotOurFormat.needs_renewing());
        assert!(!Rendered::NothingOffered.needs_renewing());
        assert!(!Rendered::NoState.needs_renewing());
    }

    #[test]
    fn every_outcome_says_something() {
        for outcome in [
            Rendered::Served(1),
            Rendered::NothingOffered,
            Rendered::NotOurFormat,
            Rendered::NoState,
            Rendered::CouldNotFetch("x".into()),
            Rendered::TooLate(1),
            Rendered::CouldNotHandOver("y".into()),
        ] {
            assert!(!outcome.said().is_empty(), "{outcome:?} says nothing");
        }
    }
}
