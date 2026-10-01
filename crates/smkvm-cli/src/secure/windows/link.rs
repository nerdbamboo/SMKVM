//! The daemon's end of the worker.
//!
//! In service mode the daemon is unchanged: it is the same `smkvm-core`
//! state machine driven by the same `client.rs` or `server.rs`, and it
//! reaches the screen through an [`Inject`] like any other. The difference
//! is which one. Ordinarily that is `WindowsInput`, calling `SendInput` in
//! this process; here it is [`Arm`], which writes the same instruction down
//! the pipe to a process that is on whichever desktop has the input.
//!
//! That is the whole of the seam, and it is deliberately this narrow. The
//! service is glue in exactly the sense `client.rs` is: it owns the link
//! and the state machine and nothing about where the cursor should be.
//!
//! Two things this has to get right, both of which it got wrong first.
//!
//! **What happens when there is no worker.** The arm reports
//! `Unsupported`, which is a refused injection as far as everything above
//! is concerned. But a refusal alone does not suspend anything --
//! `client.rs` suspends only when `platform::injection_blocked` also says
//! *why* -- so the arm without a worker has to be matched by a probe that
//! knows the same thing. That is [`Link::reach`], kept as
//! `secure::reach::Reach` so the accounting is tested on any machine.
//!
//! **What happens when the worker stops reading.** A write to a pipe
//! nobody is reading blocks once the buffer fills, and a write that blocks
//! while holding the lock every injection needs freezes the daemon
//! outright -- the exact opposite of handing the cursor back. Every write
//! from here has a deadline, and a write that misses it means the worker
//! is dead: the pipe is dropped, the reach says no worker, the cursor goes
//! home, and `watch` starts another.
//!
//! One second is still one second. The first caller to meet a wedged
//! worker waits out the deadline holding the lock every other injection
//! wants, so a stall shows as a second of dead pointer rather than an
//! immediate hand-back. That is a hundred times better than the for-ever
//! it replaced and not worth more machinery; if it ever does matter, the
//! answer is to have the reader thread mark the link dead the moment its
//! read fails, so later writers fail at once instead of queueing.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use smkvm_clipboard::{Available, Fetch};
use smkvm_input::{Inject, InputError, Monitors};
use smkvm_layout::Monitor;
use smkvm_proto::{ClipFormat, Key, MouseButton, Scroll};
use tokio::sync::mpsc::Sender;

use crate::secure::reach::Reach;
use crate::secure::watch;
use crate::secure::windows::clip::{Waiting, SERVICE_FETCH_WITHIN};
use crate::secure::windows::pipe::{Pipe, WRITE_WITHIN};
use crate::secure::wire::{frame, FromWorker, Level, ToWorker};

/// How long to wait for a worker to say what displays the machine has.
///
/// Asked once when a session starts, on the daemon's own thread. A worker
/// that does not answer in this is a worker that is being replaced, and an
/// empty list is a better answer than a daemon that never gets going.
const MONITORS_WITHIN: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Answers {
    monitors: Option<Vec<Monitor>>,
}

/// What is being announced on the person's clipboard, and where to get
/// it when something pastes.
type Announced = (Vec<ClipFormat>, Arc<Mutex<Box<dyn Fetch>>>);

/// Shared between the daemon and the thread minding the worker.
pub struct Link {
    /// The write end, while a worker is connected.
    write: Mutex<Option<Pipe>>,
    answers: Mutex<Answers>,
    answered: Condvar,
    /// Where captured input goes, on a machine that owns the keyboard.
    capture: Mutex<Option<Sender<smkvm_core::Event>>>,
    /// Whether the local keyboard and mouse are being swallowed, so that a
    /// worker starting on a new desktop is told before it hooks anything.
    swallow: Mutex<bool>,
    /// Whether input is getting to a screen, and why not when it is not.
    reach: Mutex<Reach>,
    /// Where the worker last said the input was. The service has no way
    /// of its own to find this out, so this is the only source.
    said_input_is_on: Mutex<Option<String>>,

