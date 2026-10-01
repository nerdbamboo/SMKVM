//! The half that runs as the person, and only reads.
//!
//! Measured three ways before it was written. A process running as
//! the system account sees one format on the clipboard after a file
//! is copied; a thread of that process impersonating the logged-on
//! user sees the same one; a child started with `WTSQueryUserToken`
//! and `CreateProcessAsUserW`, as that user, sees all five and reads
//! real bytes out of every one of them. So impersonation is not the
//! answer and identity is, and the identity has to belong to a
//! process rather than a thread.
//!
//! What it does **not** do is as deliberate as what it does. It does
//! not own the clipboard, offer anything, serve a render, or inject.
//! Those all work today from the system account, because they are
//! about a clipboard we put something on ourselves; only reading what
//! another process put there needs to be the person. Moving a working
//! path into a new process to fix a broken one is a mistake this
//! codebase has already made in a smaller form, so nothing that works
//! has moved.
//!
//! That restraint is also the security argument. This process is
//! reached down a pipe that admits the person at the desk, which
//! means it admits everything they are running; because
//! [`crate::secure::reading`] cannot express an injection, nobody has
//! to reason about whether one can be smuggled through.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use smkvm_clipboard::platform::windows::WindowsClipboard;
use smkvm_clipboard::{Read as _, Watch as _};

use crate::secure::reading::{FromReader, ToReader, LONGEST_READ, READER_PROTOCOL};
use crate::secure::windows::pipe;
use crate::secure::windows::token;
use crate::secure::wire::{frame_up_to, read_frame_up_to, Level};

/// One way of speaking to the service, shared by the thread
/// answering questions and the thread noticing copies.
type Saying = Arc<dyn Fn(&FromReader) -> bool + Send + Sync>;

/// How often to look at the sequence number, and how long to let a
/// copy settle once it has moved.
///
/// A quarter-second is well inside what a person notices and costs a
/// single call that touches nothing. The settle is the same reason
/// the window's own timer has one: a file copy arrives as several
/// changes while the shell assembles its data object.
const LOOK_EVERY: Duration = Duration::from_millis(250);
const SETTLE_FOR: Duration = Duration::from_millis(400);

/// A file the reader appends to, line by line, before and during
/// everything else.
///
/// Cruder than the log on purpose. Three rounds went on inference
/// about a process that reached Rust, decided something was wrong,
/// exited 1, and could not tell anybody -- the one failure this file
/// has spent days removing, surviving in the newest process. Its log
/// depends on profile paths and a tracing subscriber; its voice down
/// the pipe depends on the pipe. This depends on neither: an
/// absolute path handed to it on its command line by a parent that
/// has already checked the person can write there, opened and closed
/// per line so a process that dies in the next instruction still
/// leaves what it had said.
///
/// It is temporary. When the reader is reliable it goes.
struct Trace(Option<std::path::PathBuf>);

impl Trace {
    fn say(&self, line: &str) {
        let Some(path) = &self.0 else {
            return;
        };
        use std::io::Write as _;
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{since} {line}");
            let _ = file.flush();
        }
    }
}

