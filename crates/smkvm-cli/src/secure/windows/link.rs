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

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use smkvm_input::{Inject, InputError, Monitors};
use smkvm_layout::Monitor;
use smkvm_proto::{Key, MouseButton, Scroll};
use tokio::sync::mpsc::Sender;

use crate::secure::reach::Reach;
use crate::secure::windows::pipe::{Pipe, WRITE_WITHIN};
use crate::secure::wire::{frame, FromWorker, ToWorker};

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

/// Shared between the daemon and the thread minding the worker.
#[derive(Default)]
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
}

impl Link {
    pub fn new() -> Arc<Link> {
        Arc::new(Link::default())
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
    }

    /// The worker has gone.
    pub fn detach(&self) {
        *self.write.lock().expect("not poisoned") = None;
        self.answers.lock().expect("not poisoned").monitors = None;
        self.reach.lock().expect("not poisoned").attached(None);
    }

    /// Where captured input should go. Set once, by the server glue.
    pub fn capture_to(&self, events: Sender<smkvm_core::Event>) {
        *self.capture.lock().expect("not poisoned") = Some(events);
    }

    /// What the service's poll of the input desktop saw, so that a
    /// worker left behind on the desktop the input has moved off is known
    /// to be the wrong place to send anything.
    pub fn input_desktop(&self, desktop: Option<&str>) {
        self.reach
            .lock()
            .expect("not poisoned")
            .input_desktop(desktop);
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
    pub fn heard(&self, message: FromWorker) {
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
            FromWorker::Refused(why) => {
                tracing::warn!("the worker's injection was refused: {why}");
                self.reach
                    .lock()
                    .expect("not poisoned")
                    .refused(Instant::now());
            }
            FromWorker::Ready { .. } => {}
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
