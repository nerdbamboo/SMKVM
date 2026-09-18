//! The desk: every machine's screens, as rectangles that can be moved.
//!
//! Nothing here decides anything. Where a screen comes to rest is [`crate::snap`],
//! what is on the desk at all is [`crate::desk`], and the mapping between the
//! canvas and the global desktop is [`crate::view`]. This draws the result and
//! reports what the pointer did to it.

use egui::{Align2, Color32, CursorIcon, Sense, Shape, Stroke, StrokeKind, Vec2};
use smkvm_layout::{Point, Rect};

use crate::desk::{Desk, Presence, Screen};
use crate::snap;
use crate::theme::{self, Palette};
use crate::view::View;

/// How near an edge must come before it is pulled flush, in canvas points.
///
/// Set here rather than in desktop pixels so the pull feels the same however
/// far out the desk is zoomed.
const SNAP_WITHIN: f32 = 9.0;

/// How much of the desk must stay in sight, in canvas points.
const KEEP_IN_SIGHT: f32 = 56.0;

/// Air around the desk when it is first laid out.
const MARGIN: f32 = 36.0;

/// Below this a screen has no room for anything but its machine's name.
const ROOM_FOR_TWO_LINES: f32 = 48.0;

pub struct Canvas {
    view: Option<View>,
    /// Which screens the current view was laid out for.
    ///
    /// Refitting whenever the geometry changed would slide the desk out from
    /// under the screen being dragged, so only screens coming or going, or a
    /// window being resized before anyone has touched the view, start again.
    laid_out_for: (Vec<(String, String)>, egui::Rect),
    /// Whether the view is the person's doing now rather than this window's.
    theirs: bool,
    drag: Option<Drag>,
}

struct Drag {
    machine: String,
    monitor: String,
    /// Where within the screen the pointer took hold, in desktop pixels.
    grab: Point,
    /// Where the screen sat before any of this, and so somewhere it is known
    /// to fit.
    was: Point,
}

/// What the pointer did to the desk this frame.
#[derive(Default)]
pub struct Outcome {
    /// A screen has been let go somewhere new.
    pub moved: Option<(usize, Point)>,
    /// A line worth putting under the desk, when there is one.
    pub note: Option<String>,
}

impl Default for Canvas {
    fn default() -> Self {
        Canvas {
            view: None,
            laid_out_for: (Vec::new(), egui::Rect::ZERO),
            theirs: false,
            drag: None,
        }
    }
}

impl Canvas {
    pub fn show(&mut self, ui: &mut egui::Ui, desk: &Desk, palette: &Palette) -> Outcome {
        let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let viewport = response.rect;
        let bounds = desk.bounds();

        let here: Vec<(String, String)> = desk
            .screens
            .iter()
            .map(|s| (s.machine.clone(), s.monitor.clone()))
            .collect();
        let resized = self.laid_out_for.1 != viewport;
        if self.view.is_none() || self.laid_out_for.0 != here || (resized && !self.theirs) {
            self.view = Some(View::fit(bounds, viewport, MARGIN));
            self.theirs = false;
        }
        self.laid_out_for = (here, viewport);
        let mut view = self.view.expect("a view was just laid out");

        if desk.screens.is_empty() {
            painter.text(
                viewport.center(),
                Align2::CENTER_CENTER,
                "No screens yet.",
                theme::body(),
                palette.dim,
            );
            return Outcome::default();
        }

        // Taking hold comes first: until it is known whether the pointer went
        // down on a screen or on the space between them, steering cannot tell
        // a drag of the desk from a drag of one screen, and would move both.
        self.take_hold(&response, desk, view);
        view = self.steer(&response, ui, view);
        let (ghost, mut outcome) = self.handle_drag(&response, desk, view);
        self.show_what_can_be_picked_up(&response, desk, view);
        view = view.contained(bounds, viewport, KEEP_IN_SIGHT);
        self.view = Some(view);

        let order = machine_order(desk);
        // Screens that are not here go down first. Where a machine has been
        // arranged on top of space another one is keeping -- which is what
        // happens to a machine the configuration says nothing about, since
        // arranging automatically does not see reserved space either -- it
        // should read as one screen over another rather than as a name that
        // has gone missing.
        for absent in [true, false] {
            for (i, screen) in desk.screens.iter().enumerate() {
                if ghost.as_ref().is_some_and(|g| g.index == i) {
                    continue;
                }
                if (screen.presence == Presence::Away) != absent {
                    continue;
                }
                draw(&painter, palette, view, screen, screen.global, &order, None);
            }
        }
        if let Some(ghost) = &ghost {
            let screen = &desk.screens[ghost.index];
            draw(
                &painter,
                palette,
                view,
                screen,
                ghost.at,
                &order,
                Some(ghost.takes),
            );
        }

        if outcome.note.is_none() {
            outcome.note = self.note(&response, desk, view);
        }
        outcome
    }

