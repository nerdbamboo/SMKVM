//! The window: three screens, and what joins them to the files on disk.

use std::time::{Duration, Instant};

use egui::{Align, Align2, Layout, Margin};
use smkvm_input::Monitors;
use smkvm_layout::{DeviceId, Point};

use crate::canvas::Canvas;
use crate::desk::Desk;
use crate::machines::{self, Action};
use crate::pairing::{self, Pairing, Step};
use crate::settings;
use crate::store::{self, Store};
use crate::theme::{self, Palette};

/// How often the daemon's report is read again. It is one small file, and a
/// machine arriving should show while somebody is still looking at the window.
const LOOK_AGAIN: Duration = Duration::from_secs(2);

/// How wide a column of text and controls is allowed to get. Beyond this the
/// eye has to travel back across the whole window to find the next line.
const READABLE: f32 = 620.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Desk,
    Machines,
    Settings,
}

pub struct App {
    store: Store,
    displays: Option<Box<dyn Monitors>>,
    tab: Tab,
    canvas: Canvas,
    pairing: Pairing,
    /// Open while a machine is being introduced.
    sheet: Option<Sheet>,
    forgetting: Option<(String, DeviceId)>,
    /// Whatever went wrong that the person has not waved away yet.
    trouble: Option<String>,
    /// Taken from the desk each frame and shown beneath it on the next one.
    /// A caption a frame behind the pointer is a caption nobody can see is
    /// late, and it is what lets the canvas have the rest of the window.
    note: String,
    looked: Instant,
}

struct Sheet {
    host: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<App, String> {
        theme::install(&cc.egui_ctx);

        // A machine with no way to read its displays can still arrange the
        // others, so this is worth saying rather than dying of.
        let (mut displays, trouble) = match crate::platform::displays() {
            Ok(displays) => (Some(displays), None),
            Err(why) => (None, Some(why.to_string())),
        };
        let monitors = displays
            .as_mut()
            .and_then(|d| d.monitors().ok())
            .unwrap_or_default();

        Ok(App {
            store: Store::open(monitors).map_err(|e| e.to_string())?,
            displays,
            tab: Tab::Desk,
            canvas: Canvas::default(),
            pairing: Pairing::new().map_err(|e| format!("starting the pairing runtime: {e}"))?,
            sheet: None,
            forgetting: None,
            trouble,
            note: String::new(),
            looked: Instant::now(),
        })
    }

    fn look_again(&mut self) {
        if self.looked.elapsed() < LOOK_AGAIN {
            return;
        }
        self.looked = Instant::now();
        match &mut self.displays {
            Some(displays) => self.store.refresh(displays.as_mut()),
            None => self.store.refresh(&mut NoDisplays),
        }
    }

    fn report<T>(&mut self, outcome: Result<T, store::Error>) {
        if let Err(why) = outcome {
            self.trouble = Some(why.to_string());
        }
    }

    fn desk(&mut self) -> Desk {
        Desk::build(
            self.store.this_machine(),
            self.store.identity.id(),
            &self.store.displays,
            &self.store.config,
            self.store.status.as_ref(),
        )
    }

    fn place(&mut self, desk: &Desk, index: usize, to: Point) {
        let Some((machine, placements)) = desk.moved(index, to) else {
            return;
        };
        self.store.screen_mut(&machine).monitor = placements;
        let saved = self.store.save_config();
        self.report(saved);
    }

    fn forget(&mut self, name: &str, device: DeviceId) {
        self.store.trust.remove(device);
        // Its space goes back too. A machine that can no longer connect would
        // otherwise leave the cursor walking into a wall for ever.
        self.store.drop_machine(name);
        let saved = self.store.save_trust();
        self.report(saved);
        let saved = self.store.save_config();
        self.report(saved);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let palette = theme::palette(ui);
        // Modals live beside the window rather than inside it, so they need
        // the context rather than this ui.
        let ctx = ui.ctx().clone();
        self.look_again();
        ctx.request_repaint_after(LOOK_AGAIN);

        if let Some(peer) = self.pairing.poll() {
            let name = peer.name.clone();
            self.store.trust.add(peer.public_key, name);
            let saved = self.store.save_trust();
            self.report(saved);
        }

        tabs(ui, &palette, &mut self.tab);
        self.show_trouble(ui, &palette);
        if self.tab == Tab::Desk {
            caption(ui, &palette, &self.note);
        }

        match self.tab {
            Tab::Desk => self.show_desk(ui, &palette),
            Tab::Machines => self.show_machines(ui, &palette),
            Tab::Settings => self.show_settings(ui, &palette),
        }

        self.show_sheet(&ctx, &palette);
        self.show_forgetting(&ctx, &palette);
    }
}

