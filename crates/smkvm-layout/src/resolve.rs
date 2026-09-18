//! Turning pointer motion into a position on the global virtual desktop.
//!
//! The server owns one authoritative cursor position in global coordinates.
//! Every mouse delta from the physical pointer runs through [`Layout::resolve`],
//! which answers three things at once: where the cursor now is, which monitor
//! that lands on, and whether control just moved to a different machine.
//!
//! Monitors that sit next to each other need no special handling at all — the
//! moved-to point simply falls inside a different rect. Only motion into empty
//! space consults [`EdgeOverflow`].

use crate::geom::{map_point, segment_exit, Dir, Point, Rect};
use crate::model::{Cell, EdgeOverflow, Layout, Located};

/// The outcome of a pointer motion.
#[derive(Debug, Clone, PartialEq)]
pub struct Motion {
    /// The new authoritative position on the global desktop.
    pub global: Point,
    /// Which monitor it landed on, and where in that device's own coordinates.
    pub located: Located,
    /// Whether this motion handed control to a different machine.
    pub crossed_device: bool,
    /// Whether the landing point differs from the raw `from + delta`, because
    /// the delta pointed into a gap or into empty space. An unadjusted motion
    /// is exactly reversible; an adjusted one is not.
    pub adjusted: bool,
}

/// How far `coord` sits outside the half-open span `(start, end)`.
fn perp_dist((start, end): (i32, i32), coord: i32) -> i64 {
    if coord < start {
        (start - coord) as i64
    } else if coord >= end {
        (coord - (end - 1)) as i64
    } else {
        0
    }
}

/// Is `other` on the far side of `cur`'s edge in direction `dir`?
fn is_beyond(cur: &Cell, other: &Cell, dir: Dir) -> bool {
    is_beyond_rect(&cur.global, &other.global, dir)
}

fn is_beyond_rect(cur: &Rect, other: &Rect, dir: Dir) -> bool {
    match dir {
        Dir::Up => other.bottom() <= cur.top(),
        Dir::Down => other.top() >= cur.bottom(),
        Dir::Left => other.right() <= cur.left(),
        Dir::Right => other.left() >= cur.right(),
    }
}

/// Gap between `cur`'s edge and `other`, along the axis of travel.
fn along_gap(cur: &Cell, other: &Cell, dir: Dir) -> i64 {
    let g = match dir {
        Dir::Up => cur.global.top() - other.global.bottom(),
        Dir::Down => other.global.top() - cur.global.bottom(),
        Dir::Left => cur.global.left() - other.global.right(),
        Dir::Right => other.global.left() - cur.global.right(),
    };
    g as i64
}

/// The point at which the cursor enters `cell` travelling in `dir`, arriving at
/// perpendicular coordinate `coord`.
fn entry_point(cell: &Cell, coord: i32, dir: Dir) -> Point {
    let p = match dir {
        Dir::Up => Point::new(coord, cell.global.bottom() - 1),
        Dir::Down => Point::new(coord, cell.global.top()),
        Dir::Left => Point::new(cell.global.right() - 1, coord),
        Dir::Right => Point::new(cell.global.left(), coord),
    };
    cell.global.clamp_point(p)
}

fn nearest_index(cells: &[Cell], p: Point) -> Option<usize> {
    cells
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            a.global
                .dist2(p)
                .partial_cmp(&b.global.dist2(p))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
}

impl Layout {
    /// Which monitor a global point falls on, and where in that device's own
    /// coordinate space.
    pub fn locate(&self, global: Point) -> Option<Located> {
        let cells = self.cells();
        let cell = cells.iter().find(|c| c.global.contains(global))?;
        Some(locate_in(cell, global))
    }

    /// Snap a global point onto the nearest monitor. Used when the layout
    /// changes underneath a live cursor.
    pub fn snap(&self, global: Point) -> Option<(Point, Located)> {
        let cells = self.cells();
        if let Some(c) = cells.iter().find(|c| c.global.contains(global)) {
            return Some((global, locate_in(c, global)));
        }
        let idx = nearest_index(&cells, global)?;
        let p = cells[idx].global.clamp_point(global);
        Some((p, locate_in(&cells[idx], p)))
    }