    /// Zoom about the pointer, and move the desk with a two-finger scroll or a
    /// drag of the space between screens. None of it has a control to press,
    /// and nobody who does not reach for it ever needs to know it is there.
    fn steer(&mut self, response: &egui::Response, ui: &egui::Ui, mut view: View) -> View {
        if response.hovered() {
            let (zoom, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
            if let (true, Some(at)) = (zoom != 1.0, response.hover_pos()) {
                view = view.zoomed_about(at, zoom);
                self.theirs = true;
            }
            if scroll != Vec2::ZERO {
                view = view.panned(scroll);
                self.theirs = true;
            }
        }
        if response.dragged() && self.drag.is_none() {
            view = view.panned(response.drag_delta());
            self.theirs = true;
        }
        view
    }

    /// Work out which screen, if any, the pointer has taken hold of.
    fn take_hold(&mut self, response: &egui::Response, desk: &Desk, view: View) {
        if !response.drag_started() {
            return;
        }
        // Where the button went down, not where the pointer is now. A drag is
        // only recognised once it has moved, so by this frame the pointer has
        // already travelled, and taking hold here would offset the screen by
        // exactly that much for the rest of the drag.
        let took_hold = response.ctx.input(|i| i.pointer.press_origin());
        self.drag = took_hold.and_then(|at| {
            let p = view.to_desktop(at);
            let screen = desk.screens.iter().find(|s| s.global.contains(p))?;
            Some(Drag {
                machine: screen.machine.clone(),
                monitor: screen.monitor.clone(),
                grab: Point::new(p.x - screen.global.x, p.y - screen.global.y),
                was: screen.global.origin(),
            })
        });
    }

    fn handle_drag(
        &mut self,
        response: &egui::Response,
        desk: &Desk,
        view: View,
    ) -> (Option<Ghost>, Outcome) {
        let mut outcome = Outcome::default();
        let held = self.drag.as_ref().and_then(|drag| {
            let index = desk.find(&drag.machine, &drag.monitor)?;
            let at = response.interact_pointer_pos()?;
            let p = view.to_desktop(at);
            let size = desk.screens[index].global;
            let dropped = snap::at(Point::new(p.x - drag.grab.x, p.y - drag.grab.y), size);
            Some((index, dropped, drag.was))
        });

        let ghost = held.map(|(index, dropped, was)| {
            let others = desk.others(index);
            let within = view.desktop_length(SNAP_WITHIN);
            let resting = snap::at(snap::snap(dropped, &others, within), dropped);
            let takes = snap::is_clear(resting, &others);
            if !takes {
                outcome.note = Some("There is already a screen there.".into());
            }
            if response.drag_stopped() {
                let to = snap::settle(dropped, &others, within, was);
                if to != was {
                    outcome.moved = Some((index, to));
                }
            }
            Ghost {
                index,
                at: resting,
                takes,
            }
        });

        if response.drag_stopped() {
            self.drag = None;
        }
        (ghost, outcome)
    }

    /// A screen that can be moved should look like one before it is tried.
    fn show_what_can_be_picked_up(&self, response: &egui::Response, desk: &Desk, view: View) {
        if self.drag.is_some() {
            response.ctx.set_cursor_icon(CursorIcon::Grabbing);
            return;
        }
        let over = response
            .hover_pos()
            .map(|at| view.to_desktop(at))
            .is_some_and(|p| desk.screens.iter().any(|s| s.global.contains(p)));
        if over {
            response.ctx.set_cursor_icon(CursorIcon::Grab);
        }
    }

    /// What is worth saying about whatever the pointer is over.
    fn note(&self, response: &egui::Response, desk: &Desk, view: View) -> Option<String> {
        let under = response
            .hover_pos()
            .map(|at| view.to_desktop(at))
            .and_then(|p| desk.screens.iter().find(|s| s.global.contains(p)));
        Some(match under {
            Some(screen) => match screen.presence {
                // Said only while the pointer is on it. A machine that is
                // switched off is not a problem to be announced, but what its
                // space is for is worth knowing when you are looking at it.
                Presence::Away => format!(
                    "{} is not here. Its space is kept, so the cursor stops at its edge.",
                    screen.machine
                ),
                Presence::Held => {
                    format!(
                        "{} is connected, but not taking input at the moment.",
                        screen.machine
                    )
                }
                Presence::Here if screen.provisional => format!(
                    "{} was arranged automatically. Drag it to decide where it goes.",
                    screen.machine
                ),
                Presence::Here if screen.active => "The cursor is here.".into(),
                Presence::Here => "Drag a screen to arrange it.".into(),
            },
            None => "Drag a screen to arrange it.".into(),
        })
    }
}

impl Canvas {
    #[cfg(test)]
    fn laid_over(&self) -> Option<View> {
        self.view
    }
}

struct Ghost {
    index: usize,
    at: Rect,
    takes: bool,
}

/// Machines in the order they first appear, which is what keeps a machine's
/// colour the same for as long as the desk holds still.
fn machine_order(desk: &Desk) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    for screen in &desk.screens {
        if !order.contains(&screen.machine) {
            order.push(screen.machine.clone());
        }
    }
    order
}

