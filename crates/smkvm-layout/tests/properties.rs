//! Properties that must hold for every layout, not just the ones we thought to
//! write down.
//!
//! Cursor resolution is the piece where a subtle error shows up as the pointer
//! vanishing or sticking, and those bugs are miserable to reproduce by hand.
//! So the invariants are checked against generated arrangements instead.

mod common;

use common::dev;
use proptest::prelude::*;
use smkvm_layout::{EdgeOverflow, Layout, Monitor, Point, Rect};

/// One machine's monitors, tiled left to right in its own coordinate space,
/// the way a real desktop arrangement reports them.
fn device_monitors() -> impl Strategy<Value = Vec<Rect>> {
    prop::collection::vec((320i32..=3840, 240i32..=2160), 1..=3).prop_map(|sizes| {
        let mut x = 0;
        sizes
            .into_iter()
            .map(|(w, h)| {
                let r = Rect::new(x, 0, w, h);
                x += w;
                r
            })
            .collect()
    })
}

/// A whole layout, placed the way the server places one when nobody has opened
/// the editor.
fn layout_with(overflow: EdgeOverflow) -> impl Strategy<Value = Layout> {
    prop::collection::vec(device_monitors(), 1..=4).prop_map(move |devices| {
        let mut layout = Layout::new(overflow);
        for (i, monitors) in devices.into_iter().enumerate() {
            let monitors = monitors
                .into_iter()
                .enumerate()
                .map(|(j, r)| Monitor::new(format!("m{j}"), r))
                .collect();
            layout.report_monitors(dev(i as u8 + 1), format!("machine-{i}"), monitors);
        }
        layout.auto_place();
        layout
    })
}

fn any_layout() -> impl Strategy<Value = Layout> {
    prop_oneof![
        layout_with(EdgeOverflow::Clamp),
        layout_with(EdgeOverflow::Block),
    ]
}

/// Pick a point that really is on one of the layout's monitors.
fn point_on(layout: &Layout, i: usize, fx: f64, fy: f64) -> Point {
    let cells = layout.cells();
    let c = &cells[i % cells.len()];
    Point::new(
        c.global.x + (fx * (c.global.w - 1) as f64) as i32,
        c.global.y + (fy * (c.global.h - 1) as f64) as i32,
    )
}

/// A layout together with a point that is genuinely on one of its monitors.
fn layout_and_point() -> impl Strategy<Value = (Layout, Point)> {
    any_layout().prop_flat_map(|layout| {
        (
            Just(layout),
            any::<prop::sample::Index>(),
            0f64..1.0,
            0f64..1.0,
        )
            .prop_map(|(layout, i, fx, fy)| {
                let p = point_on(&layout, i.index(usize::MAX / 2), fx, fy);
                (layout, p)
            })
    })
}

/// A layout and two points on it. The delta between them is by construction a
/// move that has somewhere real to land, which is what the reversibility
/// property needs — filtering for it instead would throw away almost every
/// generated case.
fn layout_and_two_points() -> impl Strategy<Value = (Layout, Point, Point)> {
    any_layout().prop_flat_map(|layout| {
        (
            Just(layout),
            any::<prop::sample::Index>(),
            0f64..1.0,
            0f64..1.0,
            any::<prop::sample::Index>(),
            0f64..1.0,
            0f64..1.0,
        )
            .prop_map(|(layout, i, fx, fy, j, gx, gy)| {
                let a = point_on(&layout, i.index(usize::MAX / 2), fx, fy);
                let b = point_on(&layout, j.index(usize::MAX / 2), gx, gy);
                (layout, a, b)
            })
    })
}

