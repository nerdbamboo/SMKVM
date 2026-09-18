//! Between the global desktop and the canvas it is drawn on.
//!
//! One scale for both axes, always. A map that stretched one axis would draw
//! two edges that line up exactly as though they did not, and whether the
//! seams line up is the only question this screen is asked.

use egui::{Pos2, Vec2};
use smkvm_layout::{Point, Rect};

/// How far the desk may be zoomed.
///
/// Not a matter of taste. A scale of zero collapses the whole desktop onto one
/// point, and the mapping back to desktop pixels then divides by it.
const MIN_SCALE: f32 = 0.005;
const MAX_SCALE: f32 = 2.0;

/// How the global desktop is laid over the canvas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// Canvas points per desktop pixel.
    scale: f32,
    /// Where desktop (0, 0) falls on the canvas.
    origin: Pos2,
}

impl View {
    /// Lay the whole of `desktop` inside `viewport`, centred, with `margin`
    /// points of air around it.
    pub fn fit(desktop: Rect, viewport: egui::Rect, margin: f32) -> View {
        let room = Vec2::new(
            (viewport.width() - 2.0 * margin).max(1.0),
            (viewport.height() - 2.0 * margin).max(1.0),
        );
        let scale = if desktop.is_empty() {
            1.0
        } else {
            (room.x / desktop.w as f32).min(room.y / desktop.h as f32)
        };
        let middle = Vec2::new(
            desktop.x as f32 + desktop.w as f32 / 2.0,
            desktop.y as f32 + desktop.h as f32 / 2.0,
        );
        let scale = usable(scale);
        View {
            scale,
            origin: viewport.center() - middle * scale,
        }
    }

    pub fn to_screen(self, p: Point) -> Pos2 {
        self.origin + Vec2::new(p.x as f32, p.y as f32) * self.scale
    }

    /// Rounds rather than truncates. Truncation is towards zero, which would
    /// make the pixel at the origin cover twice the canvas of every other one
    /// and put everything just short of it a pixel out.
    pub fn to_desktop(self, p: Pos2) -> Point {
        let v = (p - self.origin) / self.scale;
        Point::new(round(v.x), round(v.y))
    }

    pub fn rect_to_screen(self, r: Rect) -> egui::Rect {
        egui::Rect::from_min_size(
            self.to_screen(r.origin()),
            Vec2::new(r.w as f32, r.h as f32) * self.scale,
        )
    }

    /// How many desktop pixels `points` of canvas covers.
    ///
    /// The snap threshold is set in canvas points and converted here, so the
    /// pull feels the same however far out the desk is zoomed. Held in desktop
    /// pixels instead it would reach across half the desk when zoomed out and
    /// be unreachable when zoomed in.
    pub fn desktop_length(self, points: f32) -> i32 {
        round(points / self.scale).max(0)
    }

    pub fn panned(self, by: Vec2) -> View {
        View {
            origin: self.origin + by,
            ..self
        }
    }

    /// Nudge the desk back until at least `keep` points of it are still in
    /// sight.
    ///
    /// Panning and zooming have no controls to press, so there would be
    /// nothing to press to undo them either. Rather than add a button against
    /// a situation that should not arise, the desk is simply never allowed to
    /// leave.
    pub fn contained(self, desk: Rect, viewport: egui::Rect, keep: f32) -> View {
        if desk.is_empty() {
            return self;
        }
        let drawn = self.rect_to_screen(desk);
        self.panned(Vec2::new(
            nudge(
                drawn.left(),
                drawn.right(),
                viewport.left(),
                viewport.right(),
                keep,
            ),
            nudge(
                drawn.top(),
                drawn.bottom(),
                viewport.top(),
                viewport.bottom(),
                keep,
            ),
        ))
    }

    /// Zoom by `factor`, leaving whatever is under `anchor` where it is.
    pub fn zoomed_about(self, anchor: Pos2, factor: f32) -> View {
        let scale = usable(self.scale * factor);
        let moved = scale / self.scale;
        View {
            scale,
            origin: anchor + (self.origin - anchor) * moved,
        }
    }
}

/// How far one span has to move for `keep` of it to overlap the other. Nothing
/// smaller than either span is ever demanded, so a desk narrower than `keep`
/// only has to be wholly inside.
fn nudge(a: f32, b: f32, low: f32, high: f32, keep: f32) -> f32 {
    let keep = keep.min(b - a).min(high - low);
    if b - low < keep {
        low + keep - b
    } else if high - a < keep {
        high - keep - a
    } else {
        0.0
    }
}

fn usable(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(MIN_SCALE, MAX_SCALE)
    } else {
        1.0
    }
}

