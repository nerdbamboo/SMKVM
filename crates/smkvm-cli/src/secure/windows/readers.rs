//! The service's end of the reader: starting one, and asking it
//! things.
//!
//! The mirror of `link` for the other helper, and deliberately much
//! smaller, because the reader can be asked only two things.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use smkvm_clipboard::{Available, ClipboardError, Read, Watch};
use smkvm_proto::ClipFormat;

use crate::secure::acl;
use crate::secure::outbox::Outbox;
use crate::secure::reading::{self, FromReader, ToReader, LONGEST_READ};
use crate::secure::windows::clip::Waiting;
use crate::secure::windows::{pipe, secret, token};
use crate::secure::wire::{frame_up_to, read_frame_up_to, Level};
use smkvm_clipboard::{Say, Throttle};

/// How long to wait for a reader to answer.
///
/// A read crosses a pipe and touches the clipboard, which another
/// application may be holding for a moment. It does not cross a
/// network, so this is generous rather than tight -- but it is
/// bounded, because the thing waiting on it may be a render with a
/// budget of its own.
pub const READER_ANSWERS_WITHIN: Duration = Duration::from_secs(3);

/// What the collector is in the middle of.
const COLLECTOR_STARTING: u8 = 0;
const COLLECTOR_READING: u8 = 1;
const COLLECTOR_HANDING_OVER: u8 = 2;
const COLLECTOR_FINISHED: u8 = 3;

fn where_it_is(doing: u8) -> &'static str {
    match doing {
        COLLECTOR_STARTING => "not started",
        COLLECTOR_READING => "waiting to read from the pipe",
        COLLECTOR_HANDING_OVER => "handing a message over",
        _ => "finished",
    }
}

/// How many messages may be waiting to be acted on.
///
/// Generous: the queue only grows when something is slow to handle,
/// and the messages in it are the person's copies.
const HANDLING_ROOM: usize = 256;

/// Seconds since the epoch, for comparing two moments.
fn now_in_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn which_one(said: &FromReader) -> u8 {
    match said {
        FromReader::Ready { .. } => 1,
        FromReader::Copied(_) => 2,
        FromReader::OnIt { .. } => 3,
        FromReader::Read { .. } => 4,
        FromReader::Said { .. } => 5,
    }
}

fn what_that_was(which: u8) -> &'static str {
    match which {
        1 => "a hello",
        2 => "a copy",
        3 => "what is on it",
        4 => "a read",
        _ => "a line",
    }
}

/// Where the reader writes what it is doing, inside the profile of
/// the person it runs as.
const READER_TRACE: &str = "smkvm-reader-trace.txt";

/// What the reader managed to say before it stopped.
///
/// Read back into the service's log, because the service's log is
/// the one somebody is already looking at, and a file nobody is told
/// about is a file nobody reads.
fn what_it_said(trace_to: &Path) -> String {
    match std::fs::read_to_string(trace_to) {
        Ok(said) if said.trim().is_empty() => {
            format!("nothing at all; {} is empty", trace_to.display())
        }
        Ok(said) => format!("[{}]", said.trim().replace('\n', " | ")),
        Err(e) => format!(
            "nothing at all; {} could not be read ({e}), so it died before its first \
             statement and the question is process creation rather than anything in it",
            trace_to.display()
        ),
    }
}

/// How long after starting a reader to look whether it is still
/// there.
///
/// Long enough for a process that cannot start at all to have gone,
/// short enough that it costs nothing when one starts normally.
const FIRST_LOOK_AFTER: Duration = Duration::from_millis(300);

/// How long a reader is given to stop politely before it is ended.
const LET_GO_GRACE: Duration = Duration::from_millis(500);

/// How long to wait for a started reader to connect.
const READER_CONNECTS_WITHIN: Duration = Duration::from_secs(10);

/// The service's end of one reader.
pub struct Reader {
    write: Mutex<pipe::Pipe>,
    answers: Waiting<Result<Vec<u8>, String>>,
    listed: Waiting<Vec<ClipFormat>>,
    /// Kept so the process is not reaped while its id is being
    /// checked, and so it can be ended.
    running: token::Started,
    pub who: String,
    /// Where it writes what it is doing, so the service can read it
    /// back when the reader stops being able to say anything.
    trace_to: PathBuf,
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

