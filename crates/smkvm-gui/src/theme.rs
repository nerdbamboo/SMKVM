//! How the window looks, in one place.
//!
//! Restraint is the point: one accent colour, one weight of rule, and a lot of
//! air. Anything that has to be explained by a legend is something that should
//! have been drawn so it did not need one.

use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle};

/// Colours for one of the two ways the window can be read.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub page: Color32,
    pub card: Color32,
    pub text: Color32,
    /// For anything the eye should pass over on its way to the point.
    pub dim: Color32,
    pub rule: Color32,
    pub accent: Color32,
    /// Where something is wrong and saying so quietly would be unkind.
    pub warn: Color32,
    /// Fill for a screen belonging to a machine that is not here.
    pub absent: Color32,
}

pub const LIGHT: Palette = Palette {
    page: Color32::from_rgb(0xF5, 0xF5, 0xF7),
    card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    text: Color32::from_rgb(0x1D, 0x1D, 0x1F),
    dim: Color32::from_rgb(0x6E, 0x6E, 0x73),
    rule: Color32::from_rgb(0xD2, 0xD2, 0xD7),
    accent: Color32::from_rgb(0x00, 0x71, 0xE3),
    warn: Color32::from_rgb(0xBF, 0x40, 0x00),
    absent: Color32::from_rgb(0xEA, 0xEA, 0xEE),
};

pub const DARK: Palette = Palette {
    page: Color32::from_rgb(0x1D, 0x1D, 0x1F),
    card: Color32::from_rgb(0x2C, 0x2C, 0x2E),
    text: Color32::from_rgb(0xF5, 0xF5, 0xF7),
    dim: Color32::from_rgb(0x98, 0x98, 0x9D),
    rule: Color32::from_rgb(0x3A, 0x3A, 0x3C),
    accent: Color32::from_rgb(0x0A, 0x84, 0xFF),
    warn: Color32::from_rgb(0xFF, 0x9F, 0x0A),
    absent: Color32::from_rgb(0x26, 0x26, 0x28),
};

pub fn palette(ui: &egui::Ui) -> Palette {
    if ui.visuals().dark_mode {
        DARK
    } else {
        LIGHT
    }
}

/// Hues a machine's screens are tinted with, so which rectangles belong
/// together is a thing you see rather than a thing you read.
///
/// Muted on purpose. A desk of six saturated blocks is a chart, not a desk.
const HUES: [Color32; 6] = [
    Color32::from_rgb(0x00, 0x71, 0xE3),
    Color32::from_rgb(0x30, 0xA0, 0x8C),
    Color32::from_rgb(0x9A, 0x5C, 0xD6),
    Color32::from_rgb(0xC1, 0x7A, 0x1F),
    Color32::from_rgb(0xC0, 0x4A, 0x74),
    Color32::from_rgb(0x4B, 0x6B, 0xB0),
];

/// The same machine gets the same hue for as long as the desk holds still,
/// which is what makes the grouping mean anything.
pub fn hue(machine: &str, order: &[String]) -> Color32 {
    let at = order.iter().position(|m| m == machine).unwrap_or(0);
    HUES[at % HUES.len()]
}

pub const CARD_RADIUS: CornerRadius = CornerRadius::same(12);
pub const SCREEN_RADIUS: CornerRadius = CornerRadius::same(6);

/// Space between a card's edge and what is in it. Generous: the air is what
/// makes a short list of things read as a short list of things.
pub const CARD_PAD: Margin = Margin {
    left: 20,
    right: 20,
    top: 16,
    bottom: 16,
};

pub fn title() -> FontId {
    FontId::new(21.0, FontFamily::Proportional)
}

pub fn body() -> FontId {
    FontId::new(14.0, FontFamily::Proportional)
}

pub fn small() -> FontId {
    FontId::new(12.0, FontFamily::Proportional)
}

/// The confirmation code, and nothing else.
///
/// Large, monospaced and spaced out, because the whole of its job is being
/// compared against another screen across a desk. A code that has to be
/// squinted at is one that gets waved through.
pub fn code() -> FontId {
    FontId::new(46.0, FontFamily::Monospace)
}

pub fn hairline(palette: &Palette) -> Stroke {
    Stroke::new(1.0, palette.rule)
}

/// Set the window's proportions once, at startup.
///
/// Applied to both the light and the dark style, so following the system from
/// one to the other changes the colours and nothing else.
pub fn install(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, title()),
            (TextStyle::Body, body()),
            (TextStyle::Button, body()),
            (TextStyle::Small, small()),
            (
                TextStyle::Monospace,
                FontId::new(13.0, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(10.0, 10.0);
        style.spacing.button_padding = egui::vec2(14.0, 7.0);
        // Narrow enough that a slider and the number beside it both fit
        // the column the settings keep for their controls.
        style.spacing.slider_width = 150.0;
        style.spacing.interact_size.y = 26.0;
        style.visuals.window_corner_radius = CARD_RADIUS;
        for widget in [
            &mut style.visuals.widgets.noninteractive,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
        ] {
            widget.corner_radius = CornerRadius::same(7);
        }
    });
}

/// An on/off switch.
///
/// Drawn here rather than taken from the toolkit because the state has to be
/// readable at a glance: a filled track that has moved says on or off by
/// position and colour at once, where a tick mark is a small glyph that has to
/// be found before it can be read.
pub fn switch(ui: &mut egui::Ui, on: &mut bool, palette: &Palette) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(44.0, 25.0), egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let moved = ui.ctx().animate_bool_with_time(response.id, *on, 0.12);
    let radius = rect.height() / 2.0;
    ui.painter().rect_filled(
        rect,
        radius,
        palette.rule.lerp_to_gamma(palette.accent, moved),
    );
    let knob = egui::lerp((rect.left() + radius)..=(rect.right() - radius), moved);
    ui.painter().circle_filled(
        egui::pos2(knob, rect.center().y),
        radius - 3.0,
        Color32::WHITE,
    );
    response
}

/// A white panel with room to breathe, for grouping a few related things.
pub fn card(palette: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.card)
        .corner_radius(CARD_RADIUS)
        .inner_margin(CARD_PAD)
}
