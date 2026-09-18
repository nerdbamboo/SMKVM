//! Per-platform clipboard backends.

#[cfg(all(unix, not(target_os = "macos")))]
pub mod x11;