    fn heard(&self, said: FromReader) {
        match said {
            FromReader::Copied(formats) => copied(formats, "reader"),
            FromReader::OnIt { id, formats } => {
                self.listed.answer(id, formats);
            }
            FromReader::Read { id, bytes } => {
                self.answers.answer(id, bytes);
            }
            // Tidied and rate-limited, because this is the one
            // message in either direction that is not a read.
            // Unsolicited, arbitrary, and chosen in severity by
            // whatever is at the other end of a pipe the person can
            // reach -- and the log is the artefact every diagnosis
            // in this project has turned on.
            FromReader::Said { level, text } => {
                let text = reading::tidy(&text);
                match SAID
                    .get_or_init(Throttle::new)
                    .asked_at(&text, Instant::now())
                {
                    Say::No => {}
                    Say::Yes => say_it(level, &text),
                    Say::YesAfter(held_back) => say_it(
                        level,
                        &format!("{text} [and {held_back} more like it in the last second]"),
                    ),
                }
            }
            FromReader::Ready { .. } => {}
        }
    }
}

/// What the reader has said lately, so that a flood is a line and a
/// count rather than a log nobody can read.
static SAID: std::sync::OnceLock<Throttle> = std::sync::OnceLock::new();

fn say_it(level: Level, text: &str) {
    match level {
        Level::Debug => tracing::debug!("reader: {text}"),
        Level::Info => tracing::info!("reader: {text}"),
        Level::Warn => tracing::warn!("reader: {text}"),
        Level::Error => tracing::error!("reader: {text}"),
    }
}

/// The reader this service is using, if it has one.
static READER: Mutex<Option<Arc<Reader>>> = Mutex::new(None);

/// Where copies go, whichever half noticed them.
///
/// One channel for the life of the daemon, rather than one per
/// reader, and that is the fix for the fault that made the first
/// version of this a no-op.
///
/// The backends are built once, at the top of the daemon, outside the
/// reconnect loop -- `client.rs` says so in its own comment, because
/// the clipboard outlives any one session. The service starts at
/// boot, before anybody has logged in, so asking "is there a reader?"
/// at that moment answers no for the life of the process. A person
/// logging in an hour later got a reader that nothing consulted, and
/// a log line saying it was working.
///
/// So nothing chooses a source once. Copies arrive here from the
/// reader when there is one and from the worker when there is not,
/// and whoever is watching is watching this.
static COPIES: Mutex<Option<std::sync::mpsc::Sender<Available>>> = Mutex::new(None);

/// Watch here for copies, from whichever half can see them.
pub fn copies_go_to(send: std::sync::mpsc::Sender<Available>) {
    *COPIES.lock().expect("not poisoned") = Some(send);
}

/// Somebody copied something. Called by both halves.
pub fn copied(formats: Vec<ClipFormat>, noticed_by: &str) {
    let held = COPIES.lock().expect("not poisoned");
    match held.as_ref() {
        Some(copies) => {
            tracing::info!(
                ?formats,
                "clipboard: the {noticed_by} says something was copied on the desktop"
            );
            if copies.send(Available { formats }).is_err() {
                tracing::warn!(
                    "clipboard: nothing is listening for copies, so what this machine \
                     copies will not reach another"
                );
            }
        }
        // Said rather than discarded. The far end of the only path
        // outward losing everything in silence is how a broken half
        // came to look like an absent one.
        None => tracing::warn!(
            ?formats,
            "clipboard: the {noticed_by} says something was copied, but nothing on this \
             side is listening"
        ),
    }
}

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
        // Asked first, then ended. `Stop` is a courtesy that lets it
        // put the clipboard down tidily; it is not a guarantee, and
        // a reader that ignores it would otherwise go on running as
        // the person with a watch open and nothing holding its
        // handle. The grace is short because nothing it does on the
        // way out takes longer.
        std::thread::sleep(LET_GO_GRACE);
        reader.running.kill();
    }
}

