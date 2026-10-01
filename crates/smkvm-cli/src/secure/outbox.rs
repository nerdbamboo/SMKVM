//! A way to say something from a thread that must not wait.
//!
//! Two threads in the worker are not allowed to stop: the one pumping
//! the clipboard window's messages, and the one capturing input. Both
//! had reasons to write to the service, and both did it the direct
//! way -- take the lock on the shared writer, write to the pipe, no
//! deadline. Each time that turned out to be the fault, it presented
//! as something else entirely: once as a clipboard held open for the
//! whole session, once as a diagnostic that printed one line and then
//! nothing, and once as a render that Windows never asked for again.
//!
//! The last of those is the reason this exists as its own thing. A
//! `WM_RENDERFORMAT` handler that returns without setting data has not
//! deferred the question, it has answered it with nothing, and Windows
//! never asks again. So a render that *blocks* is not merely slow --
//! whatever the blocking was, the person's clipboard is empty
//! afterwards and stays empty. A window-thread handler must therefore
//! never wait on anything another thread owns, and "never" has to be a
//! property of the code rather than a habit.
//!
//! This is that property, in one place. Posting cannot block: it
//! either takes the message, or refuses it and counts the refusal. A
//! caller that gets [`Posted::NoRoom`] knows immediately and can take
//! the path that renews the promise rather than the path that answers
//! nothing. Somebody draining the other end does the waiting, on a
//! thread where waiting is allowed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

/// What became of a message handed to an [`Outbox`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posted {
    /// Taken, and it is the drain's problem now.
    Sent,
    /// Refused because the queue is full, having refused this many in
    /// a row including this one.
    NoRoom(usize),
    /// Nobody is draining any more; there is no point posting again.
    Gone,
}

impl Posted {
    /// Did the message get anywhere?
    pub fn arrived(self) -> bool {
        matches!(self, Posted::Sent)
    }
}

/// Somewhere to put a message without waiting to see it delivered.
pub struct Outbox<T> {
    send: SyncSender<T>,
    dropped: AtomicUsize,
    /// How many are in the channel and not yet taken out of it.
    ///
    /// Kept here, and changed by both ends through this type, rather
    /// than inferred by subtracting one counter from another at the
    /// call sites. The derived number was reported as seventeen
    /// waiting while the drain sat idle, which is a state that
    /// cannot exist -- and because the two counters were
    /// independent, there was no way to tell a real backlog from
    /// arithmetic about two things that were not the same queue.
    ///
    /// A depth maintained by the queue itself cannot disagree with
    /// the queue.
    waiting: Arc<AtomicUsize>,
}

impl<T> Outbox<T> {
    /// A new outbox and the receiving end somebody has to drain.
    ///
    /// `room` is how many messages may be waiting. It wants to be
    /// generous, because a burst is exactly the moment the messages
    /// matter, and bounded, because an outbox nobody drains must not
    /// grow without limit.
    pub fn with_room_for(room: usize) -> (Outbox<T>, Drain<T>) {
        let (send, receive) = sync_channel(room);
        let waiting = Arc::new(AtomicUsize::new(0));
        (
            Outbox {
                send,
                dropped: AtomicUsize::new(0),
                waiting: waiting.clone(),
            },
            Drain { receive, waiting },
        )
    }

    /// How many are waiting to be taken out.
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }

    /// Hand over a message. Never waits, whatever the drain is doing.
    ///
    /// `try_send` rather than `send`, and that single word is the
    /// whole purpose of this type.
    pub fn post(&self, message: T) -> Posted {
        match self.send.try_send(message) {
            Ok(()) => {
                self.waiting.fetch_add(1, Ordering::Relaxed);
                Posted::Sent
            }
            Err(TrySendError::Full(_)) => {
                Posted::NoRoom(self.dropped.fetch_add(1, Ordering::Relaxed) + 1)
            }
            Err(TrySendError::Disconnected(_)) => Posted::Gone,
        }
    }

    /// How many have been refused since this was last asked, clearing
    /// the count.
    ///
    /// Asked by whoever is about to post something that *did* get
    /// through, so a gap in the record is admitted rather than hidden.
    /// A log that silently loses lines is worse than one that says it
    /// lost them, because the missing line is read as the event not
    /// having happened -- which has already cost a round here.
    pub fn missed(&self) -> usize {
        self.dropped.swap(0, Ordering::Relaxed)
    }

    /// Put refusals back on the count.
    ///
    /// For the case where the admission itself could not be posted.
    /// Taking the count and then losing the sentence that reports it
    /// would turn a queue that says it lost things into one that
    /// quietly does not.
    pub fn put_back(&self, n: usize) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
    }
}

