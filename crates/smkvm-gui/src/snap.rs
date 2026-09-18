//! Pulling a screen onto the seams around it.
//!
//! This is not tidiness. The server hands the cursor across an edge to a
//! monitor whose span the exit point falls inside; where it falls inside
//! nothing it slides to whichever screen is nearest, or stops dead. So a seam
//! three pixels out crosses one way along most of its length and another way
//! along the rest, which is the sort of fault nobody can describe and everybody
//! notices.
//!
//! Overlap is refused for the same reason. Resolution takes the first rect
//! containing the point, so two placements on top of each other leave part of
//! one monitor with no way to reach it, silently.

use smkvm_layout::{Point, Rect};

/// What kind of pull an edge exerts.
///
/// A seam decides how the cursor crosses. A shared outer edge only looks tidy,
/// so at the same distance it gives way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Pull {
    Seam,
    Aligned,
}

/// Pull `dragged` onto the edges of everything around it.
///
/// `within` is a distance in desktop pixels; `others` must not include the
/// dragged screen itself, which would hold it exactly where it is.
pub fn snap(dragged: Rect, others: &[Rect], within: i32) -> Point {
    let dx = along(
        (dragged.left(), dragged.right()),
        others.iter().map(|o| (o.left(), o.right())),
        within,
    );
    let dy = along(
        (dragged.top(), dragged.bottom()),
        others.iter().map(|o| (o.top(), o.bottom())),
        within,
    );
    Point::new(dragged.x.saturating_add(dx), dragged.y.saturating_add(dy))
}

/// Where a dragged screen comes to rest: snapped onto the seams around it, or
/// back where it started if it was dropped on top of a neighbour.
///
/// The pull is never what causes that. Every candidate is at most as far as
/// the gap it is closing, so the nearest one can only bring a screen up flush
/// against a neighbour and never through it -- which also means a screen
/// dropped a few pixels inside one is pulled back out rather than refused.
/// Only a screen dropped well inside another has nowhere to go, and then it
/// returns to `was`, which is clear because it is where the screen already sat.
pub fn settle(dragged: Rect, others: &[Rect], within: i32, was: Point) -> Point {
    let snapped = snap(dragged, others, within);
    if is_clear(at(snapped, dragged), others) {
        snapped
    } else {
        was
    }
}

/// Whether a screen placed here would sit on top of another.
pub fn is_clear(placed: Rect, others: &[Rect]) -> bool {
    !others.iter().any(|o| o.overlaps(&placed))
}

/// The screen moved to a different origin, keeping its size.
pub fn at(origin: Point, screen: Rect) -> Rect {
    Rect::new(origin.x, origin.y, screen.w, screen.h)
}

