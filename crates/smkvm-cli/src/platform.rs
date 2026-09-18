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
