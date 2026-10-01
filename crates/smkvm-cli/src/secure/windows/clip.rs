//! The person's clipboard, reached from a process that is not in their
//! session.
//!
//! The same fault as the desktop one, a layer along, and found the same
//! way -- on the machine, by somebody trying to paste. `client.rs` opens
//! the clipboard through `platform::clipboard()`, and in service mode
//! that runs in session 0. Session 0 has a clipboard of its own, and it
//! is not the one the person copies into. So the daemon was watching a
//! clipboard nobody writes to and offering onto a clipboard nobody
//! reads, perfectly, for ever. The drag catcher was in the same place
//! for the same reason.
//!
//! The cure is the seam that already works for input. The exchange --
//! what has been copied, what has been announced, what is being fetched
//! -- stays in the service with the link and the state machine, because
//! that is where the network is and it is the part with the decisions
//! in it. The worker owns the actual clipboard, because it is the only
//! process of ours in the session. They talk over the pipe that is
//! already there.
//!
//! ## Which direction each thing goes
//!
//! Three of the four are ordinary: the service asks and the worker
//! answers, or the service tells and the worker does. Reading the
//! clipboard, offering onto it, releasing it, catching a drag, dropping
//! files.
//!
//! The fourth runs backwards and is the only genuinely new mechanism
//! here. Offering is deliberately lazy: a copy announces what is
//! available and the contents are fetched only if something actually
//! pastes, so that a screenshot nobody pastes costs one small message.
//! But the thing that pastes is on the *worker's* desktop, and the
//! contents are on another machine entirely, reachable only through the
//! service. So the worker asks the service, mid-paste, and waits.
//! [`FromWorker::WantsPaste`] is that question and [`ToWorker::Pasted`]
//! is the answer.
//!
//! ## What a paste costs while it waits
//!
//! On Windows the answer is rendered inside `WM_RENDERFORMAT`, which
//! blocks the pasting application until it returns. That was already
//! true before any of this -- `docs/NOTES.md` has it as a known
//! shortcoming -- and the pipe adds one more hop to a wait that was
//! always a network round trip. It does not add a new kind of problem,
//! and it is bounded: a paste that is not answered within
//! [`PASTE_WITHIN`] gives up, so the worst case is an application that
//! stalls for that long rather than for ever.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smkvm_clipboard::{CatchDrag, ClipboardError, Drive, Fetch, Read, Write};
use smkvm_proto::ClipFormat;

use crate::secure::windows::link::Link;
use crate::secure::wire::ToWorker;

/// How long to wait for the worker to answer a question about the
/// clipboard.
///
/// Reading is a local operation on the far side of a pipe: microseconds,
/// plus whatever the application that owns the clipboard takes to hand
/// its contents over, which can be a moment for a large image. Five
/// seconds is generous for that and short enough that a worker being
/// replaced does not hold up the exchange.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(5);

pub use crate::secure::budget::{PASTE_WITHIN, SERVICE_FETCH_WITHIN};

/// Questions asked over the pipe that are still waiting for an answer.
///
/// One table, keyed by a number that only ever goes up. The number
/// matters: without it, two pastes of different formats in flight at
/// once would each take whichever answer arrived first, which is a
/// clipboard that hands over the wrong thing rather than one that
/// fails. Tested, because it is the part that has nothing to do with
/// Windows.
#[derive(Default)]
pub struct Waiting<T> {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<T>>>,
}

impl<T> Waiting<T> {
    pub fn new() -> Waiting<T> {
        Waiting {
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Take a number and a place for the answer to arrive.
    pub fn ask(&self) -> (u64, Receiver<T>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending.lock().expect("not poisoned").insert(id, tx);
        (id, rx)
    }

    /// An answer came back. False when nothing was waiting for it, which
    /// is an answer that arrived after its asker gave up.
    pub fn answer(&self, id: u64, value: T) -> bool {
        let taken = self.pending.lock().expect("not poisoned").remove(&id);
        match taken {
            // The receiver may itself have gone, which is the same case.
            Some(tx) => tx.send(value).is_ok(),
            None => false,
        }
    }

    /// Give up on one question.
    pub fn forget(&self, id: u64) {
        self.pending.lock().expect("not poisoned").remove(&id);
    }

    /// Give up on all of them, because the worker has gone.
    ///
    /// Dropping the senders is what wakes everybody waiting: a receive
    /// on a channel whose sender is gone returns at once rather than
    /// running out its own timeout. Without this, every question in
    /// flight when a worker died would sit for its full wait.
    pub fn nobody_is_answering(&self) {
        self.pending.lock().expect("not poisoned").clear();
    }

    /// How many are still waiting. For the tests, which is where the
    /// "an answer goes to the question that asked it" property lives.
    #[cfg(test)]
    pub fn outstanding(&self) -> usize {
        self.pending.lock().expect("not poisoned").len()
    }
}

fn as_error(e: String) -> ClipboardError {
    ClipboardError::Display(e)
}

fn no_worker() -> ClipboardError {
    ClipboardError::Display(
        "no worker holds the clipboard on the desktop the person is using".into(),
    )
}

/// Reading what is on the person's clipboard.
pub struct ReadThroughWorker(pub Arc<Link>);

impl Read for ReadThroughWorker {
    fn read(&mut self, format: &ClipFormat) -> smkvm_clipboard::Result<Vec<u8>> {
        let (id, answer) = self.0.clipboard_reads().ask();
        if !self.0.say(&ToWorker::ReadClipboard {
            id,
            format: format.clone(),
        }) {
            self.0.clipboard_reads().forget(id);
            return Err(no_worker());
        }
        match answer.recv_timeout(ANSWER_WITHIN) {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) => Err(as_error(e)),
            Err(_) => {
                self.0.clipboard_reads().forget(id);
                Err(as_error(format!(
                    "the worker did not answer within {} s",
                    ANSWER_WITHIN.as_secs()
                )))
            }
        }
    }
}

/// Putting another machine's clipboard onto the person's.
pub struct WriteThroughWorker(pub Arc<Link>);

impl Write for WriteThroughWorker {
    fn offer(
        &mut self,
        formats: &[ClipFormat],
        source: Box<dyn Fetch>,
    ) -> smkvm_clipboard::Result<()> {
        // Remembered as well as sent. A worker is replaced every time
        // the input moves to a consent prompt and back, and its
        // clipboard window goes with it; without keeping the offer here
        // the person would find that what another machine copied had
        // stopped being available because a prompt had appeared in the
        // meantime. `Link::attach` re-offers.
        self.0.hold_offer(formats.to_vec(), source);
        if !self.0.say(&ToWorker::OfferClipboard {
            formats: formats.to_vec(),
        }) {
            // Kept anyway: the next worker on the ordinary desktop gets
            // it. Not being able to offer *now* is not the offer being
            // withdrawn.
            return Err(no_worker());
        }
        Ok(())
    }

