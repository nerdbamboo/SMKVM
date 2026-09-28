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
//! there, `desktop::current()` says out of reach and `can_inject()` says
//! no, for ever -- so the client suspends at the first refusal and never
//! resumes, whatever the worker is doing.
//!
//! So the answer comes from the worker instead. Three things make it up,
//! and the third was missing at first in a way worth spelling out.
//!
//! 1. **Is there a worker at all.** Without one nothing can land.
//! 2. **Is the worker on the desktop that has the input.** A worker on
//!    `Default` cannot put anything on `Winlogon`, and -- this is the part
//!    that matters -- it will happily put it on `Default` instead. When a
//!    consent prompt appears, the input moves before the service's poll
//!    notices, and for that quarter of a second a worker recorded as
//!    attached is a worker injecting into the wrong desktop. If the person
//!    is typing an administrator's password toward the prompt, the opening
//!    characters land in whatever window is focused behind it. That is a
//!    secret in the wrong place, so the two desktops are compared here and
//!    a mismatch is no reach at all. The comparison costs nothing: the
//!    service is the system account, so the poll that reads the input
//!    desktop already succeeds.
//! 3. **Was the last thing sent refused.** Reported by the worker after
//!    the fact.
//!
//! What is deliberately *not* asked: which integrity level anything runs
//! at, and whether a window in front outranks us. The worker is the system
//! account on whichever desktop has the input, so neither question has a
//! second answer -- that is the entire point of the arrangement.
//!
//! Kept here, apart from the pipe and the Win32 calls, because it is the
//! part with a decision in it and the part where a mistake is either a
//! cursor that never comes back or a password in the wrong window.

use std::time::{Duration, Instant};

/// How long the first refusal is believed for.
///
/// A worker reports a refusal after the fact, over a pipe, so by the time
/// it is read the cause may be gone. Suspending on it is right -- the
/// pointer has stopped either way -- but staying suspended on the strength
/// of one old refusal is how a cursor never comes home. After this, the
/// question is asked again by trying.
pub const REFUSAL_STANDS_FOR: Duration = Duration::from_secs(2);

/// The longest a run of refusals is believed for.
///
/// A flat two seconds turns a *persistent* refusal into a flap: believed,
/// expired, cursor taken back, refused again, two seconds later the same
/// -- with a warning, an informational line and two messages over the
/// network each time, and a pointer that jumps between machines on a
/// two-second beat. Each consecutive refusal doubles the wait up to this,
/// so something that is genuinely stuck settles down instead of drumming.
pub const REFUSAL_BACKS_OFF_TO: Duration = Duration::from_secs(30);

/// What the service knows about its worker's reach.
#[derive(Debug, Default, Clone)]
pub struct Reach {
    /// The desktop the attached worker is on, when one is attached.
    worker_on: Option<String>,
    /// The desktop that had the input when the service last looked.
    input_on: Option<String>,
    refused_at: Option<Instant>,
    /// How many refusals in a row, for the backoff.
    streak: u32,
}

/// Why input is not landing, when it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// There is no worker. Either one is being started, or the service
    /// gave up on that desktop -- and either way the cursor belongs back
    /// on the server meanwhile.
    NoWorker,
    /// There is one, and it is on a different desktop from the input.
    /// Anything sent would land on the wrong desktop rather than not at
    /// all, which is the worse of the two and is why this is checked.
    WrongDesktop,
    /// There is one, on the right desktop, and it says the system turned
    /// an injection down.
    Refused,
}

impl Reach {
    /// A worker connected on this desktop, or went (`None`).
    pub fn attached(&mut self, desktop: Option<&str>) {
        self.worker_on = desktop.map(str::to_owned);
        // A new worker is a new question. Carrying the last one's refusal
        // across would suspend the cursor over something that happened on
        // a desktop that is no longer there.
        self.refused_at = None;
        self.streak = 0;
    }

    /// What the service's poll saw. `None` when the input desktop would
    /// not say its name, which for the system account should not happen;
    /// the last name read is kept rather than guessed at.
    pub fn input_desktop(&mut self, desktop: Option<&str>) {
        if let Some(desktop) = desktop {
            self.input_on = Some(desktop.to_owned());
        }
    }

    /// The worker reported a refused injection.
    pub fn refused(&mut self, at: Instant) {
        self.refused_at = Some(at);
        self.streak = self.streak.saturating_add(1);
    }

