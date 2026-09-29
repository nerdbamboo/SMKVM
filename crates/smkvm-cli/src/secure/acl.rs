//! Who may talk to the worker, written down as Windows reads it.
//!
//! The worker injects keystrokes at SYSTEM integrity onto whatever desktop
//! it is attached to, including the one a UAC prompt is on. Whatever can
//! reach its pipe can type the answer to an administrator's password
//! prompt, or anything else. So the pipe is not a convenience to be secured
//! later; the access control list on it is the whole of the worker's
//! security, and it is written here rather than left to whatever a default
//! would give.
//!
//! The list secures the *server* side: who may connect to a pipe this
//! process made. It says nothing about the other half of the question,
//! which is whether the pipe the worker opened is the one this process
//! made. That half is [`Guard`] and the checks in `windows::pipe`, and it
//! matters because any authenticated user may create a name in the pipe
//! namespace: a name guessed in advance and created first is a pipe the
//! worker would connect to believing it was the service.

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
///
/// This is the right list only because the worker runs as LocalSystem. A
/// worker started with the interactive user's token would be locked out by
/// it -- which is what happened in the first draft of this, and is why the
/// token and this string have to be read as one decision.
pub const PIPE_SDDL: &str = "O:SYD:P(A;;GA;;;SY)";

/// How many bytes of unguessable name each run gets.
///
/// Sixteen, because the name has to be one nobody can create before the
/// service does. A counter -- which this was -- gives
/// `smkvm-worker-0000000000000001` from every boot, so anything on the
/// machine can make that pipe first, and then the worker connects to *it*.
pub const NAME_BYTES: usize = 16;

/// The pipe's name for one run, from bytes that must come from the
/// system's random number generator rather than from a counter or a clock.
pub fn pipe_name(secret: &[u8; NAME_BYTES]) -> String {
    let mut name = String::with_capacity(13 + NAME_BYTES * 2);
    name.push_str("smkvm-worker-");
    for byte in secret {
        name.push(char::from_digit(u32::from(byte >> 4), 16).expect("a nibble is a hex digit"));
        name.push(char::from_digit(u32::from(byte & 0xf), 16).expect("a nibble is a hex digit"));
    }
    name
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

/// What the worker must satisfy itself of before it says a word.
///
/// The server-side list keeps everyone but LocalSystem out of the real
/// pipe. It cannot keep the worker out of a *different* pipe wearing the
/// same name, and a worker that starts talking to one of those is a SYSTEM
/// process taking instructions from whoever made it. These are the three
/// things that together rule that out, kept here as a list rather than as
/// remarks in the call site so that dropping one is a visible deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// The name was unguessable, so nobody could have made it first.
    UnguessableName,
    /// The worker opened the pipe refusing to let its holder impersonate
    /// it: `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`. Without this
    /// the default for a named pipe is full impersonation, so the holder of
    /// a squatted pipe can put on the worker's identity by asking.
    NoImpersonation,
    /// The worker asked which process is serving the pipe and satisfied
    /// itself that it is LocalSystem. This is the one that does not depend
    /// on the attacker having failed at something.
    ServerIsSystem,
}

impl Guard {
    /// All three, in the order the worker applies them.
    pub const ALL: [Guard; 3] = [
        Guard::UnguessableName,
        Guard::NoImpersonation,
        Guard::ServerIsSystem,
    ];
}

/// The two that may hold this machine's private key: the system account
/// and the built-in administrators.
///
/// As SDDL writes them, which is how they come back out of `icacls
/// /save`. Aliases rather than full identifiers because that is what the
/// system emits, and unlike the names `icacls` prints in its ordinary
/// output they are not translated.
pub const MAY_HOLD_THE_KEY: [&str; 2] = ["SY", "BA"];

/// Everyone an access list lets in, besides those named.
///
/// Setting a list and believing it worked is the same class of mistake as
/// the silent refused injection this whole project began with: the call
/// reports success, nothing is visibly wrong, and the consequence is
/// invisible until somebody goes looking. `icacls` in particular prints
/// "Successfully processed 0 files; Failed processing 1 files" and has
/// been known to exit zero doing it. So the list is read back and
/// checked, and this is the checking -- pure, because it is the part
/// with a decision in it, and because a parser that is wrong in the
/// permissive direction would be worse than no check at all.
///
/// Only allow entries are considered. A deny entry cannot grant anything,
/// and an audit entry is not access.
pub fn granted_to_anyone_but(sddl: &str, allowed: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = sddl;
    // Only the discretionary part. A system part (`S:`) holds audit and
    // integrity entries, which grant nobody anything, and reading them
    // as grants would refuse lists that are perfectly correct.
    if let Some(at) = rest.find("D:") {
        rest = &rest[at + 2..];
        if let Some(end) = rest.find("S:") {
            rest = &rest[..end];
        }
    } else {
        // No discretionary list at all means no restriction whatsoever --
        // every access is granted to everyone. Reported as the worst
        // possible answer rather than as an empty list of offenders,
        // which is exactly how it would read if it were counted by ACEs.
        return vec!["everyone (the list is absent, which grants all access)".into()];
    }
    for ace in rest.split('(').skip(1) {
        let ace = ace.split(')').next().unwrap_or_default();
        let fields: Vec<&str> = ace.split(';').collect();
        if fields.len() < 6 {
            continue;
        }
        if !fields[0].eq_ignore_ascii_case("A") && !fields[0].eq_ignore_ascii_case("OA") {
            continue;
        }
        let trustee = fields[5].trim();
        if trustee.is_empty() {
            continue;
        }
        if !allowed.iter().any(|a| a.eq_ignore_ascii_case(trustee)) {
            found.push(trustee.to_string());
        }
    }
    found
}