    fn release(&mut self) -> smkvm_clipboard::Result<()> {
        self.0.drop_offer();
        if !self.0.say(&ToWorker::ReleaseClipboard) {
            // Nothing is holding it, which is what release wanted.
            return Ok(());
        }
        Ok(())
    }
}

/// Catching a drag as the cursor leaves.
pub struct DragThroughWorker(pub Arc<Link>);

impl CatchDrag for DragThroughWorker {
    fn catch(&mut self, _drive: &mut dyn FnMut(Drive)) -> Option<Vec<PathBuf>> {
        // The driver is not used, and that is deliberate rather than an
        // omission. Catching a drag means standing a window under the
        // pointer, nudging the pointer so the dragging application
        // notices, and releasing the button -- all of which the worker
        // does to its own desktop, with its own pointer, without a
        // round trip per movement. Driving it from here would send each
        // nudge down the pipe and back while an application waits.
        let (id, answer) = self.0.drags().ask();
        if !self.0.say(&ToWorker::CatchDrag { id }) {
            self.0.drags().forget(id);
            return None;
        }
        match answer.recv_timeout(ANSWER_WITHIN) {
            Ok(paths) if paths.is_empty() => None,
            Ok(paths) => Some(paths),
            Err(_) => {
                self.0.drags().forget(id);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_goes_to_the_question_that_asked_it() {
        // Two pastes of different formats can be in flight at once, and
        // without the number each would take whichever answer came
        // first -- a clipboard that hands over the wrong thing, which is
        // worse than one that fails.
        let waiting: Waiting<u32> = Waiting::new();
        let (one, first) = waiting.ask();
        let (two, second) = waiting.ask();
        assert_ne!(one, two);
        assert!(waiting.answer(two, 22));
        assert!(waiting.answer(one, 11));
        assert_eq!(first.recv().unwrap(), 11);
        assert_eq!(second.recv().unwrap(), 22);
    }

    #[test]
    fn an_answer_nobody_is_waiting_for_is_dropped_rather_than_kept() {
        let waiting: Waiting<u32> = Waiting::new();
        assert!(!waiting.answer(7, 1));
        let (id, _) = waiting.ask();
        waiting.forget(id);
        assert!(!waiting.answer(id, 1));
        assert_eq!(waiting.outstanding(), 0);
    }

    #[test]
    fn a_question_is_only_answered_once() {
        let waiting: Waiting<u32> = Waiting::new();
        let (id, answer) = waiting.ask();
        assert!(waiting.answer(id, 1));
        assert!(!waiting.answer(id, 2));
        assert_eq!(answer.recv().unwrap(), 1);
    }

    #[test]
    fn a_worker_going_away_wakes_everybody_waiting_at_once() {
        // Otherwise every question in flight sits out its full timeout,
        // and the exchange stalls for seconds over a worker that is
        // already gone.
        let waiting: Waiting<u32> = Waiting::new();
        let (_, first) = waiting.ask();
        let (_, second) = waiting.ask();
        assert_eq!(waiting.outstanding(), 2);
        waiting.nobody_is_answering();
        assert_eq!(waiting.outstanding(), 0);
        assert!(first.recv().is_err());
        assert!(second.recv().is_err());
    }

    #[test]
    fn numbers_are_never_reused_within_a_run() {
        let waiting: Waiting<u32> = Waiting::new();
        let mut seen = Vec::new();
        for _ in 0..100 {
            let (id, _) = waiting.ask();
            waiting.forget(id);
            seen.push(id);
        }
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len());
    }
}
