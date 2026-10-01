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
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use smkvm_clipboard::platform::windows::WindowsClipboard;
use smkvm_clipboard::{Read as _, Watch as _};

use crate::secure::outbox::{Outbox, Posted};
use crate::secure::reading::{FromReader, ToReader, LONGEST_READ, READER_PROTOCOL};
use crate::secure::windows::pipe;
use crate::secure::windows::token;
use crate::secure::wire::{frame_up_to, read_frame_up_to, Level};

/// What a message is, for a log line, without its contents.
///
/// Contents can be a whole clipboard; this is for counting and
/// ordering, not for reading what was copied.
fn what_it_is(message: &FromReader) -> &'static str {
    match message {
        FromReader::Ready { .. } => "hello",
        FromReader::Copied(_) => "a copy",
        FromReader::OnIt { .. } => "what is on it",
        FromReader::Read { .. } => "a read",
        FromReader::Said { .. } => "a line",
    }
}

/// One way of speaking to the service, shared by the thread
/// answering questions and the thread noticing copies.
type Saying = Arc<dyn Fn(&FromReader) -> bool + Send + Sync>;

/// How many messages may be waiting to go to the service before the
/// rest are refused. Generous, because a burst is the moment they
/// matter; bounded, because a queue nobody drains must not grow.
const OUTBOX_ROOM: usize = 256;

/// How often the sequence watch says it is still there.
///
/// Rare enough to be no burden on a file somebody reads by eye,
/// often enough that thirty seconds of silence is a fact rather than
/// an absence.
const HEARTBEAT_EVERY: u64 = 5;

/// How long a single write may be outstanding before it is worth
/// saying so.
///
/// A write to a pipe whose far end is collecting returns in
/// microseconds. Two seconds is not a slow service; it is one that
/// has stopped reading.
const WRITE_IS_STUCK: u64 = 2;

/// Seconds since the epoch, for comparing two moments and nothing
/// else.
fn now_in_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

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
#[derive(Clone)]
struct Trace(Option<std::path::PathBuf>);

/// One appender at a time.
///
/// Four threads write here and two lines ran together mid-word in
/// the first run that mattered. An append is not atomic across
/// processes either, but there is only one process writing this
/// file, and within it a lock is the whole fix. This is the file
/// that gets read first when nothing else speaks, so it is the last
/// place to tolerate a line that cannot be trusted.
static APPENDING: Mutex<()> = Mutex::new(());

impl Trace {
    /// Say something that must be said even if everything else in
    /// this process is stuck.
    ///
    /// A report about a stuck writer cannot travel through the stuck
    /// writer, and it cannot share a lock with it either. That is the
    /// same trap three times now: the first version of this warning
    /// went through the outbox, which was the thing it was reporting
    /// on; the second went through the trace's own lock, which the
    /// drain also takes.
    ///
    /// So this one takes the lock only if it is free, and writes
    /// anyway if it is not. Two lines running together is a cost
    /// worth paying for a line that cannot be prevented from being
    /// written; the alternative is what we have had, which is no
    /// line at all.
    fn say_whatever_happens(&self, line: &str) {
        let held = APPENDING.try_lock();
        let mark = if held.is_ok() {
            ""
        } else {
            " [written without the lock]"
        };
        self.append(&format!("{line}{mark}"));
    }

    fn say(&self, line: &str) {
        if self.0.is_none() {
            return;
        }
        let _one_at_a_time = APPENDING.lock().expect("not poisoned");
        self.append(line);
    }

    fn append(&self, line: &str) {
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

/// Run as the reader, and say how it stopped whatever happens.
///
/// Every way out of this process was unlogged in the file we read
/// first. The ordinary end writes a line through `tracing`, which
/// goes to a log in the person's profile; an error returns to `main`
/// and is printed to a console that does not exist; a panic unwinds
/// past both. So the process could vanish a second after saying it
/// had something to report and leave no account of why, which is
/// what it did.
pub fn run(pipe_name: &str, no_log: Option<String>, trace_to: Option<PathBuf>) -> Result<()> {
    let trace = Trace(trace_to);
    // Before anything, so that even a panic in setting up has
    // somewhere to land. The default hook writes to a stderr nobody
    // is reading.
    {
        let trace = trace.clone();
        let was = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panicked| {
            trace.say(&format!("PANIC: {panicked}"));
            was(panicked);
        }));
    }
    let outcome = run_until_it_stops(pipe_name, no_log, &trace);
    match &outcome {
        Ok(()) => trace.say("stopping: ordinarily"),
        Err(e) => trace.say(&format!("stopping: {e:#}")),
    }
    outcome
}

