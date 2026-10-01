//! What the service may ask a reader, and what a reader may answer.
//!
//! A separate protocol from the worker's, and separate on purpose
//! rather than for tidiness.
//!
//! The worker runs as the system account because nothing else can
//! reach a consent prompt, and its pipe admits the system account and
//! nobody else. The reader runs as the logged-on person, because
//! nothing else can see what that person copied -- measured three
//! ways, including a thread impersonating them, which saw no more
//! than the system account did. So the reader's pipe has to admit
//! that person, and therefore admits **anything running as them**.
//!
//! That is the whole reason this file exists. If the protocol cannot
//! express an injection, nobody has to reason about whether one can
//! be smuggled through an endpoint the person's own processes can
//! reach. [`ToReader`] has three variants: ask what is on the
//! clipboard, ask for one format, and stop. There is no variant that
//! moves a pointer, presses a key, writes the clipboard, or names a
//! desktop, and a test here fails if one is added.
//!
//! What an impostor connecting to that pipe could do is lie about
//! what has been copied, and so put something of its choosing on
//! another machine's clipboard. That is worth knowing and it is not
//! an escalation: anything running as that person can already put
//! whatever it likes on the clipboard, and this program would carry
//! it. The pipe's name is unguessable and its client's process id is
//! checked against the one that was started, so an impostor has to
//! win a race it cannot see the start of.

use serde::{Deserialize, Serialize};
use smkvm_proto::ClipFormat;

use crate::secure::wire::Level;

/// Bumped when either side's messages change meaning.
pub const READER_PROTOCOL: u32 = 2;

/// Longest frame either side will send or accept.
///
/// Larger than the worker's, and it has to be: the worker's messages
/// are a handful of integers, and these carry what was copied. A
/// screenshot is megabytes. The cap is here to stop a mistake rather
/// than an attacker -- the reader's process id is checked and its
/// pipe name is unguessable -- but a length prefix with no bound is
/// how a denial of service gets written by accident.
pub const LONGEST_READ: usize = 64 * 1024 * 1024;

/// Everything the service may ask of the reader.
///
/// Three things, all of them reading. See the note at the top of this
/// file before adding a fourth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToReader {
    /// What formats are on the person's clipboard right now?
    WhatIsOnIt { id: u64 },
    /// Hand over one of them.
    Read { id: u64, format: ClipFormat },
    /// Let go and exit.
    Stop,
}

/// Everything the reader may say.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromReader {
    /// First, before anything else.
    Ready {
        protocol: u32,
        /// Who it is running as, so the log can show that the half
        /// which needs to be the person really is.
        who: String,
        session: u32,
        /// Where it is writing its own log, or why it has none.
        ///
        /// Said here, in the first thing it ever sends, because this
        /// process has no console and nothing else can ask it. It
        /// once died opening that log and the only evidence was a
        /// pipe that broke; a process that can fail before saying
        /// where it is cannot be diagnosed at all.
        log: Result<String, String>,
    },
    /// Somebody copied something here. Unasked: this is the whole
    /// point of the reader existing.
    Copied(Vec<ClipFormat>),
    /// The answer to `WhatIsOnIt`.
    OnIt { id: u64, formats: Vec<ClipFormat> },
    /// The answer to `Read`.
    Read {
        id: u64,
        bytes: Result<Vec<u8>, String>,
    },
    /// Something worth putting in the service's log.
    Said { level: Level, text: String },
}

/// Longest a line from the reader may be before it is cut short.
///
/// The log is the one artefact every diagnosis in this project has
/// turned on, and `Said` is the one thing in either direction that is
/// not a read: unsolicited, arbitrary, and chosen in severity by the
/// sender. Anything running as the person could connect and fill the
/// disk, or -- worse, because it is quiet -- embed newlines and
/// reproduce whole log lines of its own with no `reader:` prefix on
/// them.
///
/// Nothing is escalated by that. It is a diagnostics-integrity
/// problem, which in this codebase is not a small category.
pub const LONGEST_SAID: usize = 400;

