//! The few things people actually change.
//!
//! Everything else the configuration can say -- which addresses to listen on,
//! how long a heartbeat may go missing, which clipboard flavours travel -- is
//! set once by somebody who knows what it means, and is left in the file where
//! they can find it. A window full of settings nobody touches is a window that
//! hides the three that matter.

use egui::{Align, Layout};
use smkvm_config::Config;
use smkvm_layout::EdgeOverflow;

use crate::theme::{self, Palette};

/// The longest pause worth offering. Beyond about a second the cursor feels
/// stuck rather than deliberate.
const LONGEST_PAUSE_MS: u32 = 1000;

/// Room set aside for the controls, so every one of them lines up down the
/// right-hand side however long the words beside it are.
const CONTROL_COLUMN: f32 = 250.0;

/// Returns whether anything was changed.
pub fn show(ui: &mut egui::Ui, palette: &Palette, config: &mut Config) -> bool {
    let before = (
        config.behavior.switch_delay_ms,
        config.behavior.edge_overflow,
        config.clipboard.enabled,
    );

    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Settings")
            .font(theme::title())
            .color(palette.text),
    );
    ui.add_space(14.0);

    theme::card(palette).show(ui, |ui| {
        row(
            ui,
            palette,
            "Pause before crossing",
            "How long the pointer rests against an edge before it moves to the next screen.",
            |ui| {
                let mut ms = config.behavior.switch_delay_ms;
                let slider = egui::Slider::new(&mut ms, 0..=LONGEST_PAUSE_MS)
                    .step_by(10.0)
                    .custom_formatter(|n, _| match n as u32 {
                        0 => "Off".to_string(),
                        ms => format!("{ms} ms"),
                    });
                ui.add(slider);
                config.behavior.switch_delay_ms = ms;
            },
        );

        separator(ui, palette);
        row(
            ui,
            palette,
            "Slide onto the nearest screen",
            "At an edge with nothing beyond it. Without this the pointer stops there.",
            |ui| {
                let mut slides = config.behavior.edge_overflow == EdgeOverflow::Clamp;
                theme::switch(ui, &mut slides, palette);
                config.behavior.edge_overflow = match slides {
                    true => EdgeOverflow::Clamp,
                    false => EdgeOverflow::Block,
                };
            },
        );

        separator(ui, palette);
        row(
            ui,
            palette,
            "Share the clipboard",
            "What you copy on one machine can be pasted on another.",
            |ui| {
                theme::switch(ui, &mut config.clipboard.enabled, palette);
            },
        );
    });

    before
        != (
            config.behavior.switch_delay_ms,
            config.behavior.edge_overflow,
            config.clipboard.enabled,
        )
}

fn row(
    ui: &mut egui::Ui,
    palette: &Palette,
    title: &str,
    about: &str,
    control: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        // The words are given their own width rather than their natural one.
        // Left to take what they like they run under the control, and a
        // sentence with a slider lying across it is unreadable.
        let words = (ui.available_width() - CONTROL_COLUMN).max(160.0);
        ui.allocate_ui_with_layout(egui::vec2(words, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.label(
                egui::RichText::new(title)
                    .font(theme::body())
                    .color(palette.text),
            );
            ui.label(
                egui::RichText::new(about)
                    .font(theme::small())
                    .color(palette.dim),
            );
        });
        ui.allocate_ui_with_layout(
            egui::vec2(CONTROL_COLUMN, 0.0),
            Layout::right_to_left(Align::Center),
            control,
        );
    });
}

fn separator(ui: &mut egui::Ui, palette: &Palette) {
    ui.add_space(8.0);
    let (rule, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter()
        .hline(rule.x_range(), rule.center().y, theme::hairline(palette));
    ui.add_space(8.0);
}
