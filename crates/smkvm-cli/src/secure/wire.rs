//! What the service and the worker say to each other.
//!
//! A private protocol between two processes of the same build on one
//! machine, so it is deliberately not the protocol that goes over the
//! network: nothing here is ever parsed from anything but the pipe, and
//! the pipe admits LocalSystem alone. Keeping it separate also keeps
//! `PROTO_VERSION` about the wire between machines, where a mismatch means
//! "update the other machine" -- a mismatch here can only mean two halves
//! of one installation have got out of step, which is answered by the
//! handshake below rather than by a number every machine has to agree on.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use smkvm_layout::Monitor;
use smkvm_proto::{ClipFormat, Key, MouseButton, Scroll};

/// Bumped whenever anything in this file changes shape.
///
/// The worker says it first; a service that hears a different one kills the
/// worker rather than guessing, because the alternative -- misreading a
/// frame and injecting whatever the bytes happen to decode as -- is a
/// program typing at random into a consent prompt.
pub const WORKER_PROTOCOL: u32 = 2;

/// Longest frame either side will send or accept.
///
/// Every message here is a handful of integers except the monitor list,
/// which is a few hundred bytes. A length that says otherwise is a fault or
/// an attack, and answering it by allocating what it asks for is how a
/// length prefix becomes a denial of service.
pub const LONGEST_FRAME: usize = 64 * 1024;

/// What the service tells the worker to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToWorker {
    MoveTo {
        x: i32,
        y: i32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Wheel(Scroll),
    Key {
        key: Key,
        down: bool,
    },
    /// Push what has been buffered at the system.
    Flush,
    HideCursor,
    ShowCursor,
    /// Say what displays this machine has.
    TellMonitors,
    /// Stop letting the local keyboard and mouse through to this machine,
    /// or start again. Only meaningful on the machine that owns them.
    Swallow(bool),
    /// Let go and exit. Sent before the worker is replaced, so a worker
    /// leaving a desktop does not leave a key held down on it.
    Stop,

    // ---- the clipboard, which is the person's and not session 0's ----
    /// Take up, or put down, the person's clipboard.
    ///
    /// Only a worker on the ordinary desktop is asked. See
    /// `watch::serves_the_clipboard` for why.
    ServeClipboard(bool),
    /// Read what is on it, for a machine that is pasting elsewhere.
    ReadClipboard {
        id: u64,
        format: ClipFormat,
    },
    /// Put another machine's clipboard onto this one: announce these
    /// formats and fetch the contents only if something pastes.
    OfferClipboard {
        formats: Vec<ClipFormat>,
    },
    /// Give the clipboard back to whatever had it.
    ReleaseClipboard,
    /// The contents the worker asked for with [`FromWorker::WantsPaste`],
    /// having been fetched from the machine that copied them.
    Pasted {
        id: u64,
        bytes: Result<Vec<u8>, String>,
    },
    /// The cursor is leaving with the button held: find out whether
    /// something is being dragged, and what.
    CatchDrag {
        id: u64,
    },
    /// Files have arrived; let go of them where the pointer is.
    DropFiles(Vec<PathBuf>),
}

/// What the worker says back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromWorker {
    /// First frame, always. Names the desktop the worker actually ended up
    /// on, which is not necessarily the one it was asked for: the input can
    /// move again between the service deciding and the worker starting.
    Ready {
        protocol: u32,
        desktop: String,
    },
    /// Something the local keyboard or mouse did.
    Saw(Saw),
    Monitors(Vec<Monitor>),
    /// Which desktop has the input, as seen from inside the session.
    ///
    /// The service cannot find this out for itself. `OpenInputDesktop`
    /// is per window station, and a service lives in session 0 on
    /// `Service-0x0-3e7$`, which is not the station the screens are on;
    /// asked from there it reports session 0's own answer or nothing.
    /// The worker is on `WinSta0`, so it can simply look -- which is why
    /// the watching was inverted and this frame exists.
    ///
    /// `None` when the worker looked and could not tell.
    InputDesktop(Option<String>),
    /// An injection the system turned down, with whatever it said. The
    /// service treats this exactly as the daemon treats a local refusal.
    Refused(String),

    // ---- the clipboard ----
    /// Somebody copied something here.
    ClipboardChanged(Vec<ClipFormat>),
    /// The answer to [`ToWorker::ReadClipboard`].
    ClipboardRead {
        id: u64,
        bytes: Result<Vec<u8>, String>,
    },
    /// Something on this machine is pasting what another machine copied,
    /// so the contents are wanted now. The service fetches them over the
    /// link and answers with [`ToWorker::Pasted`].
    ///
    /// This is the one exchange that runs the other way round, and it is
    /// why the pipe needs request and answer in both directions.
    WantsPaste {
        id: u64,
        format: ClipFormat,
    },
    /// The answer to [`ToWorker::CatchDrag`]: what was being dragged, if
    /// anything.
    DragCaught {
        id: u64,
        paths: Vec<PathBuf>,
    },
}