/// `as` saturates at the ends of the range rather than wrapping, so a desk
/// dragged somewhere absurd stops there instead of reappearing on the far side.
fn round(v: f32) -> i32 {
    if v.is_finite() {
        v.round() as i32
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> egui::Rect {
        egui::Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(800.0, 600.0))
    }

    #[test]
    fn fit_centres_the_desktop() {
        let desktop = Rect::new(-500, 200, 4000, 1000);
        let view = View::fit(desktop, viewport(), 24.0);
        let drawn = view.rect_to_screen(desktop);
        assert!((drawn.center() - viewport().center()).length() < 0.01);
    }

    #[test]
    fn fit_leaves_the_margin_and_keeps_the_shape() {
        let desktop = Rect::new(0, 0, 4000, 1000);
        let view = View::fit(desktop, viewport(), 24.0);
        let drawn = view.rect_to_screen(desktop);
        assert!(drawn.width() <= 800.0 - 48.0 + 0.01);
        assert!(drawn.height() <= 600.0 - 48.0 + 0.01);
        // The wider axis is the one that ran out of room, so it should be
        // using all of it.
        assert!((drawn.width() - (800.0 - 48.0)).abs() < 0.01);
        let aspect = drawn.width() / drawn.height();
        assert!((aspect - 4.0).abs() < 0.001);
    }

    #[test]
    fn fit_survives_a_desktop_with_nothing_in_it() {
        let view = View::fit(Rect::new(0, 0, 0, 0), viewport(), 24.0);
        assert!(view.scale.is_finite() && view.scale > 0.0);
    }

    #[test]
    fn a_point_survives_the_round_trip() {
        for desktop in [
            Rect::new(0, 0, 3840, 2160),
            Rect::new(-1920, -1080, 7680, 2160),
            Rect::new(0, 0, 20000, 4000),
        ] {
            let view = View::fit(desktop, viewport(), 24.0);
            for p in [
                Point::new(0, 0),
                desktop.origin(),
                Point::new(desktop.right() - 1, desktop.bottom() - 1),
                Point::new(desktop.x + 137, desktop.y + 911),
            ] {
                assert_eq!(
                    view.to_desktop(view.to_screen(p)),
                    p,
                    "{p:?} in {desktop:?}"
                );
            }
        }
    }

    #[test]
    fn every_pixel_gets_the_same_share_of_the_canvas() {
        let view = View::fit(Rect::new(0, 0, 800, 600), viewport(), 0.0);
        assert_eq!(view.scale, 1.0);
        // Truncation would hand both of these to pixel zero, making it twice
        // the width of its neighbours.
        assert_eq!(view.to_desktop(view.to_screen(Point::new(0, 0))).x, 0);
        assert_eq!(
            view.to_desktop(view.to_screen(Point::new(0, 0)) + Vec2::new(-0.6, 0.0))
                .x,
            -1
        );
        assert_eq!(
            view.to_desktop(view.to_screen(Point::new(0, 0)) + Vec2::new(0.6, 0.0))
                .x,
            1
        );
    }

    #[test]
    fn zooming_leaves_what_is_under_the_pointer_alone() {
        let view = View::fit(Rect::new(0, 0, 4000, 1000), viewport(), 24.0);
        let anchor = Pos2::new(300.0, 250.0);
        let under = view.to_desktop(anchor);
        for factor in [1.1, 0.9, 2.0, 0.25] {
            let zoomed = view.zoomed_about(anchor, factor);
            let still = zoomed.to_desktop(anchor);
            let slip = ((still.x - under.x).abs()).max((still.y - under.y).abs());
            assert!(slip <= 1, "slipped by {slip} at {factor}");
        }
    }

    #[test]
    fn zooming_cannot_reach_a_scale_that_divides_by_nothing() {
        let view = View::fit(Rect::new(0, 0, 4000, 1000), viewport(), 24.0);
        let mut out = view;
        let mut into = view;
        for _ in 0..200 {
            out = out.zoomed_about(Pos2::ZERO, 0.5);
            into = into.zoomed_about(Pos2::ZERO, 2.0);
        }
        assert_eq!(out.scale, MIN_SCALE);
        assert_eq!(into.scale, MAX_SCALE);
    }

    #[test]
    fn the_snap_threshold_grows_as_the_desk_shrinks() {
        let near = View::fit(Rect::new(0, 0, 800, 600), viewport(), 0.0);
        let far = View::fit(Rect::new(0, 0, 8000, 6000), viewport(), 0.0);
        assert_eq!(near.desktop_length(8.0), 8);
        assert!(far.desktop_length(8.0) > near.desktop_length(8.0));
    }

    #[test]
    fn the_desk_cannot_be_pushed_out_of_sight() {
        let desk = Rect::new(0, 0, 4000, 1000);
        let view = View::fit(desk, viewport(), 24.0);
        for shove in [
            Vec2::new(5000.0, 0.0),
            Vec2::new(-5000.0, 0.0),
            Vec2::new(0.0, 4000.0),
            Vec2::new(0.0, -4000.0),
            Vec2::new(-3000.0, 2000.0),
        ] {
            let held = view.panned(shove).contained(desk, viewport(), 40.0);
            let drawn = held.rect_to_screen(desk);
            let seen = drawn.intersect(viewport());
            assert!(seen.width() >= 39.0 && seen.height() >= 39.0, "{shove:?}");
        }
    }

    #[test]
    fn a_desk_already_in_view_is_left_where_it_is() {
        let desk = Rect::new(0, 0, 4000, 1000);
        let view = View::fit(desk, viewport(), 24.0);
        assert_eq!(view.contained(desk, viewport(), 40.0), view);
    }

    #[test]
    fn panning_moves_the_desk_and_nothing_else() {
        let view = View::fit(Rect::new(0, 0, 4000, 1000), viewport(), 24.0);
        let moved = view.panned(Vec2::new(37.0, -12.0));
        assert_eq!(moved.scale, view.scale);
        assert_eq!(
            moved.to_screen(Point::new(100, 100)),
            view.to_screen(Point::new(100, 100)) + Vec2::new(37.0, -12.0)
        );
    }
}