/// `held` is `Some(whether it will take)` for the screen being dragged.
fn draw(
    painter: &egui::Painter,
    palette: &Palette,
    view: View,
    screen: &Screen,
    global: Rect,
    order: &[String],
    held: Option<bool>,
) {
    let at = view.rect_to_screen(global);
    let hue = theme::hue(&screen.machine, order);

    if held == Some(false) {
        painter.rect_stroke(
            at,
            theme::SCREEN_RADIUS,
            Stroke::new(2.0, palette.warn),
            StrokeKind::Inside,
        );
        write(painter, at, palette, screen, palette.warn, None);
        return;
    }

    match screen.presence {
        Presence::Away => {
            painter.rect_filled(at, theme::SCREEN_RADIUS, palette.absent);
            // Drawn as an outline that is not quite there, because the space
            // is spoken for without anything being in it.
            for dash in dashed(at, Stroke::new(1.5, palette.dim)) {
                painter.add(dash);
            }
            write(painter, at, palette, screen, palette.dim, Some("not here"));
        }
        Presence::Here | Presence::Held => {
            let lifted = held.is_some();
            painter.rect(
                at,
                theme::SCREEN_RADIUS,
                tint(hue, palette, if lifted { 56 } else { 34 }),
                Stroke::new(if lifted { 2.0 } else { 1.5 }, hue),
                StrokeKind::Inside,
            );
            // A screen the daemon placed on its own is drawn with a dashed
            // edge inside the solid one: it is real and reachable, but where
            // it sits is nobody's decision yet.
            if screen.provisional && !lifted {
                for dash in dashed(at.shrink(4.0), Stroke::new(1.0, palette.dim)) {
                    painter.add(dash);
                }
            }
            // The machine holding the cursor gets a second, heavier edge, so
            // a glance at the window says where the pointer is.
            if screen.active {
                painter.rect_stroke(
                    at.shrink(1.0),
                    theme::SCREEN_RADIUS,
                    Stroke::new(3.0, palette.accent),
                    StrokeKind::Inside,
                );
            }
            let second = match screen.presence {
                Presence::Held => Some("not taking input"),
                _ if screen.active => Some("cursor is here"),
                _ => screen.label.as_deref().or(Some(screen.monitor.as_str())),
            };
            write(painter, at, palette, screen, hue, second);
        }
    }
}

/// A hue laid over the page at low strength, so a desk of six machines reads
/// as a desk rather than as a chart.
fn tint(hue: Color32, palette: &Palette, strength: u8) -> Color32 {
    palette.page.lerp_to_gamma(hue, f32::from(strength) / 255.0)
}

