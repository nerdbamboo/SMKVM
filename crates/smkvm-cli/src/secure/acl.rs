//! Who may talk to the worker, written down as Windows reads it.
//!
//! The worker injects keystrokes at SYSTEM integrity onto whatever desktop
//! it is attached to, including the one a UAC prompt is on. Whatever can
//! reach its pipe can type the administrator's password prompt's answer, or
//! anything else. So the pipe is not a convenience to be secured later; the
//! access control list on it is the whole of the worker's security, and it
//! is written here rather than left to whatever a default would give.

/// The pipe's security descriptor, in the language Windows parses.
///
/// Read left to right:
///
/// * `O:SY` -- owned by LocalSystem. The owner of an object can always
///   rewrite its access control list, so an owner in the person's session
///   would make everything after this advisory. (There is no `G:`; a
///   primary group takes no part in an access check on Windows and saying
///   one here would only be noise.)
/// * `D:P` -- a *protected* discretionary access control list: nothing is
///   inherited from the pipe namespace's container. Without `P`, an
///   inherited allow entry from somewhere else would be added to the ones
///   written here, and the list would say more than it appears to.
/// * `(A;;GA;;;SY)` -- one entry, and only one: LocalSystem may do
///   everything. There is no entry for Administrators, and that omission is
///   deliberate. An administrator can become LocalSystem if they set out to,
///   but they have to set out to; leaving `BA` out of this list means no
///   process merely running as administrator -- an installer, a script, a
///   compromised elevated shell -- can open the pipe and drive the keyboard
///   of a machine that is showing a consent prompt. There is no entry for
///   the interactive user either, which is the same argument made shorter.
///
/// Nothing else is granted, so nothing else is allowed: a discretionary
/// access control list that is present and lists nobody else denies
/// everybody else. (This is why the list must be *present* rather than
/// absent; `NULL` would mean "everyone, everything", which is the shape of
/// this mistake that has been made most often.)
pub const PIPE_SDDL: &str = "O:SYD:P(A;;GA;;;SY)";

/// The pipe's name, given the identifier that makes it this run's.
///
/// A fresh name per run, rather than one constant: the pipe is created with
/// `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a name already taken is a refusal
/// rather than a second server quietly attaching to somebody else's, and a
/// name nobody can guess in advance is one nobody has squatted on before
/// the service got there. The worker is told the name on its command line,
/// which is the only place it is ever written down.
pub fn pipe_name(run: u64) -> String {
    format!("smkvm-worker-{run:016x}")
}

/// Where that name lives, as a path to open.
///
/// Always `\\.\pipe\`, never `\\<host>\pipe\`: this pipe is between two
/// processes on one machine and has no business being reachable from
/// another. The server side additionally sets `PIPE_REJECT_REMOTE_CLIENTS`,
/// because a local path here does not by itself stop a remote open there.
pub fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_grants_system_and_nobody_else() {
        // If this ever gains an entry, the comment above has to gain a
        // paragraph saying who and why. The test is here to force that.
        assert_eq!(PIPE_SDDL.matches("(A;").count(), 1);
        assert!(PIPE_SDDL.contains("(A;;GA;;;SY)"));
        assert!(!PIPE_SDDL.contains("BA"));
        assert!(!PIPE_SDDL.contains("IU"));
        assert!(!PIPE_SDDL.contains("WD"));
    }

    #[test]
    fn the_list_is_protected_and_owned_by_system() {
        assert!(PIPE_SDDL.starts_with("O:SY"));
        assert!(PIPE_SDDL.contains("D:P"));
    }

    #[test]
    fn the_name_is_local_and_differs_per_run() {
        let one = pipe_name(1);
        let two = pipe_name(0xdead_beef);
        assert_ne!(one, two);
        assert_eq!(pipe_path(&two), r"\\.\pipe\smkvm-worker-00000000deadbeef");
        assert!(!pipe_path(&one).starts_with(r"\\smkvm"));
    }
}