/// The events the worker captures, mirrored here rather than shared.
///
/// `smkvm_core::Event` has this shape already, but it belongs to the state
/// machine crate and is not serialised anywhere; giving it derives to suit
/// a pipe would make the state machine's vocabulary answerable to a wire
/// format, which is the arrangement `smkvm-core` exists to avoid. Two small
/// types and one conversion is the cheaper of the two prices.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Saw {
    PointerAt { x: i32, y: i32 },
    PointerBy { dx: i32, dy: i32 },
    Button { button: MouseButton, down: bool },
    Wheel(Scroll),
    Key { key: Key, down: bool, repeat: bool },
}

impl From<Saw> for smkvm_core::Event {
    fn from(saw: Saw) -> Self {
        match saw {
            Saw::PointerAt { x, y } => smkvm_core::Event::PointerAt { x, y },
            Saw::PointerBy { dx, dy } => smkvm_core::Event::PointerBy { dx, dy },
            Saw::Button { button, down } => smkvm_core::Event::Button { button, down },
            Saw::Wheel(scroll) => smkvm_core::Event::Wheel(scroll),
            Saw::Key { key, down, repeat } => smkvm_core::Event::Key { key, down, repeat },
        }
    }
}

/// Why a worker's first frame was not acceptable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotWelcome {
    #[error(
        "the worker speaks protocol {theirs} and this speaks {ours}: two halves of one \
         installation are different builds. Reinstall from one binary"
    )]
    WrongProtocol { theirs: u32, ours: u32 },
    #[error("the worker said something else before saying hello")]
    SpokeOutOfTurn,
    #[error(
        "the worker called its desktop {0:?}, which is not a name a desktop has. It could \
         not read its own, and a worker that does not know where it is cannot be placed"
    )]
    NamelessDesktop(String),
}

/// Is this something Windows would call a desktop?
///
/// `Default`, `Winlogon` and `Screen-saver` are the ones that matter, and
/// a desktop name is a window-station object name: letters, digits and a
/// little punctuation, never empty. The reason to check rather than take
/// whatever arrives is not tidiness. A worker that cannot read its own
/// desktop used to report `"?"`, which was accepted -- and then `"?"` can
/// never equal the name the service's poll reads, so the service replaces
/// the worker on every look. A deliberate replacement counts no failure,
/// so nothing ever gave up: a process running as the system account
/// started and killed four times a second, indefinitely. Refusing the
/// name turns that into a start that failed, which the give-up counter
/// does cover.
fn is_a_desktop_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.'))
}

