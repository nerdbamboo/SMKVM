//! The global virtual desktop: where every monitor of every machine sits, and
//! what happens when the cursor crosses between them.
//!
//! This crate is the piece that replaces the "one machine is one rectangle"
//! model SMKVM is built to get away from. A machine contributes as many
//! rectangles as it has monitors, each placed independently, so a two-monitor
//! desktop can sit beneath two single-monitor machines and have the seams line
//! up exactly.
//!
//! It is deliberately free of I/O: no sockets, no platform calls, no clock.
//! Everything here is a pure function of the layout and a pointer delta, which
//! is what lets the tricky part be tested exhaustively.
//!
//! ```
//! use smkvm_layout::{DeviceId, EdgeOverflow, Layout, Monitor, Point, Rect};
//!
//! let a = DeviceId::from_bytes([1; 32]);
//! let b = DeviceId::from_bytes([2; 32]);
//!
//! let mut layout = Layout::new(EdgeOverflow::Clamp);
//! layout.report_monitors(a, "top", vec![Monitor::new("HDMI-1", Rect::new(0, 0, 1920, 1080))]);
//! layout.report_monitors(b, "bottom", vec![Monitor::new("DP-1", Rect::new(0, 0, 1920, 1080))]);
//! layout.place(a, &"HDMI-1".into(), Point::new(0, 0));
//! layout.place(b, &"DP-1".into(), Point::new(0, 1080));
//!
//! // Walking off the bottom of the top machine arrives on the other one at
//! // the same horizontal position, five pixels in.
//! let m = layout.resolve(Point::new(400, 1079), 0, 5).unwrap();
//! assert!(m.crossed_device);
//! assert_eq!(m.located.local, Point::new(400, 4));
//! ```

#![forbid(unsafe_code)]

mod geom;
mod model;
mod resolve;

pub use geom::{map_point, segment_exit, Dir, Exit, Point, Rect};
pub use model::{
    Cell, Device, DeviceId, EdgeOverflow, Layout, Located, Monitor, MonitorId, ParseDeviceIdError,
    Reconcile,
};
pub use resolve::Motion;

impl From<&str> for MonitorId {
    fn from(s: &str) -> Self {
        MonitorId::new(s)
    }
}

impl From<String> for MonitorId {
    fn from(s: String) -> Self {
        MonitorId::new(s)
    }
}
