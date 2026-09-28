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
    fn uninstall_removes_whichever_is_there_without_being_told() {
        assert_eq!(uninstall(TASK), [Step::RemoveTask]);
        assert_eq!(uninstall(SERVICE), [Step::RemoveService]);
        assert_eq!(uninstall(BOTH), [Step::RemoveTask, Step::RemoveService]);
        assert_eq!(uninstall(NEITHER), []);
        assert!(nothing_was_registered(NEITHER));
        assert!(!nothing_was_registered(SERVICE));
    }
}
