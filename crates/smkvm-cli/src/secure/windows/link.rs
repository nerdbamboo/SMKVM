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
//! What the arm does when there is no worker is the part worth stating. It
//! reports `Unsupported`, which the client already treats as a refused
//! injection: it looks at what is in the way, finds the secure desktop or
//! nothing, and hands the cursor back to the server -- which is precisely
//! the behaviour of the build before any of this existed. A worker that
//! cannot be started therefore costs the reach it was going to add and
//! nothing else.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use smkvm_input::{Inject, InputError, Monitors};
use smkvm_layout::Monitor;
use smkvm_proto::{Key, MouseButton, Scroll};
use tokio::sync::mpsc::Sender;

use crate::secure::windows::pipe::Pipe;
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
}

impl Link {
    pub fn new() -> Arc<Link> {
        Arc::new(Link::default())
    }

    /// A worker has connected. Anything the old one was told that still
    /// holds is told to this one.
    pub fn attach(&self, pipe: Pipe) {
        let swallowing = *self.swallow.lock().expect("not poisoned");
        *self.write.lock().expect("not poisoned") = Some(pipe);
        self.say(&ToWorker::Swallow(swallowing));
    }

    /// The worker has gone.
    pub fn detach(&self) {
        *self.write.lock().expect("not poisoned") = None;
        self.answers.lock().expect("not poisoned").monitors = None;
    }

    /// Where captured input should go. Set once, by the server glue.
    pub fn capture_to(&self, events: Sender<smkvm_core::Event>) {
        *self.capture.lock().expect("not poisoned") = Some(events);
    }

    /// One instruction down the pipe. False when there is nobody to take it.
    pub fn say(&self, message: &ToWorker) -> bool {
        use std::io::Write as _;
        let mut held = self.write.lock().expect("not poisoned");
        let Some(pipe) = held.as_mut() else {
            return false;
        };
        let Ok(bytes) = frame(message) else {
            return false;
        };
        if pipe.write_all(&bytes).is_err() {
            // A broken pipe is a worker that has gone; forget it here so
            // the next instruction fails quickly rather than on the write.
            *held = None;
            return false;
        }
        true
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
            FromWorker::Refused(why) => tracing::warn!("the worker's injection was refused: {why}"),
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
            // reaching the screen. The client suspends and the cursor goes
            // home, exactly as it did before there was a worker at all.
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