/// The other end, which is the only way to take a message out.
///
/// A plain `Receiver` would do the job, and then the depth would be
/// maintained in one place and decremented in another, which is the
/// arrangement that produced a number nobody could trust. Taking a
/// message out goes through here so that the count and the queue
/// cannot come apart.
pub struct Drain<T> {
    receive: Receiver<T>,
    waiting: Arc<AtomicUsize>,
}

impl<T> Drain<T> {
    /// The next message, waiting until there is one.
    pub fn next(&self) -> Option<T> {
        let message = self.receive.recv().ok()?;
        self.waiting.fetch_sub(1, Ordering::Relaxed);
        Some(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_full_outbox_refuses_rather_than_waits() {
        // The invariant this type exists for. If this test ever hangs
        // rather than fails, the property is gone.
        let (out, _keep) = Outbox::with_room_for(2);
        assert_eq!(out.post(1), Posted::Sent);
        assert_eq!(out.post(2), Posted::Sent);
        let began = Instant::now();
        assert_eq!(out.post(3), Posted::NoRoom(1));
        assert!(
            began.elapsed() < Duration::from_millis(100),
            "posting to a full outbox waited {:?}",
            began.elapsed()
        );
    }

    #[test]
    fn refusals_are_counted_and_handed_over_once() {
        let (out, _keep) = Outbox::with_room_for(1);
        assert_eq!(out.post(1), Posted::Sent);
        assert_eq!(out.post(2), Posted::NoRoom(1));
        assert_eq!(out.post(3), Posted::NoRoom(2));
        assert_eq!(out.missed(), 2);
        // Handed over once, not for ever.
        assert_eq!(out.missed(), 0);
    }

    #[test]
    fn an_outbox_nobody_is_draining_says_so_instead_of_waiting() {
        let (out, drain) = Outbox::with_room_for(4);
        drop(drain);
        let began = Instant::now();
        assert_eq!(out.post(1), Posted::Gone);
        assert!(began.elapsed() < Duration::from_millis(100));
        // And `Gone` is not counted as a refusal: there is nothing to
        // catch up on later, so saying "1 line was dropped" next time
        // would be a lie about a queue that no longer exists.
        assert_eq!(out.missed(), 0);
    }

    #[test]
    fn an_admission_that_cannot_be_said_is_not_forgotten() {
        let (out, _keep) = Outbox::with_room_for(1);
        assert!(out.post(1).arrived());
        assert!(!out.post(2).arrived());
        let missed = out.missed();
        assert_eq!(missed, 1);
        // The report of it could not go out either, so it is owed
        // again rather than written off.
        out.put_back(missed);
        assert_eq!(out.missed(), 1);
    }

    #[test]
    fn what_was_posted_comes_out_in_the_order_it_went_in() {
        // One drain, in order. Two writers interleaving frames is what
        // the shared lock was protecting against, and an outbox keeps
        // that guarantee while giving up the waiting.
        let (out, drain) = Outbox::with_room_for(8);
        for i in 0..5 {
            assert!(out.post(i).arrived());
        }
        let seen: Vec<i32> = (0..5).map(|_| drain.next().unwrap()).collect();
        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn room_frees_up_again_once_the_drain_moves() {
        let (out, drain) = Outbox::with_room_for(1);
        assert!(out.post(1).arrived());
        assert!(!out.post(2).arrived());
        assert_eq!(drain.next().unwrap(), 1);
        assert!(out.post(3).arrived());
    }

    #[test]
    fn the_queue_keeps_its_own_count() {
        let (out, drain) = Outbox::with_room_for(8);
        assert_eq!(out.waiting(), 0);
        out.post(1);
        out.post(2);
        assert_eq!(out.waiting(), 2);
        drain.next();
        assert_eq!(out.waiting(), 1);
        drain.next();
        assert_eq!(out.waiting(), 0);
    }

    #[test]
    fn a_refused_message_is_not_counted_as_waiting() {
        // The number has to mean "in the queue", not "handed over at
        // some point", or an idle drain beside a non-zero count is
        // unreadable -- which is exactly how it was read.
        let (out, _keep) = Outbox::with_room_for(1);
        out.post(1);
        out.post(2);
        out.post(3);
        assert_eq!(out.waiting(), 1);
    }
}
