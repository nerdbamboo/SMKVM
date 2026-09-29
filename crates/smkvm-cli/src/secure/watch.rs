//! Deciding, from what the input desktop looks like, whether to move the
//! worker.
//!
//! The Win32 half of this is three calls and no judgement: open the input
//! desktop, read its name, close it. Every decision that follows -- is this
//! the desktop the worker is already on, is a name that could not be read a
//! reason to act, how often may a worker that keeps dying be started again
//! -- is judgement, and none of it needs Windows to exercise. So it is all
//! here, where it is tested on whatever machine happens to be building.
//!
//! Getting it wrong is not a small thing in either direction. Relaunching
//! too eagerly puts a process on the consent desktop several times a second
//! and the pointer stutters between them; relaunching too reluctantly is
//! the bug this whole arrangement exists to fix, back again.

use std::time::{Duration, Instant};

/// How often to ask which desktop has the input.
///
/// There is no notification for a desktop switch -- `WTSRegisterSession-
/// Notification` reports sessions, not desktops -- so this is a poll, and
/// the interval is the whole of the latency between a consent prompt
/// appearing and the cursor being able to reach it. Four times a second is
/// under the time it takes to move a hand to the mouse, and four opens of a
/// kernel object per second is not a cost anything notices.
pub const LOOK_EVERY: Duration = Duration::from_millis(250);

/// How many failed starts on one desktop before the service stops trying.
///
/// A worker that cannot be started on a desktop will not start on the next
/// attempt either, and a service that spawns processes in a tight loop for
/// the rest of the day is worse than one that says so once and waits. The
/// count resets the moment the input goes somewhere else, because the next
/// desktop is a different question.
pub const GIVE_UP_AFTER: u32 = 5;

/// How long a worker must last to count as having worked.
///
/// A worker that starts, connects, says hello and then dies a second later
/// is a failure, and it has to be counted as one: without this it resets
/// the failure count on every hello and the service starts a process as
/// the system account four times a second, for ever, with a warning line
/// each time. That is the worst outcome in this file and it is not one the
/// original counter covered, because the counter only ever saw starts that
/// returned an error. Likely causes are real: hooks or `SendInput`
/// faulting on the Winlogon desktop, or the capture thread panicking.
pub const SETTLED_AFTER: Duration = Duration::from_secs(10);

/// What the service saw when it looked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// The input desktop, by name: `Default` in the ordinary case,
    /// `Winlogon` for a consent prompt or the lock screen, `Screen-saver`
    /// for what it says.
    Desktop(String),
    /// It would not open, or would not give its name. Running as
    /// LocalSystem this should not happen -- being refused is what a
    /// process in the person's session gets -- so it is treated as "no news"
    /// rather than as news: whatever the worker is on, it stays on, and the
    /// next look decides.
    Unreadable,
}

/// What to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing; the worker is where it should be, or there is nothing to go on.
    Stay,
    /// Stop whatever worker is running and start one attached to this
    /// desktop, named as `lpDesktop` wants it.
    Move { desktop: String, on: String },
    /// Say once that this desktop cannot be reached, and stop trying until
    /// the input moves somewhere else.
    GiveUp { desktop: String, tries: u32 },
}

/// The desktop an interactive session always has.
///
/// Where a worker is put when there is no worker to ask. The service
/// cannot find out which desktop has the input -- that question can only
/// be answered from inside the session -- so the first worker goes to
/// the one that is always there, and from then on the worker says where
/// the input actually is.
pub const ALWAYS_THERE: &str = "Default";

/// Should a worker on this desktop take up the person's clipboard?
///
/// Only on the ordinary one, and the reason is not what it first looks
/// like. The Windows clipboard is per *window station*, not per desktop,
/// so a worker on `Winlogon` is on `WinSta0` and could probably reach
/// it. It should not, for two better reasons.
///
/// Nothing is copied or pasted at a consent prompt; the person's
/// applications are not even on that desktop. And owning a clipboard
/// means owning a *window* that answers when something pastes -- a
/// window on the desktop the worker is attached to, which is destroyed
/// when that worker is replaced. A worker that took the clipboard onto
/// `Winlogon` would therefore throw away the standing offer every time
/// a prompt appeared, and the person would find that what another
/// machine had copied had quietly stopped being available.
///
/// So the clipboard belongs to the `Default` worker. A switch to
/// `Winlogon` suspends it rather than losing it: the exchange, and the
/// offer it is holding, live in the service, which re-offers as soon as
/// a worker is back on the ordinary desktop.
pub fn serves_the_clipboard(desktop: &str) -> bool {
    desktop.eq_ignore_ascii_case(ALWAYS_THERE)
}

