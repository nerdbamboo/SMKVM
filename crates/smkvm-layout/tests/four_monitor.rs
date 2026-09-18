//! Cursor behaviour on a real multi-monitor arrangement.
//!
//! These exercise the case Barrier's one-rectangle-per-machine model cannot
//! express: a wide two-monitor desktop sitting beneath two narrower machines.

mod common;

use common::{dev, mid, stacked_desks};
use smkvm_layout::{EdgeOverflow, Point};

#[test]
fn crossing_up_keeps_the_horizontal_position() {
    let layout = stacked_desks(EdgeOverflow::Clamp);

    // Straight up out of the left-hand desktop monitor.
    let m = layout.resolve(Point::new(1500, 1085), 0, -10).unwrap();
    assert!(m.crossed_device);
    assert_eq!(m.located.device, dev(1));
    assert_eq!(m.global, Point::new(1500, 1075));
    // 1500 global is 860 into a machine whose left edge sits at global 640.
    assert_eq!(m.located.local, Point::new(860, 1075));
    assert!(!m.adjusted);
}

#[test]
fn the_vertical_seam_is_continuous() {
    let layout = stacked_desks(EdgeOverflow::Clamp);

    // The last pixel of the lower-left monitor and the first pixel of the
    // lower-right one must lead to adjacent pixels on different machines.
    let left = layout.resolve(Point::new(2559, 1085), 0, -10).unwrap();
    let right = layout.resolve(Point::new(2560, 1085), 0, -10).unwrap();

    assert_eq!(left.located.device, dev(1));
    assert_eq!(left.located.local, Point::new(1919, 1075));
    assert_eq!(right.located.device, dev(2));
    assert_eq!(right.located.local, Point::new(0, 1075));
}

#[test]
fn crossing_down_lands_on_the_matching_monitor() {
    let layout = stacked_desks(EdgeOverflow::Clamp);

    // Down from the right-hand machine arrives on the right-hand monitor of
    // the desktop, at that monitor's own local coordinates.
    let m = layout.resolve(Point::new(3000, 1075), 0, 10).unwrap();
    assert_eq!(m.located.device, dev(3));
    assert_eq!(m.located.monitor, mid("DP-0"));
    assert_eq!(m.located.local, Point::new(3000, 5));
    assert!(!m.adjusted);
}

#[test]
fn moving_between_one_machines_own_monitors_is_not_a_switch() {
    let layout = stacked_desks(EdgeOverflow::Clamp);

    let m = layout.resolve(Point::new(2559, 1200), 1, 0).unwrap();
    assert!(!m.crossed_device, "same machine, different monitor");
    assert_eq!(m.located.device, dev(3));
    assert_eq!(m.located.monitor, mid("DP-0"));
    // The desktop's own coordinate space is continuous across its monitors.
    assert_eq!(m.located.local, Point::new(2560, 120));
}

#[test]
fn outer_stretches_clamp_onto_the_nearest_machine() {
    let layout = stacked_desks(EdgeOverflow::Clamp);

    // Global x below 640 has nothing above it. Clamp mode slides the cursor
    // onto the closest machine in that direction rather than dead-ending.
    let m = layout.resolve(Point::new(300, 1085), 0, -10).unwrap();
    assert_eq!(m.located.device, dev(1));
    assert_eq!(m.located.local, Point::new(0, 1079));
    assert!(m.adjusted);

    // Likewise past the right-hand end.
    let m = layout.resolve(Point::new(4800, 1085), 0, -10).unwrap();
    assert_eq!(m.located.device, dev(2));
    assert_eq!(m.located.local, Point::new(1919, 1079));
    assert!(m.adjusted);
}

#[test]
fn block_mode_stops_at_the_edge_instead() {
    let layout = stacked_desks(EdgeOverflow::Block);

    let m = layout.resolve(Point::new(300, 1085), 0, -10).unwrap();
    assert!(!m.crossed_device);
    assert_eq!(m.global, Point::new(300, 1080));
    assert!(m.adjusted);

    // An aligned crossing still works in block mode.
    let m = layout.resolve(Point::new(1500, 1085), 0, -10).unwrap();
    assert!(m.crossed_device);
    assert_eq!(m.located.device, dev(1));
}

#[test]
fn an_unadjusted_crossing_is_exactly_reversible() {
    let layout = stacked_desks(EdgeOverflow::Clamp);

    let start = Point::new(1500, 1085);
    let out = layout.resolve(start, 0, -10).unwrap();
    assert!(!out.adjusted);
    let back = layout.resolve(out.global, 0, 10).unwrap();
    assert!(!back.adjusted);
    assert_eq!(back.global, start);
    assert_eq!(back.located.device, dev(3));
}

#[test]
fn a_large_delta_still_lands_on_a_monitor() {
    let layout = stacked_desks(EdgeOverflow::Clamp);

    for (dx, dy) in [(9999, 0), (-9999, 0), (0, 9999), (0, -9999), (5000, -5000)] {
        let m = layout.resolve(Point::new(1500, 1500), dx, dy).unwrap();
        assert!(
            layout.locate(m.global).is_some(),
            "delta ({dx},{dy}) left the cursor at {:?}, which is on no monitor",
            m.global
        );
    }
}

#[test]
fn an_offline_machine_never_receives_the_cursor() {
    let mut layout = stacked_desks(EdgeOverflow::Clamp);
    layout.set_online(dev(1), false);

    // Straight up would normally land on machine 1; with it offline the
    // cursor slides onto the other machine instead of vanishing.
    let m = layout.resolve(Point::new(1500, 1085), 0, -10).unwrap();
    assert_ne!(m.located.device, dev(1));
    assert!(layout.locate(m.global).is_some());
}

#[test]
fn a_cursor_resting_on_the_very_edge_can_still_cross() {
    // Pushing outward from the last pixel of a screen must cross, not clamp
    // back to where it already is. Getting this wrong pins the cursor at the
    // boundary: it arrives at the edge and then nothing gets it any further.
    //
    // The push has to overshoot every monitor, so that the crossing is decided
    // by working out which edge was left through. A short push lands inside
    // the neighbour directly and never exercises that.
    let layout = stacked_desks(EdgeOverflow::Clamp);

    let edge = Point::new(1500, 1080);
    assert_eq!(
        layout.locate(edge).unwrap().device,
        dev(3),
        "on the top edge"
    );
    let m = layout.resolve(edge, 0, -5000).unwrap();
    assert_eq!(m.located.device, dev(1), "stuck on the top edge");

    // The same going sideways, from the last pixel of the left-hand monitor.
    let seam = Point::new(2559, 1500);
    let m = layout.resolve(seam, 9000, 0).unwrap();
    assert_eq!(m.located.monitor, mid("DP-0"), "stuck on the seam");

    // Repeating the push keeps making progress rather than settling into a
    // position that resolves to itself.
    let mut at = Point::new(1500, 1080);
    let mut seen = vec![at];
    for _ in 0..3 {
        at = layout.resolve(at, 0, -5000).unwrap().global;
        assert!(!seen.contains(&at) || seen.len() > 2, "went in a circle");
        seen.push(at);
    }
    assert_eq!(layout.locate(at).unwrap().device, dev(1));

    // An edge with genuinely nothing beyond it does stay put, which is the
    // behaviour the case above must not be confused with.
    let right = Point::new(5119, 1500);
    let m = layout.resolve(right, 40, 0).unwrap();
    assert_eq!(m.global, right);
    assert!(m.adjusted);
}