    /// Advance the cursor from `from` by `(dx, dy)`.
    ///
    /// Returns `None` only when no monitor is placed and online, in which case
    /// there is nowhere for a cursor to be.
    pub fn resolve(&self, from: Point, dx: i32, dy: i32) -> Option<Motion> {
        let cells = self.cells();
        if cells.is_empty() {
            return None;
        }
        let cand = from.offset(dx, dy);
        let origin_device = cells
            .iter()
            .find(|c| c.global.contains(from))
            .map(|c| c.device);

        // The common case: the new point is on some monitor. Adjacent monitors,
        // whether on this machine or another, fall out of this with no edge
        // logic at all.
        if let Some(cell) = cells.iter().find(|c| c.global.contains(cand)) {
            return Some(finish(cell, cand, cand, origin_device));
        }

        // A screen that is configured but not here leaves a hole. Walking into
        // one stops at its edge, because the alternative -- sliding on to
        // whatever machine is nearest -- sends the cursor somewhere the person
        // was not heading.
        let into_a_hole = self.reserved().iter().any(|r| r.contains(cand));

        // The cursor is heading into empty space. Work out which edge of the
        // current monitor it left through.
        let cur_idx = cells
            .iter()
            .position(|c| c.global.contains(from))
            .or_else(|| nearest_index(&cells, from))?;
        let cur = &cells[cur_idx];
        let start = cur.global.clamp_point(from);

        let Some(exit) = segment_exit(&cur.global, start, cand) else {
            // No motion, or `from` was already adrift: just settle in place.
            let p = cur.global.clamp_point(cand);
            return Some(finish(cur, p, cand, origin_device));
        };
        let dir = exit.dir;
        let coord = if dir.is_horizontal() {
            exit.at.y
        } else {
            exit.at.x
        };

        let beyond: Vec<&Cell> = cells
            .iter()
            .enumerate()
            .filter(|(i, c)| *i != cur_idx && is_beyond(cur, c, dir))
            .map(|(_, c)| c)
            .collect();

        // Prefer a monitor the exit point actually lines up with: crossing an
        // edge should land at the same perpendicular coordinate.
        let aligned = beyond
            .iter()
            .filter(|c| perp_dist(c.global.perp_span(dir), coord) == 0)
            .min_by_key(|c| along_gap(cur, c, dir));
        if let Some(cell) = aligned {
            let p = entry_point(cell, coord, dir);
            return Some(finish(cell, p, cand, origin_device));
        }

        if into_a_hole {
            let p = cur.global.clamp_point(cand);
            return Some(finish(cur, p, cand, origin_device));
        }

        match self.edge_overflow {
            EdgeOverflow::Block => {
                let p = cur.global.clamp_point(cand);
                Some(finish(cur, p, cand, origin_device))
            }
            EdgeOverflow::Clamp => {
                // Nothing lines up, so slide onto whichever monitor in this
                // direction is closest to the exit point. This is what keeps
                // the outer stretches of a wide screen from being dead ends.
                let nearest_hole = self
                    .reserved()
                    .iter()
                    .filter(|r| is_beyond_rect(&cur.global, r, dir))
                    .map(|r| perp_dist(r.perp_span(dir), coord))
                    .min();
                let target = beyond.iter().min_by_key(|c| {
                    (
                        perp_dist(c.global.perp_span(dir), coord),
                        along_gap(cur, c, dir),
                    )
                });
                // A hole nearer than any live screen is the one in the way.
                if let (Some(hole), Some(cell)) = (nearest_hole, target) {
                    if hole < perp_dist(cell.global.perp_span(dir), coord) {
                        let p = cur.global.clamp_point(cand);
                        return Some(finish(cur, p, cand, origin_device));
                    }
                }
                match target {
                    Some(cell) => {
                        let (s, e) = cell.global.perp_span(dir);
                        let p = entry_point(cell, coord.clamp(s, e - 1), dir);
                        Some(finish(cell, p, cand, origin_device))
                    }
                    None => {
                        let p = cur.global.clamp_point(cand);
                        Some(finish(cur, p, cand, origin_device))
                    }
                }
            }
        }
    }
}

fn locate_in(cell: &Cell, global: Point) -> Located {
    Located {
        device: cell.device,
        monitor: cell.monitor.clone(),
        local: map_point(&cell.global, &cell.local, global),
    }
}

fn finish(
    cell: &Cell,
    global: Point,
    cand: Point,
    origin_device: Option<crate::model::DeviceId>,
) -> Motion {
    Motion {
        global,
        located: locate_in(cell, global),
        crossed_device: origin_device.is_some_and(|d| d != cell.device),
        adjusted: global != cand,
    }
}