/// The window station every interactive desktop lives on.
///
/// There is exactly one that has a screen attached, and it is always called
/// this. A service's own station is `Service-0x0-3e7$`, which has none,
/// which is why the worker has to be told where to go rather than
/// inheriting it.
pub const STATION: &str = "WinSta0";

/// A desktop name as `STARTUPINFOW.lpDesktop` wants it: station, backslash,
/// desktop.
///
/// The backslash is not optional and not a path separator being tidy about
/// itself -- `lpDesktop` given a bare desktop name means "this desktop on
/// the calling process's window station", and the calling process here is a
/// service, whose station has no screen. The process would start, see
/// nothing, hook nothing and report no error at all -- the shape of
/// failure this repository has lost the most time to.
pub fn on_station(desktop: &str) -> String {
    format!(r"{STATION}\{desktop}")
}

/// Where the worker is, where the input is, and what has been tried.
#[derive(Debug, Default)]
pub struct Watch {
    /// The desktop the running worker said it landed on, if one is running.
    worker_on: Option<String>,
    /// When it said so, to tell a worker that worked from one that fell
    /// over immediately.
    worker_since: Option<Instant>,
    /// The desktop the failures below are about.
    failing: Option<String>,
    tries: u32,
    /// Whether [`Step::GiveUp`] has already been reported for `failing`.
    said: bool,
}

impl Watch {
    pub fn new() -> Self {
        Self::default()
    }

    /// The desktop the worker is believed to be on, for the log and the
    /// status report.
    pub fn worker_on(&self) -> Option<&str> {
        self.worker_on.as_deref()
    }

    /// One report of where the input is.
    pub fn saw(&mut self, seen: Seen) -> Step {
        let desktop = match seen {
            Seen::Desktop(desktop) => desktop,
            // Nobody is there to ask. Somebody has to be, or nothing
            // will ever answer, so one goes on the desktop that always
            // exists and reports from there.
            Seen::Unreadable if self.worker_on.is_none() => ALWAYS_THERE.to_string(),
            Seen::Unreadable => return Step::Stay,
        };
        if self.worker_on.as_deref() == Some(desktop.as_str()) {
            return Step::Stay;
        }
        // The input has moved somewhere the failures are not about, so the
        // failures are no longer the question.
        if self.failing.as_deref() != Some(desktop.as_str()) {
            self.forget_failures();
        }
        if self.tries >= GIVE_UP_AFTER {
            if self.said {
                return Step::Stay;
            }
            self.said = true;
            return Step::GiveUp {
                desktop,
                tries: self.tries,
            };
        }
        Step::Move {
            on: on_station(&desktop),
            desktop,
        }
    }

    /// A worker started, and said which desktop it actually landed on.
    ///
    /// Its own word rather than the service's intention: the input can move
    /// again between the decision and the start, and a worker recorded as
    /// being where it is not is a worker that never gets relaunched.
    ///
    /// The failure count is *not* cleared here. A worker that says hello
    /// and then dies has not worked, and clearing on hello is exactly what
    /// let the relaunch loop run for ever. It is cleared in
    /// [`Watch::worker_gone`], once the worker has lasted long enough to
    /// have been worth starting.
    ///
    /// `wanted` is the desktop it was sent to. A worker that lands
    /// somewhere else is counted as a failure of the desktop it was sent
    /// to, and that is the second door into the relaunch loop. The first
    /// was a worker that died at once, which [`Watch::worker_gone`]
    /// covers. This one is a worker that lives but is never where it was
    /// asked to be: the next look sees the mismatch, replaces it, and a
    /// replacement counts nothing -- so a process running as the system
    /// account is started and killed four times a second with no counter
    /// ever reaching its limit. It was reachable through a desktop name
    /// that could not be read, and `wire::welcome` now refuses those; this
    /// is the same door shut from the other side, so that the next name
    /// which cannot match does not reopen it.
    pub fn worker_started(&mut self, landed_on: &str, wanted: &str, at: Instant) {
        self.worker_on = Some(landed_on.to_string());
        self.worker_since = Some(at);
        if landed_on != wanted {
            self.count_failure(wanted);
        }
    }

