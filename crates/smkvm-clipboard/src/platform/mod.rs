//! Per-platform clipboard backends.

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub mod windows_drag;

#[cfg(all(unix, not(target_os = "macos")))]
pub mod x11;