    /// An injection went to the worker and nothing has come back refusing
    /// it, long enough after the last refusal that the last refusal is
    /// over. This is what ends a run and puts the backoff back to the
    /// start; a refusal is reported asynchronously, so "was not refused"
    /// can only be read as "was sent, and the window in which a refusal
    /// would have arrived has passed".
    pub fn sent(&mut self, at: Instant) {
        let Some(last) = self.refused_at else {
            return;
        };
        if at.saturating_duration_since(last) >= self.believed_for() {
            self.refused_at = None;
            self.streak = 0;
        }
    }

    /// How long the current run of refusals is believed for.
    fn believed_for(&self) -> Duration {
        let doubled = REFUSAL_STANDS_FOR
            .checked_mul(1u32 << self.streak.saturating_sub(1).min(16))
            .unwrap_or(REFUSAL_BACKS_OFF_TO);
        doubled.min(REFUSAL_BACKS_OFF_TO)
    }

    /// Is input reaching the screen right now?
    pub fn blocked(&self, now: Instant) -> Option<Blocked> {
        // Spelled out rather than `?`, which on an `Option<Blocked>`
        // would turn "there is no worker" into "nothing is blocking",
        // which is the inverse of the truth and exactly the shape of
        // mistake this module exists to prevent.
        let Some(worker_on) = self.worker_on.as_deref() else {
            return Some(Blocked::NoWorker);
        };
        // Only when the service has actually looked. Before the first
        // poll there is no worker either, so this cannot be the answer in
        // practice; it is written this way so that an unreadable input
        // desktop does not silently become agreement.
        if let Some(input_on) = self.input_on.as_deref() {
            if input_on != worker_on {
                return Some(Blocked::WrongDesktop);
            }
        }
        match self.refused_at {
            Some(at) if now.saturating_duration_since(at) < self.believed_for() => {
                Some(Blocked::Refused)
            }
            _ => None,
        }
    }

    pub fn possible(&self, now: Instant) -> bool {
        self.blocked(now).is_none()
    }

    /// The whole of it in a sentence, for the log line somebody
    /// diagnosing a pointer that will not move is going to read first.
    pub fn why(&self, now: Instant) -> Option<String> {
        let blocked = self.blocked(now)?;
        Some(match blocked {
            Blocked::WrongDesktop => format!(
                "{} -- the worker is on {} and the input is on {}",
                blocked.why(),
                self.worker_on.as_deref().unwrap_or("?"),
                self.input_on.as_deref().unwrap_or("?")
            ),
            _ => blocked.why().to_string(),
        })
    }
}

