//! The service's end of the reader: starting one, and asking it
//! things.
//!
//! The mirror of `link` for the other helper, and deliberately much
//! smaller, because the reader can be asked only two things.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use smkvm_clipboard::{Available, ClipboardError, Read, Watch};
use smkvm_proto::ClipFormat;

use crate::secure::acl;
use crate::secure::reading::{self, FromReader, ToReader, LONGEST_READ};
use crate::secure::windows::clip::Waiting;
use crate::secure::windows::{pipe, secret, token};
use crate::secure::wire::{frame_up_to, read_frame_up_to, Level};

/// How long to wait for a reader to answer.
///
/// A read crosses a pipe and touches the clipboard, which another
/// application may be holding for a moment. It does not cross a
/// network, so this is generous rather than tight -- but it is
/// bounded, because the thing waiting on it may be a render with a
/// budget of its own.
pub const READER_ANSWERS_WITHIN: Duration = Duration::from_secs(3);

/// How long to wait for a started reader to connect.
const READER_CONNECTS_WITHIN: Duration = Duration::from_secs(10);

/// The service's end of one reader.
pub struct Reader {
    write: Mutex<pipe::Pipe>,
    answers: Waiting<Result<Vec<u8>, String>>,
    listed: Waiting<Vec<ClipFormat>>,
    /// Where copies the reader notices are sent.
    copies: Mutex<Option<std::sync::mpsc::Sender<Available>>>,
    /// Kept so the process is not reaped while its id is being
    /// checked, and so it can be ended.
    _running: token::Started,
    pub who: String,
}

impl Reader {
    /// Ask, without waiting for an answer.
    fn say(&self, message: &ToReader) -> bool {
        let Ok(bytes) = frame_up_to(message, LONGEST_READ) else {
            return false;
        };
        let mut held = self.write.lock().expect("not poisoned");
        held.write_within(&bytes, pipe::WRITE_WITHIN).is_ok()
    }

    /// Nothing more will be answered; wake anything waiting.
    pub fn nobody_is_answering(&self) {
        self.answers.nobody_is_answering();
        self.listed.nobody_is_answering();
    }

    /// Where to send copies this reader notices.
    pub fn watch(&self) -> std::sync::mpsc::Receiver<Available> {
        let (tx, rx) = std::sync::mpsc::channel();
        *self.copies.lock().expect("not poisoned") = Some(tx);
        rx
    }

    fn heard(&self, said: FromReader) {
        match said {
            FromReader::Copied(formats) => {
                let held = self.copies.lock().expect("not poisoned");
                match held.as_ref() {
                    Some(copies) => {
                        tracing::info!(
                            ?formats,
                            "clipboard: the reader says something was copied on the desktop"
                        );
                        if copies.send(Available { formats }).is_err() {
                            tracing::warn!(
                                "clipboard: nothing is listening for copies, so what this \
                                 machine copies will not reach another"
                            );
                        }
                    }
                    // Said rather than discarded, for the same reason
                    // the worker's version of this is: the far end of
                    // the only path outward losing everything in
                    // silence is how a broken half came to look like
                    // an absent one.
                    None => tracing::warn!(
                        ?formats,
                        "clipboard: the reader says something was copied, but nothing on \
                         this side is listening"
                    ),
                }
            }
            FromReader::OnIt { id, formats } => {
                self.listed.answer(id, formats);
            }
            FromReader::Read { id, bytes } => {
                self.answers.answer(id, bytes);
            }
            FromReader::Said { level, text } => match level {
                Level::Debug => tracing::debug!("reader: {text}"),
                Level::Info => tracing::info!("reader: {text}"),
                Level::Warn => tracing::warn!("reader: {text}"),
                Level::Error => tracing::error!("reader: {text}"),
            },
            FromReader::Ready { .. } => {}
        }
    }
}

/// The reader this service is using, if it has one.
static READER: Mutex<Option<Arc<Reader>>> = Mutex::new(None);

/// The reader now, or nothing if there is nobody logged in.
pub fn reader() -> Option<Arc<Reader>> {
    READER.lock().expect("not poisoned").clone()
}

/// Stop whatever reader there is, and say why.
///
/// Said rather than done quietly, because from outside a clipboard
/// that has stopped reporting copies looks identical whether it was
/// let go on purpose or died.
pub fn let_go(why: &str) {
    let held = READER.lock().expect("not poisoned").take();
    if let Some(reader) = held {
        tracing::info!(
            who = %reader.who,
            "clipboard: letting the reader go, because {why}"
        );
        reader.say(&ToReader::Stop);
        reader.nobody_is_answering();
    }
}