proptest! {
    /// Automatic placement must never leave two monitors fighting over the
    /// same pixels, or the cursor's location becomes ambiguous.
    #[test]
    fn auto_placement_never_overlaps(layout in any_layout()) {
        let cells = layout.cells();
        prop_assert!(!cells.is_empty());
        for (i, a) in cells.iter().enumerate() {
            for b in &cells[i + 1..] {
                prop_assert!(
                    !a.global.overlaps(&b.global),
                    "{:?} overlaps {:?}", a.global, b.global
                );
            }
        }
    }

    /// Every monitor gets a spot. An unplaced monitor is one the cursor can
    /// never reach.
    #[test]
    fn auto_placement_leaves_nothing_behind(layout in any_layout()) {
        prop_assert!(layout.unplaced().is_empty());
    }

    /// Whatever the delta, the cursor ends up somewhere real. This is the
    /// invariant that stops a pointer from disappearing into a gap.
    #[test]
    fn the_cursor_always_lands_on_a_monitor(
        (layout, from) in layout_and_point(),
        dx in -10_000i32..=10_000,
        dy in -10_000i32..=10_000,
    ) {
        let m = layout.resolve(from, dx, dy).unwrap();
        prop_assert!(
            layout.locate(m.global).is_some(),
            "({dx},{dy}) from {from:?} ended at {:?}, off every monitor", m.global
        );
        // And the reported landing agrees with an independent lookup.
        prop_assert_eq!(layout.locate(m.global).unwrap(), m.located);
    }

    /// Motion that did not have to be adjusted is exactly undoable. Without
    /// this, nudging the pointer over a seam and back would drift.
    #[test]
    fn unadjusted_motion_is_reversible((layout, from, to) in layout_and_two_points()) {
        let (dx, dy) = (to.x - from.x, to.y - from.y);
        let out = layout.resolve(from, dx, dy).unwrap();
        prop_assert!(!out.adjusted, "a move onto a real monitor needs no adjusting");
        prop_assert_eq!(out.global, to);

        let back = layout.resolve(out.global, -dx, -dy).unwrap();
        prop_assert!(!back.adjusted);
        prop_assert_eq!(back.global, from);
    }

    /// A monitor placed at its native size maps global to local and back with
    /// no drift, which is what makes crossing a seam pixel-accurate.
    #[test]
    fn global_and_local_coordinates_agree((layout, p) in layout_and_point()) {
        let located = layout.locate(p).unwrap();
        let cell = layout
            .cells()
            .into_iter()
            .find(|c| c.device == located.device && c.monitor == located.monitor)
            .unwrap();
        prop_assert_eq!((cell.global.w, cell.global.h), (cell.local.w, cell.local.h));
        let round = smkvm_layout::map_point(&cell.local, &cell.global, located.local);
        prop_assert_eq!(round, p);
    }

    /// No global point may belong to two monitors at once.
    #[test]
    fn a_point_belongs_to_exactly_one_monitor((layout, p) in layout_and_point()) {
        let hits = layout.cells().iter().filter(|c| c.global.contains(p)).count();
        prop_assert_eq!(hits, 1);
    }

    /// In blocking mode the cursor may never travel against the direction it
    /// was pushed. A backwards jump is how a "stuck at the edge" bug shows up:
    /// the pointer snaps somewhere behind where it started.
    #[test]
    fn block_mode_motion_is_monotonic(
        layout in layout_with(EdgeOverflow::Block),
        i in any::<prop::sample::Index>(),
        fx in 0f64..1.0,
        fy in 0f64..1.0,
        dx in -4_000i32..=4_000,
        dy in -4_000i32..=4_000,
    ) {
        let from = point_on(&layout, i.index(usize::MAX / 2), fx, fy);
        let m = layout.resolve(from, dx, dy).unwrap();
        if dx <= 0 {
            prop_assert!(m.global.x <= from.x, "moved right on a leftward push");
        }
        if dx >= 0 {
            prop_assert!(m.global.x >= from.x, "moved left on a rightward push");
        }
        if dy <= 0 {
            prop_assert!(m.global.y <= from.y, "moved down on an upward push");
        }
        if dy >= 0 {
            prop_assert!(m.global.y >= from.y, "moved up on a downward push");
        }
    }

    /// In blocking mode a machine change may only happen between monitors that
    /// genuinely face each other. Sliding diagonally onto a machine that is
    /// merely nearby is what clamping mode is for.
    #[test]
    fn block_mode_only_crosses_between_facing_monitors(
        layout in layout_with(EdgeOverflow::Block),
        i in any::<prop::sample::Index>(),
        fx in 0f64..1.0,
        fy in 0f64..1.0,
        dx in -4_000i32..=4_000,
        dy in -4_000i32..=4_000,
    ) {
        let from = point_on(&layout, i.index(usize::MAX / 2), fx, fy);
        let src = layout.locate(from).unwrap();
        let m = layout.resolve(from, dx, dy).unwrap();
        prop_assume!(m.located.monitor != src.monitor || m.located.device != src.device);

        let cells = layout.cells();
        let find = |l: &smkvm_layout::Located| {
            cells
                .iter()
                .find(|c| c.device == l.device && c.monitor == l.monitor)
                .unwrap()
                .global
        };
        let (a, b) = (find(&src), find(&m.located));
        let x_overlap = a.left() < b.right() && b.left() < a.right();
        let y_overlap = a.top() < b.bottom() && b.top() < a.bottom();
        prop_assert!(
            x_overlap || y_overlap,
            "jumped from {a:?} to {b:?}, which do not face each other"
        );
    }
}
