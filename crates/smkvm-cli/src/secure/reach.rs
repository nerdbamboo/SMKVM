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

/// How long a refusal has to arrive in, for its absence to mean anything.
///
/// A refusal travels from the worker's failed `SendInput`, down the pipe,
/// through the service's reader thread. That is sub-millisecond work; a
/// second is a thousand times it. The number matters because "this
/// injection was not refused" is the only evidence that a run of refusals
/// is over, and that evidence does not exist at the moment of sending --
/// it exists once this much time has passed with nothing coming back.
pub const REFUSAL_ARRIVES_WITHIN: Duration = Duration::from_secs(1);

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
    /// The first injection sent since the last refusal, if any has been.
    /// Not evidence of anything yet -- see [`Reach::run_is_over`].
    sent_at: Option<Instant>,
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
        self.sent_at = None;
        self.streak = 0;
    }

    /// What the service's poll saw. `None` when the input desktop would
    /// not say its name, which for the system account should not happen;
    /// the last name read is kept rather than guessed at.
    ///
    /// Keeping it cuts both ways and the asymmetry is deliberate. A known
    /// *mismatch* must survive a look that read nothing, or one failed
    /// poll would say the worker is in the right place when the last
    /// thing actually observed said it was not -- and that is the case
    /// with the bad outcome. A known *agreement* also survives, so if the
    /// input moves at the same moment the poll starts failing, this keeps
    /// saying reachable until a poll succeeds. That is the smaller risk
    /// of the two and it needs `OpenInputDesktop` to fail for the system
    /// account, which is the thing this module elsewhere says should not
    /// happen; `watch` holds still on the same reading for the same
    /// reason. The converse -- treating an unreadable poll as unknown and
    /// suspending -- would hand the cursor back every time a poll
    /// hiccupped. If it ever does matter, the answer is to age the
    /// reading and call a name older than a few polls unknown.
    pub fn input_desktop(&mut self, desktop: Option<&str>) {
        if let Some(desktop) = desktop {
            self.input_on = Some(desktop.to_owned());
        }
    }

    /// The worker reported a refused injection.
    pub fn refused(&mut self, at: Instant) {
        // Whether this refusal continues a run or starts a new one is
        // decided here, by whether the previous run had been shown to be
        // over. Deciding it when the injection was *sent* is what broke
        // this: see [`Reach::sent`].
        if self.run_is_over(at) {
            self.streak = 0;
        }
        self.refused_at = Some(at);
        self.sent_at = None;
        self.streak = self.streak.saturating_add(1);
    }

    /// An injection went to the worker. Recorded, not believed.
    ///
    /// This used to end the run of refusals there and then, on the
    /// reasoning that an injection which went out and was not refused is
    /// evidence the trouble is over. The reasoning is sound and the
    /// timing made it worthless, in a way that measuring found and
    /// reading did not: the client only injects once `blocked()` has
    /// returned `None`, which happens exactly when the current interval
    /// expires, so the very first injection after every resume satisfied
    /// the test by construction -- and did so before the refusal for that
    /// injection could possibly have arrived. The streak reset on every
    /// cycle and the doubling below never took effect at all. Simulated
    /// over a minute of a worker refusing everything: twenty-nine resumes
    /// with the reset, four without it. The reset was worse than not
    /// having one.
    ///
    /// So the send is only remembered. What makes it evidence is
    /// [`REFUSAL_ARRIVES_WITHIN`] passing with nothing coming back, which
    /// is a judgement about the past and can only be made later.
    pub fn sent(&mut self, at: Instant) {
        // Only an injection made while input was believed to be landing
        // says anything about whether it lands. Suspending is itself
        // several injections -- the keys held are released and the
        // pointer is shown -- and those go out microseconds after the
        // refusal that caused the suspension. Counting them would make
        // every refusal look like it had been survived one grace period
        // later, which is the same defect as resetting at send time
        // wearing different clothes, and a test caught it being written
        // for the second time.
        if self.blocked(at).is_some() {
            return;
        }
        // The first send since the last refusal is the one the grace
        // period is measured from; later ones say nothing new.
        if self.sent_at.is_none() {
            self.sent_at = Some(at);
        }
    }

    /// Has an injection survived long enough with no refusal to say the
    /// run of refusals is over?
    fn run_is_over(&self, now: Instant) -> bool {
        let (Some(sent), Some(refused)) = (self.sent_at, self.refused_at) else {
            return false;
        };
        // `sent_at` is cleared by every refusal, so a send that is still
        // recorded is necessarily one no refusal has followed. All that
        // remains is whether a refusal would have arrived by now.
        sent > refused && now.saturating_duration_since(sent) >= REFUSAL_ARRIVES_WITHIN
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
            Some(at)
                if now.saturating_duration_since(at) < self.believed_for()
                    && !self.run_is_over(now) =>
            {
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

    /// One minute of a worker that refuses every injection, at the
    /// granularity the real loop runs at, returning the gap before each
    /// resume.
    ///
    /// The client only injects once `possible()` is true, and the refusal
    /// for that injection comes back a moment later. That is the whole
    /// cycle, and its *rate* is the property that was wrong: the doubling
    /// and the ceiling were both implemented correctly and were both
    /// cancelled by resetting the streak at the moment of sending. A test
    /// that asserted the interval doubles once passed throughout, because
    /// it never ran two cycles.
    fn resumes_in_a_minute() -> Vec<Duration> {
        let start = Instant::now();
        let mut reach = settled("Default");
        // The first injection has already been refused; the client is
        // suspended and waiting to try again.
        reach.sent(start);
        reach.refused(start);

        let mut suspended = true;
        let mut refusal_due: Option<Instant> = None;
        let mut resumes = Vec::new();
        let mut last = start;

        for tick in 1..=6_000u64 {
            let now = start + Duration::from_millis(10 * tick);
            if let Some(due) = refusal_due {
                if due <= now {
                    reach.refused(due);
                    refusal_due = None;
                    suspended = true;
                }
            }
            if suspended && reach.possible(now) {
                suspended = false;
                resumes.push(now.saturating_duration_since(last));
                last = now;
                // Resuming injects, and that injection is refused a
                // moment later, like every other one.
                reach.sent(now);
                refusal_due = Some(now + Duration::from_millis(10));
            }
        }
        resumes
    }

    #[test]
    fn a_worker_that_refuses_everything_is_not_retried_thirty_times_a_minute() {
        // The measurement, not the argument. Before the fix this was 29
        // resumes a minute, every gap 2.01 s: the person watches the
        // pointer jump between machines twice a second-and-a-bit for as
        // long as the worker stays stuck, with a warning, an
        // informational line and two network messages each time. With the
        // reset simply deleted it was 4. This asserts the rate, because
        // the rate is what was wrong; asserting that the interval doubles
        // once passed the whole time it was broken.
        let resumes = resumes_in_a_minute();
        assert!(
            resumes.len() <= 6,
            "{} resumes in a minute under continuous refusal: {resumes:?}",
            resumes.len()
        );
        // And it really is backing off rather than merely being slow.
        for pair in resumes.windows(2) {
            assert!(
                pair[1] >= pair[0],
                "the wait got shorter across a run of refusals: {resumes:?}"
            );
        }
        assert!(
            resumes
                .last()
                .is_some_and(|gap| *gap >= REFUSAL_STANDS_FOR * 4),
            "the last wait of the minute should be well past the first: {resumes:?}"
        );
    }

    #[test]
    fn an_injection_that_outlives_the_wait_for_a_refusal_ends_the_run() {
        let mut reach = settled("Default");
        let now = Instant::now();
        reach.refused(now);
        reach.refused(now);
        reach.refused(now);
        // Something went out after the run's window closed, and the time
        // in which a refusal would have come back has passed with
        // nothing. Only now is the run over.
        let out = now + REFUSAL_BACKS_OFF_TO;
        reach.sent(out);
        let later = out + REFUSAL_ARRIVES_WITHIN;
        reach.refused(later);
        assert_eq!(
            reach.blocked(later + REFUSAL_STANDS_FOR),
            None,
            "the next refusal should be believed for the first interval again"
        );
    }

    #[test]
    fn an_injection_is_not_evidence_at_the_moment_it_is_sent() {
        // The defect exactly. The client injects the instant the wait
        // expires, so a send judged at send time always looks like
        // success -- before the refusal it is about could have arrived.
        let mut reach = settled("Default");
        let now = Instant::now();
        reach.refused(now);
        reach.refused(now);
        let expired = now + REFUSAL_STANDS_FOR * 2;
        assert_eq!(reach.blocked(expired), None, "the wait is up");
        reach.sent(expired);
        // The refusal for that injection lands a moment later. It must
        // continue the run rather than start a new one.
        reach.refused(expired + Duration::from_millis(10));
        assert_eq!(
            reach.blocked(expired + Duration::from_millis(10) + REFUSAL_STANDS_FOR * 2),
            Some(Blocked::Refused),
            "the third refusal was believed for no longer than the second"
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
