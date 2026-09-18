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
            while let Ok(event) = incoming.recv() {
                let event = match event {
                    Captured::PointerAt { x, y } => Event::PointerAt { x, y },
                    Captured::PointerBy { dx, dy } => Event::PointerBy { dx, dy },
                    Captured::Button { button, down } => Event::Button { button, down },
                    Captured::Wheel(scroll) => Event::Wheel(scroll),
                    Captured::Key { key, down, repeat } => Event::Key { key, down, repeat },
                };
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
