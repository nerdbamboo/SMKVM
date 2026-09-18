//! Per-platform input backends.
//!
//! Each provides [`crate::Inject`], and the client picks one at startup. The
//! loopback backend is always available and is what lets the whole client and
//! server be exercised in tests with no display server at all.

pub mod loopback;

#[cfg(all(unix, not(target_os = "macos")))]
pub mod x11;