/// Pull the security descriptors out of what `icacls /save` writes.
///
/// The format is a line naming the file, then a line of SDDL, repeated.
/// Written as a tolerant scan rather than a strict parse because the
/// exact shape is not worth depending on, and because the only thing
/// done with the result is to look for entries that should not be there
/// -- a line missed is a check not made, which the caller treats as a
/// reason to refuse rather than as a pass.
pub fn descriptors_in(saved: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut name: Option<String> = None;
    for line in saved.lines() {
        let line = line.trim_end_matches(['\r', '\u{feff}']).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("D:") || line.starts_with("O:") || line.starts_with("G:") {
            if let Some(name) = name.take() {
                out.push((name, line.to_string()));
            }
        } else {
            name = Some(line.trim_start_matches('\u{feff}').to_string());
        }
    }
    out
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
    fn the_name_is_local_and_says_every_byte_it_was_given() {
        let secret = [
            0x00, 0x0f, 0xa5, 0xff, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0,
            0xb0, 0xc0,
        ];
        let name = pipe_name(&secret);
        assert_eq!(name, "smkvm-worker-000fa5ff102030405060708090a0b0c0");
        assert_eq!(pipe_path(&name), format!(r"\\.\pipe\{name}"));
        assert!(!pipe_path(&name).starts_with(r"\\smkvm"));
    }

    #[test]
    fn a_different_secret_is_a_different_name() {
        // The point of the whole thing: two runs must not be able to
        // produce the same name, and no run's name may be derivable from
        // the run before it.
        let mut one = [0u8; NAME_BYTES];
        let mut two = [0u8; NAME_BYTES];
        one[0] = 1;
        two[0] = 2;
        assert_ne!(pipe_name(&one), pipe_name(&two));
        assert_eq!(pipe_name(&one).len(), 13 + NAME_BYTES * 2);
    }

    #[test]
    fn a_list_naming_only_the_two_that_may_hold_the_key_has_nobody_else_on_it() {
        let good = "D:PAI(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)";
        assert!(granted_to_anyone_but(good, &MAY_HOLD_THE_KEY).is_empty());
    }

    #[test]
    fn ordinary_accounts_on_the_list_are_reported() {
        // `BU` is the built-in Users group -- every ordinary account on
        // the machine. This is the exact list the first version of the
        // directory produced, and the exact thing the install then
        // printed a line claiming was not so.
        let bad = "D:PAI(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)(A;OICIID;0x1200a9;;;BU)";
        assert_eq!(granted_to_anyone_but(bad, &MAY_HOLD_THE_KEY), vec!["BU"]);
    }

    #[test]
    fn a_stranger_who_made_the_directory_first_is_reported() {
        // A local account that pre-created the directory keeps an
        // explicit entry, written out as its full identifier rather than
        // an alias. Anything not on the allowed list counts, whatever
        // shape it takes.
        let squatted = "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;S-1-5-11)";
        assert_eq!(
            granted_to_anyone_but(squatted, &MAY_HOLD_THE_KEY),
            vec!["S-1-5-11"]
        );
    }

    #[test]
    fn a_denial_grants_nobody_anything_and_a_system_list_is_not_a_grant() {
        let with_a_denial = "D:PAI(D;;FA;;;BU)(A;;FA;;;SY)(A;;FA;;;BA)";
        assert!(granted_to_anyone_but(with_a_denial, &MAY_HOLD_THE_KEY).is_empty());
        // An integrity label lives in the system part and is not access.
        let labelled = "D:PAI(A;;FA;;;SY)(A;;FA;;;BA)S:(ML;;NWNRNX;;;LW)";
        assert!(granted_to_anyone_but(labelled, &MAY_HOLD_THE_KEY).is_empty());
    }

    #[test]
    fn no_list_at_all_is_the_worst_answer_rather_than_a_clean_one() {
        // A descriptor with no discretionary part grants everything to
        // everybody. Counting offending entries would find none and call
        // it safe, which is the direction a check must never fail in.
        let none = "O:BAG:BA";
        assert_eq!(granted_to_anyone_but(none, &MAY_HOLD_THE_KEY).len(), 1);
        assert!(granted_to_anyone_but(none, &MAY_HOLD_THE_KEY)[0].contains("absent"));
    }

    #[test]
    fn what_icacls_saves_is_read_back_as_a_file_and_its_list() {
        let saved = "smkvm\r\nD:PAI(A;OICI;FA;;;SY)\r\ndevice.toml\r\nD:AI(A;ID;FA;;;SY)\r\n";
        let found = descriptors_in(saved);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "smkvm");
        assert_eq!(found[1].0, "device.toml");
        assert!(found[1].1.starts_with("D:"));
    }

    #[test]
    fn the_worker_side_is_three_checks_and_deleting_one_is_visible() {
        assert_eq!(Guard::ALL.len(), 3);
        assert!(Guard::ALL.contains(&Guard::ServerIsSystem));
    }
}
