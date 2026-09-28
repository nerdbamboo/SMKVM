//! Whether input is reaching the screen, when the screen is not this
//! process's.
//!
//! `platform::injection_blocked` and `platform::injection_possible` decide
//! whether the client hands the cursor back and when it takes it again.
//! Both answer by looking around the process that asks: which desktop has
//! the input, what is in front of it, does a probe injection land. In the
//! daemon that is exactly right, because the process that asks is the
//! process that injects.
//!
//! In the service it is exactly wrong, and wrong in the silent direction.
//! The service lives in session 0 on the window station
//! `Service-0x0-3e7$`, which has no screen and never will. Asked from
//! there, `desktop::current()` says `OutOfReach` and `can_inject()` says
//! no, for ever -- so the client suspends at the first refusal and never
//! resumes, whatever the worker is doing. The machine would look exactly
//! like the bug this whole change exists to fix, with a service running.
//!
//! The answer in service mode comes from the worker instead, and it is
//! shorter than the local one because of who the worker is. The worker
//! runs as the system account on whichever desktop has the input, so there
//! is no desktop it is on the wrong side of and no window in front of it
//! that outranks it -- that is the entire point of the arrangement. What
//! is left to ask is only whether there is a worker at all, and whether
//! the last thing sent to it was refused.
//!
//! Kept here, apart from the pipe and the Win32 calls, because it is the
//! part with a decision in it and the part where a mistake is a cursor
//! that never comes back.

use std::time::{Duration, Instant};

/// How long a refusal is believed for.
///
/// A worker reports a refusal after the fact, over a pipe, so by the time
/// it is read the cause may be gone. Suspending on it is right -- the
/// pointer has stopped either way -- but staying suspended on the strength
/// of one old refusal is how a cursor never comes home. After this, the
/// question is asked again by trying.
pub const REFUSAL_STANDS_FOR: Duration = Duration::from_secs(2);

/// What the service knows about its worker's reach.
#[derive(Debug, Default, Clone, Copy)]
pub struct Reach {
    attached: bool,
    refused_at: Option<Instant>,
}

/// Why input is not landing, when it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// There is no worker on the input desktop. Either one is being
    /// started, or the service gave up on that desktop -- and either way
    /// the cursor belongs back on the server meanwhile.
    NoWorker,
    /// There is one, and it says the system turned an injection down.
    Refused,
}

impl Reach {
    /// A worker connected, or went.
    pub fn attached(&mut self, attached: bool) {
        self.attached = attached;
        // A new worker is a new question. Carrying the last one's refusal
        // across would suspend the cursor over something that happened on
        // a desktop that is no longer there.
        self.refused_at = None;
    }

    /// The worker reported a refused injection.
    pub fn refused(&mut self, at: Instant) {
        self.refused_at = Some(at);
    }

    /// Is input reaching the screen right now?
    pub fn blocked(&self, now: Instant) -> Option<Blocked> {
        if !self.attached {
            return Some(Blocked::NoWorker);
        }
        match self.refused_at {
            Some(at) if now.saturating_duration_since(at) < REFUSAL_STANDS_FOR => {
                Some(Blocked::Refused)
            }
            _ => None,
        }
    }

    pub fn possible(&self, now: Instant) -> bool {
        self.blocked(now).is_none()
    }
}

impl Blocked {
    /// What to say in the log, which is where somebody diagnosing a
    /// pointer that will not move is going to look first.
    pub fn why(self) -> &'static str {
        match self {
            Blocked::NoWorker => {
                "there is no worker on the desktop that has the input, so nothing sent \
                 from here would land there. One is being started, or could not be"
            }
            Blocked::Refused => {
                "the worker on the input desktop says the system turned its last \
                 injection down"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_worker_means_the_cursor_goes_home() {
        let reach = Reach::default();
        assert_eq!(reach.blocked(Instant::now()), Some(Blocked::NoWorker));
        assert!(!reach.possible(Instant::now()));
    }

    #[test]
    fn a_worker_on_the_input_desktop_is_the_whole_of_the_answer() {
        // Deliberately not asking about integrity levels or which desktop
        // this process is on: the worker is the system account on the
        // desktop that has the input, so neither question has a second
        // answer.
        let mut reach = Reach::default();
        reach.attached(true);
        assert_eq!(reach.blocked(Instant::now()), None);
        assert!(reach.possible(Instant::now()));
    }

    #[test]
    fn a_refusal_suspends_and_then_stops_suspending() {
        let mut reach = Reach::default();
        let now = Instant::now();
        reach.attached(true);
        reach.refused(now);
        assert_eq!(reach.blocked(now), Some(Blocked::Refused));
        assert_eq!(
            reach.blocked(now + REFUSAL_STANDS_FOR - Duration::from_millis(1)),
            Some(Blocked::Refused)
        );
        // The cursor comes home and then comes back. A refusal that
        // suspended for ever would be the original bug wearing a new hat.
        assert_eq!(reach.blocked(now + REFUSAL_STANDS_FOR), None);
    }

    #[test]
    fn a_new_worker_does_not_inherit_the_old_ones_refusal() {
        let mut reach = Reach::default();
        let now = Instant::now();
        reach.attached(true);
        reach.refused(now);
        // The input moved and a worker started on the new desktop.
        reach.attached(false);
        reach.attached(true);
        assert_eq!(reach.blocked(now), None);
    }

    #[test]
    fn a_refusal_while_detached_still_reports_the_more_useful_reason() {
        let mut reach = Reach::default();
        let now = Instant::now();
        reach.attached(true);
        reach.refused(now);
        reach.attached(false);
        assert_eq!(reach.blocked(now), Some(Blocked::NoWorker));
        assert!(Blocked::NoWorker.why().contains("no worker"));
        assert!(Blocked::Refused.why().contains("turned its last"));
    }
}
