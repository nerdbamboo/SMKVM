//! Integer rectangle geometry.
//!
//! All coordinates are half-open: a rect covers `x .. x + w` horizontally and
//! `y .. y + h` vertically, so `right()` and `bottom()` are one past the last
//! pixel. Keeping that convention everywhere is what makes adjacent monitors
//! tile without a one-pixel seam or overlap.

use serde::{Deserialize, Serialize};

/// A point on the global virtual desktop, or in a device's local coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    pub fn offset(self, dx: i32, dy: i32) -> Self {
        Self {
            x: self.x.saturating_add(dx),
            y: self.y.saturating_add(dy),
        }
    }
}

/// Which way a cursor left a rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    /// The axis this direction moves along. Horizontal directions are resolved
    /// against rectangles' x spans, vertical ones against their y spans.
    pub fn is_horizontal(self) -> bool {
        matches!(self, Dir::Left | Dir::Right)
    }

    pub fn opposite(self) -> Dir {
        match self {
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
        }
    }
}

/// A half-open integer rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn left(&self) -> i32 {
        self.x
    }

    pub const fn top(&self) -> i32 {
        self.y
    }

    /// One past the rightmost pixel.
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }

    /// One past the bottommost pixel.
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub const fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn origin(&self) -> Point {
        Point::new(self.x, self.y)
    }

    pub fn contains(&self, p: Point) -> bool {
        !self.is_empty()
            && p.x >= self.left()
            && p.x < self.right()
            && p.y >= self.top()
            && p.y < self.bottom()
    }

    /// Move `p` to the nearest point inside the rect. Returns `p` unchanged if
    /// it is already inside.
    pub fn clamp_point(&self, p: Point) -> Point {
        debug_assert!(!self.is_empty());
        Point::new(
            p.x.clamp(self.left(), self.right() - 1),
            p.y.clamp(self.top(), self.bottom() - 1),
        )
    }

    /// The rect's extent along the axis perpendicular to `dir`, as a half-open
    /// `(start, end)` pair. Crossing an edge upward, for instance, cares about
    /// which rects overlap in x.
    pub fn perp_span(&self, dir: Dir) -> (i32, i32) {
        if dir.is_horizontal() {
            (self.top(), self.bottom())
        } else {
            (self.left(), self.right())
        }
    }

    /// The rect's extent along the axis parallel to `dir`.
    pub fn along_span(&self, dir: Dir) -> (i32, i32) {
        if dir.is_horizontal() {
            (self.left(), self.right())
        } else {
            (self.top(), self.bottom())
        }
    }

    /// Smallest rect covering both. An empty rect is treated as "nothing".
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.left().min(other.left());
        let y = self.top().min(other.top());
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        Rect::new(x, y, r - x, b - y)
    }

    pub fn overlaps(&self, other: &Rect) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.left() < other.right()
            && other.left() < self.right()
            && self.top() < other.bottom()
            && other.top() < self.bottom()
    }

    /// Squared distance from `p` to the nearest point of this rect, as f64 to
    /// keep large desktops from overflowing i32.
    pub fn dist2(&self, p: Point) -> f64 {
        let dx = if p.x < self.left() {
            (self.left() - p.x) as f64
        } else if p.x >= self.right() {
            (p.x - (self.right() - 1)) as f64
        } else {
            0.0
        };
        let dy = if p.y < self.top() {
            (self.top() - p.y) as f64
        } else if p.y >= self.bottom() {
            (p.y - (self.bottom() - 1)) as f64
        } else {
            0.0
        };
        dx * dx + dy * dy
    }
}

/// Where a segment leaves a rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Exit {
    /// The point on the boundary where the segment crosses out.
    pub at: Point,
    pub dir: Dir,
}

/// Find where the segment `from -> to` leaves `rect`.
///
/// `from` must be inside `rect`. Returns `None` when `to` is also inside (the
/// segment never leaves) or when there is no motion at all.
pub fn segment_exit(rect: &Rect, from: Point, to: Point) -> Option<Exit> {
    if rect.is_empty() || rect.contains(to) {
        return None;
    }
    let dx = (to.x - from.x) as f64;
    let dy = (to.y - from.y) as f64;
    if dx == 0.0 && dy == 0.0 {
        return None;
    }

    // Parameter t in (0, 1] at which the segment first crosses each edge.
    // The smallest such t wins; ties prefer the axis with the larger travel so
    // a diagonal exit through a corner is attributed to the dominant motion.
    let mut best: Option<(f64, Dir)> = None;
    let mut consider = |t: f64, dir: Dir, travel: f64| {
        // Zero counts. A cursor already resting on the last pixel of an edge
        // and pushed further crosses at t = 0, and excluding that would pin it
        // there: every subsequent push would clamp back to where it already
        // is. The direction guards below mean t = 0 can only arise for the
        // edge actually being travelled towards.
        if !(0.0..=1.0).contains(&t) || !t.is_finite() {
            return;
        }
        match best {
            None => best = Some((t, dir)),
            Some((bt, bdir)) => {
                let better = t < bt
                    || (t == bt && {
                        let btravel = if bdir.is_horizontal() {
                            dx.abs()
                        } else {
                            dy.abs()
                        };
                        travel > btravel
                    });
                if better {
                    best = Some((t, dir));
                }
            }
        }
    };

    if dx < 0.0 {
        consider(
            (rect.left() as f64 - from.x as f64) / dx,
            Dir::Left,
            dx.abs(),
        );
    } else if dx > 0.0 {
        consider(
            ((rect.right() - 1) as f64 - from.x as f64) / dx,
            Dir::Right,
            dx.abs(),
        );
    }
    if dy < 0.0 {
        consider((rect.top() as f64 - from.y as f64) / dy, Dir::Up, dy.abs());
    } else if dy > 0.0 {
        consider(
            ((rect.bottom() - 1) as f64 - from.y as f64) / dy,
            Dir::Down,
            dy.abs(),
        );
    }

    let (t, dir) = best?;
    let at = Point::new(
        (from.x as f64 + dx * t).round() as i32,
        (from.y as f64 + dy * t).round() as i32,
    );
    Some(Exit {
        at: rect.clamp_point(at),
        dir,
    })
}

/// Map a point from one rect's coordinate space into another's.
///
/// When the two rects have the same size this is an exact translation, which is
/// the common case and the one that gives pixel-for-pixel cursor continuity.
/// Differing sizes scale proportionally, which is how a monitor can be given
/// more or less room on the global desktop than its native pixel count.
pub fn map_point(src: &Rect, dst: &Rect, p: Point) -> Point {
    debug_assert!(!src.is_empty() && !dst.is_empty());
    let x = if src.w == dst.w {
        dst.x + (p.x - src.x)
    } else {
        let t = (p.x - src.x) as f64 / src.w as f64;
        dst.x + (t * dst.w as f64).floor() as i32
    };
    let y = if src.h == dst.h {
        dst.y + (p.y - src.y)
    } else {
        let t = (p.y - src.y) as f64 / src.h as f64;
        dst.y + (t * dst.h as f64).floor() as i32
    };
    dst.clamp_point(Point::new(x, y))
}