impl App {
    fn show_trouble(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        let Some(trouble) = self.trouble.clone() else {
            return;
        };
        egui::Panel::top("trouble")
            .frame(
                egui::Frame::new()
                    .fill(palette.card)
                    .inner_margin(Margin::symmetric(20, 12)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(trouble)
                            .font(theme::small())
                            .color(palette.warn),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("OK").clicked() {
                            self.trouble = None;
                        }
                    });
                });
            });
    }

    fn show_desk(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        let desk = self.desk();
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(palette.page)
                    .inner_margin(Margin::same(10)),
            )
            .show(ui, |ui| {
                let outcome = self.canvas.show(ui, &desk, palette);
                self.note = outcome.note.unwrap_or_default();
                if let Some((index, to)) = outcome.moved {
                    self.place(&desk, index, to);
                }
            });
    }

    fn show_machines(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        let listed = machines::listed(
            &self.store.trust,
            &self.store.config,
            self.store.status.as_ref(),
            self.store.this_machine(),
        );
        let running = self.store.status.is_some();
        let mut action = None;
        page(ui, palette, |ui| {
            action = machines::show(
                ui,
                palette,
                &self.store.config,
                self.store.this_machine(),
                running,
                &listed,
            );
        });
        match action {
            Some(Action::Pair) => {
                self.sheet = Some(Sheet {
                    host: String::new(),
                });
                self.pairing.cancel();
            }
            Some(Action::Forget { name, device }) => self.forgetting = Some((name, device)),
            None => {}
        }
    }

    fn show_settings(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        let mut changed = false;
        page(ui, palette, |ui| {
            changed = settings::show(ui, palette, &mut self.store.config);
        });
        if changed {
            let saved = self.store.save_config();
            self.report(saved);
        }
    }

    fn show_forgetting(&mut self, ctx: &egui::Context, palette: &Palette) {
        let Some((name, device)) = self.forgetting.clone() else {
            return;
        };
        let mut done = false;
        egui::Modal::new(egui::Id::new("forgetting")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(
                egui::RichText::new(format!("Forget {name}?"))
                    .font(theme::title())
                    .color(palette.text),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(
                    "It will have to be paired again before it can connect, \
                     and the space it was keeping on the desk is given back.",
                )
                .font(theme::body())
                .color(palette.dim),
            );
            ui.add_space(16.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Forget").clicked() {
                    done = true;
                }
                if ui.button("Cancel").clicked() {
                    done = true;
                    // Nothing to undo: forgetting has not happened yet.
                    self.forgetting = None;
                }
            });
        });
        if done && self.forgetting.is_some() {
            self.forget(&name, device);
            self.forgetting = None;
        }
    }

    fn show_sheet(&mut self, ctx: &egui::Context, palette: &Palette) {
        let Some(mut sheet) = self.sheet.take() else {
            return;
        };
        let pairing = &mut self.pairing;
        let me = self.store.config.identity.name.clone();
        let port = self.store.config.network.port;
        let mut keep = true;

        egui::Modal::new(egui::Id::new("pairing")).show(ctx, |ui| {
            ui.set_width(440.0);
            match pairing.step.clone() {
                Step::Idle => {
                    heading(ui, palette, "Pair a machine");
                    told(
                        ui,
                        palette,
                        "Pairing takes both machines. Start it here, then start it on the other one.",
                    );
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        let field = egui::TextEdit::singleline(&mut sheet.host)
                            .hint_text("Name or address of the other machine")
                            .desired_width(250.0);
                        ui.add(field);
                        let named = !sheet.host.trim().is_empty();
                        if ui.add_enabled(named, egui::Button::new("Reach out")).clicked() {
                            pairing.reach(ui.ctx(), me.clone(), pairing::with_port(&sheet.host, port));
                        }
                    });
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Wait for it to reach out").clicked() {
                            pairing.listen(ui.ctx(), me.clone(), port);
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Cancel").clicked() {
                                keep = false;
                            }
                        });
                    });
                }
                Step::Listening(port) => {
                    heading(ui, palette, "Waiting for the other machine");
                    told(
                        ui,
                        palette,
                        &format!(
                            "Start pairing on it and point it at this machine, on port {port}."
                        ),
                    );
                    ui.add_space(16.0);
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                }
                Step::Reaching(host) => {
                    heading(ui, palette, "Reaching out");
                    told(ui, palette, &format!("Waiting for {host} to answer."));
                    ui.add_space(16.0);
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                }
                Step::Compare { code, peer } => {
                    heading(ui, palette, "Is this the same code?");
                    show_code(ui, palette, &code);
                    told(
                        ui,
                        palette,
                        &format!(
                            "{peer} should be showing these same six digits. If it is showing \
                             anything else, something is relaying the connection: do not continue."
                        ),
                    );
                    ui.add_space(18.0);
                    ui.horizontal(|ui| {
                        if ui.button("They match").clicked() {
                            pairing.answer(true);
                        }
                        if ui.button("They don't").clicked() {
                            pairing.answer(false);
                        }
                    });
                }
                Step::Paired(name) => {
                    heading(ui, palette, &format!("Paired with {name}"));
                    told(
                        ui,
                        palette,
                        "It can connect from now on. Its screens appear on the desk once it does.",
                    );
                    ui.add_space(16.0);
                    if ui.button("Done").clicked() {
                        keep = false;
                    }
                }
                Step::Refused => {
                    heading(ui, palette, "Not paired");
                    told(ui, palette, "Nothing was recorded, and nothing was changed.");
                    ui.add_space(16.0);
                    if ui.button("Done").clicked() {
                        keep = false;
                    }
                }
                Step::Failed(why) => {
                    heading(ui, palette, "Pairing did not finish");
                    told(ui, palette, &why);
                    ui.add_space(16.0);
                    if ui.button("Done").clicked() {
                        keep = false;
                    }
                }
            }
        });

        match keep {
            true => self.sheet = Some(sheet),
            false => self.pairing.cancel(),
        }
    }
}