/// The best correction along one axis, comparing a span against every other.
///
/// Worked in i64 so a desktop placed somewhere absurd subtracts rather than
/// wraps; anything that survives the threshold is small enough to fit back.
fn along((lo, hi): (i32, i32), others: impl Iterator<Item = (i32, i32)>, within: i32) -> i32 {
    let (lo, hi) = (i64::from(lo), i64::from(hi));
    let within = i64::from(within.max(0));
    let mut best: Option<(i64, Pull, i64)> = None;
    for (olo, ohi) in others {
        let (olo, ohi) = (i64::from(olo), i64::from(ohi));
        for (delta, pull) in [
            (ohi - lo, Pull::Seam),
            (olo - hi, Pull::Seam),
            (olo - lo, Pull::Aligned),
            (ohi - hi, Pull::Aligned),
        ] {
            let off = delta.abs();
            if off > within {
                continue;
            }
            if best.is_none_or(|(was_off, was_pull, _)| (off, pull) < (was_off, was_pull)) {
                best = Some((off, pull, delta));
            }
        }
    }
    best.map_or(0, |(_, _, delta)| delta as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A screen the size of a common monitor, placed at an origin.
    fn screen(x: i32, y: i32) -> Rect {
        Rect::new(x, y, 1920, 1080)
    }

    #[test]
    fn a_seam_closes_from_outside() {
        let neighbour = screen(0, 0);
        let dragged = screen(1925, 0);
        assert_eq!(
            snap(dragged, &[neighbour], 8),
            Point::new(1920, 0),
            "a five pixel gap should close"
        );
    }

    #[test]
    fn a_seam_closes_from_inside() {
        let neighbour = screen(0, 0);
        let dragged = screen(1915, 0);
        assert_eq!(snap(dragged, &[neighbour], 8), Point::new(1920, 0));
    }

    #[test]
    fn a_seam_closes_on_every_side() {
        let neighbour = screen(0, 0);
        for (dragged, expected) in [
            (screen(-1925, 0), Point::new(-1920, 0)),
            (screen(1926, 0), Point::new(1920, 0)),
            (screen(0, -1084), Point::new(0, -1080)),
            (screen(0, 1083), Point::new(0, 1080)),
        ] {
            assert_eq!(snap(dragged, &[neighbour], 8), expected, "{dragged:?}");
        }
    }

    #[test]
    fn outer_edges_line_up() {
        // Stacked, so the pull that matters is the one lining up their left
        // edges rather than any seam.
        let neighbour = screen(0, 0);
        let dragged = screen(4, 1080);
        assert_eq!(snap(dragged, &[neighbour], 8), Point::new(0, 1080));
    }

    #[test]
    fn the_axes_are_decided_apart() {
        // Flush on x against one neighbour, lined up on y with another.
        let right_of = screen(0, 0);
        let above = Rect::new(4000, 1085, 1000, 1000);
        let dragged = screen(1923, 1083);
        assert_eq!(snap(dragged, &[right_of, above], 8), Point::new(1920, 1085));
    }

    #[test]
    fn nothing_out_of_reach_pulls() {
        let neighbour = screen(0, 0);
        let dragged = screen(1940, 30);
        assert_eq!(snap(dragged, &[neighbour], 8), dragged.origin());
    }

    #[test]
    fn the_threshold_is_inclusive() {
        let neighbour = screen(0, 0);
        assert_eq!(snap(screen(1928, 0), &[neighbour], 8).x, 1920);
        assert_eq!(snap(screen(1929, 0), &[neighbour], 8).x, 1929);
    }

    #[test]
    fn a_threshold_of_nothing_pulls_nothing() {
        let neighbour = screen(0, 0);
        let dragged = screen(1921, 3);
        assert_eq!(snap(dragged, &[neighbour], 0), dragged.origin());
    }

    #[test]
    fn the_nearer_edge_wins() {
        let near = screen(0, 0);
        let far = screen(1927, 4000);
        // Seam with `near` is two pixels away, lining up with `far` is seven.
        let dragged = screen(1922, 2000);
        assert_eq!(snap(dragged, &[near, far], 8).x, 1920);
    }

    #[test]
    fn a_seam_beats_a_shared_edge_at_the_same_distance() {
        let dragged = Rect::new(100, 0, 50, 50);
        let shares_an_edge = Rect::new(95, 4000, 105, 50);
        let offers_a_seam = Rect::new(155, 4000, 45, 50);
        assert_eq!(
            snap(dragged, &[shares_an_edge, offers_a_seam], 8).x,
            105,
            "both pull five pixels, and the seam is the one that changes crossing"
        );
    }

    #[test]
    fn a_screen_with_the_desk_to_itself_stays_put() {
        let dragged = screen(347, -22);
        assert_eq!(snap(dragged, &[], 8), dragged.origin());
    }

    #[test]
    fn settle_takes_a_free_drop() {
        let neighbour = screen(0, 0);
        let dragged = screen(1924, 6);
        assert_eq!(
            settle(dragged, &[neighbour], 8, Point::new(9000, 0)),
            Point::new(1920, 0)
        );
    }

    #[test]
    fn a_screen_dropped_just_inside_one_is_pulled_back_out() {
        let neighbour = screen(0, 0);
        let dragged = screen(1915, 0);
        assert_eq!(
            settle(dragged, &[neighbour], 8, Point::new(9000, 0)),
            Point::new(1920, 0),
            "a near miss is what snapping is for, not something to refuse"
        );
    }

    #[test]
    fn settle_puts_back_a_screen_dropped_on_top_of_another() {
        let neighbour = screen(0, 0);
        let was = Point::new(0, 2000);
        let dragged = screen(400, 300);
        assert_eq!(settle(dragged, &[neighbour], 8, was), was);
    }

    #[test]
    fn a_hole_left_by_an_absent_machine_is_as_solid_as_a_screen() {
        // Reserved space is passed in alongside live screens on purpose: the
        // server treats it as a wall, so the editor must not let a screen be
        // dropped into it.
        let reserved = screen(1920, 0);
        let was = Point::new(0, 0);
        assert_eq!(settle(screen(2200, 40), &[reserved], 8, was), was);
        assert_eq!(snap(screen(3835, 0), &[reserved], 8), Point::new(3840, 0));
    }

    proptest! {
        #[test]
        fn a_screen_never_comes_to_rest_on_top_of_another(
            cells in prop::collection::vec((0i32..6, 0i32..4), 1..8),
            x in -400i32..1600,
            y in -400i32..1200,
            within in 0i32..40,
        ) {
            // Neighbours on a coarse grid, so they never overlap each other
            // and somewhere to fall back to always exists.
            let mut others: Vec<Rect> = Vec::new();
            for (cx, cy) in cells {
                let r = Rect::new(cx * 200, cy * 200, 180, 180);
                if !others.contains(&r) {
                    others.push(r);
                }
            }
            let parked = Point::new(-5000, -5000);
            let dragged = Rect::new(x, y, 180, 180);
            let rested = settle(dragged, &others, within, parked);
            prop_assert!(is_clear(at(rested, dragged), &others));

            // No candidate reaches further than the gap it closes, so the pull
            // can bring a screen up flush against a neighbour but never
            // through one. This is why a drop that was clear is never refused.
            if is_clear(dragged, &others) {
                prop_assert_eq!(rested, snap(dragged, &others, within));
            }
        }
    }
}
