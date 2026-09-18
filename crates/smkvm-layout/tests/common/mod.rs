//! Helpers shared by the layout tests.
//!
//! Layouts here are built the same way the running server builds them: devices
//! report monitors, then placements are applied. Nothing is special-cased for a
//! particular machine count or arrangement.

#![allow(dead_code)]

use smkvm_layout::{DeviceId, EdgeOverflow, Layout, Monitor, MonitorId, Point, Rect};

pub fn dev(n: u8) -> DeviceId {
    DeviceId::from_bytes([n; 32])
}

pub fn mid(s: &str) -> MonitorId {
    MonitorId::new(s)
}

/// A machine and its monitors, each given as `(id, local rect, global origin)`.
pub struct Spec<'a> {
    pub device: DeviceId,
    pub name: &'a str,
    pub monitors: Vec<(&'a str, Rect, Point)>,
}

pub fn build(overflow: EdgeOverflow, specs: &[Spec<'_>]) -> Layout {
    let mut layout = Layout::new(overflow);
    for spec in specs {
        let monitors = spec
            .monitors
            .iter()
            .map(|(id, local, _)| Monitor::new(*id, *local))
            .collect();
        layout.report_monitors(spec.device, spec.name, monitors);
        for (id, _, origin) in &spec.monitors {
            assert!(
                layout.place(spec.device, &mid(id), *origin),
                "failed to place {id}"
            );
        }
    }
    layout
}

/// The arrangement this project was built for: two single-monitor machines on
/// top, one two-monitor machine beneath, the top row centred so the vertical
/// seams line up. Expressed purely as data.
pub fn stacked_desks(overflow: EdgeOverflow) -> Layout {
    build(
        overflow,
        &[
            Spec {
                device: dev(1),
                name: "WIN-STUDY",
                monitors: vec![("primary", Rect::new(0, 0, 1920, 1080), Point::new(640, 0))],
            },
            Spec {
                device: dev(2),
                name: "WIN-LAPTOP",
                monitors: vec![("primary", Rect::new(0, 0, 1920, 1080), Point::new(2560, 0))],
            },
            Spec {
                device: dev(3),
                name: "ubuntu-box",
                monitors: vec![
                    ("DP-2", Rect::new(0, 0, 2560, 1440), Point::new(0, 1080)),
                    (
                        "DP-0",
                        Rect::new(2560, 0, 2560, 1440),
                        Point::new(2560, 1080),
                    ),
                ],
            },
        ],
    )
}