/// What a line from the reader may be written as.
///
/// Control characters go, including the newlines that would let a
/// line forge others, and the whole thing is cut to something a log
/// can hold. The cut is announced, so a truncated line cannot be
/// mistaken for a complete one.
pub fn tidy(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(LONGEST_SAID)
        .collect();
    // Counted in characters, not bytes, because the cut above is in
    // characters: by bytes, a line of Korean would be reported as
    // truncated when nothing had been removed, on exactly the
    // machines this runs on.
    if text.chars().count() > LONGEST_SAID {
        out.push_str(" [cut short]");
    }
    out
}

/// Who the reader says it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Who {
    pub who: String,
    pub session: u32,
    pub log: Result<String, String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum NotWelcome {
    #[error(
        "the reader speaks protocol {theirs} and this speaks {ours}: two halves of one \
         installation are different builds. Reinstall from one binary"
    )]
    WrongProtocol { theirs: u32, ours: u32 },
    #[error("the reader said something else before saying hello")]
    SpokeOutOfTurn,
    #[error(
        "the reader says it is running as {0:?}, which is the system account. The whole \
         reason it exists is to be the person instead, and as the system account it will \
         see nothing they copied"
    )]
    TheSystemAccountAgain(String),
    #[error("the reader did not say who it is running as")]
    Nameless,
}

/// Names that mean the system account, in the forms it appears in.
///
/// Compared in upper case because the account's name is translated on
/// a Korean or Japanese Windows but its well-known form is not, and
/// because a name is only a sanity check here. Nothing is granted on
/// the strength of it: it catches the reader having been started the
/// wrong way, which is a mistake in this program, not an attack.
const THE_SYSTEM_ACCOUNT: &[&str] = &["NT AUTHORITY\\SYSTEM", "SYSTEM", "LOCALSYSTEM"];