    /// Clipboard reads and drag catches waiting for a worker's answer.
    clipboard_reads: Waiting<Result<Vec<u8>, String>>,
    drags: Waiting<Vec<PathBuf>>,
    /// Where noticed copies go, while anything is listening.
    clipboard_changes: Mutex<Option<std::sync::mpsc::Sender<Available>>>,
    /// What this machine is currently announcing on the person's
    /// clipboard, and where to get it.
    ///
    /// Kept here rather than only in the worker because the worker is
    /// replaced every time the input goes to a consent prompt and back,
    /// and its clipboard window goes with it. Without this, a copy made
    /// on another machine would stop being available because a prompt
    /// had appeared on this one.
    /// `Fetch` is `Send` and not `Sync` -- it is written to be handed
    /// to one place and used there -- so it is behind a lock of its own
    /// rather than shared directly. That also serialises pastes, which
    /// is right: the thing it fetches through is one network link.
    offer: Mutex<Option<Announced>>,
    /// Whether the worker now attached is on a desktop that should hold
    /// the person's clipboard at all.
    clipboard_is_the_workers: Mutex<bool>,
}

impl Link {
    pub fn new() -> Arc<Link> {
        Arc::new(Link {
            write: Mutex::new(None),
            answers: Mutex::new(Answers::default()),
            answered: Condvar::new(),
            capture: Mutex::new(None),
            swallow: Mutex::new(false),
            reach: Mutex::new(Reach::default()),
            said_input_is_on: Mutex::new(None),
            clipboard_reads: Waiting::new(),
            drags: Waiting::new(),
            clipboard_changes: Mutex::new(None),
            offer: Mutex::new(None),
            clipboard_is_the_workers: Mutex::new(false),
        })
    }

    /// A worker has connected. Anything the old one was told that still
    /// holds is told to this one.
    ///
    /// Called only after the worker's first frame has been read and
    /// accepted. Attaching first and checking afterwards -- which is what
    /// this did -- means an instruction has already been sent to a process
    /// whose build has not been established.
    pub fn attach(&self, pipe: Pipe, desktop: &str) {
        let swallowing = *self.swallow.lock().expect("not poisoned");
        *self.write.lock().expect("not poisoned") = Some(pipe);
        // The desktop the worker said it landed on, not the one it was
        // sent to. Anything sent while those differ lands on the wrong
        // desktop, which is worse than not landing.
        self.reach
            .lock()
            .expect("not poisoned")
            .attached(Some(desktop));
        self.say(&ToWorker::Swallow(swallowing));

        // The clipboard is the ordinary desktop's; see
        // `watch::serves_the_clipboard`.
        let theirs = watch::serves_the_clipboard(desktop);
        *self.clipboard_is_the_workers.lock().expect("not poisoned") = theirs;
        self.say(&ToWorker::ServeClipboard(theirs));
        if theirs {
            // Whatever was being announced before the last worker went
            // is announced again. This is what makes a consent prompt
            // suspend the clipboard rather than lose it.
            let standing = self.offer.lock().expect("not poisoned").clone();
            if let Some((formats, _)) = standing {
                tracing::debug!(
                    formats = formats.len(),
                    "offering the far machine's clipboard again on the new worker"
                );
                self.say(&ToWorker::OfferClipboard { formats });
            }
        }
    }

    pub fn clipboard_reads(&self) -> &Waiting<Result<Vec<u8>, String>> {
        &self.clipboard_reads
    }

    pub fn drags(&self) -> &Waiting<Vec<PathBuf>> {
        &self.drags
    }

    /// Where to send copies the worker notices.
    pub fn watch_clipboard(&self) -> std::sync::mpsc::Receiver<Available> {
        let (tx, rx) = std::sync::mpsc::channel();
        *self.clipboard_changes.lock().expect("not poisoned") = Some(tx);
        rx
    }

    /// Remember what is being announced, so a new worker can be told.
    pub fn hold_offer(&self, formats: Vec<ClipFormat>, source: Box<dyn Fetch>) {
        *self.offer.lock().expect("not poisoned") = Some((formats, Arc::new(Mutex::new(source))));
    }

    pub fn drop_offer(&self) {
        *self.offer.lock().expect("not poisoned") = None;
    }

    /// The worker has gone.
    pub fn detach(&self) {
        *self.write.lock().expect("not poisoned") = None;
        self.answers.lock().expect("not poisoned").monitors = None;
        self.reach.lock().expect("not poisoned").attached(None);
        // Nobody is reporting any more, so what the last one said is no
        // longer a fact about now. Kept as unknown rather than as the
        // old answer, which would have the service believing a desktop
        // nothing is watching.
        *self.said_input_is_on.lock().expect("not poisoned") = None;
        *self.clipboard_is_the_workers.lock().expect("not poisoned") = false;
        // Everything asked of the worker that has not come back never
        // will. Woken now rather than left to time out one by one,
        // which would stall the exchange for seconds over a worker that
        // is already gone. The standing offer is deliberately *not*
        // dropped: it is what the next worker will be told.
        self.clipboard_reads.nobody_is_answering();
        self.drags.nobody_is_answering();
    }