impl Blocked {
    pub fn why(self) -> &'static str {
        match self {
            Blocked::NoWorker => {
                "there is no worker on the desktop that has the input, so nothing sent \
                 from here would land there. One is being started, or could not be"
            }
            Blocked::WrongDesktop => {
                "the desktop with the input has changed and the worker has not moved to \
                 it yet, so anything sent now would land on the desktop it left"
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

    /// A reach with a worker settled on one desktop and the input there
    /// too, which is the ordinary state.
    fn settled(desktop: &str) -> Reach {
        let mut reach = Reach::default();
        reach.input_desktop(Some(desktop));
        reach.attached(Some(desktop));
        reach
    }

    #[test]
    fn no_worker_means_the_cursor_goes_home() {
        let reach = Reach::default();
        assert_eq!(reach.blocked(Instant::now()), Some(Blocked::NoWorker));
        assert!(!reach.possible(Instant::now()));
    }

    #[test]
    fn a_worker_on_the_desktop_with_the_input_is_the_whole_of_the_answer() {
        let reach = settled("Default");
        assert_eq!(reach.blocked(Instant::now()), None);
        assert!(reach.possible(Instant::now()));
    }

    #[test]
    fn a_worker_left_behind_on_the_old_desktop_is_not_a_place_to_send_input() {
        // The one that matters. A consent prompt appears, the input moves
        // to Winlogon, and for up to a poll the worker is still on
        // Default. Saying "reachable" here means the keystrokes of an
        // administrator's password go into whatever window is focused
        // behind the prompt.
        let mut reach = settled("Default");
        reach.input_desktop(Some("Winlogon"));
        assert_eq!(reach.blocked(Instant::now()), Some(Blocked::WrongDesktop));
        assert!(!reach.possible(Instant::now()));
        let why = reach.why(Instant::now()).expect("it is blocked");
        assert!(why.contains("Default"), "{why}");
        assert!(why.contains("Winlogon"), "{why}");
    }

    #[test]
    fn the_worker_catching_up_makes_it_reachable_again() {
        let mut reach = settled("Default");
        reach.input_desktop(Some("Winlogon"));
        assert!(!reach.possible(Instant::now()));
        // The service noticed and moved it.
        reach.attached(None);
        assert_eq!(reach.blocked(Instant::now()), Some(Blocked::NoWorker));
        reach.attached(Some("Winlogon"));
        assert_eq!(reach.blocked(Instant::now()), None);
    }

    #[test]
    fn an_unreadable_input_desktop_keeps_the_last_name_rather_than_agreeing() {
        let mut reach = settled("Winlogon");
        reach.input_desktop(None);
        assert_eq!(reach.blocked(Instant::now()), None);
        // And a mismatch already known is not cleared by a look that
        // could not read anything.
        let mut reach = settled("Default");
        reach.input_desktop(Some("Winlogon"));
        reach.input_desktop(None);
        assert_eq!(reach.blocked(Instant::now()), Some(Blocked::WrongDesktop));
    }

    #[test]
    fn a_refusal_suspends_and_then_stops_suspending() {
        let mut reach = settled("Default");
        let now = Instant::now();
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
    fn refusals_in_a_row_are_believed_for_longer_each_time() {
        // Without this, a persistent refusal is a pointer jumping between
        // machines every two seconds, for ever, logging as it goes.
        let mut reach = settled("Default");
        let now = Instant::now();
        reach.refused(now);
        assert_eq!(reach.blocked(now + REFUSAL_STANDS_FOR), None);
        reach.refused(now);
        assert_eq!(
            reach.blocked(now + REFUSAL_STANDS_FOR),
            Some(Blocked::Refused)
        );
        assert_eq!(reach.blocked(now + REFUSAL_STANDS_FOR * 2), None);
        reach.refused(now);
        assert_eq!(
            reach.blocked(now + REFUSAL_STANDS_FOR * 2),
            Some(Blocked::Refused)
        );
    }

    #[test]
    fn the_backoff_has_a_ceiling_and_does_not_overflow() {
        let mut reach = settled("Default");
        let now = Instant::now();
        for _ in 0..200 {
            reach.refused(now);
        }
        assert_eq!(
            reach.blocked(now + REFUSAL_BACKS_OFF_TO - Duration::from_millis(1)),
            Some(Blocked::Refused)
        );
        assert_eq!(reach.blocked(now + REFUSAL_BACKS_OFF_TO), None);
    }

    #[test]
    fn an_injection_that_was_not_refused_puts_the_backoff_back_to_the_start() {
        let mut reach = settled("Default");
        let now = Instant::now();
        reach.refused(now);
        reach.refused(now);
        reach.refused(now);
        // Something went out after the run's window closed and nothing
        // came back about it, so the run is over.
        reach.sent(now + REFUSAL_BACKS_OFF_TO);
        let later = now + REFUSAL_BACKS_OFF_TO;
        reach.refused(later);
        assert_eq!(
            reach.blocked(later + REFUSAL_STANDS_FOR),
            None,
            "the next refusal should be believed for the first interval again"
        );
    }

    #[test]
    fn an_injection_inside_the_window_does_not_end_the_run() {
        // Injections keep going out while suspended -- the probe is one --
        // so sending must not by itself be taken as success.
        let mut reach = settled("Default");
        let now = Instant::now();
        reach.refused(now);
        reach.refused(now);
        reach.sent(now + Duration::from_millis(1));
        assert_eq!(
            reach.blocked(now + REFUSAL_STANDS_FOR),
            Some(Blocked::Refused)
        );
    }

    #[test]
    fn a_new_worker_does_not_inherit_the_old_ones_refusal() {
        let mut reach = settled("Default");
        let now = Instant::now();
        reach.refused(now);
        reach.attached(None);
        reach.attached(Some("Default"));
        assert_eq!(reach.blocked(now), None);
    }

    #[test]
    fn no_worker_beats_a_stale_refusal_and_a_mismatch() {
        let mut reach = settled("Default");
        let now = Instant::now();
        reach.refused(now);
        reach.input_desktop(Some("Winlogon"));
        reach.attached(None);
        assert_eq!(reach.blocked(now), Some(Blocked::NoWorker));
        assert!(Blocked::NoWorker.why().contains("no worker"));
        assert!(Blocked::WrongDesktop.why().contains("has not moved"));
        assert!(Blocked::Refused.why().contains("turned its last"));
    }
}
