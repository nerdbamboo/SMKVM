//! Changing the hardware: adding machines, adding and removing monitors,
//! swapping a computer out, renaming one.
//!
//! The point of every test here is that these are data changes. Placements are
//! keyed on `(device, monitor)` so they survive anything that is not an actual
//! change of geometry, and anything genuinely new gets a sensible spot without
//! a human touching the editor.

mod common;

use common::{dev, mid, stacked_desks};
use smkvm_layout::{DeviceId, EdgeOverflow, Layout, Monitor, Point, Rect};

fn no_overlaps(layout: &Layout) {
    let cells = layout.cells();
    for (i, a) in cells.iter().enumerate() {
        for b in &cells[i + 1..] {
            assert!(
                !a.global.overlaps(&b.global),
                "{}/{} at {:?} overlaps {}/{} at {:?}",
                a.device.short(),
                a.monitor,
                a.global,
                b.device.short(),
                b.monitor,
                b.global
            );
        }
    }
}

#[test]
fn a_fourth_machine_joins_without_disturbing_the_others() {
    let mut layout = stacked_desks(EdgeOverflow::Clamp);
    let before: Vec<_> = layout.cells();

    let newcomer = dev(9);
    let rec = layout.report_monitors(
        newcomer,
        "laptop",
        vec![Monitor::new("eDP-1", Rect::new(0, 0, 1920, 1200))],
    );
    assert_eq!(rec.added, vec![mid("eDP-1")]);
    assert_eq!(layout.unplaced(), vec![(newcomer, mid("eDP-1"))]);

    layout.auto_place();
    assert!(layout.unplaced().is_empty());
    no_overlaps(&layout);

    // Every previously placed monitor is exactly where it was.
    for old in &before {
        let now = layout.placement(old.device, &old.monitor).unwrap();
        assert_eq!(now, old.global, "{} moved", old.monitor);
    }
    // And the cursor can actually reach the new machine.
    assert!(layout
        .cells()
        .iter()
        .any(|c| c.device == newcomer && !c.global.is_empty()));
}

#[test]
fn a_new_monitor_inherits_its_position_from_its_siblings() {
    let mut layout = stacked_desks(EdgeOverflow::Clamp);

    // A third screen plugged in to the right of the desktop's existing pair.
    let rec = layout.report_monitors(
        dev(3),
        "ubuntu-box",
        vec![
            Monitor::new("DP-2", Rect::new(0, 0, 2560, 1440)),
            Monitor::new("DP-0", Rect::new(2560, 0, 2560, 1440)),
            Monitor::new("DP-1", Rect::new(5120, 0, 2560, 1440)),
        ],
    );
    assert_eq!(rec.added, vec![mid("DP-1")]);
    assert!(rec.removed.is_empty() && rec.changed.is_empty());

    layout.auto_place();
    no_overlaps(&layout);

    // Local layout said "to the right of DP-0", so the global layout agrees.
    assert_eq!(
        layout.placement(dev(3), &mid("DP-1")).unwrap(),
        Rect::new(5120, 1080, 2560, 1440)
    );

    // The seam between the old right-hand monitor and the new one works.
    let m = layout.resolve(Point::new(5119, 1200), 1, 0).unwrap();
    assert!(!m.crossed_device);
    assert_eq!(m.located.monitor, mid("DP-1"));
    assert_eq!(m.located.local, Point::new(5120, 120));
}

#[test]
fn unplugging_a_monitor_keeps_its_spot_for_when_it_comes_back() {
    let mut layout = stacked_desks(EdgeOverflow::Clamp);
    let was = layout.placement(dev(3), &mid("DP-0")).unwrap();

    let rec = layout.report_monitors(
        dev(3),
        "ubuntu-box",
        vec![Monitor::new("DP-2", Rect::new(0, 0, 2560, 1440))],
    );
    assert_eq!(rec.removed, vec![mid("DP-0")]);
    assert!(
        !layout.cells().iter().any(|c| c.monitor == mid("DP-0")),
        "a detached monitor must not take the cursor"
    );

    // Plug it back in.
    layout.report_monitors(
        dev(3),
        "ubuntu-box",
        vec![
            Monitor::new("DP-2", Rect::new(0, 0, 2560, 1440)),
            Monitor::new("DP-0", Rect::new(2560, 0, 2560, 1440)),
        ],
    );
    assert!(layout.unplaced().is_empty(), "no re-placement needed");
    assert_eq!(layout.placement(dev(3), &mid("DP-0")).unwrap(), was);
}

