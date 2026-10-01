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

use std::collections::HashMap;
use std::time::{Duration, Instant};

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
type Saying = Box<dyn Fn(&str, bool) + Send + Sync>;
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
/// Say something that happens once per human action.
///
/// The offer arriving, the offer being let go, a render that could
/// not be served: things somebody would want to see once, and which
/// cannot repeat faster than a person can copy and paste.
pub(crate) fn witness(text: &str) {
    witness_at(Loudly::Always, text)
}

/// Say something that happens once per *machine* action, and may
/// therefore happen very fast indeed.
///
/// Entering a render, promising a format, taking the clipboard. These
/// earned their place while this path was blind, and they have been
/// read for the last time by anybody who is not debugging: eighty-five
/// identical render lines inside one second is not a record of
/// anything. They go to the log at debug and are rate-limited on the
/// way, so a spin shows up as a handful of lines and a count rather
/// than as a megabyte.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn step(text: &str) {
    witness_at(Loudly::OnlyWhenDebugging, text)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Loudly {
    Always,
    OnlyWhenDebugging,
}

static REPEATS: std::sync::OnceLock<Throttle> = std::sync::OnceLock::new();

fn witness_at(loudly: Loudly, text: &str) {
    // Prefixed here rather than by whoever relays it, so that one
    // search finds these lines whichever way they arrived. Under the
    // scheduled task there is no worker and no relay and they reach
    // the log straight from here; under the service they arrive again
    // through the pipe, where the worker's own prefix makes them
    // `worker: clipboard: ...`. Somebody reading a log at a machine
    // with a consent prompt in the way should not have to know which
    // arrangement produced it in order to grep for the thing they
    // need.
    //
    // Throttled before anything else is done with it, including the
    // formatting, because on the hot path the cost of these lines is
    // itself part of the problem.
    let throttle = REPEATS.get_or_init(Throttle::new);
    let said = match throttle.asked_at(text, Instant::now()) {
        Say::No => return,
        Say::Yes => format!("clipboard: {text}"),
        Say::YesAfter(held_back) => {
            format!("clipboard: {text} [and {held_back} more like it in the last second]")
        }
    };
    match loudly {
        Loudly::Always => tracing::info!("{said}"),
        Loudly::OnlyWhenDebugging => tracing::debug!("{said}"),
    }
    if let Some(say) = WITNESS.get() {
        // The relay carries the level too, so a step does not arrive
        // at the service as something a person is meant to read.
        say(&said, loudly == Loudly::Always);
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

/// How often one repeated line may be said, and how many of them in
/// that time.
const SAME_LINE_WITHIN: Duration = Duration::from_secs(1);
const SAME_LINE_AT_MOST: u32 = 2;

/// What to do with a line that may be one of very many identical ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Say {
    /// Say it.
    Yes,
    /// Say it, and mention this many that were held back since the
    /// last one got through.
    YesAfter(u32),
    /// Hold it back; one just like it was said a moment ago.
    No,
}

/// Lets a line through a few times a second and counts the rest.
///
/// This exists because the diagnostics in this crate became the
/// problem they were added to solve. A log of 62 MB with
/// eighty-five identical render lines inside one second is not an
/// instrument: the lines cost real work on a thread that must not be
/// slowed, they filled the worker's outbox until it was discarding
/// two hundred thousand messages at a time, and -- worst of the
/// three -- a reader could not find anything in it, which produced a
/// wrong conclusion that cost a round.
///
/// A repeated line carries almost no information after the second
/// copy. What matters is that it happened, and how often. So the
/// first couple in each second go out and the rest are counted, and
/// the count is handed to the next one that gets through, exactly as
/// the worker's outbox does with messages it had to refuse.
///
/// Keyed on the text itself, so two different lines never throttle
/// each other and a line that genuinely varies is never held back.
#[derive(Default)]
pub struct Throttle {
    seen: std::sync::Mutex<HashMap<String, Spell>>,
}

#[derive(Clone, Copy)]
struct Spell {
    began: Instant,
    said: u32,
    held_back: u32,
}

impl Throttle {
    pub fn new() -> Throttle {
        Throttle::default()
    }

    /// Decide what to do with this line, as of `now`.
    pub fn asked_at(&self, line: &str, now: Instant) -> Say {
        let mut seen = self.seen.lock().expect("not poisoned");
        // Keeps the table from growing without bound in a long run:
        // anything not seen for a while cannot be in a burst.
        if seen.len() > 512 {
            seen.retain(|_, spell| now.duration_since(spell.began) < SAME_LINE_WITHIN);
        }
        let spell = seen.entry(line.to_string()).or_insert(Spell {
            began: now,
            said: 0,
            held_back: 0,
        });
        if now.duration_since(spell.began) >= SAME_LINE_WITHIN {
            let held_back = spell.held_back;
            *spell = Spell {
                began: now,
                said: 1,
                held_back: 0,
            };
            return if held_back > 0 {
                Say::YesAfter(held_back)
            } else {
                Say::Yes
            };
        }
        if spell.said < SAME_LINE_AT_MOST {
            spell.said += 1;
            let held_back = std::mem::take(&mut spell.held_back);
            return if held_back > 0 {
                Say::YesAfter(held_back)
            } else {
                Say::Yes
            };
        }
        spell.held_back += 1;
        Say::No
    }
}

/// What to say about one delayed-rendering promise, given what the
/// three questions answered.
///
/// `handed_back` is whether `SetClipboardData` returned a handle,
/// `last` is the error number left behind when it did not, and
/// `listed` is what `IsClipboardFormatAvailable` said afterwards.
///
/// This is a pure function with tests because getting it wrong once
/// already cost a round. `SetClipboardData` returns the handle it was
/// given, and a delayed-render promise gives it a null one -- so on
/// success it returns null, which the Rust binding cannot tell from
/// failure and reports as an error carrying whatever `GetLastError`
/// happened to hold. Read naively that says the promise was refused
/// at the moment it was made. The way out is the one Win32 has always
/// documented for a call that can return null on success: clear the
/// error first, and if the result is null with the error still clear,
/// the call succeeded.
pub fn promise_said(handed_back: bool, last: u32, listed: bool) -> String {
    let made = handed_back || last == 0;
    let how = if handed_back {
        "promised".to_string()
    } else if last == 0 {
        // The ordinary case, and it happens every single time.
        "promised (returned null, which is the promise itself)".to_string()
    } else {
        format!("REFUSED (error {last})")
    };
    let seen = match (listed, made) {
        (true, _) => "listed",
        // A promise the system accepted but does not list is the one
        // combination that means nothing can ever paste it.
        (false, true) => "NOT LISTED -- nothing can paste it",
        (false, false) => "not listed, which agrees",
    };
    if !made && listed {
        // Said loudly because reading half of this is how a refusal
        // that never happened became a whole round of work.
        format!("{how} BUT {seen} -- these disagree; trust the listing")
    } else {
        format!("{how}, {seen}")
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
    use super::{promise_said, Rendered, Say, Throttle};
    use std::time::{Duration, Instant};

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

    #[test]
    fn a_null_return_with_no_error_is_the_promise_not_a_refusal() {
        // The whole point. This is what every successful promise looks
        // like from Rust, and calling it a refusal cost a round.
        let said = promise_said(false, 0, true);
        assert!(said.contains("promised"), "{said}");
        assert!(!said.contains("REFUSED"), "{said}");
    }

    #[test]
    fn a_stale_error_number_does_not_make_it_a_refusal_when_the_format_is_listed() {
        // What was actually seen on the machine: error 6, and the
        // format listed in the same breath. Both halves are reported,
        // and the contradiction is named rather than resolved
        // silently.
        let said = promise_said(false, 6, true);
        assert!(said.contains("disagree"), "{said}");
        assert!(said.contains("trust the listing"), "{said}");
    }

    #[test]
    fn a_promise_the_system_took_but_does_not_list_is_the_one_that_matters() {
        let said = promise_said(true, 0, false);
        assert!(said.contains("NOT LISTED"), "{said}");
    }

    #[test]
    fn a_refusal_the_listing_agrees_with_reads_as_a_refusal() {
        let said = promise_said(false, 6, false);
        assert!(said.contains("REFUSED"), "{said}");
        assert!(!said.contains("disagree"), "{said}");
    }

    #[test]
    fn the_first_few_of_a_burst_get_through_and_the_rest_are_counted() {
        let throttle = Throttle::new();
        let now = Instant::now();
        assert_eq!(throttle.asked_at("render", now), Say::Yes);
        assert_eq!(throttle.asked_at("render", now), Say::Yes);
        // The eighty-three others in that second.
        for _ in 0..83 {
            assert_eq!(throttle.asked_at("render", now), Say::No);
        }
        // The next second admits what it held back rather than
        // pretending the burst did not happen.
        let later = now + Duration::from_millis(1100);
        assert_eq!(throttle.asked_at("render", later), Say::YesAfter(83));
    }

    #[test]
    fn different_lines_do_not_hold_each_other_back() {
        let throttle = Throttle::new();
        let now = Instant::now();
        for i in 0..50 {
            // A line that genuinely varies says something new every
            // time, and must never be suppressed by its neighbours.
            assert_eq!(throttle.asked_at(&format!("paste {i}"), now), Say::Yes);
        }
    }

    #[test]
    fn a_quiet_line_is_never_held_back() {
        let throttle = Throttle::new();
        let mut now = Instant::now();
        for _ in 0..20 {
            assert_eq!(throttle.asked_at("a copy settled", now), Say::Yes);
            now += Duration::from_secs(2);
        }
    }

    #[test]
    fn the_table_does_not_grow_without_bound() {
        let throttle = Throttle::new();
        let now = Instant::now();
        for i in 0..600 {
            throttle.asked_at(&format!("line {i}"), now);
        }
        let later = now + Duration::from_secs(5);
        throttle.asked_at("one more", later);
        assert!(
            throttle.seen.lock().unwrap().len() < 600,
            "stale entries were never cleared"
        );
    }
}