fn run_until_it_stops(pipe_name: &str, no_log: Option<String>, trace: &Trace) -> Result<()> {
    let trace = trace.clone();
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

    // Nothing writes to the pipe from a thread that must not stop.
    //
    // This was a shared `Mutex<Pipe>` and an unbounded `write_all`,
    // which is the exact pattern that wedged the worker three times
    // and which `secure::outbox` was written to end. It was
    // harmless here until the clipboard's own lines were relayed
    // down the pipe -- and then the window thread began calling it
    // from inside `WM_CLIPBOARDUPDATE`, which is the one thread in
    // this process that may never wait.
    //
    // The symptom was exact: the first notification arrived, the
    // handler said so, the write did not come back, and after that
    // there was no settle, no second notification -- the message was
    // posted and never collected -- and no word from the sequence
    // watch either, because its first `tell` queued behind the same
    // lock. Two independent paths stopping together, sharing one
    // thing.
    //
    // So everything is posted, and one thread does the waiting.
    let (outbox, collect) = Outbox::with_room_for(OUTBOX_ROOM);
    let outbox = Arc::new(outbox);
    // What the drain is in the middle of, for somebody else to look
    // at.
    //
    // The drain is the one thread here allowed to wait, and it will
    // therefore wait for ever by design. From outside, a thread
    // waiting for ever and a thread that died are the same thing --
    // the twin of the rule about diagnostics: **a path that waits by
    // design has to say what it is waiting on**, or its patience is
    // indistinguishable from its death. Ninety seconds of a drain
    // neither sending nor complaining is what that costs.
    //
    // Zero means idle. Anything else is the second the current write
    // began.
    let writing_since = Arc::new(AtomicU64::new(0));
    let posted = Arc::new(AtomicUsize::new(0));
    let delivered = Arc::new(AtomicUsize::new(0));
    {
        let mut writing = to_service.share()?;
        let draining = trace.clone();
        let began = writing_since.clone();
        let done = delivered.clone();
        std::thread::Builder::new()
            .name("smkvm-reader-outbox".into())
            .spawn(move || {
                let mut failing = false;
                let mut sent = 0usize;
                while let Ok(message) = collect.recv() {
                    began.store(now_in_seconds().max(1), Ordering::Relaxed);
                    let outcome = say(&mut writing, &message);
                    began.store(0, Ordering::Relaxed);
                    match outcome {
                        Ok(()) => {
                            sent += 1;
                            done.store(sent, Ordering::Relaxed);
                            // The first few and then every so often.
                            // A drain that is working has to leave a
                            // mark, or its silence cannot be told
                            // from its absence.
                            if failing || sent <= 5 || sent % 50 == 0 {
                                draining.say(&format!(
                                    "outbox: sent {} ({sent} in all)",
                                    what_it_is(&message)
                                ));
                            }
                            failing = false;
                        }
                        Err(e) => {
                            if !failing {
                                // Through `tracing` and the trace
                                // file, never through the outbox:
                                // that would be the outbox trying
                                // to report its own failure to
                                // report.
                                draining.say(&format!(
                                    "outbox: CANNOT reach the service ({e}); {sent} sent \
                                     before this"
                                ));
                                tracing::warn!("the reader cannot reach the service: {e}");
                                failing = true;
                            }
                            // Deliberately still draining. Ending
                            // here drops the receiver and refuses
                            // every later message for the life of
                            // the process on the strength of one
                            // failed write.
                        }
                    }
                }
                // Only when every sender has gone, which in this
                // process means it is ending.
                draining.say(&format!("outbox: nothing more to send; {sent} sent in all"));
            })
            .context("starting the reader's outbox")?;
    }
    let speak: Saying = {
        let outbox = outbox.clone();
        let posting = trace.clone();
        let counted = posted.clone();
        Arc::new(move |message: &FromReader| {
            let became = outbox.post(message.clone());
            if became.arrived() {
                counted.fetch_add(1, Ordering::Relaxed);
            }
            // Every post, at the moment of posting, with what became
            // of it.
            //
            // This is the line that was missing. "Posted and not
            // delivered" and "never posted" have shared a symptom
            // for two rounds, and the round meant to separate them
            // added tracing only to the drain's *failure* paths --
            // so a healthy drain and an absent one both produced a
            // file with no outbox line in it. The same mistake in a
            // new place: an absence asked to carry evidence.
            posting.say(&format!(
                "posted {} -> {}",
                what_it_is(message),
                match became {
                    Posted::Sent => "queued".to_string(),
                    Posted::NoRoom(n) => format!("REFUSED, {n} in a row; the queue is full"),
                    Posted::Gone => "REFUSED, nobody is draining".to_string(),
                }
            ));
            became.arrived()
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
    let looking = trace.clone();
    let stalled = writing_since.clone();
    let posted_so_far = posted.clone();
    let handed_over = delivered.clone();
    std::thread::Builder::new()
        .name("smkvm-reader-sequence".into())
        .spawn(move || {
            tell(
                &polling,
                Level::Info,
                format!(
                    "watching the sequence number as well, every {} ms; it is at {} now",
                    LOOK_EVERY.as_millis(),
                    smkvm_clipboard::platform::windows::sequence_number()
                ),
            );
            // Said once per stall rather than once per tick.
            let mut complained = false;
            let mut last_beat = now_in_seconds();
            loop {
                std::thread::sleep(LOOK_EVERY);

                // The drain's watchdog rides on this thread because
                // it is already ticking and has nothing else to do.
                let since = stalled.load(Ordering::Relaxed);
                // `saturating_sub`: the post is counted after the
                // post returns, and the drain can have sent the
                // message before that happens, so for an instant
                // the two can cross.
                let waiting = posted_so_far
                    .load(Ordering::Relaxed)
                    .saturating_sub(handed_over.load(Ordering::Relaxed));
                if since != 0 && now_in_seconds().saturating_sub(since) >= WRITE_IS_STUCK {
                    if !complained {
                        looking.say_whatever_happens(&format!(
                            "the write to the service has been outstanding for {} s, with \
                             {waiting} more waiting behind it. Nothing this machine copies \
                             is reaching the service, and the service is not collecting",
                            now_in_seconds().saturating_sub(since)
                        ));
                        complained = true;
                    }
                } else if complained && since == 0 {
                    looking.say_whatever_happens(&format!(
                        "the write to the service got through; {waiting} still waiting"
                    ));
                    complained = false;
                }

                // A heartbeat, so that silence means something.
                //
                // This thread ticks four times a second and said
                // nothing for thirty of them, and there was no way
                // to tell that from its having stopped. A watch that
                // only speaks on change cannot be used to prove it
                // is still watching.
                if now_in_seconds().saturating_sub(last_beat) >= HEARTBEAT_EVERY {
                    last_beat = now_in_seconds();
                    looking.say_whatever_happens(&format!(
                        "still watching: sequence {}, {} posted, {} sent, {waiting} waiting, \
                         told of {} changes",
                        smkvm_clipboard::platform::windows::sequence_number(),
                        posted_so_far.load(Ordering::Relaxed),
                        handed_over.load(Ordering::Relaxed),
                        smkvm_clipboard::platform::windows::NOTICED.load(Ordering::Relaxed)
                    ));
                }

                let now = smkvm_clipboard::platform::windows::sequence_number();
                if now == claimed.load(Ordering::Relaxed) {
                    continue;
                }
                looking.say(&format!(
                    "the sequence number moved to {now}; letting it settle"
                ));
                // Settle first: a copy arrives as several changes, and
                // the shell's file copy assembles itself over a second
                // or so.
                std::thread::sleep(SETTLE_FOR);
                let settled = smkvm_clipboard::platform::windows::sequence_number();
                if claimed.swap(settled, Ordering::Relaxed) == settled {
                    looking.say(&format!(
                        "the copy at sequence {settled} was already announced by the window"
                    ));
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
                            looking
                                .say("the sequence watch is stopping: a copy could not be posted");
                            return;
                        }
                    }
                    Err(e) => tell(
                        &polling,
                        Level::Warn,
                        format!(
                            "the clipboard changed (sequence {settled}) but would not open: {e}"
                        ),
                    ),
                }
            }
        })
        .context("starting the reader's watch on the sequence number")?;

    // Copies are noticed on their own thread, because `next_change`
    // blocks until one happens and this one has questions to answer
    // meanwhile.
    let noticing = speak.clone();
    let claimed = announced.clone();
    let watching = trace.clone();
    std::thread::Builder::new()
        .name("smkvm-reader-watch".into())
        .spawn(move || {
            while let Some(copied) = clipboard.next_change() {
                let settled = smkvm_clipboard::platform::windows::sequence_number();
                if claimed.swap(settled, Ordering::Relaxed) == settled {
                    // The sequence watch got there first. One copy,
                    // one notice -- but said, because "this path
                    // works and deferred" and "this path never ran"
                    // were indistinguishable, and that cost a round.
                    watching.say(&format!(
                        "told about a copy (sequence {settled}), already announced by the \
                         sequence watch"
                    ));
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
                    // Said before leaving. This return skipped the
                    // line below it, so a watch that ended because a
                    // post was refused ended in silence -- exactly
                    // the state the trace exists to make impossible.
                    watching.say("the clipboard watch is stopping: a copy could not be posted");
                    return;
                }
            }
            watching.say("the clipboard watch has ended");
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
                // Through the trace as well. This is the ordinary
                // way the process ends and it was invisible in the
                // one file that is read when nothing else speaks.
                trace.say(&format!("the service stopped talking: {e}"));
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
                trace.say("the service said to stop");
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
