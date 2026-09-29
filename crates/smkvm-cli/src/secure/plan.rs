//! Which of the two arrangements is installed, and what changing that means.
//!
//! There are now two ways for this to start by itself on Windows, and the
//! rule about them is short: never both at once. A scheduled task and a
//! service would each start a daemon, and two daemons on one machine fight
//! over the link -- the one that loses is left holding whatever it did
//! last, a pointer parked out of the way, keys down. `ensure_not_running`
//! catches it at the second start, which is a refusal in a log nobody is
//! reading rather than something working.
//!
//! So `install` is "make this the arrangement", not "add one more", and
//! `uninstall` removes whichever is there without being told which. The
//! deciding is here, apart from the doing, because the doing needs Windows
//! and the deciding is where the mistake would be.

/// The two ways this can start by itself on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrangement {
    /// A scheduled task in the person's session, at highest privileges.
    /// Reaches every window they can see. This is the default and the one
    /// three machines are running.
    Task,
    /// A service as LocalSystem, which puts a worker on whichever desktop
    /// has the input. Reaches the consent prompt and the lock screen too.
    Service,
}

/// What is registered on this machine now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Present {
    pub task: bool,
    pub service: bool,
}

/// One thing to do to the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    RemoveTask,
    RemoveService,
    RegisterTask,
    RegisterService,
}

/// Getting from what is there to what was asked for.
///
/// Removals come first, always. Registering the new one and then removing
/// the old leaves a window in which both exist, and a logon or a reboot in
/// that window starts two daemons.
pub fn install(wanted: Arrangement, present: Present) -> Vec<Step> {
    let mut steps = Vec::new();
    match wanted {
        Arrangement::Task => {
            if present.service {
                steps.push(Step::RemoveService);
            }
            steps.push(Step::RegisterTask);
        }
        Arrangement::Service => {
            if present.task {
                steps.push(Step::RemoveTask);
            }
            steps.push(Step::RegisterService);
        }
    }
    steps
}

/// Undoing whichever was installed.
///
/// Both are removed when both are somehow there, so that a machine left in
/// a state this code says cannot happen can still be cleaned up by the
/// command whose job that is.
pub fn uninstall(present: Present) -> Vec<Step> {
    let mut steps = Vec::new();
    if present.task {
        steps.push(Step::RemoveTask);
    }
    if present.service {
        steps.push(Step::RemoveService);
    }
    steps
}

/// Is this command line one of this program's daemons?
///
/// Asked of every `smkvm.exe` on the machine when the arrangement is
/// switched, because switching has to leave exactly one daemon running
/// and reading a status report is not enough to find the other.
///
/// The report is written into the profile of whoever is running the
/// daemon, and an install happens over ssh as an administrator who is
/// not that person -- so the report simply is not among the places the
/// installer can look, `running_pid` finds nothing, and the daemon the
/// task was running carries on. That happened: for one whole test a
/// machine had the old daemon, the new service and its worker, two
/// daemons fighting over one clipboard. `plan` exists to make that
/// impossible and it was defeated by looking in the wrong profile.
///
/// So the question is asked of the process list instead, where nobody
/// needs permission to somebody else's profile to get an answer. A
/// command line rather than just a name, because `smkvm status` and
/// `smkvm service install` are the same executable and must not be
/// killed.
pub fn is_a_daemon(command_line: &str) -> bool {
    // The three ways a daemon is started, plus the worker, which is a
    // daemon's arm and dies with it.
    const DAEMONS: [&str; 5] = ["run", "serve", "connect", "service-main", "desktop-worker"];
    // Everything else this program can be asked to do. Listed rather
    // than assumed, so that a word which is neither -- a flag, or the
    // value of one -- is stepped over instead of being taken for a
    // command.
    const ENDS_BY_ITSELF: [&str; 8] = [
        "service", "status", "init", "pair", "devices", "forget", "monitors", "import",
    ];
    // Global flags that take a value, whose value must not be read as
    // the command. `--log-file x.log run` is a daemon; without this it
    // looked like whatever `x.log` was.
    const TAKES_A_VALUE: [&str; 2] = ["--config", "--log-file"];

    let mut skip_next = false;
    for word in after_the_program(command_line).split_whitespace() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if TAKES_A_VALUE.contains(&word) {
            skip_next = true;
            continue;
        }
        if DAEMONS.contains(&word) {
            return true;
        }
        if ENDS_BY_ITSELF.contains(&word) {
            return false;
        }
    }
    false
}

/// Everything after the program's own path, which may be quoted and
/// may contain spaces.
fn after_the_program(command_line: &str) -> &str {
    let line = command_line.trim();
    if let Some(rest) = line.strip_prefix('"') {
        return match rest.split_once('"') {
            Some((_, rest)) => rest,
            None => "",
        };
    }
    match line.split_once(char::is_whitespace) {
        Some((_, rest)) => rest,
        None => "",
    }
}

