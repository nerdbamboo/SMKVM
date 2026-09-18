//! Who this machine has been introduced to, and who is here now.
//!
//! Three things know about a machine and none of them knows everything: the
//! list of paired machines holds its key, the configuration holds its place on
//! the desk, and the running daemon knows whether it is connected. A machine
//! can be in any of them without being in the others, and each of those is a
//! state worth telling somebody about.

use egui::{Align, Layout};
use smkvm_config::status::{MachineState, Status};
use smkvm_config::Config;
use smkvm_layout::DeviceId;
use smkvm_net::trust::Trust;

use crate::theme::{self, Palette};

/// A machine as the window knows it, from whichever sources mention it.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub name: String,
    pub device: Option<DeviceId>,
    /// Paired, and so able to complete a handshake at all.
    pub paired: bool,
    /// What the daemon says, when one is running.
    pub state: Option<MachineState>,
    /// Whether the cursor is on it right now.
    pub active: bool,
    /// Whether it has any screens on the desk.
    pub placed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Pair,
    Forget { name: String, device: DeviceId },
}

/// Every machine but this one, paired ones first and each group by name.
pub fn listed(trust: &Trust, config: &Config, status: Option<&Status>, here: &str) -> Vec<Listed> {
    let placed = |name: &str| {
        config
            .screen
            .iter()
            .any(|s| s.name == name && !s.monitor.is_empty())
    };
    let state = |name: &str| status?.machine(name).map(|m| m.state);
    let active = |name: &str| {
        status
            .and_then(|s| s.machine(name))
            .is_some_and(|m| m.active)
    };

    let mut machines: Vec<Listed> = trust
        .peers()
        .filter(|peer| peer.name != here)
        .map(|peer| Listed {
            name: peer.name.clone(),
            device: Some(peer.id),
            paired: true,
            state: state(&peer.name),
            active: active(&peer.name),
            placed: placed(&peer.name),
        })
        .collect();

    // A machine an import brought across is known by name long before it has
    // been paired, and saying nothing about it would make it look as though
    // the import had not worked.
    for screen in &config.screen {
        if screen.name == here || machines.iter().any(|m| m.name == screen.name) {
            continue;
        }
        machines.push(Listed {
            name: screen.name.clone(),
            device: screen.device,
            paired: false,
            state: state(&screen.name),
            active: active(&screen.name),
            placed: !screen.monitor.is_empty(),
        });
    }

    machines.sort_by(|a, b| b.paired.cmp(&a.paired).then_with(|| a.name.cmp(&b.name)));
    machines
}

pub fn show(
    ui: &mut egui::Ui,
    palette: &Palette,
    config: &Config,
    here: &str,
    running: bool,
    machines: &[Listed],
) -> Option<Action> {
    let mut action = None;
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Machines")
            .font(theme::title())
            .color(palette.text),
    );
    ui.add_space(14.0);

    theme::card(palette).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(here)
                        .font(theme::body())
                        .color(palette.text),
                );
                ui.label(
                    egui::RichText::new(role_of(config))
                        .font(theme::small())
                        .color(palette.dim),
                );
                if !running {
                    ui.label(
                        egui::RichText::new("SMKVM is not running, so nothing is connected.")
                            .font(theme::small())
                            .color(palette.dim),
                    );
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Pair a machine").clicked() {
                    action = Some(Action::Pair);
                }
            });
        });
    });

    ui.add_space(14.0);
    theme::card(palette).show(ui, |ui| {
        if machines.is_empty() {
            ui.label(
                egui::RichText::new("No other machines yet.")
                    .font(theme::body())
                    .color(palette.text),
            );
            ui.label(
                egui::RichText::new("Pair one here, and start pairing on it too.")
                    .font(theme::small())
                    .color(palette.dim),
            );
            return;
        }
        for (i, machine) in machines.iter().enumerate() {
            if i > 0 {
                ui.add_space(6.0);
                let (rule, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 1.0),
                    egui::Sense::hover(),
                );
                ui.painter()
                    .hline(rule.x_range(), rule.center().y, theme::hairline(palette));
                ui.add_space(6.0);
            }
            if let Some(forget) = row(ui, palette, machine, running) {
                action = Some(forget);
            }
        }
    });
    action
}

