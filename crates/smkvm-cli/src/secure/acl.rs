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

/// The access list on the reader's pipe.
///
/// The reader runs as the person at the desk, so it cannot open a
/// pipe that admits the system account alone, and this one admits
/// interactive users as well. That is not a weakening of the
/// consent-prompt boundary -- a pipe is not a desktop, and the worker
/// that reaches the secure desktop keeps its own pipe, which still
/// admits nobody but the system account.
///
/// Two deliberate choices in these fourteen characters.
///
/// `IU` rather than the person's own SID, because the SID is not
/// known until a session exists and the pipe is made before the
/// reader is started -- made first on purpose, so that nothing can
/// be sitting on the name when the reader goes looking for it.
/// Interactive users is the set that contains exactly the person at
/// the screen.
///
/// `GRGW` rather than `GA`, which is the difference that matters.
/// Generic all includes `WRITE_DAC`, so a process running as that
/// person could rewrite this list and let anything at all in. Read
/// and write is what talking down a pipe needs and is all they get.
pub const READER_PIPE_SDDL: &str = "O:SYD:P(A;;GA;;;SY)(A;;GRGW;;;IU)";

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
    named_pipe("smkvm-worker-", secret)
}

/// The same, for the half that runs as the person.
///
/// A separate name because they are separate pipes, with different
/// access lists and different protocols, and a reader that announced
/// itself as connecting to `smkvm-worker-…` was read twice as a sign
/// that the two ends were computing different names. They were not;
/// the name was simply wrong about what it named. A diagnostic that
/// has to be explained away once will be explained away again.
pub fn reader_pipe_name(secret: &[u8; NAME_BYTES]) -> String {
    named_pipe("smkvm-reader-", secret)
}

fn named_pipe(prefix: &str, secret: &[u8; NAME_BYTES]) -> String {
    let mut name = String::with_capacity(prefix.len() + NAME_BYTES * 2);
    name.push_str(prefix);
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

/// The entry types that cannot grant anybody anything.
///
/// The list is written this way round -- what is harmless -- and
/// everything else counts as a grant, *including a type this build has
/// never heard of*. The first version listed the two granting types it
/// knew, `A` and `OA`, and ignored the rest. That left `XA` and `ZA`,
/// the callback forms, invisible: a conditional allow with a trivially
/// true condition grants Everyone full control and read back as a clean
/// list. Not a theoretical gap -- it is available to exactly the
/// attacker this check exists for, in exactly the scenario it exists
/// for: somebody who owned the directory before the install and left an
/// explicit entry, which `/setowner`, `/inheritance:r`, `/remove` and
/// `/grant:r` all leave alone.
///
/// Denials (`D`, `OD`, `XD`, `ZD`) grant nothing by definition. Audit
/// and alarm entries (`AU`, `AL`, `OU`, `OL`, `XU`) record access rather
/// than permitting it. Mandatory labels (`ML`), resource attributes
/// (`RA`), scoped policy ids (`SP`), process trust labels (`TL`) and
/// access filters (`FL`) live in the system part and are not access.
/// Anything else, known or not, is treated as letting somebody in.
const NEVER_GRANTS: [&str; 14] = [
    "D", "OD", "XD", "ZD", "AU", "AL", "OU", "OL", "XU", "ML", "RA", "SP", "TL", "FL",
];

/// What is said when the list grants everything to everybody.
const NO_LIST: &str = "everyone (the list is absent, which grants all access)";

/// What is said when the descriptor carries two of them.
///
/// The system function that produces this text serialises exactly one
/// discretionary list, so this should not be reachable. It is refused
/// rather than reasoned about because the alternative -- taking the
/// first and discarding the rest -- is the same shape as every other
/// bypass found in this file: a way for something later in the string to
/// go unexamined.
const TWO_LISTS: &str =
    "everyone (the descriptor carries two discretionary lists, so which one applies \
     cannot be told from here)";

/// One entry, as much as is needed of it.
struct Entry<'a> {
    kind: String,
    flags: String,
    trustee: &'a str,
    raw: &'a str,
    /// Whether it had the six fields every real entry has.
    whole: bool,
}

