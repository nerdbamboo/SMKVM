//! How long each hop of a paste may take.
//!
//! Serving a paste in service mode is three waits nested inside one
//! another: the pasting application waits for the render, the render
//! waits for the worker to hear back from the service, and the service
//! waits for the machine that did the copying. Getting the arithmetic
//! wrong is not a matter of degree -- it produced the worst paste this
//! program has managed, on a real machine, and it is worth writing down
//! exactly how.
//!
//! The worker waited thirty seconds. The service's own patience was
//! also thirty. So when a fetch was slow the worker gave up at the
//! exact moment the answer might have arrived; the answer was then
//! discarded as belonging to nobody; `WM_RENDERALLFORMATS` asked again;
//! that second attempt queued behind the first fetch, which still held
//! the lock, for another thirty seconds; and the data it eventually got
//! was written into a clipboard that had been closed for a minute.
//! Sixty seconds, then `ERROR_CLIPBOARD_NOT_OPEN`, and on the person's
//! screen simply nothing at all.
//!
//! Two rules came out of it, and the tests below are what keep them.
//!
//! **Every inner wait expires before the wait outside it**, with room
//! to spare. The moment an inner deadline outlives an outer one, an
//! answer arrives for a question nobody is waiting on, and the next
//! attempt queues behind work already abandoned.
//!
//! **A deadline that cannot be met is not made longer.** A fetch too
//! slow for a paste is given up on, and what it eventually returns is
//! kept for the next paste rather than thrown away. The worst case is
//! then one paste that does nothing and a second that is instant --
//! which is a thing a person can understand and work with, unlike a
//! minute of silence.

use std::time::Duration;

/// The outermost: how long the pasting application will wait.
///
/// Owned by `smkvm-clipboard`, because that is what serves the render;
/// named here so the whole ladder reads in one place.
#[cfg_attr(not(test), allow(dead_code))]
pub const RENDER_BUDGET: Duration = smkvm_clipboard::RENDER_BUDGET;

/// How long the worker waits, inside a render, for the service.
pub const PASTE_WITHIN: Duration = Duration::from_millis(2_500);

/// How long the service gives the machine that did the copying.
pub const SERVICE_FETCH_WITHIN: Duration = Duration::from_millis(1_800);

/// The least each hop must leave the one outside it.
///
/// Only the tests use it, and that is the point: it is the margin the
/// arithmetic is required to keep, not a value anything waits for.
#[cfg(test)]
///
/// Not zero, because a pipe write, a thread wake and a lock acquisition
/// all happen between the hops and none of them is instant. Half a
/// second is far more than those need and still leaves the whole thing
/// inside four.
pub const HEADROOM: Duration = Duration::from_millis(500);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_wait_expires_before_the_wait_outside_it() {
        // The invariant the sixty-second fault broke. Worth a test that
        // runs on any machine, because the code it governs runs on
        // exactly one and this arithmetic is the whole of it.
        assert!(
            SERVICE_FETCH_WITHIN + HEADROOM <= PASTE_WITHIN,
            "the service must give up, with room to spare, before the worker stops \
             listening: {SERVICE_FETCH_WITHIN:?} + {HEADROOM:?} > {PASTE_WITHIN:?}"
        );
        assert!(
            PASTE_WITHIN + HEADROOM <= RENDER_BUDGET,
            "the worker must give up, with room to spare, before the clipboard closes: \
             {PASTE_WITHIN:?} + {HEADROOM:?} > {RENDER_BUDGET:?}"
        );
    }

    #[test]
    fn the_whole_thing_is_over_before_a_person_gives_up_on_it() {
        // A paste that does nothing is survivable; a paste that hangs
        // an application for a minute is what this replaced. Four
        // seconds is about as long as somebody will sit with a frozen
        // window before deciding it is broken.
        assert!(RENDER_BUDGET <= Duration::from_secs(5));
    }

    #[test]
    fn no_hop_is_so_short_that_an_ordinary_fetch_cannot_finish() {
        // The other direction, and the one a well-meaning tightening
        // would break: text across a local network is milliseconds, but
        // the far machine has to be woken, read its own clipboard and
        // answer. Anything under a second here would make the first
        // paste fail every time rather than occasionally.
        assert!(SERVICE_FETCH_WITHIN >= Duration::from_millis(1_000));
    }
}