/// A machine with no way to read its displays still has a desk to arrange.
struct NoDisplays;

impl Monitors for NoDisplays {
    fn monitors(&mut self) -> smkvm_input::Result<Vec<smkvm_layout::Monitor>> {
        Ok(Vec::new())
    }
}

fn tabs(ui: &mut egui::Ui, palette: &Palette, tab: &mut Tab) {
    egui::Panel::top("tabs")
        .frame(
            egui::Frame::new()
                .fill(palette.page)
                .inner_margin(Margin::symmetric(20, 12)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (which, name) in [
                    (Tab::Desk, "Desk"),
                    (Tab::Machines, "Machines"),
                    (Tab::Settings, "Settings"),
                ] {
                    if ui.selectable_label(*tab == which, name).clicked() {
                        *tab = which;
                    }
                }
            });
        });
}

fn caption(ui: &mut egui::Ui, palette: &Palette, note: &str) {
    egui::Panel::bottom("caption")
        .frame(
            egui::Frame::new()
                .fill(palette.page)
                .inner_margin(Margin::symmetric(20, 12)),
        )
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(note)
                    .font(theme::small())
                    .color(palette.dim),
            );
        });
}

/// A column of readable width, with air around it.
fn page(ui: &mut egui::Ui, palette: &Palette, add: impl FnOnce(&mut egui::Ui)) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(palette.page)
                .inner_margin(Margin::symmetric(24, 16)),
        )
        .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let width = ui.available_width().min(READABLE);
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 0.0),
                    Layout::top_down(Align::Min),
                    add,
                );
            });
        });
}

fn heading(ui: &mut egui::Ui, palette: &Palette, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .font(theme::title())
            .color(palette.text),
    );
    ui.add_space(8.0);
}

fn told(ui: &mut egui::Ui, palette: &Palette, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .font(theme::body())
            .color(palette.dim),
    );
}

/// The whole job of this number is being compared against another screen
/// across a desk, so it is given the room to be read at a glance and grouped
/// the way six digits are read aloud.
fn show_code(ui: &mut egui::Ui, palette: &Palette, code: &str) {
    ui.add_space(14.0);
    let grouped = match code.len() {
        6 => format!("{}  {}", &code[..3], &code[3..]),
        _ => code.to_string(),
    };
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 68.0), egui::Sense::hover());
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        grouped,
        theme::code(),
        palette.text,
    );
    ui.add_space(14.0);
}