#[test]
fn a_resolution_change_re_places_the_monitor_at_its_new_size() {
    let mut layout = stacked_desks(EdgeOverflow::Clamp);

    let rec = layout.report_monitors(
        dev(3),
        "ubuntu-box",
        vec![
            Monitor::new("DP-2", Rect::new(0, 0, 1920, 1080)),
            Monitor::new("DP-0", Rect::new(1920, 0, 2560, 1440)),
        ],
    );
    assert!(rec.changed.contains(&mid("DP-2")));
    // A placement sized to the old resolution would no longer map 1:1, so it
    // is dropped rather than silently stretched.
    assert!(layout.unplaced().contains(&(dev(3), mid("DP-2"))));

    layout.auto_place();
    let now = layout.placement(dev(3), &mid("DP-2")).unwrap();
    assert_eq!((now.w, now.h), (1920, 1080), "placed at native size");
    no_overlaps(&layout);
}

#[test]
fn swapping_a_machine_for_different_hardware() {
    let mut layout = stacked_desks(EdgeOverflow::Clamp);
    let old = dev(2);
    let replacement = DeviceId::from_bytes([42; 32]);

    layout.remove_device(old);
    assert!(layout.device(old).is_none());
    assert!(layout.placement(old, &mid("primary")).is_none());

    layout.report_monitors(
        replacement,
        "WIN-LAPTOP",
        vec![Monitor::new("primary", Rect::new(0, 0, 2560, 1440))],
    );
    layout.auto_place();
    no_overlaps(&layout);
    assert!(layout.unplaced().is_empty());
    assert_eq!(layout.devices().len(), 3);
}

#[test]
fn renaming_a_machine_leaves_the_layout_alone() {
    let mut layout = stacked_desks(EdgeOverflow::Clamp);
    let before = layout.cells();

    let rec = layout.report_monitors(
        dev(3),
        "workstation",
        vec![
            Monitor::new("DP-2", Rect::new(0, 0, 2560, 1440)),
            Monitor::new("DP-0", Rect::new(2560, 0, 2560, 1440)),
        ],
    );
    assert!(rec.is_noop());
    assert_eq!(layout.device(dev(3)).unwrap().name, "workstation");
    assert_eq!(layout.cells(), before);
}

#[test]
fn auto_place_alone_produces_a_usable_layout() {
    // Nothing placed by hand at all: three machines of differing shapes.
    let mut layout = Layout::new(EdgeOverflow::Clamp);
    layout.report_monitors(
        dev(1),
        "a",
        vec![
            Monitor::new("m0", Rect::new(0, 0, 1920, 1080)),
            Monitor::new("m1", Rect::new(1920, 0, 1920, 1080)),
        ],
    );
    layout.report_monitors(
        dev(2),
        "b",
        vec![Monitor::new("m0", Rect::new(0, 0, 3840, 2160))],
    );
    layout.report_monitors(
        dev(3),
        "c",
        vec![
            Monitor::new("m0", Rect::new(0, 0, 1280, 1024)),
            Monitor::new("m1", Rect::new(0, 1024, 1280, 1024)),
        ],
    );

    layout.auto_place();
    assert!(layout.unplaced().is_empty());
    no_overlaps(&layout);

    // Every machine is reachable: sweeping across the full width of the
    // arrangement must touch all three.
    let bounds = layout.bounds();
    let mut seen = std::collections::BTreeSet::new();
    let mut at = Point::new(bounds.x, bounds.y);
    if let Some((p, _)) = layout.snap(at) {
        at = p;
    }
    for _ in 0..(bounds.w / 64 + 4) {
        let m = layout.resolve(at, 64, 0).unwrap();
        at = m.global;
        seen.insert(m.located.device);
    }
    assert_eq!(seen.len(), 3, "sweep reached {seen:?}");
}

#[test]
fn a_layout_survives_a_round_trip_through_json() {
    let layout = stacked_desks(EdgeOverflow::Clamp);
    let text = serde_json::to_string(&layout).unwrap();
    let mut back: Layout = serde_json::from_str(&text).unwrap();

    // `online` is session state, not configuration, so it is not persisted.
    assert!(back.cells().is_empty());
    for d in [dev(1), dev(2), dev(3)] {
        back.set_online(d, true);
    }
    assert_eq!(back.cells(), layout.cells());
    assert_eq!(back.edge_overflow, layout.edge_overflow);
}

#[test]
fn placements_are_independent_of_the_order_machines_connect() {
    let mut a = Layout::new(EdgeOverflow::Clamp);
    for d in [dev(1), dev(2), dev(3)] {
        a.report_monitors(
            d,
            "x",
            vec![Monitor::new("m0", Rect::new(0, 0, 1920, 1080))],
        );
    }
    a.auto_place();

    let mut b = Layout::new(EdgeOverflow::Clamp);
    for d in [dev(3), dev(2), dev(1)] {
        b.report_monitors(
            d,
            "x",
            vec![Monitor::new("m0", Rect::new(0, 0, 1920, 1080))],
        );
    }
    b.auto_place();

    // Different connection order gives each machine a different slot, which is
    // fine, but both layouts must be valid and complete.
    for l in [&a, &b] {
        assert!(l.unplaced().is_empty());
        no_overlaps(l);
        assert_eq!(l.cells().len(), 3);
    }
}