    /// Where captured input should go. Set once, by the server glue.
    pub fn capture_to(&self, events: Sender<smkvm_core::Event>) {
        *self.capture.lock().expect("not poisoned") = Some(events);
    }

    /// Where the worker last said the input was, if one has said.
    pub fn input_is_on(&self) -> Option<String> {
        self.said_input_is_on.lock().expect("not poisoned").clone()
    }

    /// Is input reaching a screen, and if not, why not -- in words.
    pub fn blocked(&self) -> Option<String> {
        self.reach.lock().expect("not poisoned").why(Instant::now())
    }

    /// Would an injection land right now? The other half of [`Link::blocked`],
    /// asked while suspended to know when to take the cursor back.
    pub fn possible(&self) -> bool {
        self.reach
            .lock()
            .expect("not poisoned")
            .possible(Instant::now())
    }

    /// One instruction down the pipe. False when there is nobody to take it.
    pub fn say(&self, message: &ToWorker) -> bool {
        let mut held = self.write.lock().expect("not poisoned");
        let Some(pipe) = held.as_mut() else {
            return false;
        };
        let Ok(bytes) = frame(message) else {
            return false;
        };
        // `write_all` on a pipe with a deadline stops on the first short
        // or failed write, so a partial frame cannot be followed later by
        // the rest of it -- which would be read as a frame of its own.
        // Either the whole instruction went or the worker is dead.
        let sent = pipe
            .write_within(&bytes, WRITE_WITHIN)
            .and_then(|written| {
                if written == bytes.len() {
                    Ok(())
                } else {
                    // Kept as a failure rather than looping: a pipe that
                    // takes part of a frame within the deadline and stops
                    // is one whose reader has stopped.
                    Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "the worker took only part of the instruction",
                    ))
                }
            })
            .map_err(|e| {
                // Written at warn because a worker going away mid-session
                // is the thing somebody diagnosing a stopped pointer needs
                // to see, and it is otherwise entirely silent.
                tracing::warn!("the worker stopped taking instructions: {e}");
            })
            .is_ok();
        if !sent {
            // Forgotten here rather than left for the minding thread to
            // notice: the next injection must fail at once and the cursor
            // must go home now, not in a quarter of a second.
            *held = None;
            drop(held);
            self.reach.lock().expect("not poisoned").attached(None);
        } else {
            // Something went out. Whether it lands is the worker's to
            // say, and it says so by *not* reporting a refusal; that is
            // what ends a run of them and puts the backoff back to the
            // start.
            drop(held);
            self.reach
                .lock()
                .expect("not poisoned")
                .sent(Instant::now());
        }
        sent
    }

    /// Something the worker said. Called from the thread reading the pipe.
    pub fn heard(self: &Arc<Self>, message: FromWorker) {
        match message {
            FromWorker::Saw(saw) => {
                let held = self.capture.lock().expect("not poisoned");
                if let Some(events) = held.as_ref() {
                    // Dropped rather than waited on: this is the thread that
                    // also notices the worker dying, and a full queue must
                    // not stop it doing that. What is lost is pointer motion.
                    let _ = events.try_send(smkvm_core::Event::from(saw));
                }
            }
            FromWorker::Monitors(monitors) => {
                self.answers.lock().expect("not poisoned").monitors = Some(monitors);
                self.answered.notify_all();
            }
            FromWorker::InputDesktop(name) => {
                // Both halves of the same fact: what the minding thread
                // decides about, and what the reach compares the
                // worker's own desktop against.
                *self.said_input_is_on.lock().expect("not poisoned") = name.clone();
                self.reach
                    .lock()
                    .expect("not poisoned")
                    .input_desktop(name.as_deref());
            }
            FromWorker::Refused(why) => {
                tracing::warn!("the worker's injection was refused: {why}");
                self.reach
                    .lock()
                    .expect("not poisoned")
                    .refused(Instant::now());
            }
            FromWorker::ClipboardChanged(formats) => {
                let held = self.clipboard_changes.lock().expect("not poisoned");
                match held.as_ref() {
                    Some(changes) => {
                        tracing::info!(
                            ?formats,
                            "clipboard: the worker says something was copied on the desktop"
                        );
                        if changes.send(Available { formats }).is_err() {
                            tracing::warn!(
                                "clipboard: nothing is listening for copies any more, so what \
                                 this machine copies will not reach another"
                            );
                        }
                    }
                    // Dropped in silence until now. This is the far
                    // end of the only path by which a copy made here
                    // reaches another machine, and an unwired
                    // receiver here loses every copy without a word.
                    None => tracing::warn!(
                        ?formats,
                        "clipboard: the worker says something was copied, but nothing on this \
                         side is listening for copies, so it goes nowhere"
                    ),
                }
            }
            FromWorker::ClipboardRead { id, bytes } => {
                self.clipboard_reads.answer(id, bytes);
            }
            FromWorker::DragCaught { id, paths } => {
                self.drags.answer(id, paths);
            }
            FromWorker::Said { level, text } => match level {
                // Marked, so that a line the worker asked for is never
                // mistaken for one the service noticed itself -- they
                // are two different processes and which of them saw a
                // thing is most of the diagnosis.
                Level::Debug => tracing::debug!("worker: {text}"),
                Level::Info => tracing::info!("worker: {text}"),
                Level::Warn => tracing::warn!("worker: {text}"),
                Level::Error => tracing::error!("worker: {text}"),
            },
            FromWorker::WantsPaste { id, format } => self.someone_is_pasting(id, format),
            FromWorker::Ready { .. } => {}
        }
    }

    /// Something on the person's desktop is pasting what another machine
    /// copied, so the contents are wanted now.
    ///
    /// On its own thread, and that is the point rather than tidiness.
    /// Fetching means a round trip to the machine that did the copying,
    /// and this is called from the thread that reads the pipe -- the
    /// same thread that notices the worker dying. Doing the fetch here
    /// would stop that thread for the length of a network transfer, so
    /// a slow paste would make the service blind to its own worker.
    fn someone_is_pasting(self: &Arc<Self>, id: u64, format: ClipFormat) {
        // The first thing, before anything can go wrong. Whether this
        // line appears is the difference between "the worker's
        // request never arrived" and "it arrived and nothing came of
        // it", and those two have looked identical from the log for
        // two rounds.
        tracing::info!(?format, id, "clipboard: the worker is asking for a paste");
        let standing = self.offer.lock().expect("not poisoned").clone();
        let Some((_, source)) = standing else {
            // Nothing is being announced, so there is nothing to fetch.
            // Answered rather than ignored: the worker is inside a paste
            // and something has to end it.
            self.say(&ToWorker::Pasted {
                id,
                bytes: Err("this machine is not announcing anything to paste".into()),
            });
            return;
        };
        let link = self.clone();
        let started = std::thread::Builder::new()
            .name("smkvm-paste".into())
            .spawn(move || {
                // Never queued behind another fetch. Waiting for one in
                // flight is how a second render came to take sixty
                // seconds -- thirty behind the first, thirty of its
                // own -- and produce data for a clipboard that had long
                // since closed. A paste that cannot start now is
                // refused now, and the one already running will leave
                // its answer where the next paste finds it.
                let Ok(source) = source.try_lock() else {
                    link.say(&ToWorker::Pasted {
                        id,
                        bytes: Err(
                            "these contents are already being fetched for another paste; \
                             try again in a moment"
                                .into(),
                        ),
                    });
                    return;
                };
                // Said before the call, not only after it.
                //
                // `fetch` here has no deadline of its own --
                // `SERVICE_FETCH_WITHIN` is measured against it, not
                // imposed on it -- so if the far machine never
                // answers, this thread waits for ever and the only
                // record of the attempt is a line that never comes.
                // That is precisely the shape of fault that cost
                // three rounds on the render, and it is not going to
                // cost another one by being invisible.
                tracing::info!(
                    ?format,
                    id,
                    "clipboard: asking the far machine for what it copied"
                );
                let asked_at = Instant::now();
                let bytes = source
                    .fetch(&format)
                    .map_err(|e| format!("fetching what was copied: {e}"));
                let took = asked_at.elapsed();
                // Said on the way through, not only when it is late.
                // The question "where does the time go" cannot be
                // answered by a line that appears only once the answer
                // is already bad, and raising a budget without knowing
                // is how a number comes to hide a loop.
                tracing::info!(
                    ?format,
                    took_ms = took.as_millis() as u64,
                    budget_ms = SERVICE_FETCH_WITHIN.as_millis() as u64,
                    bytes = bytes.as_ref().map(Vec::len).unwrap_or(0),
                    "the far machine answered a paste"
                );
                if took > SERVICE_FETCH_WITHIN {
                    // Said, because it is the one number that decides
                    // whether a paste works first time. The share of
                    // the render's budget this hop is allowed is
                    // `SERVICE_FETCH_WITHIN`; past it the worker has
                    // stopped listening and the contents will be kept
                    // for the next paste instead of serving this one.
                    tracing::warn!(
                        ?format,
                        took_ms = took.as_millis() as u64,
                        budget_ms = SERVICE_FETCH_WITHIN.as_millis() as u64,
                        "the far machine took longer than a paste can wait, so this paste \
                         produced nothing; the contents are kept and pasting again should \
                         be immediate"
                    );
                }
                link.say(&ToWorker::Pasted { id, bytes });
            });
        if started.is_err() {
            self.say(&ToWorker::Pasted {
                id,
                bytes: Err("this machine could not start a thread to fetch it".into()),
            });
        }
    }

    fn ask_monitors(&self) -> Option<Vec<Monitor>> {
        self.answers.lock().expect("not poisoned").monitors = None;
        if !self.say(&ToWorker::TellMonitors) {
            return None;
        }
        let held = self.answers.lock().expect("not poisoned");
        let (held, _) = self
            .answered
            .wait_timeout_while(held, MONITORS_WITHIN, |a| a.monitors.is_none())
            .expect("not poisoned");
        held.monitors.clone()
    }
}