    /// A worker could not be started for this desktop.
    pub fn worker_failed(&mut self, desktop: &str) {
        self.worker_on = None;
        self.worker_since = None;
        self.count_failure(desktop);
    }

    /// The worker exited, or its pipe went.
    ///
    /// Whether that is a failure depends on how long it lasted: a worker
    /// replaced because the input moved, or one that ran all afternoon, is
    /// not a failure and clears the count. One that lasted less than
    /// [`SETTLED_AFTER`] is a failure on the desktop it was on, and counts
    /// towards giving up there.
    pub fn worker_gone(&mut self, at: Instant) {
        let was_on = self.worker_on.take();
        let since = self.worker_since.take();
        let (Some(was_on), Some(since)) = (was_on, since) else {
            return;
        };
        if at.saturating_duration_since(since) < SETTLED_AFTER {
            self.count_failure(&was_on);
        } else {
            self.forget_failures();
        }
    }

    fn count_failure(&mut self, desktop: &str) {
        if self.failing.as_deref() != Some(desktop) {
            self.failing = Some(desktop.to_string());
            self.tries = 0;
            self.said = false;
        }
        self.tries += 1;
    }

    fn forget_failures(&mut self) {
        self.failing = None;
        self.tries = 0;
        self.said = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saw(watch: &mut Watch, name: &str) -> Step {
        watch.saw(Seen::Desktop(name.into()))
    }

    /// A moment far enough after `at` that a worker started then counts as
    /// having worked.
    fn later(at: Instant) -> Instant {
        at + SETTLED_AFTER + Duration::from_secs(1)
    }

    #[test]
    fn the_first_look_starts_a_worker_on_the_station_with_a_screen() {
        let mut watch = Watch::new();
        assert_eq!(
            saw(&mut watch, "Default"),
            Step::Move {
                desktop: "Default".into(),
                on: r"WinSta0\Default".into()
            }
        );
    }

    #[test]
    fn a_worker_already_where_it_should_be_is_left_alone() {
        let mut watch = Watch::new();
        let now = Instant::now();
        saw(&mut watch, "Default");
        watch.worker_started("Default", "Default", now);
        assert_eq!(saw(&mut watch, "Default"), Step::Stay);
        assert_eq!(saw(&mut watch, "Default"), Step::Stay);
    }

    #[test]
    fn a_consent_prompt_moves_the_worker_and_going_back_moves_it_again() {
        let mut watch = Watch::new();
        let now = Instant::now();
        saw(&mut watch, "Default");
        watch.worker_started("Default", "Default", now);
        assert_eq!(
            saw(&mut watch, "Winlogon"),
            Step::Move {
                desktop: "Winlogon".into(),
                on: r"WinSta0\Winlogon".into()
            }
        );
        watch.worker_started("Winlogon", "Winlogon", now);
        assert_eq!(saw(&mut watch, "Winlogon"), Step::Stay);
        assert_eq!(
            saw(&mut watch, "Default"),
            Step::Move {
                desktop: "Default".into(),
                on: r"WinSta0\Default".into()
            }
        );
    }

    #[test]
    fn with_nobody_to_ask_a_worker_goes_where_one_always_can_live() {
        // The hardware failure, pinned. The service cannot find out
        // which desktop has the input -- only something inside the
        // session can -- so on a fresh service every look says
        // "unreadable". Treating that as no news meant no worker was
        // ever started, no line was ever logged, and the machine sat
        // there for ever looking like it was working.
        let mut watch = Watch::new();
        assert_eq!(
            watch.saw(Seen::Unreadable),
            Step::Move {
                desktop: "Default".into(),
                on: r"WinSta0\Default".into()
            }
        );
    }

    #[test]
    fn a_worker_that_cannot_tell_is_not_a_reason_to_replace_it() {
        // With one running, the same answer means something different:
        // it looked and could not say, and moving it would be churn.
        let mut watch = Watch::new();
        watch.worker_started("Default", "Default", Instant::now());
        assert_eq!(watch.saw(Seen::Unreadable), Step::Stay);
    }

    #[test]
    fn giving_up_covers_the_bootstrap_too() {
        // Otherwise a session that will not take a worker at all is a
        // process started four times a second for ever, which is the
        // relaunch loop by another door.
        let mut watch = Watch::new();
        for _ in 0..GIVE_UP_AFTER {
            assert!(matches!(watch.saw(Seen::Unreadable), Step::Move { .. }));
            watch.worker_failed("Default");
        }
        assert!(matches!(watch.saw(Seen::Unreadable), Step::GiveUp { .. }));
    }

    #[test]
    fn a_desktop_that_would_not_say_its_name_changes_nothing() {
        let mut watch = Watch::new();
        let now = Instant::now();
        saw(&mut watch, "Default");
        watch.worker_started("Default", "Default", now);
        assert_eq!(watch.saw(Seen::Unreadable), Step::Stay);
        assert_eq!(watch.worker_on(), Some("Default"));
    }

    #[test]
    fn the_worker_is_recorded_where_it_landed_not_where_it_was_sent() {
        // The input moved between the decision and the start. If the
        // service believed its own intention, the next look would see
        // "Winlogon wanted, Winlogon recorded" and never correct itself.
        let mut watch = Watch::new();
        let now = Instant::now();
        saw(&mut watch, "Winlogon");
        watch.worker_started("Default", "Winlogon", now);
        assert_eq!(
            saw(&mut watch, "Winlogon"),
            Step::Move {
                desktop: "Winlogon".into(),
                on: r"WinSta0\Winlogon".into()
            }
        );
    }

    #[test]
    fn a_worker_that_dies_after_a_good_run_is_started_again() {
        let mut watch = Watch::new();
        let now = Instant::now();
        saw(&mut watch, "Default");
        watch.worker_started("Default", "Default", now);
        watch.worker_gone(later(now));
        assert!(matches!(saw(&mut watch, "Default"), Step::Move { .. }));
    }

    #[test]
    fn a_desktop_that_keeps_refusing_is_given_up_on_once_and_then_left() {
        let mut watch = Watch::new();
        for _ in 0..GIVE_UP_AFTER {
            assert!(matches!(saw(&mut watch, "Winlogon"), Step::Move { .. }));
            watch.worker_failed("Winlogon");
        }
        assert_eq!(
            saw(&mut watch, "Winlogon"),
            Step::GiveUp {
                desktop: "Winlogon".into(),
                tries: GIVE_UP_AFTER
            }
        );
        // Said once. The log does not fill up four times a second.
        for _ in 0..10 {
            assert_eq!(saw(&mut watch, "Winlogon"), Step::Stay);
        }
    }

    #[test]
    fn a_worker_that_says_hello_and_dies_at_once_is_a_failure_too() {
        // The loop this prevents: start, hello, die, start, hello, die --
        // a process started as the system account four times a second for
        // the rest of the day, which the old counter never saw because the
        // start itself kept succeeding.
        let mut watch = Watch::new();
        let mut now = Instant::now();
        for _ in 0..GIVE_UP_AFTER {
            assert!(matches!(saw(&mut watch, "Winlogon"), Step::Move { .. }));
            watch.worker_started("Winlogon", "Winlogon", now);
            now += Duration::from_millis(250);
            watch.worker_gone(now);
        }
        assert_eq!(
            saw(&mut watch, "Winlogon"),
            Step::GiveUp {
                desktop: "Winlogon".into(),
                tries: GIVE_UP_AFTER
            }
        );
    }

    #[test]
    fn a_worker_that_lasted_clears_what_came_before_it() {
        // Four bad starts then one that worked: the machine has shown it
        // can run a worker there, so the next fall is a fresh question and
        // not the fifth strike.
        let mut watch = Watch::new();
        let now = Instant::now();
        for _ in 0..GIVE_UP_AFTER - 1 {
            saw(&mut watch, "Winlogon");
            watch.worker_failed("Winlogon");
        }
        saw(&mut watch, "Winlogon");
        watch.worker_started("Winlogon", "Winlogon", now);
        watch.worker_gone(later(now));
        for _ in 0..GIVE_UP_AFTER {
            assert!(matches!(saw(&mut watch, "Winlogon"), Step::Move { .. }));
            watch.worker_failed("Winlogon");
        }
        assert!(matches!(saw(&mut watch, "Winlogon"), Step::GiveUp { .. }));
    }

    #[test]
    fn replacing_a_worker_because_the_input_moved_is_not_a_failure() {
        // `worker_gone` is not called on a deliberate replacement, but if
        // it ever is, a long-lived worker must not be counted against the
        // desktop it served perfectly well.
        let mut watch = Watch::new();
        let now = Instant::now();
        saw(&mut watch, "Default");
        watch.worker_started("Default", "Default", now);
        watch.worker_gone(later(now));
        saw(&mut watch, "Winlogon");
        watch.worker_started("Winlogon", "Winlogon", later(now));
        watch.worker_gone(later(later(now)));
        // Nothing has been counted against anything.
        assert!(matches!(saw(&mut watch, "Default"), Step::Move { .. }));
    }

    #[test]
    fn giving_up_on_one_desktop_does_not_give_up_on_the_next() {
        let mut watch = Watch::new();
        let now = Instant::now();
        for _ in 0..GIVE_UP_AFTER {
            saw(&mut watch, "Winlogon");
            watch.worker_failed("Winlogon");
        }
        assert!(matches!(saw(&mut watch, "Winlogon"), Step::GiveUp { .. }));
        assert!(matches!(saw(&mut watch, "Default"), Step::Move { .. }));
        // And coming back to it is a fresh question, because whatever was
        // in the way -- a desktop still being built, a session still
        // logging in -- may not be any more.
        watch.worker_started("Default", "Default", now);
        assert!(matches!(saw(&mut watch, "Winlogon"), Step::Move { .. }));
    }

    #[test]
    fn a_worker_that_never_lands_where_it_was_sent_is_given_up_on() {
        // The second door into the relaunch loop. A worker that starts,
        // says hello and lives, but reports a desktop that is not the one
        // it was sent to, is replaced on the very next look -- and a
        // replacement counts no failure, so without this nothing ever
        // reaches the limit and a process running as the system account
        // is started and killed four times a second for ever.
        let mut watch = Watch::new();
        let now = Instant::now();
        for _ in 0..GIVE_UP_AFTER {
            assert!(matches!(saw(&mut watch, "Winlogon"), Step::Move { .. }));
            watch.worker_started("somewhere else", "Winlogon", now);
        }
        assert_eq!(
            saw(&mut watch, "Winlogon"),
            Step::GiveUp {
                desktop: "Winlogon".into(),
                tries: GIVE_UP_AFTER
            }
        );
    }

    #[test]
    fn one_worker_losing_a_race_to_a_moving_input_is_not_held_against_it() {
        // The input really can move between the decision and the start,
        // and that is not the desktop's fault. One mismatch counts, and a
        // worker that then lands where it was sent and lasts clears it.
        let mut watch = Watch::new();
        let now = Instant::now();
        saw(&mut watch, "Winlogon");
        watch.worker_started("Default", "Winlogon", now);
        saw(&mut watch, "Winlogon");
        watch.worker_started("Winlogon", "Winlogon", now);
        watch.worker_gone(later(now));
        for _ in 0..GIVE_UP_AFTER - 1 {
            assert!(matches!(saw(&mut watch, "Winlogon"), Step::Move { .. }));
            watch.worker_failed("Winlogon");
        }
        assert!(matches!(saw(&mut watch, "Winlogon"), Step::Move { .. }));
    }

    #[test]
    fn the_clipboard_belongs_to_the_ordinary_desktop_and_no_other() {
        assert!(serves_the_clipboard("Default"));
        assert!(serves_the_clipboard("default"));
        assert!(!serves_the_clipboard("Winlogon"));
        assert!(!serves_the_clipboard("Screen-saver"));
    }

    #[test]
    fn a_bare_desktop_name_is_never_what_is_handed_to_windows() {
        assert_eq!(on_station("Winlogon"), r"WinSta0\Winlogon");
        assert!(on_station("Default").starts_with(r"WinSta0\"));
    }
}
