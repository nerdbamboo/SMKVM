//! Choosing the backends this machine can actually provide.
//!
//! A machine can always receive the cursor; whether it can own one depends on
//! having a way to capture input, which not every platform has yet. Saying so
//! plainly beats starting and then doing nothing.

#[cfg_attr(windows, allow(unused_imports))]
use anyhow::{bail, Context, Result};
use smkvm_core::Event;
use smkvm_input::{Inject, Monitors};
use tokio::sync::mpsc::Sender;

use std::path::PathBuf;

use smkvm_clipboard::CatchDrag;

use crate::clipboard::Backends;

/// Open the backend that puts input on this machine's screen.
pub fn injector() -> Result<Box<dyn InjectAndReport>> {
    #[cfg(windows)]
    {
        Ok(Box::new(smkvm_input::platform::windows::WindowsInput::new()))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let x11 = smkvm_input::platform::x11::X11Input::open()
            .context("opening the display to inject input")?;
        Ok(Box::new(x11))
    }
    #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
    {
        bail!("this platform has no way to inject input yet")
    }
}

/// An injector that can also say what displays it has.
pub trait InjectAndReport: Inject + Monitors + Send {}
impl<T: Inject + Monitors + Send> InjectAndReport for T {}

/// Why input injected here is not landing, if it is not.
///
/// Asked when an injection was refused, and every so often while the cursor
/// is here, since the secure desktop refuses nothing -- input to it simply
/// goes nowhere. The second string is for the log: what is in the way and
/// what to do about it.
pub fn injection_blocked() -> Option<(smkvm_proto::SuspendReason, String)> {
    #[cfg(windows)]
    {
        use smkvm_input::platform::windows::{can_inject, desktop, privilege};
        use smkvm_proto::SuspendReason;

        match desktop::current() {
            desktop::InputDesktop::Ours => {}
            desktop::InputDesktop::Elsewhere(name) => {
                return Some((
                    SuspendReason::SecureDesktop,
                    format!(
                        "input is going to the {name} desktop -- a UAC prompt, the lock screen \
                         or Ctrl+Alt+Del -- which a program in the user's session cannot reach"
                    ),
                ))
            }
            desktop::InputDesktop::OutOfReach => {
                return Some((
                    SuspendReason::SecureDesktop,
                    "input is going to a desktop this program is not allowed on, which is what \
                     a UAC prompt or the lock screen does"
                        .into(),
                ))
            }
        }
        if let Some(front) = privilege::foreground_outranks_us() {
            return Some((
                SuspendReason::Elevated,
                format!(
                    "{} is in front and runs at a {} integrity level while smkvm runs at a {} \
                     one, so Windows refuses input from smkvm. Click a different window, or run \
                     smkvm as administrator (a scheduled task with 'run with highest privileges') \
                     so it outranks everything it has to type into",
                    front.program,
                    privilege::describe_level(front.theirs),
                    privilege::describe_level(front.ours)
                ),
            ));
        }
        if !can_inject() {
            return Some((
                SuspendReason::Other,
                "the system refuses injected input right now and does not say why".into(),
            ));
        }
        None
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Say, once at startup, how far this machine's input reaches.
///
/// The limit it reports is invisible in every other way. A daemon at an
/// ordinary integrity level works perfectly until an administrator's window
/// is in front, and then `SendInput` returns zero, the capture hook stops
/// being called, and Windows says nothing at all -- which is a pointer that
/// has frozen for no stated reason. Reading it in the log on the day it is
/// installed beats discovering it the first time a PowerShell is clicked.
pub fn report_rank() {
    #[cfg(windows)]
    {
        use smkvm_input::platform::windows::privilege;
        match privilege::our_level() {
            // High or above: nothing in the session outranks this.
            Some(rid) if rid >= 0x3000 => tracing::info!(
                level = privilege::describe_level(rid),
                "running high enough for every window, including ones run as administrator"
            ),
            Some(rid) => tracing::warn!(
                level = privilege::describe_level(rid),
                "windows that run as administrator will not take input from here, and what is \
                 typed into them will not be captured -- Windows refuses both and says nothing. \
                 `smkvm service install` from an administrator prompt registers a login task \
                 that outranks them"
            ),
            None => {}
        }
    }
}

/// Would injected input land now? The other half of [`injection_blocked`],
/// asked while suspended to know when to take the cursor again.
pub fn injection_possible() -> bool {
    #[cfg(windows)]
    {
        use smkvm_input::platform::windows::{can_inject, desktop};
        desktop::current().is_reachable() && can_inject()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Open this machine's clipboard for watching, reading and offering.
pub fn clipboard() -> Result<Backends> {
    #[cfg(windows)]
    {
        let clipboard = smkvm_clipboard::platform::windows::WindowsClipboard::start()
            .context("watching the clipboard")?;
        let handle = clipboard.handle();
        Ok(Backends {
            watch: Box::new(clipboard),
            read: Box::new(handle.clone()),
            write: Box::new(handle),
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use smkvm_clipboard::platform::x11::{X11Clipboard, X11Writer};
        // Three connections: one blocks waiting for changes, one answers
        // pastes for as long as an offer stands, and one reads on demand.
        // None of them can wait on another.
        let writer = X11Writer::open().context("opening the display to offer the clipboard")?;
        let mut watcher =
            X11Clipboard::open().context("opening the display to watch the clipboard")?;
        watcher.ignore_changes_by(writer.owner_window());
        watcher
            .watch()
            .context("asking to be told of clipboard changes")?;
        let reader = X11Clipboard::open().context("opening the display to read the clipboard")?;
        Ok(Backends {
            watch: Box::new(watcher),
            read: Box::new(reader),
            write: Box::new(writer),
        })
    }
    #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
    {
        bail!("this platform has no clipboard backend yet")
    }
}

/// Something that can pick up the files of a drag as the cursor leaves.
pub fn drop_catcher() -> Result<Box<dyn CatchDrag>> {
    #[cfg(windows)]
    {
        let catcher = smkvm_clipboard::platform::windows_drag::DropCatcher::start()
            .context("making the window that catches drags")?;
        Ok(Box::new(catcher))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let catcher = smkvm_clipboard::platform::x11::DndCatcher::open()
            .context("opening the display to catch drags")?;
        Ok(Box::new(catcher))
    }
    #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
    {
        bail!("this platform has no way to catch a drag yet")
    }
}

/// Drop files that have landed here wherever the pointer is when the button
/// comes up, the way the platform's own drag would. Returns whether the
/// platform can do that at all; where it cannot, the files stay where they
/// landed and are on the clipboard.
pub fn native_drop(paths: Vec<PathBuf>) -> bool {
    #[cfg(windows)]
    {
        match smkvm_clipboard::platform::windows_drag::drop_files(paths) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("could not start the drop: {e}");
                false
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = paths;
        false
    }
}

/// Start watching the local keyboard and mouse, feeding events to the server.
///
/// The returned value keeps the capture alive; dropping it stops it.
#[cfg(windows)]
pub fn start_capture(events: Sender<Event>) -> Result<CaptureHandle> {
    use smkvm_input::platform::windows::capture::{Capture, Captured};

    let (capture, incoming) = Capture::start().context("installing the input hooks")?;
    std::thread::Builder::new()
        .name("smkvm-capture-pump".into())
        .spawn(move || {
            // Counted, because two separate sources feed this and a fault in
            // either is invisible from the outside: the pointer simply never
            // leaves the screen. Raw motion is what decides crossings, so
            // silence from it looks exactly like a cursor that will not go.
            let (mut moves, mut raw, mut keys) = (0u64, 0u64, 0u64);
            let mut reported = std::time::Instant::now();

            while let Ok(event) = incoming.recv() {
                let event = match event {
                    Captured::PointerAt { x, y } => {
                        moves += 1;
                        Event::PointerAt { x, y }
                    }
                    Captured::PointerBy { dx, dy } => {
                        raw += 1;
                        Event::PointerBy { dx, dy }
                    }
                    Captured::Button { button, down } => Event::Button { button, down },
                    Captured::Wheel(scroll) => Event::Wheel(scroll),
                    Captured::Key { key, down, repeat } => {
                        keys += 1;
                        Event::Key { key, down, repeat }
                    }
                };
                if reported.elapsed() >= std::time::Duration::from_secs(10) {
                    tracing::debug!(pointer = moves, raw_motion = raw, keys, "captured so far");
                    reported = std::time::Instant::now();
                }
                if events.blocking_send(event).is_err() {
                    break;
                }
            }
        })
        .context("starting the capture pump")?;
    Ok(CaptureHandle { capture })
}

#[cfg(windows)]
pub struct CaptureHandle {
    capture: smkvm_input::platform::windows::capture::Capture,
}

#[cfg(windows)]
impl CaptureHandle {
    /// Start or stop letting local input through to this machine.
    pub fn set_swallow(&self, swallow: bool) {
        self.capture.set_swallow(swallow);
    }
}

#[cfg(not(windows))]
pub fn start_capture(_events: Sender<Event>) -> Result<CaptureHandle> {
    bail!(
        "this build cannot capture the keyboard and mouse on this platform, so it \
         cannot be the machine that owns them. Run it as a client instead, with \
         `smkvm connect`."
    )
}

#[cfg(not(windows))]
pub struct CaptureHandle;

#[cfg(not(windows))]
impl CaptureHandle {
    pub fn set_swallow(&self, _swallow: bool) {}
}
