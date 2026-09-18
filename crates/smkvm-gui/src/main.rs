//! The SMKVM window: arranging the desk, pairing machines, and the few
//! settings worth changing.
//!
//! It reads and writes the same files the daemon does and nothing else. There
//! is no second format, no state kept here that is not on disk, and nothing
//! this window knows that `smkvm` does not.
//!
//! Drawn through OpenGL in a single binary, deliberately. A webview would mean
//! shipping one or depending on whichever one the machine happens to have, and
//! these builds are cross-compiled from Linux and updated in place.

#![forbid(unsafe_code)]
// A double-clicked window should not also open a console behind it. Running it
// from a terminal on Linux is unaffected; this only concerns Windows, where a
// program is one or the other.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod canvas;
mod desk;
mod machines;
mod pairing;
mod platform;
mod settings;
mod snap;
mod store;
mod theme;
mod view;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SMKVM")
            .with_inner_size([980.0, 700.0])
            .with_min_inner_size([720.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "SMKVM",
        options,
        Box::new(|cc| match app::App::new(cc) {
            Ok(app) => Ok(Box::new(app)),
            // Nothing is drawn yet, so this is the one failure that has to
            // reach the person some other way than through the window.
            Err(why) => Err(why.into()),
        }),
    )
}