/// The daemon's injector when the work is done by a worker.
pub struct Arm(pub Arc<Link>);

impl Arm {
    fn say(&mut self, message: ToWorker) -> smkvm_input::Result<()> {
        if self.0.say(&message) {
            Ok(())
        } else {
            // The same error a platform with no injector gives, because to
            // everything above this it is the same situation: input is not
            // reaching the screen. `platform::injection_blocked` then says
            // why, and the client suspends and the cursor goes home --
            // exactly as it did before there was a worker at all.
            Err(InputError::Unsupported("a worker on the input desktop"))
        }
    }
}

impl Inject for Arm {
    fn move_to(&mut self, x: i32, y: i32) -> smkvm_input::Result<()> {
        self.say(ToWorker::MoveTo { x, y })
    }
    fn button(&mut self, button: MouseButton, down: bool) -> smkvm_input::Result<()> {
        self.say(ToWorker::Button { button, down })
    }
    fn wheel(&mut self, scroll: Scroll) -> smkvm_input::Result<()> {
        self.say(ToWorker::Wheel(scroll))
    }
    fn key(&mut self, key: Key, down: bool) -> smkvm_input::Result<()> {
        self.say(ToWorker::Key { key, down })
    }
    fn flush(&mut self) -> smkvm_input::Result<()> {
        self.say(ToWorker::Flush)
    }
    fn hide_cursor(&mut self) -> smkvm_input::Result<()> {
        self.say(ToWorker::HideCursor)
    }
    fn show_cursor(&mut self) -> smkvm_input::Result<()> {
        self.say(ToWorker::ShowCursor)
    }
}