/// Everyone an access list lets in, besides those named.
///
/// Setting a list and believing it worked is the same class of mistake
/// as the silent refused injection this whole project began with: the
/// call reports success, nothing is visibly wrong, and the consequence
/// is invisible until somebody goes looking. `icacls` in particular
/// prints "Successfully processed 0 files; Failed processing 1 files"
/// and has been known to exit zero doing it. So the list is read back
/// and checked, and this is the checking -- pure, because it is the part
/// with a decision in it, and because a parser that is wrong in the
/// permissive direction is worse than no check at all.
///
/// Every rule here fails towards "somebody can read it". An entry that
/// cannot be read is an offender; a type that is not recognised is an
/// offender; a list that says it does not exist is the worst answer
/// rather than an empty one. The cost of being wrong that way is a
/// refused install with a confusing message. The cost of being wrong the
/// other way is a private key anyone on the machine can copy, which no
/// rollback undoes.
pub fn granted_to_anyone_but(sddl: &str, allowed: &[&str]) -> Vec<String> {
    let entries = match read_list(sddl) {
        Ok(entries) => entries,
        Err(why) => return vec![why.into()],
    };
    let mut found = Vec::new();
    for entry in entries {
        if NEVER_GRANTS.contains(&entry.kind.as_str()) {
            continue;
        }
        if !entry.whole {
            found.push(format!(
                "an entry this build could not read: ({})",
                entry.raw
            ));
            continue;
        }
        if entry.trustee.is_empty() {
            found.push(format!("an entry naming nobody: ({})", entry.raw));
            continue;
        }
        if !allowed
            .iter()
            .any(|a| a.eq_ignore_ascii_case(entry.trustee))
        {
            found.push(entry.trustee.to_string());
        }
    }
    found
}

/// Everyone a *directory's* list passes on to the files inside it,
/// besides those named.
///
/// The directory itself may legitimately let ordinary accounts in -- they
/// have to be able to walk into the readable corner. What they must not
/// have is anything that propagates, because a file created later
/// inherits it, and the file created later is the private key. So this
/// asks a narrower question than [`granted_to_anyone_but`]: not "who is
/// on this list" but "who does this list hand down".
pub fn inheritably_granted_to_anyone_but(sddl: &str, allowed: &[&str]) -> Vec<String> {
    let entries = match read_list(sddl) {
        Ok(entries) => entries,
        Err(why) => return vec![why.into()],
    };
    let mut found = Vec::new();
    for entry in entries {
        if NEVER_GRANTS.contains(&entry.kind.as_str()) {
            continue;
        }
        if !entry.whole {
            found.push(format!(
                "an entry this build could not read: ({})",
                entry.raw
            ));
            continue;
        }
        let inherits = entry.flags.contains("OI") || entry.flags.contains("CI");
        if !inherits {
            continue;
        }
        if entry.trustee.is_empty() {
            found.push(format!("an entry naming nobody: ({})", entry.raw));
            continue;
        }
        if !allowed
            .iter()
            .any(|a| a.eq_ignore_ascii_case(entry.trustee))
        {
            found.push(entry.trustee.to_string());
        }
    }
    found
}

/// The entries of the discretionary list, or the reason there is no
/// single list to read -- which is always a reason to assume the worst.
fn read_list(sddl: &str) -> Result<Vec<Entry<'_>>, &'static str> {
    let dacl = discretionary_part(sddl)?;
    let (flags, raws) = flags_and_entries(dacl);
    // The spelling Windows actually emits for a null list. The first
    // version tested a descriptor with no `D:` at all and believed that
    // covered it -- a test written for precisely this bug that missed it
    // by guessing how the system would say it.
    if flags.to_ascii_uppercase().contains("NO_ACCESS_CONTROL") {
        return Err(NO_LIST);
    }
    Ok(raws
        .into_iter()
        .map(|raw| {
            let fields: Vec<&str> = raw.split(';').collect();
            Entry {
                kind: fields
                    .first()
                    .map(|k| k.trim().to_ascii_uppercase())
                    .unwrap_or_default(),
                flags: fields
                    .get(1)
                    .map(|f| f.trim().to_ascii_uppercase())
                    .unwrap_or_default(),
                trustee: fields.get(5).map(|t| t.trim()).unwrap_or_default(),
                raw,
                whole: fields.len() >= 6,
            }
        })
        .collect())
}