fn dashed(at: egui::Rect, stroke: Stroke) -> Vec<Shape> {
    let corners = [
        at.left_top(),
        at.right_top(),
        at.right_bottom(),
        at.left_bottom(),
        at.left_top(),
    ];
    Shape::dashed_line(&corners, stroke, 5.0, 4.0)
}

fn write(
    painter: &egui::Painter,
    at: egui::Rect,
    palette: &Palette,
    screen: &Screen,
    colour: Color32,
    second: Option<&str>,
) {
    // A name spilling over the edge of its screen would say the screen was
    // somewhere it is not, so nothing is drawn that does not fit.
    if at.width() < 44.0 || at.height() < 22.0 {
        return;
    }
    let painter = painter.with_clip_rect(at.shrink(3.0));
    let room = at.height() >= ROOM_FOR_TWO_LINES && second.is_some();
    let middle = at.center();
    if room {
        painter.text(
            middle - Vec2::new(0.0, 8.0),
            Align2::CENTER_CENTER,
            &screen.machine,
            theme::body(),
            colour,
        );
        painter.text(
            middle + Vec2::new(0.0, 9.0),
            Align2::CENTER_CENTER,
            second.expect("checked just above"),
            theme::small(),
            palette.dim,
        );
    } else {
        painter.text(
            middle,
            Align2::CENTER_CENTER,
            &screen.machine,
            theme::body(),
            colour,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desk::Presence;
    use egui::{Event, Modifiers, PointerButton, Pos2, RawInput};

    const VIEWPORT: egui::Rect = egui::Rect {
        min: Pos2::new(0.0, 0.0),
        max: Pos2::new(800.0, 600.0),
    };

    fn screen(machine: &str, global: Rect, presence: Presence) -> Screen {
        Screen {
            machine: machine.into(),
            monitor: "primary".into(),
            global,
            presence,
            label: None,
            active: false,
            provisional: false,
        }
    }

    /// Two machines with a gap of eighty pixels between them.
    fn desk() -> Desk {
        Desk {
            screens: vec![
                screen("here", Rect::new(0, 0, 1920, 1080), Presence::Here),
                screen("there", Rect::new(2000, 0, 1920, 1080), Presence::Away),
            ],
            here: "here".into(),
        }
    }

    /// The view the canvas lays out for itself on the first frame, so a test
    /// can work out where on the canvas a given part of the desk has landed.
    fn view(desk: &Desk) -> View {
        View::fit(desk.bounds(), VIEWPORT, MARGIN)
    }

    /// Run one frame against real pointer events and report what came of it.
    fn frame(ctx: &egui::Context, canvas: &mut Canvas, desk: &Desk, events: Vec<Event>) -> Outcome {
        let input = RawInput {
            screen_rect: Some(VIEWPORT),
            events,
            ..Default::default()
        };
        let mut outcome = Outcome::default();
        let output = ctx.run_ui(input, |ui| {
            outcome = canvas.show(ui, desk, &crate::theme::LIGHT);
        });
        output.drop_without_applying_deltas();
        outcome
    }

    fn press(at: Pos2) -> Vec<Event> {
        vec![
            Event::PointerMoved(at),
            Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ]
    }

    fn release(at: Pos2) -> Vec<Event> {
        vec![Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }]
    }

    /// Carry out a whole drag, from the middle of one screen to wherever the
    /// given desktop offset puts it, and report what the canvas committed.
    fn drag(desk: &Desk, from: usize, by: Point) -> Option<(usize, Point)> {
        let ctx = egui::Context::default();
        let mut canvas = Canvas::default();
        let view = view(desk);
        let middle = desk.screens[from].global;
        let took_hold =
            view.to_screen(Point::new(middle.x + middle.w / 2, middle.y + middle.h / 2));
        let let_go = view.to_screen(Point::new(
            middle.x + middle.w / 2 + by.x,
            middle.y + middle.h / 2 + by.y,
        ));

        frame(&ctx, &mut canvas, desk, Vec::new());
        frame(&ctx, &mut canvas, desk, press(took_hold));
        frame(&ctx, &mut canvas, desk, vec![Event::PointerMoved(let_go)]);
        frame(&ctx, &mut canvas, desk, vec![Event::PointerMoved(let_go)]);
        frame(&ctx, &mut canvas, desk, release(let_go)).moved
    }

    #[test]
    fn dragging_a_screen_nearly_flush_closes_the_seam() {
        // Dropped five pixels short of its neighbour's edge, it should come to
        // rest exactly against it.
        let desk = desk();
        assert_eq!(
            drag(&desk, 0, Point::new(75, 0)),
            Some((0, Point::new(80, 0)))
        );
    }

    #[test]
    fn a_screen_dropped_where_it_started_is_not_written_down() {
        let desk = desk();
        assert_eq!(drag(&desk, 0, Point::new(0, 0)), None);
    }

    #[test]
    fn a_screen_dropped_on_top_of_another_goes_back() {
        // Well inside the other one, so snapping has nothing to rescue.
        let desk = desk();
        assert_eq!(drag(&desk, 0, Point::new(2400, 0)), None);
    }

    #[test]
    fn the_space_a_machine_that_is_away_keeps_is_as_solid_as_a_screen() {
        // `there` is not here at all, and its ground is still not free.
        let desk = desk();
        assert_eq!(desk.screens[1].presence, Presence::Away);
        assert_eq!(drag(&desk, 0, Point::new(2400, 0)), None);
    }

    #[test]
    fn a_screen_can_be_taken_the_other_way_too() {
        let desk = desk();
        let moved = drag(&desk, 1, Point::new(-75, 0));
        assert_eq!(moved, Some((1, Point::new(1920, 0))), "flush on its left");
    }

    #[test]
    fn taking_hold_of_a_screen_does_not_also_move_the_desk() {
        // Whether the pointer went down on a screen or on the space between
        // them is only known after the pointer has moved, and if the desk
        // steers on that first frame it lurches by however far the drag has
        // got before it is recognised.
        let desk = desk();
        let ctx = egui::Context::default();
        let mut canvas = Canvas::default();
        let middle = desk.screens[0].global;
        let took_hold =
            view(&desk).to_screen(Point::new(middle.x + middle.w / 2, middle.y + middle.h / 2));

        frame(&ctx, &mut canvas, &desk, Vec::new());
        let before = canvas.laid_over().expect("laid out on the first frame");
        frame(&ctx, &mut canvas, &desk, press(took_hold));
        frame(
            &ctx,
            &mut canvas,
            &desk,
            vec![Event::PointerMoved(took_hold + Vec2::new(40.0, 20.0))],
        );
        assert_eq!(
            canvas.laid_over(),
            Some(before),
            "the desk should hold still"
        );
    }

    #[test]
    fn dragging_the_space_between_screens_moves_the_desk() {
        let desk = desk();
        let ctx = egui::Context::default();
        let mut canvas = Canvas::default();
        let empty = Pos2::new(400.0, 560.0);

        frame(&ctx, &mut canvas, &desk, Vec::new());
        let before = canvas.laid_over().expect("laid out on the first frame");
        frame(&ctx, &mut canvas, &desk, press(empty));
        frame(
            &ctx,
            &mut canvas,
            &desk,
            vec![Event::PointerMoved(empty + Vec2::new(40.0, 0.0))],
        );
        assert_ne!(canvas.laid_over(), Some(before), "the desk should follow");
    }

    #[test]
    fn nothing_is_committed_when_the_drag_began_on_empty_space() {
        let ctx = egui::Context::default();
        let mut canvas = Canvas::default();
        let desk = desk();
        // Below everything, where the desk has nothing at all.
        let empty = Pos2::new(400.0, 560.0);
        frame(&ctx, &mut canvas, &desk, Vec::new());
        frame(&ctx, &mut canvas, &desk, press(empty));
        frame(
            &ctx,
            &mut canvas,
            &desk,
            vec![Event::PointerMoved(empty + Vec2::new(40.0, 0.0))],
        );
        let outcome = frame(
            &ctx,
            &mut canvas,
            &desk,
            release(empty + Vec2::new(40.0, 0.0)),
        );
        assert_eq!(outcome.moved, None);
    }
}