impl Monitors for Arm {
    fn monitors(&mut self) -> smkvm_input::Result<Vec<Monitor>> {
        self.0.ask_monitors().ok_or(InputError::Display(
            "no worker answered with this machine's displays".into(),
        ))
    }
}

/// Set once by the service before it starts the daemon, and read by
/// `platform::injector`.
///
/// A global, which is not how anything else in this program is arranged,
/// and the reason is worth writing down: the alternative is threading an
/// injector choice through `run`, `serve`, `connect` and `session` -- every
/// one of which is code that three machines depend on and none of which has
/// anything to do with desktops. A value that is written once before the
/// daemon starts and only read afterwards is the smaller change, and the
/// one that cannot alter what happens when it is not set.
static ARM: std::sync::OnceLock<Arc<Link>> = std::sync::OnceLock::new();

pub fn use_worker(link: Arc<Link>) {
    let _ = ARM.set(link);
}

pub fn worker() -> Option<Arc<Link>> {
    ARM.get().cloned()
}

/// Feed what the worker captures to the server's event queue.
pub fn capture_through_worker(events: Sender<smkvm_core::Event>) -> bool {
    match worker() {
        Some(link) => {
            link.capture_to(events);
            true
        }
        None => false,
    }
}

/// Start or stop swallowing the local keyboard and mouse, remembered so a
/// worker that starts later is told.
pub fn set_swallow(swallow: bool) {
    if let Some(link) = worker() {
        *link.swallow.lock().expect("not poisoned") = swallow;
        link.say(&ToWorker::Swallow(swallow));
    }
}