/// The text of the discretionary part, if there is one.
///
/// Found by walking the descriptor and noticing `D:` and the `S:` that
/// ends it only *outside* brackets. Searching the whole string for them
/// is what the first version did, and a conditional entry whose
/// expression contains those two characters was enough to cut the list
/// short so that every entry after it went unexamined -- a grant to
/// Everyone, following an entry naming a perfectly allowed principal,
/// read back clean. A condition is attacker-written text sitting inside
/// the thing being parsed, so nothing may be located by searching
/// through it.
fn discretionary_part(sddl: &str) -> Result<&str, &'static str> {
    let bytes = sddl.as_bytes();
    let mut depth = 0usize;
    let mut start: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
                continue;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
                continue;
            }
            _ => {}
        }
        let marker = depth == 0
            && i + 1 < bytes.len()
            && bytes[i + 1] == b':'
            && matches!(bytes[i].to_ascii_uppercase(), b'O' | b'G' | b'D' | b'S');
        if marker {
            if let Some(from) = start {
                // A second discretionary part is refused rather than
                // resolved; see `TWO_LISTS`.
                if bytes[i].eq_ignore_ascii_case(&b'D') {
                    return Err(TWO_LISTS);
                }
                // Any other later component ends the discretionary one.
                return Ok(&sddl[from..i]);
            }
            if bytes[i].eq_ignore_ascii_case(&b'D') {
                start = Some(i + 2);
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    start.map(|from| &sddl[from..]).ok_or(NO_LIST)
}

/// The flags before the first entry, and each entry's text without its
/// outermost brackets.
///
/// Bracket depth is tracked rather than split on. Splitting on `(` --
/// which is what this did -- tears a conditional entry into fragments
/// that are each too short to look like an entry, so the real one is
/// lost and the fragments are skipped as unreadable. A grant to an
/// ordinary group written that way read back as a clean list. Getting
/// this right is also what makes refusing a short entry safe: after
/// tokenising properly, an entry with fewer than six fields is genuinely
/// malformed rather than an artefact of the scan.
fn flags_and_entries(dacl: &str) -> (&str, Vec<&str>) {
    let bytes = dacl.as_bytes();
    let mut depth = 0usize;
    let mut flags_end = dacl.len();
    let mut entries = Vec::new();
    let mut entry_start = 0usize;
    for (i, byte) in bytes.iter().enumerate() {
        match byte {
            b'(' => {
                if depth == 0 {
                    if entries.is_empty() {
                        flags_end = flags_end.min(i);
                    }
                    entry_start = i + 1;
                }
                depth += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    entries.push(&dacl[entry_start..i]);
                }
            }
            _ => {}
        }
    }
    // Opened and never closed. Kept rather than dropped, so the field
    // count refuses it instead of it vanishing.
    if depth > 0 {
        entries.push(&dacl[entry_start..]);
    }
    (&dacl[..flags_end], entries)
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

    fn strangers(sddl: &str) -> Vec<String> {
        granted_to_anyone_but(sddl, &MAY_HOLD_THE_KEY)
    }

    /// The question every one of these asks: would this list be blessed
    /// while somebody who should not be able to read the key can?
    fn says_it_is_private(sddl: &str) -> bool {
        strangers(sddl).is_empty()
    }

    #[test]
    fn a_list_naming_only_the_two_that_may_hold_the_key_has_nobody_else_on_it() {
        assert!(says_it_is_private(
            "D:PAI(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)"
        ));
        // Lower case, and the components in the order Windows writes
        // them, with an owner and a group in front.
        assert!(says_it_is_private(
            "O:BAG:BAd:pai(a;oiciid;fa;;;sy)(a;;fa;;;ba)"
        ));
    }

    #[test]
    fn ordinary_accounts_on_the_list_are_reported() {
        // `BU` is the built-in Users group -- every ordinary account on
        // the machine. This is the exact list the first version of the
        // directory produced, and the exact thing the install then
        // printed a line claiming was not so.
        let bad = "D:PAI(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)(A;OICIID;0x1200a9;;;BU)";
        assert_eq!(strangers(bad), vec!["BU"]);
    }

    #[test]
    fn a_stranger_who_made_the_directory_first_is_reported() {
        let squatted = "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;S-1-5-11)";
        assert_eq!(strangers(squatted), vec!["S-1-5-11"]);
    }

    // ---------------------------------------------------------------
    // The four ways this said SAFE while the key was readable.
    //
    // Found by running inputs through it rather than by reading it, so
    // they are kept verbatim: every one of these is a list the parser
    // blessed and an attacker could have written. The tests that were
    // here before were all inputs somebody had thought of, and the gap
    // was the spelling nobody did.
    // ---------------------------------------------------------------

    #[test]
    fn the_spelling_windows_actually_uses_for_no_list_at_all() {
        // The old test covered a descriptor with *no* `D:` component.
        // This is what the system emits instead, and it parsed to zero
        // entries and read clean -- a test written for exactly this bug
        // that missed it by guessing the wording.
        assert!(!says_it_is_private("O:SYG:SYD:NO_ACCESS_CONTROL"));
        assert!(strangers("O:SYG:SYD:NO_ACCESS_CONTROL")[0].contains("absent"));
        assert!(!says_it_is_private("D:NO_ACCESS_CONTROLS:AI"));
        // And still the case with no discretionary component at all.
        assert!(!says_it_is_private("O:BAG:BA"));
    }

    #[test]
    fn a_conditional_allow_is_a_grant_like_any_other() {
        // `XA` and `ZA` were skipped entirely, because the type test
        // listed the granting types instead of the harmless ones. A
        // condition that is trivially true grants Everyone everything.
        assert_eq!(
            strangers(r#"D:P(A;;FA;;;SY)(XA;;FA;;;WD;(1==1))"#),
            vec!["WD"]
        );
        assert_eq!(strangers(r#"D:P(A;;FA;;;SY)(ZA;;FA;;;WD;(x))"#), vec!["WD"]);
    }

    #[test]
    fn a_condition_cannot_cut_the_list_short() {
        // The `S:` inside the condition ended the scan, so the grant
        // after it was never examined. Note the truncating entry names
        // an *allowed* principal, so fixing the type test alone does
        // nothing here: two separate faults, one attacker.
        assert_eq!(
            strangers(r#"D:P(XA;;FA;;;BA;(@a=="S:"))(A;;FA;;;WD)"#),
            vec!["WD"]
        );
        // The same list without the trick, which always worked.
        assert_eq!(
            strangers(r#"D:P(XA;;FA;;;BA;(@a=="x"))(A;;FA;;;WD)"#),
            vec!["WD"]
        );
        // And a condition may not smuggle in a discretionary part either.
        assert_eq!(
            strangers(r#"D:P(XA;;FA;;;BA;(@a=="D:(A;;FA;;;SY)"))(A;;FA;;;WD)"#),
            vec!["WD"]
        );
    }

    #[test]
    fn a_condition_with_brackets_of_its_own_does_not_fragment_the_entry() {
        // Splitting on `(` tore this into pieces that were each too
        // short to look like an entry: the real one was lost to the type
        // test and the fragments were skipped as unreadable, so a grant
        // to an ordinary group read back clean.
        assert_eq!(
            strangers("D:P(XA;;FA;;;BU;(Member_of{SID(BA)}))"),
            vec!["BU"]
        );
        // Deeper nesting, and a second entry after it that must still be
        // seen.
        assert_eq!(
            strangers("D:P(XA;;FA;;;BA;(a((b))c))(A;;FA;;;WD)"),
            vec!["WD"]
        );
    }

    #[test]
    fn a_truncated_entry_is_refused_rather_than_skipped() {
        // Skipping what cannot be read is the permissive direction, and
        // a short entry is exactly what somebody would write to be
        // skipped. Safe to refuse only because entries are tokenised by
        // brackets first -- before that, the scan produced short
        // fragments of its own and refusing would have rejected sound
        // input.
        for broken in [
            // Cut off mid-entry, with the closing bracket missing.
            "D:P(A;;FA;;;",
            // Cut off earlier, so it has too few fields to be an entry.
            "D:P(A;;FA",
            // An empty entry.
            "D:P(A;;FA;;;SY)()",
            // Whole, but naming nobody.
            "D:P(A;;FA;;;SY)(A;;FA;;;)",
        ] {
            assert!(!says_it_is_private(broken), "{broken} was blessed");
            let said = &strangers(broken)[0];
            assert!(
                said.contains("could not read") || said.contains("naming nobody"),
                "{broken} was refused for an unexpected reason: {said}"
            );
        }
    }

    // ---------------------------------------------------------------
    // Each type that grants nobody anything, pinned one at a time, so
    // the list is a fact rather than an assumption.
    // ---------------------------------------------------------------

    #[test]
    fn every_non_granting_type_is_ignored_and_nothing_else_is() {
        for kind in [
            "D", "OD", "XD", "ZD", "AU", "AL", "OU", "OL", "XU", "ML", "RA", "SP", "TL", "FL",
        ] {
            let list = format!("D:P(A;;FA;;;SY)({kind};;FA;;;WD)");
            assert!(
                says_it_is_private(&list),
                "{kind} was treated as granting, and it cannot grant"
            );
        }
        for kind in ["A", "OA", "XA", "ZA"] {
            let list = format!("D:P(A;;FA;;;SY)({kind};;FA;;;WD)");
            assert_eq!(strangers(&list), vec!["WD"], "{kind} grants and was missed");
        }
    }

    #[test]
    fn a_type_this_build_has_never_heard_of_counts_as_granting() {
        // The rule that makes the list above safe to keep: being
        // unrecognised must not be a way through. If Windows grows a new
        // allow form, this refuses it until somebody has looked.
        assert_eq!(strangers("D:P(A;;FA;;;SY)(QQ;;FA;;;WD)"), vec!["WD"]);
        assert_eq!(strangers("D:P(A;;FA;;;SY)(;;FA;;;WD)"), vec!["WD"]);
    }

    #[test]
    fn what_a_directory_hands_down_is_a_narrower_question() {
        // Ordinary accounts may reach the directory -- they have to walk
        // into the readable corner -- but must inherit nothing from it,
        // because what is created there later is the key.
        let right = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x1200a9;;;BU)";
        assert!(
            inheritably_granted_to_anyone_but(right, &MAY_HOLD_THE_KEY).is_empty(),
            "read on the directory object alone hands nothing down"
        );
        // The first version's list, which is what put the key where
        // anyone could read it.
        let wrong = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";
        assert_eq!(
            inheritably_granted_to_anyone_but(wrong, &MAY_HOLD_THE_KEY),
            vec!["BU"]
        );
        // Either inheritance flag alone is enough to reach a new file.
        for flags in ["OI", "CI", "OICIIO", "CIOI"] {
            let list = format!("D:P(A;OICI;FA;;;SY)(A;{flags};FA;;;WD)");
            assert_eq!(
                inheritably_granted_to_anyone_but(&list, &MAY_HOLD_THE_KEY),
                vec!["WD"],
                "{flags} propagates and was missed"
            );
        }
        // And the same evasions must not work on this question either.
        assert!(
            !inheritably_granted_to_anyone_but("O:SYD:NO_ACCESS_CONTROL", &MAY_HOLD_THE_KEY)
                .is_empty()
        );
        assert_eq!(
            inheritably_granted_to_anyone_but(
                "D:P(XA;;FA;;;BA;(@a==\"S:\"))(A;OICI;FA;;;WD)",
                &MAY_HOLD_THE_KEY
            ),
            vec!["WD"]
        );
    }

    #[test]
    fn two_discretionary_lists_are_refused_rather_than_resolved() {
        // Not reachable through the system function that writes this
        // text, which serialises exactly one. Refused anyway, because
        // taking the first and discarding the rest is the same shape as
        // every other bypass in this file.
        let doubled = "D:P(A;;FA;;;SY)D:P(A;;FA;;;WD)";
        assert!(!says_it_is_private(doubled));
        assert!(strangers(doubled)[0].contains("two discretionary lists"));
        // One list followed by a system part is still perfectly ordinary.
        assert!(says_it_is_private("D:P(A;;FA;;;SY)S:(ML;;NW;;;LW)"));
    }

    #[test]
    fn a_denial_grants_nobody_anything_and_a_system_list_is_not_a_grant() {
        assert!(says_it_is_private(
            "D:PAI(D;;FA;;;BU)(A;;FA;;;SY)(A;;FA;;;BA)"
        ));
        // An integrity label lives in the system part and is not access.
        assert!(says_it_is_private(
            "D:PAI(A;;FA;;;SY)(A;;FA;;;BA)S:(ML;;NWNRNX;;;LW)"
        ));
        // A system part that grants Everyone is still not access.
        assert!(says_it_is_private("D:P(A;;FA;;;SY)S:(AU;SAFA;FA;;;WD)"));
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

    #[test]
    fn the_readers_pipe_lets_the_person_in_and_no_further() {
        // The person at the desk must be able to open it, or the
        // reader cannot exist. They must not be able to rewrite who
        // else can, which is what generic all would give them.
        assert!(
            READER_PIPE_SDDL.contains("(A;;GRGW;;;IU)"),
            "the person at the desk cannot open the reader's pipe"
        );
        assert!(
            !READER_PIPE_SDDL.contains("(A;;GA;;;IU)"),
            "generic all includes WRITE_DAC, which would let the person rewrite this list"
        );
    }

    #[test]
    fn the_workers_pipe_still_admits_nobody_but_the_system_account() {
        // The reader's pipe being open to the person must not have
        // quietly opened the worker's. The worker is the half that
        // reaches a consent prompt.
        assert_eq!(PIPE_SDDL, "O:SYD:P(A;;GA;;;SY)");
        assert!(granted_to_anyone_but(PIPE_SDDL, &MAY_HOLD_THE_KEY).is_empty());
    }

    #[test]
    fn the_two_pipes_are_never_the_same_name() {
        // Not merely different prefixes: the same secret must not
        // produce a name either end could mistake for the other's.
        let secret = [7u8; NAME_BYTES];
        let worker = pipe_name(&secret);
        let reader = reader_pipe_name(&secret);
        assert_ne!(worker, reader);
        assert!(worker.starts_with("smkvm-worker-"), "{worker}");
        assert!(reader.starts_with("smkvm-reader-"), "{reader}");
        assert_eq!(worker.len(), reader.len());
    }
}
