//! Asking this machine what displays it has.
//!
//! Only the reading half of what the daemon opens. A window never injects
//! anything, so nothing reached from here can press a key or move a pointer.

use smkvm_input::{Monitors, Result};

pub fn displays() -> Result<Box<dyn Monitors>> {
    #[cfg(windows)]
    {
        Ok(Box::new(smkvm_input::platform::windows::WindowsInput::new()))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Ok(Box::new(smkvm_input::platform::x11::X11Input::open()?))
    }
    #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
    {
        Err(smkvm_input::InputError::Unsupported(
            "reading this machine's displays",
        ))
    }
}