/// The first thing a reader says, checked.
pub fn welcome(first: FromReader) -> Result<Who, NotWelcome> {
    let FromReader::Ready {
        protocol,
        who,
        session,
        log,
    } = first
    else {
        return Err(NotWelcome::SpokeOutOfTurn);
    };
    if protocol != READER_PROTOCOL {
        return Err(NotWelcome::WrongProtocol {
            theirs: protocol,
            ours: READER_PROTOCOL,
        });
    }
    let named = who.trim();
    if named.is_empty() {
        return Err(NotWelcome::Nameless);
    }
    // The one mistake worth refusing outright. A reader running as the
    // system account is not a reader; it is the thing that was already
    // there and already could not see anything. Letting it attach
    // would produce a clipboard that silently never reports a copy,
    // which is the fault this whole arrangement exists to end.
    let shouted = named.to_ascii_uppercase();
    if THE_SYSTEM_ACCOUNT.iter().any(|s| shouted == *s) {
        return Err(NotWelcome::TheSystemAccountAgain(named.to_string()));
    }
    Ok(Who {
        who: named.to_string(),
        session,
        log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// If this fails to compile because a variant was added, that is
    /// the test working.
    ///
    /// The question to answer before changing it: can anything running
    /// as the person at the desk now make this program do something
    /// other than read? The endpoint is reachable by every process
    /// they own. "It is only a small convenience" is how the answer
    /// becomes yes.
    fn what_it_is(asked: &ToReader) -> &'static str {
        match asked {
            ToReader::WhatIsOnIt { .. } => "read: what is on it",
            ToReader::Read { .. } => "read: one format",
            ToReader::Stop => "stop",
        }
    }

    #[test]
    fn the_reader_can_only_be_asked_to_read_or_to_stop() {
        let every = [
            ToReader::WhatIsOnIt { id: 1 },
            ToReader::Read {
                id: 2,
                format: ClipFormat::Text,
            },
            ToReader::Stop,
        ];
        let named: Vec<&str> = every.iter().map(what_it_is).collect();
        assert_eq!(
            named,
            vec!["read: what is on it", "read: one format", "stop"],
            "the reader's endpoint is reachable by anything running as the person at the \
             desk; everything it accepts must be a read"
        );
    }

    #[test]
    fn a_reader_speaking_another_protocol_is_refused() {
        let said = welcome(FromReader::Ready {
            protocol: READER_PROTOCOL + 1,
            who: "SOMEWHERE\\someone".into(),
            session: 1,
            log: Ok("C:\\somewhere\\smkvm.log".into()),
        });
        assert_eq!(
            said,
            Err(NotWelcome::WrongProtocol {
                theirs: READER_PROTOCOL + 1,
                ours: READER_PROTOCOL
            })
        );
    }

    #[test]
    fn a_reader_running_as_the_system_account_is_refused() {
        // The failure this exists to prevent is silent: such a reader
        // attaches, works, and never reports a copy, which is exactly
        // the fault the reader was added to end.
        for named in [
            "NT AUTHORITY\\SYSTEM",
            "nt authority\\system",
            "SYSTEM",
            "  LocalSystem  ",
        ] {
            let said = welcome(FromReader::Ready {
                protocol: READER_PROTOCOL,
                who: named.into(),
                session: 1,
                log: Ok(String::new()),
            });
            assert!(
                matches!(said, Err(NotWelcome::TheSystemAccountAgain(_))),
                "{named} was accepted"
            );
        }
    }

    #[test]
    fn a_reader_that_is_somebody_is_welcome() {
        let said = welcome(FromReader::Ready {
            protocol: READER_PROTOCOL,
            who: "  SOMEWHERE\\someone  ".into(),
            session: 7,
            log: Ok("C:\\somewhere\\smkvm.log".into()),
        });
        assert_eq!(
            said,
            Ok(Who {
                who: "SOMEWHERE\\someone".into(),
                session: 7,
                log: Ok("C:\\somewhere\\smkvm.log".into()),
            })
        );
    }

    #[test]
    fn a_reader_that_will_not_say_who_it_is_is_refused() {
        let said = welcome(FromReader::Ready {
            protocol: READER_PROTOCOL,
            who: "   ".into(),
            session: 1,
            log: Ok(String::new()),
        });
        assert_eq!(said, Err(NotWelcome::Nameless));
    }

    #[test]
    fn a_reader_that_speaks_before_saying_hello_is_refused() {
        let said = welcome(FromReader::Copied(vec![ClipFormat::Text]));
        assert_eq!(said, Err(NotWelcome::SpokeOutOfTurn));
    }

    #[test]
    fn what_was_copied_survives_the_wire() {
        let said = FromReader::Copied(vec![ClipFormat::Uris, ClipFormat::Text]);
        let bytes = crate::secure::wire::frame(&said).expect("framed");
        let back: FromReader =
            postcard::from_bytes(&bytes[4..]).expect("the same shape on the way back");
        assert_eq!(back, said);
    }

    #[test]
    fn a_clipboard_too_large_to_send_is_refused_rather_than_sent() {
        // The cap is generous because a screenshot is megabytes, but
        // it exists: a length prefix with no bound is how a denial of
        // service gets written by accident.
        const { assert!(LONGEST_READ > 1024 * 1024, "an image would not fit") };
        const { assert!(LONGEST_READ <= 128 * 1024 * 1024, "no bound worth the name") };
    }

    #[test]
    fn a_line_from_the_reader_cannot_forge_another() {
        // Newlines are the whole of it: `reader: {text}` prefixes the
        // first line only, so everything after a newline would appear
        // unprefixed and indistinguishable from the service's own.
        let forged = tidy("ordinary\nclipboard: KEY PRIVATE\r\nmore");
        assert!(!forged.contains('\n'), "{forged}");
        assert!(!forged.contains('\r'), "{forged}");
        assert_eq!(forged, "ordinary clipboard: KEY PRIVATE  more");
    }

    #[test]
    fn a_very_long_line_is_cut_and_says_so() {
        let said = tidy(&"x".repeat(LONGEST_SAID * 4));
        assert!(said.chars().count() <= LONGEST_SAID + " [cut short]".len());
        assert!(said.ends_with("[cut short]"), "{said}");
    }

    #[test]
    fn an_ordinary_line_is_left_alone() {
        let said = tidy("read Uris in 3 ms: 114 bytes");
        assert_eq!(said, "read Uris in 3 ms: 114 bytes");
    }

    #[test]
    fn counting_is_in_characters_not_bytes() {
        // Three bytes each in UTF-8. Cut by byte length this would be
        // called truncated when nothing was removed.
        let korean = "\u{d55c}".repeat(LONGEST_SAID);
        let said = tidy(&korean);
        assert!(!said.contains("[cut short]"), "{said}");
    }
}