/// Read the worker's first frame, and say whether it may be spoken to.
///
/// Separate from the reading of it, and pure, for two reasons. It is the
/// decision that keeps a process of a different build from being handed
/// keystrokes, so it is worth testing on every machine rather than only on
/// the one that cannot run it. And it is the decision the service got
/// round the wrong way once: the first draft attached the pipe -- and so
/// sent the worker an instruction -- before reading this at all.
pub fn welcome(first: FromWorker) -> Result<String, NotWelcome> {
    match first {
        FromWorker::Ready { protocol, .. } if protocol != WORKER_PROTOCOL => {
            Err(NotWelcome::WrongProtocol {
                theirs: protocol,
                ours: WORKER_PROTOCOL,
            })
        }
        FromWorker::Ready { desktop, .. } if !is_a_desktop_name(desktop.trim()) => {
            Err(NotWelcome::NamelessDesktop(desktop))
        }
        FromWorker::Ready { desktop, .. } => Ok(desktop.trim().to_string()),
        _ => Err(NotWelcome::SpokeOutOfTurn),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("the pipe: {0}")]
    Pipe(#[from] std::io::Error),
    #[error("a frame of {0} bytes, which is more than anything here ever is")]
    TooLong(usize),
    #[error("a frame that did not decode; the two halves are not the same build")]
    Garbled,
}

/// One message, ready to go down the pipe: four bytes of length, then the
/// message.
///
/// A pipe in byte mode delivers whatever arrived, which is not necessarily
/// what was written in one call, so the length has to be on the wire. (The
/// pipe is byte mode rather than message mode on purpose: message mode
/// bounds a read by the *reader's* buffer as well, and a short buffer there
/// loses the rest of the message rather than returning it next time.)
pub fn frame<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_stdvec(message).map_err(|_| FrameError::Garbled)?;
    if body.len() > LONGEST_FRAME {
        return Err(FrameError::TooLong(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Read exactly one message, or say why not.
pub fn read_frame<T: serde::de::DeserializeOwned, R: std::io::Read>(
    from: &mut R,
) -> Result<T, FrameError> {
    let mut length = [0u8; 4];
    from.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    // Checked before anything is allocated. A length prefix trusted far
    // enough to reserve against is a length prefix that can exhaust memory.
    if length > LONGEST_FRAME {
        return Err(FrameError::TooLong(length));
    }
    let mut body = vec![0u8; length];
    from.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(|_| FrameError::Garbled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(m: T) {
        let bytes = frame(&m).expect("it fits");
        let mut cursor = std::io::Cursor::new(bytes);
        let back: T = read_frame(&mut cursor).expect("it reads back");
        assert_eq!(m, back);
        assert_eq!(
            cursor.position() as usize,
            cursor.get_ref().len(),
            "the frame said its own length exactly"
        );
    }

    #[test]
    fn every_instruction_survives_the_pipe() {
        round_trip(ToWorker::MoveTo { x: -1920, y: 43 });
        round_trip(ToWorker::Button {
            button: MouseButton::Left,
            down: true,
        });
        round_trip(ToWorker::Key {
            key: Key(0x04),
            down: false,
        });
        round_trip(ToWorker::Flush);
        round_trip(ToWorker::Swallow(true));
        round_trip(ToWorker::Stop);
        round_trip(ToWorker::ServeClipboard(true));
        round_trip(ToWorker::ReadClipboard {
            id: 1,
            format: ClipFormat::Text,
        });
        round_trip(ToWorker::OfferClipboard {
            formats: vec![ClipFormat::Text],
        });
        round_trip(ToWorker::ReleaseClipboard);
        round_trip(ToWorker::Pasted {
            id: 2,
            bytes: Ok(Vec::new()),
        });
        round_trip(ToWorker::CatchDrag { id: 3 });
        round_trip(ToWorker::DropFiles(vec![PathBuf::from("/tmp/x")]));
    }

    #[test]
    fn every_report_survives_the_pipe() {
        round_trip(FromWorker::Ready {
            protocol: WORKER_PROTOCOL,
            desktop: "Winlogon".into(),
        });
        round_trip(FromWorker::Saw(Saw::PointerBy { dx: 3, dy: -4 }));
        round_trip(FromWorker::Monitors(Vec::new()));
        round_trip(FromWorker::InputDesktop(Some("Winlogon".into())));
        round_trip(FromWorker::ClipboardChanged(vec![ClipFormat::Text]));
        round_trip(FromWorker::ClipboardRead {
            id: 7,
            bytes: Ok(vec![1, 2, 3]),
        });
        round_trip(FromWorker::ClipboardRead {
            id: 8,
            bytes: Err("the owning application would not hand it over".into()),
        });
        round_trip(FromWorker::WantsPaste {
            id: 9,
            format: ClipFormat::Text,
        });
        round_trip(FromWorker::DragCaught {
            id: 10,
            paths: vec![PathBuf::from(r"C:\a\b.txt")],
        });
        round_trip(FromWorker::InputDesktop(None));
        round_trip(FromWorker::Refused("SendInput returned 0".into()));
    }

    #[test]
    fn two_frames_in_one_read_do_not_run_together() {
        // The whole reason the length is on the wire: a byte-mode pipe is
        // free to hand both of these over in one go.
        let mut bytes = frame(&ToWorker::Flush).unwrap();
        bytes.extend(frame(&ToWorker::Stop).unwrap());
        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(
            read_frame::<ToWorker, _>(&mut cursor).unwrap(),
            ToWorker::Flush
        );
        assert_eq!(
            read_frame::<ToWorker, _>(&mut cursor).unwrap(),
            ToWorker::Stop
        );
    }

    #[test]
    fn a_length_larger_than_anything_real_is_refused_before_it_is_believed() {
        let mut bytes = (LONGEST_FRAME as u32 + 1).to_le_bytes().to_vec();
        // Deliberately no body: if the length were acted on, this would try
        // to read 64 KiB that is not there rather than refusing the length.
        bytes.extend_from_slice(b"..");
        let mut cursor = std::io::Cursor::new(bytes);
        assert!(matches!(
            read_frame::<ToWorker, _>(&mut cursor),
            Err(FrameError::TooLong(_))
        ));
    }

    #[test]
    fn nonsense_is_an_error_rather_than_whatever_it_decodes_as() {
        let mut bytes = 3u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0xff, 0xff, 0xff]);
        let mut cursor = std::io::Cursor::new(bytes);
        assert!(matches!(
            read_frame::<ToWorker, _>(&mut cursor),
            Err(FrameError::Garbled)
        ));
    }

    #[test]
    fn a_worker_of_the_right_build_is_welcome_and_says_where_it_is() {
        assert_eq!(
            welcome(FromWorker::Ready {
                protocol: WORKER_PROTOCOL,
                desktop: "Winlogon".into()
            }),
            Ok("Winlogon".to_string())
        );
    }

    #[test]
    fn a_worker_of_another_build_is_not_spoken_to() {
        assert_eq!(
            welcome(FromWorker::Ready {
                protocol: WORKER_PROTOCOL + 1,
                desktop: "Winlogon".into()
            }),
            Err(NotWelcome::WrongProtocol {
                theirs: WORKER_PROTOCOL + 1,
                ours: WORKER_PROTOCOL
            })
        );
    }

    #[test]
    fn a_worker_that_says_anything_else_first_is_not_spoken_to() {
        // Including something harmless-looking. The rule is that the
        // first frame is the hello, not that the first frame is checked
        // if it happens to be one.
        assert_eq!(
            welcome(FromWorker::Monitors(Vec::new())),
            Err(NotWelcome::SpokeOutOfTurn)
        );
        assert_eq!(
            welcome(FromWorker::Saw(Saw::PointerAt { x: 0, y: 0 })),
            Err(NotWelcome::SpokeOutOfTurn)
        );
        assert_eq!(
            welcome(FromWorker::Ready {
                protocol: WORKER_PROTOCOL,
                desktop: "  ".into()
            }),
            Err(NotWelcome::NamelessDesktop("  ".into()))
        );
    }

    #[test]
    fn a_worker_that_could_not_read_its_own_desktop_is_not_welcome() {
        // `"?"` was what the worker reported when `GetUserObjectInformationW`
        // would not answer, and it was accepted. A name the service's poll
        // can never match makes the service replace the worker on every
        // look, and a deliberate replacement counts no failure -- so a
        // process running as the system account is started and killed four
        // times a second for ever. Refused here, it is a start that failed,
        // which the give-up counter covers.
        for bad in ["?", "", "   ", "Win\\logon", "a\u{0}b", &"x".repeat(65)] {
            assert!(
                welcome(FromWorker::Ready {
                    protocol: WORKER_PROTOCOL,
                    desktop: bad.into()
                })
                .is_err(),
                "{bad:?} was welcomed"
            );
        }
        for good in ["Default", "Winlogon", "Screen-saver"] {
            assert_eq!(
                welcome(FromWorker::Ready {
                    protocol: WORKER_PROTOCOL,
                    desktop: good.into()
                }),
                Ok(good.to_string())
            );
        }
    }

    #[test]
    fn what_the_worker_saw_becomes_what_the_state_machine_knows() {
        assert_eq!(
            smkvm_core::Event::from(Saw::PointerAt { x: 7, y: 9 }),
            smkvm_core::Event::PointerAt { x: 7, y: 9 }
        );
        assert_eq!(
            smkvm_core::Event::from(Saw::Wheel(Scroll::new(0, Scroll::NOTCH))),
            smkvm_core::Event::Wheel(Scroll::new(0, Scroll::NOTCH))
        );
    }
}