fn row(ui: &mut egui::Ui, palette: &Palette, machine: &Listed, running: bool) -> Option<Action> {
    let mut action = None;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(&machine.name)
                    .font(theme::body())
                    .color(palette.text),
            );
            if let Some(note) = second_line(machine) {
                ui.label(
                    egui::RichText::new(note)
                        .font(theme::small())
                        .color(palette.dim),
                );
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some(device) = machine.device.filter(|_| machine.paired) {
                let forget = egui::Button::new(
                    egui::RichText::new("Forget")
                        .font(theme::small())
                        .color(palette.dim),
                )
                .frame(false);
                if ui.add(forget).clicked() {
                    action = Some(Action::Forget {
                        name: machine.name.clone(),
                        device,
                    });
                }
            }
            ui.add_space(10.0);
            // Left blank with nothing running: the state of a machine nobody
            // is talking to is not news, and the panel above has already said
            // why the column is empty.
            if running {
                let (word, colour) = match machine.state {
                    Some(MachineState::Connected) if machine.active => {
                        ("Has the cursor", palette.accent)
                    }
                    Some(MachineState::Connected) => ("Connected", palette.text),
                    Some(MachineState::Suspended) => ("Not taking input", palette.warn),
                    _ => ("Away", palette.dim),
                };
                ui.label(egui::RichText::new(word).font(theme::small()).color(colour));
            }
        });
    });
    action
}

fn second_line(machine: &Listed) -> Option<&'static str> {
    match (machine.paired, machine.placed) {
        (false, _) => Some("Known by name, not paired yet."),
        (true, false) => Some("No screens on the desk yet."),
        (true, true) => None,
    }
}

fn role_of(config: &Config) -> String {
    match config.network.role {
        smkvm_proto::Role::Server => "Shares its keyboard and mouse.".to_string(),
        smkvm_proto::Role::Client => match &config.network.server {
            Some(server) => format!("Receives from {server}."),
            None => "Receives from another machine.".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smkvm_config::status::Machine;
    use smkvm_config::Screen;
    use smkvm_proto::Role;

    fn trust_with(names: &[&str]) -> Trust {
        let mut trust = Trust::new();
        for (i, name) in names.iter().enumerate() {
            trust.add(vec![i as u8 + 1; 32], *name);
        }
        trust
    }

    fn config_with(screens: &[(&str, bool)]) -> Config {
        let mut config = Config::fresh("this-one", Role::Server);
        config.screen = screens
            .iter()
            .map(|(name, placed)| Screen {
                name: (*name).into(),
                device: None,
                grid: None,
                monitor: match placed {
                    true => vec![crate::store::placement(
                        "primary",
                        smkvm_layout::Rect::new(0, 0, 1920, 1080),
                    )],
                    false => Vec::new(),
                },
            })
            .collect();
        config
    }

    #[test]
    fn a_paired_machine_is_listed_even_with_nothing_else_known_about_it() {
        let listed = listed(
            &trust_with(&["over-there"]),
            &config_with(&[]),
            None,
            "this-one",
        );
        assert_eq!(listed.len(), 1);
        assert!(listed[0].paired);
        assert!(!listed[0].placed);
        assert_eq!(listed[0].state, None);
        assert_eq!(second_line(&listed[0]), Some("No screens on the desk yet."));
    }

    #[test]
    fn a_machine_an_import_brought_across_is_not_hidden() {
        let listed = listed(
            &Trust::new(),
            &config_with(&[("from-barrier", false)]),
            None,
            "this-one",
        );
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].paired);
        assert_eq!(
            second_line(&listed[0]),
            Some("Known by name, not paired yet.")
        );
    }

    #[test]
    fn a_machine_in_both_places_is_listed_once() {
        let listed = listed(
            &trust_with(&["over-there"]),
            &config_with(&[("over-there", true)]),
            None,
            "this-one",
        );
        assert_eq!(listed.len(), 1);
        assert!(listed[0].paired && listed[0].placed);
        assert_eq!(second_line(&listed[0]), None);
    }

    #[test]
    fn this_machine_is_not_one_of_the_others() {
        let listed = listed(
            &trust_with(&["this-one", "over-there"]),
            &config_with(&[("this-one", true)]),
            None,
            "this-one",
        );
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "over-there");
    }

    #[test]
    fn what_the_daemon_says_is_carried_through() {
        let status = Status::new(
            Role::Server,
            "this-one",
            vec![Machine::new("over-there", None, MachineState::Suspended)],
        );
        let listed = listed(
            &trust_with(&["over-there", "elsewhere"]),
            &config_with(&[]),
            Some(&status),
            "this-one",
        );
        let by = |name: &str| listed.iter().find(|m| m.name == name).unwrap().state;
        assert_eq!(by("over-there"), Some(MachineState::Suspended));
        assert_eq!(by("elsewhere"), None, "a machine it did not mention");
    }

    #[test]
    fn paired_machines_come_first_and_each_group_reads_in_order() {
        let listed = listed(
            &trust_with(&["zeta", "alpha"]),
            &config_with(&[("beta", false), ("omega", false)]),
            None,
            "this-one",
        );
        let names: Vec<&str> = listed.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta", "beta", "omega"]);
    }
}