/// What to print when there is nothing registered at all.
pub fn nothing_was_registered(present: Present) -> bool {
    !present.task && !present.service
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEITHER: Present = Present {
        task: false,
        service: false,
    };
    const TASK: Present = Present {
        task: true,
        service: false,
    };
    const SERVICE: Present = Present {
        task: false,
        service: true,
    };
    const BOTH: Present = Present {
        task: true,
        service: true,
    };

    #[test]
    fn a_fresh_machine_gets_what_was_asked_for_and_nothing_else() {
        assert_eq!(install(Arrangement::Task, NEITHER), [Step::RegisterTask]);
        assert_eq!(
            install(Arrangement::Service, NEITHER),
            [Step::RegisterService]
        );
    }

    #[test]
    fn asking_again_for_what_is_already_there_just_reregisters_it() {
        // Re-registering is how an upgraded binary's new path gets in, so
        // it must not be skipped as a no-op.
        assert_eq!(install(Arrangement::Task, TASK), [Step::RegisterTask]);
        assert_eq!(
            install(Arrangement::Service, SERVICE),
            [Step::RegisterService]
        );
    }

    #[test]
    fn switching_removes_the_other_one_first() {
        assert_eq!(
            install(Arrangement::Service, TASK),
            [Step::RemoveTask, Step::RegisterService]
        );
        assert_eq!(
            install(Arrangement::Task, SERVICE),
            [Step::RemoveService, Step::RegisterTask]
        );
    }

    #[test]
    fn no_plan_ever_leaves_both_registered() {
        for wanted in [Arrangement::Task, Arrangement::Service] {
            for present in [NEITHER, TASK, SERVICE, BOTH] {
                let steps = install(wanted, present);
                let mut end = present;
                for step in &steps {
                    match step {
                        Step::RemoveTask => end.task = false,
                        Step::RemoveService => end.service = false,
                        Step::RegisterTask => end.task = true,
                        Step::RegisterService => end.service = true,
                    }
                }
                assert!(
                    !(end.task && end.service),
                    "{wanted:?} onto {present:?} left both registered"
                );
                // And the removal is never after the registration, which
                // is the window a reboot would fall into.
                let registered = steps
                    .iter()
                    .position(|s| matches!(s, Step::RegisterTask | Step::RegisterService));
                let removed = steps
                    .iter()
                    .position(|s| matches!(s, Step::RemoveTask | Step::RemoveService));
                if let (Some(r), Some(d)) = (registered, removed) {
                    assert!(
                        d < r,
                        "{wanted:?} onto {present:?} registered before removing"
                    );
                }
            }
        }
    }

    #[test]
    fn a_daemon_is_told_apart_from_a_command_that_ends_by_itself() {
        // The switch has to stop the other arrangement's daemon, and
        // the installer is itself an `smkvm.exe`. Killing by name alone
        // would have it stop itself partway through an install.
        for daemon in [
            r"C:\smkvm\smkvm.exe run --unattended",
            r"C:\smkvm\smkvm.exe serve",
            r"C:\smkvm\smkvm.exe connect 10.0.0.2",
            r"C:\smkvm\smkvm.exe service-main",
            r"C:\smkvm\smkvm.exe desktop-worker --unattended --pipe smkvm-worker-abc",
            r"smkvm.exe run",
            r"smkvm run --unattended --verbose",
        ] {
            assert!(is_a_daemon(daemon), "{daemon:?} is a daemon and was missed");
        }
        for passing in [
            r"C:\smkvm\smkvm.exe service install --system",
            r"C:\smkvm\smkvm.exe service uninstall",
            r"C:\smkvm\smkvm.exe service start",
            r"C:\smkvm\smkvm.exe service stop",
            r"C:\smkvm\smkvm.exe status",
            r"C:\smkvm\smkvm.exe monitors",
            r"C:\smkvm\smkvm.exe pair 10.0.0.2",
            r"C:\smkvm\smkvm.exe init --role client --server x",
            r"C:\smkvm\smkvm.exe",
        ] {
            assert!(
                !is_a_daemon(passing),
                "{passing:?} ends by itself and would have been killed"
            );
        }
    }

    #[test]
    fn a_program_path_with_spaces_in_it_does_not_look_like_an_argument() {
        // The path is quoted by the scheduler and by the service
        // manager, and `Program Files` would otherwise make `Files`
        // the first word.
        assert!(is_a_daemon(
            r#""C:\Program Files\smkvm\smkvm.exe" run --unattended"#
        ));
        assert!(!is_a_daemon(r#""C:\Program Files\smkvm\smkvm.exe" status"#));
        // And an unquoted one with no arguments at all.
        assert!(!is_a_daemon(r#""C:\Program Files\smkvm\smkvm.exe""#));
        assert!(!is_a_daemon(""));
    }

    #[test]
    fn flags_before_the_command_do_not_hide_it() {
        // Global flags may come first, and a daemon behind one is still
        // a daemon.
        assert!(is_a_daemon("smkvm.exe --unattended run"));
        assert!(is_a_daemon("smkvm.exe --log-file x.log run"));
        assert!(!is_a_daemon("smkvm.exe --verbose status"));
    }

    #[test]
    fn uninstall_removes_whichever_is_there_without_being_told() {
        assert_eq!(uninstall(TASK), [Step::RemoveTask]);
        assert_eq!(uninstall(SERVICE), [Step::RemoveService]);
        assert_eq!(uninstall(BOTH), [Step::RemoveTask, Step::RemoveService]);
        assert_eq!(uninstall(NEITHER), []);
        assert!(nothing_was_registered(NEITHER));
        assert!(!nothing_was_registered(SERVICE));
    }
}