/// Start a reader in the interactive session, as the person in it.
pub fn start(exe: &Path, session: u32, environment: token::Environment) -> Result<Arc<Reader>> {
    token::enable_tcb_privilege()?;

    // The pipe is made before the reader is started, so there is no
    // moment when its name exists and nothing is listening on it.
    // Its access list admits the person at the desk, which it has to
    // -- and therefore admits everything else they are running, which
    // is why the protocol it carries cannot express anything but a
    // read.
    let name = acl::reader_pipe_name(&secret::name_bytes()?);
    let listening =
        pipe::create_with(&name, acl::READER_PIPE_SDDL).context("making a pipe for the reader")?;

    let mut handle = windows::Win32::Foundation::HANDLE::default();
    // SAFETY: a place for the token, owned before any return.
    unsafe { windows::Win32::System::RemoteDesktop::WTSQueryUserToken(session, &mut handle) }
        .with_context(|| format!("asking who is logged in at session {session}"))?;
    let theirs = token::Owned(handle);

    // Somewhere plain for the reader to say what it is doing,
    // worked out here because it cannot work one out for itself
    // before it has said anything -- resolving a writable path is
    // one of the things that has gone wrong.
    //
    // The person's own profile root: proven writable by that account
    // on this machine, since the probe's child wrote its report
    // there, and reachable by the service, which is the system
    // account. The reader is handed it absolute and resolves
    // nothing.
    let trace_to = token::home_in_session(session)
        .context("finding where the person at the desk keeps things")?
        .join(READER_TRACE);
    let _ = std::fs::remove_file(&trace_to);

    // `Default` and nowhere else. The reader has no business on the
    // desktop a consent prompt is on: the person cannot copy anything
    // there, and the worker is already there for the half that
    // matters.
    let running = token::start_on_desktop_as(
        &theirs,
        exe,
        &format!(
            "clipboard-reader --pipe {name} --trace-to \"{}\"",
            trace_to.display()
        ),
        r"WinSta0\Default",
        // Chosen by the caller, which alternates it. See
        // `mind_readers`: the first try gives the reader the
        // person's own environment, which is what it ought to have,
        // and a later one gives it the service's, which is what the
        // probe's child had when it worked. Those two creations
        // differ in nothing else, so if one starts and the other
        // does not, the environment block is the answer.
        environment,
    )
    .context("starting the reader as the person at the desk")?;

    // Said at creation, before anything can go wrong with it.
    //
    // The service knew this number and never printed it, so telling
    // "died instantly" from "never existed" took fourteen samples of
    // the process list. One line is cheaper than that and is
    // available at the only moment it is certainly true.
    tracing::info!(
        pid = running.pid,
        session,
        environment = environment.named(),
        trace = %trace_to.display(),
        "clipboard: started a reader"
    );

    // And immediately: is it still there?
    //
    // `CreateProcessAsUserW` can return success and a handle for a
    // process that is gone a moment later -- a malformed environment
    // is one documented way -- and waiting for the pipe to break
    // reports that as "the reader said nothing", which says nothing
    // about why. A short look here turns it into an exit code.
    std::thread::sleep(FIRST_LOOK_AFTER);
    if let ended @ (token::Ended::With(_) | token::Ended::CouldNotAsk(_)) = running.how_it_ended() {
        bail!(
            "the reader (pid {}, started with {}) was gone {} ms after it was started: \
             {ended}. What it managed to say for itself: {}",
            running.pid,
            environment.named(),
            FIRST_LOOK_AFTER.as_millis(),
            what_it_said(&trace_to)
        );
    }

    // Every failure from here ends the process it started.
    //
    // Dropping `listening` does break the pipe, and the reader does
    // exit when its next read fails -- but that makes this
    // function's cleanup depend on the helper behaving, which is
    // exactly the conclusion an earlier review reached about the
    // worker and exactly what `start_worker` was changed to stop
    // doing. A function that starts a process as somebody cleans up
    // after itself.
    let settled = (|| -> Result<(pipe::Pipe, reading::Who)> {
        pipe::accept(&listening, READER_CONNECTS_WITHIN).context("waiting for the reader")?;
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
        Ok((reading, who))
    })();
    let (mut reading, who) = match settled {
        Ok(settled) => settled,
        Err(e) => {
            // The exit code before the kill, because after it the
            // code is ours and says nothing. A broken pipe only says
            // the process is not there; the code says something
            // about why, and not having it cost a deployment.
            let said = format!(
                "{e:#} (reader pid {}, started with {}: {}). What it said for itself: {}",
                running.pid,
                environment.named(),
                running.how_it_ended(),
                what_it_said(&trace_to)
            );
            running.kill();
            bail!("{said}");
        }
    };

    let reader = Arc::new(Reader {
        write: Mutex::new(listening),
        answers: Waiting::new(),
        listed: Waiting::new(),
        running,
        who: who.who.clone(),
        trace_to,
    });
    match &who.log {
        Ok(path) => tracing::info!(
            who = %who.who,
            session,
            "clipboard: a reader is running as the person at the desk, logging to {path}"
        ),
        Err(why) => tracing::warn!(
            who = %who.who,
            session,
            "clipboard: a reader is running as the person at the desk but has no log of \
             its own ({why}); everything it says arrives here instead"
        ),
    }

    // Reading the pipe and acting on what is read are two jobs, and
    // one thread cannot have both.
    //
    // It had both, and the result was a deadlock with the reader
    // through a single pipe. Handling a `Copied` does not come back
    // promptly -- it reaches the clipboard layer, which may turn
    // round and ask this same reader for the contents -- so the
    // collector stopped reading, the pipe filled, and the reader's
    // drain blocked inside a write with fifteen messages behind it.
    // Each half waiting on the other through the one channel they
    // share.
    //
    // This is the mirror of the rule the outbox already encodes.
    // Writing had to be somebody's whole job; so does reading. The
    // collector now does nothing but empty the pipe, and a second
    // thread does the work -- so a slow or circular piece of
    // handling costs a delay in one message rather than the
    // conversation.
    //
    // Deliberately not a deadline on the round trip. A deadlock with
    // a timeout is still a deadlock; it merely reports itself
    // eventually, and the person's clipboard is broken for the
    // length of the timeout rather than for ever.
    let (inbox, handle_these) = Outbox::with_room_for(HANDLING_ROOM);
    let inbox = Arc::new(inbox);
    // The collector has never reported on itself, and it is the one
    // thread in this conversation that has stopped in every run.
    let what_the_collector_is_doing = Arc::new(AtomicU8::new(COLLECTOR_STARTING));
    let collected = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let what_is_being_handled = Arc::new(AtomicU8::new(0));
    let handling_since = Arc::new(AtomicU64::new(0));

    {
        let listener = reader.clone();
        let doing = what_is_being_handled.clone();
        let since = handling_since.clone();
        std::thread::Builder::new()
            .name("smkvm-reader-handling".into())
            .spawn(move || {
                while let Some(said) = handle_these.next() {
                    doing.store(which_one(&said), Ordering::Relaxed);
                    since.store(now_in_seconds().max(1), Ordering::Relaxed);
                    listener.heard(said);
                    since.store(0, Ordering::Relaxed);
                }
            })
            .context("starting the thread that acts on what the reader says")?;
    }

    // The watchdog is a thread of its own, and that is the point
    // rather than tidiness.
    //
    // Four watchdogs in this file have now been silent in exactly
    // the case they were written for, and all four for the same
    // reason: each was reached through the thing it was watching.
    // This one shares no thread, no lock and no channel with the
    // collector or the handler; it reads two atomics and writes to
    // the service's own log.
    {
        let since = handling_since.clone();
        let doing = what_is_being_handled.clone();
        let collecting = what_the_collector_is_doing.clone();
        let taken = collected.clone();
        let lost = dropped.clone();
        let depth = inbox.clone();
        let pid = reader.running.pid;
        std::thread::Builder::new()
            .name("smkvm-reader-watchdog".into())
            .spawn(move || {
                let mut complained = false;
                let mut beat = 0u32;
                loop {
                    std::thread::sleep(Duration::from_secs(1));

                    // A heartbeat from this side too, so the two
                    // ends can be compared. The reader says how
                    // many it has sent; until now nothing here said
                    // how many had been received.
                    beat += 1;
                    if beat % 5 == 0 {
                        tracing::info!(
                            pid,
                            "clipboard: collected {} from the reader, {} dropped, {} \
                             waiting to be acted on; the collector is {}",
                            taken.load(Ordering::Relaxed),
                            lost.load(Ordering::Relaxed),
                            depth.waiting(),
                            where_it_is(collecting.load(Ordering::Relaxed))
                        );
                    }
                    let began = since.load(Ordering::Relaxed);
                    let stuck = began != 0 && now_in_seconds().saturating_sub(began) >= 2;
                    if stuck && !complained {
                        tracing::warn!(
                            pid,
                            "clipboard: acting on {} from the reader has taken {} s and \
                             has not come back. The pipe is still being emptied, so the \
                             reader is not blocked by it",
                            what_that_was(doing.load(Ordering::Relaxed)),
                            now_in_seconds().saturating_sub(began)
                        );
                        complained = true;
                    } else if !stuck && complained {
                        tracing::info!(pid, "clipboard: the reader's work came back");
                        complained = false;
                    }
                }
            })
            .context("starting the watchdog for the reader's work")?;
    }

    let listener = reader.clone();
    let collecting = what_the_collector_is_doing.clone();
    let taken = collected.clone();
    let lost = dropped.clone();
    std::thread::Builder::new()
        .name("smkvm-reader-link".into())
        .spawn(move || {
            tracing::info!(
                pid = listener.running.pid,
                "clipboard: collecting from the reader"
            );
            let mut count = 0usize;
            loop {
                // Nothing on this thread may wait, and that now
                // includes writing a log line. `tracing` goes to a
                // file; a file can block; and this is the one thread
                // in the conversation whose stopping stops
                // everything. It keeps numbers in atomics and the
                // watchdog says them.
                collecting.store(COLLECTOR_READING, Ordering::Relaxed);
                match read_frame_up_to::<FromReader, _>(&mut reading, LONGEST_READ) {
                    Ok(said) => {
                        collecting.store(COLLECTOR_HANDING_OVER, Ordering::Relaxed);
                        count += 1;
                        taken.store(count, Ordering::Relaxed);
                        // `post` is `try_send` and cannot wait. If
                        // the queue is full the notice is dropped
                        // and counted: a lost notice costs one
                        // paste, a full pipe costs the feature.
                        if !inbox.post(said).arrived() {
                            lost.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        collecting.store(COLLECTOR_FINISHED, Ordering::Relaxed);
                        tracing::warn!(
                            pid = listener.running.pid,
                            "clipboard: the reader stopped talking after {count} \
                             messages ({e}): {}. What it said for itself: {}. Nothing \
                             this machine copies will reach another until one starts again",
                            listener.running.how_it_ended(),
                            what_it_said(&listener.trace_to)
                        );
                        break;
                    }
                }
            }
            listener.nobody_is_answering();
            let mut held = READER.lock().expect("not poisoned");
            if held.as_ref().is_some_and(|now| Arc::ptr_eq(now, &listener)) {
                *held = None;
            }
        })
        .context("listening to the reader")?;

    *READER.lock().expect("not poisoned") = Some(reader.clone());
    Ok(reader)
}

/// Copies, from whichever half noticed them.
pub struct WatchWhoeverSees {
    pub copies: std::sync::mpsc::Receiver<Available>,
}

impl Watch for WatchWhoeverSees {
    fn next_change(&mut self) -> Option<Available> {
        self.copies.recv().ok()
    }
}

/// Reading the person's clipboard: through the reader when there is
/// one, and through the worker when there is not.
///
/// Asked on every call rather than chosen once. There is no moment at
/// which this can be decided: the backends are built before anybody
/// has logged in, and the reader appears later.
pub struct ReadWhoeverCan(pub Arc<crate::secure::windows::link::Link>);

impl Read for ReadWhoeverCan {
    fn read(&mut self, format: &ClipFormat) -> smkvm_clipboard::Result<Vec<u8>> {
        let Some(reader) = reader() else {
            // No reader, so the worker answers. It will see nothing a
            // person copied -- that is the whole finding -- but when
            // nobody is logged in there is nothing to see, and when
            // somebody is and the reader has died, the worker's
            // refusal is a better answer than a refusal of ours.
            return crate::secure::windows::clip::ReadThroughWorker(self.0.clone()).read(format);
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