/// Start a reader in the interactive session, as the person in it.
pub fn start(exe: &Path, session: u32) -> Result<Arc<Reader>> {
    token::enable_tcb_privilege()?;

    // The pipe is made before the reader is started, so there is no
    // moment when its name exists and nothing is listening on it.
    // Its access list admits the person at the desk, which it has to
    // -- and therefore admits everything else they are running, which
    // is why the protocol it carries cannot express anything but a
    // read.
    let name = acl::pipe_name(&secret::name_bytes()?);
    let listening =
        pipe::create_with(&name, acl::READER_PIPE_SDDL).context("making a pipe for the reader")?;

    let mut handle = windows::Win32::Foundation::HANDLE::default();
    // SAFETY: a place for the token, owned before any return.
    unsafe { windows::Win32::System::RemoteDesktop::WTSQueryUserToken(session, &mut handle) }
        .with_context(|| format!("asking who is logged in at session {session}"))?;
    let theirs = token::Owned(handle);

    // `Default` and nowhere else. The reader has no business on the
    // desktop a consent prompt is on: the person cannot copy anything
    // there, and the worker is already there for the half that
    // matters.
    let running = token::start_on_desktop(
        &theirs,
        exe,
        &format!("clipboard-reader --pipe {name}"),
        r"WinSta0\Default",
    )
    .context("starting the reader as the person at the desk")?;

    pipe::accept(&listening, READER_CONNECTS_WITHIN)
        .context("waiting for the reader to connect")?;
    listening.client_is(running.pid)?;

    let mut reading = listening.share()?;
    let first: FromReader = read_frame_up_to(&mut reading, LONGEST_READ)
        .map_err(|e| anyhow::anyhow!("the reader said nothing we could read: {e}"))?;
    let who = reading::welcome(first).map_err(|e| anyhow::anyhow!("{e}"))?;
    if who.session != session {
        bail!(
            "the reader says it is in session {} and it was started in {session}",
            who.session
        );
    }

    let reader = Arc::new(Reader {
        write: Mutex::new(listening),
        answers: Waiting::new(),
        listed: Waiting::new(),
        copies: Mutex::new(None),
        _running: running,
        who: who.who.clone(),
    });
    tracing::info!(
        who = %who.who,
        session,
        "clipboard: a reader is running as the person at the desk"
    );

    let listener = reader.clone();
    std::thread::Builder::new()
        .name("smkvm-reader-link".into())
        .spawn(move || {
            loop {
                match read_frame_up_to::<FromReader, _>(&mut reading, LONGEST_READ) {
                    Ok(said) => listener.heard(said),
                    Err(e) => {
                        tracing::warn!("the reader stopped talking: {e}");
                        break;
                    }
                }
            }
            // Everything waiting is woken rather than left to its own
            // deadline, and the slot is emptied so the next read says
            // "nobody is logged in" at once instead of waiting out
            // three seconds against a process that has gone.
            listener.nobody_is_answering();
            *READER.lock().expect("not poisoned") = None;
        })
        .context("listening to the reader")?;

    *READER.lock().expect("not poisoned") = Some(reader.clone());
    Ok(reader)
}

/// Watching the person's clipboard, through the reader.
pub struct WatchThroughReader {
    pub copies: std::sync::mpsc::Receiver<Available>,
}

impl Watch for WatchThroughReader {
    fn next_change(&mut self) -> Option<Available> {
        self.copies.recv().ok()
    }
}

/// Reading the person's clipboard, through the reader.
pub struct ReadThroughReader;

impl Read for ReadThroughReader {
    fn read(&mut self, format: &ClipFormat) -> smkvm_clipboard::Result<Vec<u8>> {
        // Absence is answered at once and by name.
        //
        // Between logoff and logon there is nobody to be, so there is
        // no reader; the same is true for the moment after one has
        // died and before another is started. Waiting out a deadline
        // to say so would turn an ordinary state of the machine into
        // something that looks like a hang.
        let Some(reader) = reader() else {
            return Err(ClipboardError::Display(
                "nobody is logged in here, so this machine has no clipboard to read".into(),
            ));
        };
        let (id, answer) = reader.answers.ask();
        if !reader.say(&ToReader::Read {
            id,
            format: format.clone(),
        }) {
            reader.answers.forget(id);
            return Err(ClipboardError::Display(
                "the reader could not be asked; nothing was read".into(),
            ));
        }
        match answer.recv_timeout(READER_ANSWERS_WITHIN) {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) => Err(ClipboardError::Display(e)),
            Err(_) => {
                reader.answers.forget(id);
                Err(ClipboardError::Display(format!(
                    "the reader did not answer within {} s",
                    READER_ANSWERS_WITHIN.as_secs()
                )))
            }
        }
    }
}