/// Run as the reader until the pipe closes or the service says stop.
pub fn run(pipe_name: &str, no_log: Option<String>, trace_to: Option<PathBuf>) -> Result<()> {
    let trace = Trace(trace_to);
    // The first statement, before the log, before the connect, before
    // anything that has ever failed here.
    trace.say(&format!(
        "alive: running as {}, told to use pipe {pipe_name}",
        token::whoami()
    ));
    match &no_log {
        None => trace.say(&format!(
            "log: opened at {}",
            smkvm_config::paths::log_file().display()
        )),
        Some(why) => trace.say(&format!("log: NOT opened: {why}")),
    }
    // `connect` checks that the process serving this pipe is the
    // system account before a byte is sent. That guard matters more
    // here than it does for the worker: this process runs as the
    // person, so without it anything else running as them could put
    // up a pipe of that name and be told what they copy.
    trace.say("connecting");
    let to_service = match pipe::connect_as_the_person(pipe_name) {
        Ok(to_service) => to_service,
        Err(e) => {
            trace.say(&format!("connecting: FAILED: {e:#}"));
            return Err(e).context("reaching the service, which must own the pipe");
        }
    };
    trace.say("connected, and the pipe is owned by the system account");
    let mut from_service = to_service.share()?;
    trace.say("sharing the pipe worked");

    // One writer, shared. There are two threads that speak: this one
    // answering questions, and the watch noticing copies.
    let speaking = Arc::new(Mutex::new(to_service.share()?));
    let speak: Saying = {
        let speaking = speaking.clone();
        Arc::new(move |message: &FromReader| {
            let mut held = speaking.lock().expect("not poisoned");
            say(&mut *held, message).is_ok()
        })
    };

    // The first thing, before the clipboard and before anything that
    // can fail. Who this is, where it is, and where it is writing --
    // or why it is not.
    //
    // This process has no console and nothing can ask it anything.
    // When it could not open its log it exited here, before a line
    // reached disk anywhere, and the only evidence was a pipe that
    // broke. Saying where it is has to come before anything that
    // could stop it saying so.
    let who = token::whoami();
    let session = token::console_session().unwrap_or_default();
    let log = match &no_log {
        None => Ok(smkvm_config::paths::log_file().display().to_string()),
        Some(why) => Err(why.clone()),
    };
    speak(&FromReader::Ready {
        protocol: READER_PROTOCOL,
        who: who.clone(),
        session,
        log: log.clone(),
    });
    trace.say("said hello");
    match &log {
        Ok(path) => tell(
            &speak,
            Level::Info,
            format!("reading as {who} in session {session}, logging to {path}"),
        ),
        // Not fatal, and said down the pipe because it cannot be
        // said anywhere else. A reader with no log of its own is
        // worth having; a reader that exits because of it is not.
        Err(why) => tell(
            &speak,
            Level::Warn,
            format!(
                "reading as {who} in session {session}, with no log of its own: {why}. \
                 Everything it would have written is going down this pipe instead"
            ),
        ),
    }

    // Point the clipboard crate's own lines at the pipe, before the
    // clipboard is started and so before it has anything to say.
    //
    // This was missing, and it is most of why "the reader said
    // nothing" was true. `witness_through` is a process-wide slot
    // that only the worker ever filled, so every line the clipboard
    // crate produced in *this* process -- the settle timer finding a
    // copy, the clipboard refusing to open, the formats it saw --
    // went to this process's own log and nowhere else. The half
    // whose whole purpose is noticing copies was the half whose
    // noticing could not be read from the side that cares.
    {
        let speak = speak.clone();
        let trace_said = Trace(trace.0.clone());
        smkvm_clipboard::witness_through(Box::new(move |text, for_a_person| {
            // Into the trace as well. While the reader is still a
            // thing being established rather than a thing being
            // used, the file that survives everything is worth more
            // than tidiness.
            trace_said.say(text);
            speak(&FromReader::Said {
                level: if for_a_person {
                    Level::Info
                } else {
                    Level::Debug
                },
                text: text.to_string(),
            });
        }));
    }

    trace.say("starting to watch the clipboard");
    let mut clipboard = match WindowsClipboard::start() {
        Ok(clipboard) => clipboard,
        Err(e) => {
            trace.say(&format!("watching the clipboard: FAILED: {e}"));
            return Err(e).context("watching the person's clipboard");
        }
    };
    trace.say("watching the clipboard");
    let mut handle = clipboard.handle();

    // What is already there, before any change is reported.
    //
    // Without this, a copy made before this process started -- during
    // a logon, or while a dead reader was being replaced -- stays
    // invisible until the person copies something again, which reads
    // as the feature not working rather than as a gap.
    match clipboard.available() {
        Ok(already) => {
            tell(
                &speak,
                Level::Info,
                format!("what is already on the clipboard: {:?}", already.formats),
            );
            if !already.formats.is_empty() {
                speak(&FromReader::Copied(already.formats));
            }
        }
        Err(e) => tell(
            &speak,
            Level::Warn,
            format!("could not see what is already on the clipboard: {e}"),
        ),
    }

    // Two ways of noticing a copy, running side by side, each
    // saying which one it was.
    //
    // The window registers as a format listener, the registration is
    // accepted, the window pumps -- and `WM_CLIPBOARDUPDATE` never
    // arrives here, while the worker's window in the same session on
    // the same clipboard is told about every copy. The two processes
    // differ in the identity they run as and in nothing else anybody
    // has found, which is the same axis as the finding this whole
    // arrangement exists for, now in the notification rather than
    // the enumeration.
    //
    // So the sequence number is watched as well. It asks nobody's
    // permission, needs no window and no message queue, and moves on
    // every change to the clipboard. Running both is the
    // measurement -- whichever notices says so, and if only one ever
    // does, that is the answer -- and it is also the way out, since
    // a copy noticed by polling is as good a copy as one announced.
    //
    // Whoever gets there first announces it; the other sees the
    // sequence already claimed and stays quiet, so one copy is one
    // notice however many ways it was spotted.
    let announced = Arc::new(AtomicU32::new(
        smkvm_clipboard::platform::windows::sequence_number(),
    ));

    let polling = speak.clone();
    let claimed = announced.clone();
    let asking = handle.clone();
    std::thread::Builder::new()
        .name("smkvm-reader-sequence".into())
        .spawn(move || loop {
            std::thread::sleep(LOOK_EVERY);
            let now = smkvm_clipboard::platform::windows::sequence_number();
            if now == claimed.load(Ordering::Relaxed) {
                continue;
            }
            // Settle first: a copy arrives as several changes, and
            // the shell's file copy assembles itself over a second
            // or so.
            std::thread::sleep(SETTLE_FOR);
            let settled = smkvm_clipboard::platform::windows::sequence_number();
            if claimed.swap(settled, Ordering::Relaxed) == settled {
                continue;
            }
            match asking.available() {
                Ok(there) if there.formats.is_empty() => tell(
                    &polling,
                    Level::Info,
                    format!(
                        "the clipboard changed (sequence {settled}) but holds none of the \
                         formats we share"
                    ),
                ),
                Ok(there) => {
                    tell(
                        &polling,
                        Level::Info,
                        format!(
                            "something was copied here: {:?} -- noticed by watching the \
                             sequence number ({settled}), not by being told. This process \
                             has been told of {} changes since it started",
                            there.formats,
                            smkvm_clipboard::platform::windows::NOTICED.load(Ordering::Relaxed)
                        ),
                    );
                    if !polling(&FromReader::Copied(there.formats)) {
                        return;
                    }
                }
                Err(e) => tell(
                    &polling,
                    Level::Warn,
                    format!("the clipboard changed (sequence {settled}) but would not open: {e}"),
                ),
            }
        })
        .context("starting the reader's watch on the sequence number")?;

    // Copies are noticed on their own thread, because `next_change`
    // blocks until one happens and this one has questions to answer
    // meanwhile.
    let noticing = speak.clone();
    let claimed = announced.clone();
    std::thread::Builder::new()
        .name("smkvm-reader-watch".into())
        .spawn(move || {
            while let Some(copied) = clipboard.next_change() {
                let settled = smkvm_clipboard::platform::windows::sequence_number();
                if claimed.swap(settled, Ordering::Relaxed) == settled {
                    // The sequence watch got there first. One copy,
                    // one notice.
                    continue;
                }
                tell(
                    &noticing,
                    Level::Info,
                    format!(
                        "something was copied here: {:?} -- noticed by being told \
                         (sequence {settled})",
                        copied.formats
                    ),
                );
                if !noticing(&FromReader::Copied(copied.formats)) {
                    return;
                }
            }
            tell(
                &noticing,
                Level::Warn,
                "no longer watching this clipboard; copies made here will not reach \
                 another machine",
            );
        })
        .context("starting the reader's watch")?;

    trace.say("answering");
    loop {
        let asked: ToReader = match read_frame_up_to(&mut from_service, LONGEST_READ) {
            Ok(asked) => asked,
            Err(e) => {
                tracing::info!("the service stopped talking to this reader: {e}");
                return Ok(());
            }
        };
        match asked {
            ToReader::WhatIsOnIt { id } => {
                let formats = handle.available().map(|a| a.formats).unwrap_or_default();
                speak(&FromReader::OnIt { id, formats });
            }
            ToReader::Read { id, format } => {
                let began = std::time::Instant::now();
                let bytes = handle.read(&format).map_err(|e| e.to_string());
                tell(
                    &speak,
                    Level::Debug,
                    format!(
                        "read {format:?} in {} ms: {}",
                        began.elapsed().as_millis(),
                        match &bytes {
                            Ok(b) => format!("{} bytes", b.len()),
                            Err(e) => e.clone(),
                        }
                    ),
                );
                speak(&FromReader::Read { id, bytes });
            }
            ToReader::Stop => {
                tell(&speak, Level::Info, "the service says to stop");
                return Ok(());
            }
        }
    }
}

/// Say something into this process's log and the service's.
///
/// Both, for the same reason the worker does it: which of the two a
/// line arrives in is itself part of the diagnosis, and this
/// process's own log has been the less reliable of them.
fn tell(speak: &Saying, level: Level, text: impl Into<String>) {
    let text = text.into();
    match level {
        Level::Debug => tracing::debug!("{text}"),
        Level::Info => tracing::info!("{text}"),
        Level::Warn => tracing::warn!("{text}"),
        Level::Error => tracing::error!("{text}"),
    }
    speak(&FromReader::Said { level, text });
}

fn say<W: std::io::Write>(to: &mut W, message: &FromReader) -> Result<()> {
    let bytes = frame_up_to(message, LONGEST_READ)?;
    to.write_all(&bytes).context("writing to the service")?;
    Ok(())
}
