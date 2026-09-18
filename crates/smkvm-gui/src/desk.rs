//! What is on the desk, and who each piece of it belongs to.
//!
//! The desk is assembled from the two things that are actually known: the
//! displays this machine has, straight from the operating system, and where
//! the configuration says every machine's screens sit. What is connected comes
//! from the daemon's report when one is running, and when none is, nothing is
//! claimed to be.
//!
//! A screen that is configured but not here is still drawn, in its place. Its
//! space is reserved rather than empty -- the server treats it as a wall, and a
//! desk that showed open ground there would be describing a different machine
//! from the one the person is sitting at.

use smkvm_config::status::{MachineState, Status};
use smkvm_config::{Config, MonitorPlacement};
use smkvm_layout::{DeviceId, Layout, Monitor, Point, Rect};

/// The name that stands for whichever monitor the system calls primary.
///
/// A machine with one screen should not need its Windows display path, some
/// forty characters of it, written into a configuration file. The daemon
/// resolves the word the same way, in `smkvm_core::Placement::PRIMARY`.
pub const PRIMARY: &str = "primary";

/// Whether a screen is somewhere the cursor can actually go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Here, and able to take the cursor.
    Here,
    /// Here, but not accepting input at the moment -- on Windows, a prompt or
    /// the lock screen holding the desktop.
    Held,
    /// Configured, and not here. Its space is a wall.
    Away,
}

/// One screen in its place on the global desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    pub machine: String,
    /// The owning machine's own name for the output, or [`PRIMARY`].
    pub monitor: String,
    pub global: Rect,
    pub presence: Presence,
    /// What the monitor calls itself, where the machine knows.
    pub label: Option<String>,
}

/// Every screen on the desk, in a stable order: this machine first, then the
/// others as the configuration lists them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Desk {
    pub screens: Vec<Screen>,
    /// This machine's name, as the configuration gives it.
    pub here: String,
}

impl Desk {
    pub fn build(
        here: &str,
        device: DeviceId,
        displays: &[Monitor],
        config: &Config,
        status: Option<&Status>,
    ) -> Desk {
        let mut screens = local(here, device, displays, config);
        for machine in config.screen.iter().filter(|s| s.name != here) {
            let presence = presence_of(&machine.name, status);
            screens.extend(machine.monitor.iter().map(|placement| Screen {
                machine: machine.name.clone(),
                monitor: placement.id.clone(),
                global: placement.rect(),
                presence,
                // Only the machine itself knows what its monitors are called,
                // and the configuration does not carry it.
                label: None,
            }));
        }
        Desk {
            screens,
            here: here.to_string(),
        }
    }

    /// Everything placed, taken together.
    pub fn bounds(&self) -> Rect {
        self.screens
            .iter()
            .fold(Rect::new(0, 0, 0, 0), |all, s| all.union(&s.global))
    }

    pub fn find(&self, machine: &str, monitor: &str) -> Option<usize> {
        self.screens
            .iter()
            .position(|s| s.machine == machine && s.monitor == monitor)
    }

    /// Everything except one screen: what that screen snaps against, and what
    /// it must not be dropped on top of.
    pub fn others(&self, except: usize) -> Vec<Rect> {
        self.screens
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != except)
            .map(|(_, s)| s.global)
            .collect()
    }

    /// How a machine's screens should be recorded once one of them has been
    /// dragged to `to`.
    ///
    /// Every screen of that machine is written, not just the moved one. Left
    /// out, an unplaced sibling would be arranged automatically against its
    /// new anchor and quietly follow it across the desk, which is not what
    /// dragging one screen looks like it should do -- and a machine that is
    /// not here has no automatic placement at all, so the two would behave
    /// differently for no reason the person could see.
    pub fn moved(&self, index: usize, to: Point) -> Option<(String, Vec<MonitorPlacement>)> {
        let moved = self.screens.get(index)?;
        let placements = self
            .screens
            .iter()
            .enumerate()
            .filter(|(_, s)| s.machine == moved.machine)
            .map(|(i, s)| {
                let at = if i == index {
                    Rect::new(to.x, to.y, s.global.w, s.global.h)
                } else {
                    s.global
                };
                crate::store::placement(&s.monitor, at)
            })
            .collect();
        Some((moved.machine.clone(), placements))
    }
}

/// This machine's own screens: the displays it has, where the configuration
/// puts them, and anything the configuration places that is not plugged in.
fn local(here: &str, device: DeviceId, displays: &[Monitor], config: &Config) -> Vec<Screen> {
    let configured = config.screen.iter().find(|s| s.name == here);

    // Built through the same type the daemon arranges with, so a machine whose
    // screens the configuration says nothing about lands where it would land
    // for real rather than somewhere this window invented.
    let mut layout = Layout::new(config.behavior.edge_overflow);
    layout.report_monitors(device, here, displays.to_vec());
    for placement in configured.iter().flat_map(|s| s.monitor.iter()) {
        if let Some(display) = attached(displays, &placement.id) {
            layout.place_rect(device, &display.id, placement.rect());
        }
    }
    layout.auto_place();

    let mut screens: Vec<Screen> = displays
        .iter()
        .filter_map(|display| {
            let global = layout.placement(device, &display.id)?;
            Some(Screen {
                machine: here.to_string(),
                // Kept as the configuration writes it, so saving does not
                // silently replace `primary` with a device path.
                monitor: named_as(configured, display),
                global,
                presence: Presence::Here,
                label: display.label.clone(),
            })
        })
        .collect();

    // A monitor the configuration places but that is not plugged in keeps its
    // space: the server walls it off, and unplugging one and putting it back
    // is meant to restore where it was.
    screens.extend(
        configured
            .iter()
            .flat_map(|s| s.monitor.iter())
            .filter(|placement| attached(displays, &placement.id).is_none())
            .map(|placement| Screen {
                machine: here.to_string(),
                monitor: placement.id.clone(),
                global: placement.rect(),
                presence: Presence::Away,
                label: None,
            }),
    );
    screens
}

fn attached<'a>(displays: &'a [Monitor], id: &str) -> Option<&'a Monitor> {
    displays
        .iter()
        .find(|m| m.id.as_str() == id || (id == PRIMARY && m.primary))
}

fn named_as(configured: Option<&smkvm_config::Screen>, display: &Monitor) -> String {
    let written = configured
        .iter()
        .flat_map(|s| s.monitor.iter())
        .find(|p| attached(std::slice::from_ref(display), &p.id).is_some());
    match written {
        Some(placement) => placement.id.clone(),
        None => display.id.0.clone(),
    }
}

fn presence_of(machine: &str, status: Option<&Status>) -> Presence {
    // Without a daemon nothing is connected, which is the same ground the
    // cursor finds as a machine that is switched off.
    let Some(status) = status else {
        return Presence::Away;
    };
    match status.machines.iter().find(|m| m.name == machine) {
        Some(m) => match m.state {
            MachineState::Connected => Presence::Here,
            MachineState::Suspended => Presence::Held,
            MachineState::Away => Presence::Away,
        },
        None => Presence::Away,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smkvm_config::status::Machine;
    use smkvm_proto::Role;

    fn here() -> DeviceId {
        DeviceId::from_bytes([7; 32])
    }

    fn display(id: &str, local: Rect, primary: bool) -> Monitor {
        let mut m = Monitor::new(id, local);
        m.primary = primary;
        m
    }

    fn config(machines: Vec<smkvm_config::Screen>) -> Config {
        let mut config = Config::fresh("this-one", Role::Server);
        config.screen = machines;
        config
    }

    fn machine(name: &str, monitors: Vec<MonitorPlacement>) -> smkvm_config::Screen {
        smkvm_config::Screen {
            name: name.into(),
            device: None,
            grid: None,
            monitor: monitors,
        }
    }

    fn at(id: &str, rect: Rect) -> MonitorPlacement {
        crate::store::placement(id, rect)
    }

    #[test]
    fn a_machine_nobody_has_arranged_yet_keeps_its_own_shape() {
        let displays = vec![
            display("DP-2", Rect::new(0, 0, 2560, 1440), true),
            display("DP-0", Rect::new(2560, 0, 2560, 1440), false),
        ];
        let desk = Desk::build("this-one", here(), &displays, &config(vec![]), None);
        assert_eq!(desk.screens.len(), 2);
        let left = &desk.screens[0];
        let right = &desk.screens[1];
        assert_eq!(right.global.x - left.global.x, 2560, "still side by side");
        assert_eq!(left.global.y, right.global.y);
        assert!(desk.screens.iter().all(|s| s.presence == Presence::Here));
    }

    #[test]
    fn the_configuration_decides_where_this_machine_sits() {
        let displays = vec![display("DP-2", Rect::new(0, 0, 2560, 1440), true)];
        let config = config(vec![machine(
            "this-one",
            vec![at("DP-2", Rect::new(640, 1080, 2560, 1440))],
        )]);
        let desk = Desk::build("this-one", here(), &displays, &config, None);
        assert_eq!(desk.screens[0].global, Rect::new(640, 1080, 2560, 1440));
    }

    #[test]
    fn primary_stands_for_the_screen_the_system_calls_primary() {
        // The identifier a Windows machine reports is a device path, which is
        // exactly why the short word exists.
        let path = r"\\?\DISPLAY#SAM0F94#5&1ecb2b1&0&UID4353#{e6f07b5f}";
        let displays = vec![display(path, Rect::new(0, 0, 1920, 1080), true)];
        let config = config(vec![machine(
            "this-one",
            vec![at(PRIMARY, Rect::new(3840, 0, 1920, 1080))],
        )]);
        let desk = Desk::build("this-one", here(), &displays, &config, None);
        assert_eq!(desk.screens.len(), 1, "one screen, not one of each name");
        assert_eq!(desk.screens[0].global, Rect::new(3840, 0, 1920, 1080));
        assert_eq!(
            desk.screens[0].monitor, PRIMARY,
            "saving should not replace the short word with the device path"
        );
    }

    #[test]
    fn a_monitor_that_is_unplugged_keeps_its_space() {
        let displays = vec![display("DP-2", Rect::new(0, 0, 2560, 1440), true)];
        let config = config(vec![machine(
            "this-one",
            vec![
                at("DP-2", Rect::new(0, 0, 2560, 1440)),
                at("DP-0", Rect::new(2560, 0, 2560, 1440)),
            ],
        )]);
        let desk = Desk::build("this-one", here(), &displays, &config, None);
        let gone = desk.screens.iter().find(|s| s.monitor == "DP-0").unwrap();
        assert_eq!(gone.presence, Presence::Away);
        assert_eq!(gone.global, Rect::new(2560, 0, 2560, 1440));
    }

    #[test]
    fn another_machine_is_away_until_a_daemon_says_otherwise() {
        let displays = vec![display("DP-2", Rect::new(0, 0, 2560, 1440), true)];
        let config = config(vec![
            machine("this-one", vec![at("DP-2", Rect::new(0, 1080, 2560, 1440))]),
            machine("up-there", vec![at(PRIMARY, Rect::new(640, 0, 1920, 1080))]),
        ]);

        let unknown = Desk::build("this-one", here(), &displays, &config, None);
        let theirs = unknown.screens.iter().find(|s| s.machine == "up-there");
        assert_eq!(theirs.unwrap().presence, Presence::Away);

        for (state, expected) in [
            (MachineState::Connected, Presence::Here),
            (MachineState::Suspended, Presence::Held),
            (MachineState::Away, Presence::Away),
        ] {
            let status = Status::new(
                Role::Server,
                "this-one",
                vec![Machine {
                    name: "up-there".into(),
                    device: None,
                    state,
                }],
            );
            let desk = Desk::build("this-one", here(), &displays, &config, Some(&status));
            let theirs = desk.screens.iter().find(|s| s.machine == "up-there");
            assert_eq!(theirs.unwrap().presence, expected, "{state:?}");
        }
    }

    #[test]
    fn a_stale_report_is_worth_nothing() {
        // The window only passes on a report it believes; this is the shape of
        // what it does when there is none.
        let displays = vec![display("DP-2", Rect::new(0, 0, 2560, 1440), true)];
        let config = config(vec![machine(
            "up-there",
            vec![at(PRIMARY, Rect::new(0, 0, 1920, 1080))],
        )]);
        let desk = Desk::build("this-one", here(), &displays, &config, None);
        assert!(desk
            .screens
            .iter()
            .all(|s| s.presence == Presence::Away || s.machine == "this-one"));
    }

    #[test]
    fn dragging_one_screen_writes_down_all_of_that_machines_screens() {
        let displays = vec![
            display("DP-2", Rect::new(0, 0, 2560, 1440), true),
            display("DP-0", Rect::new(2560, 0, 2560, 1440), false),
        ];
        let desk = Desk::build("this-one", here(), &displays, &config(vec![]), None);
        let moved = desk.find("this-one", "DP-0").unwrap();
        let (machine, placements) = desk.moved(moved, Point::new(0, 1440)).unwrap();

        assert_eq!(machine, "this-one");
        assert_eq!(placements.len(), 2, "the sibling is pinned where it was");
        let by = |id: &str| {
            placements
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.rect())
                .unwrap()
        };
        assert_eq!(by("DP-0"), Rect::new(0, 1440, 2560, 1440));
        assert_eq!(
            by("DP-2"),
            desk.screens[desk.find("this-one", "DP-2").unwrap()].global
        );
    }

    #[test]
    fn dragging_leaves_other_machines_where_they_are() {
        let displays = vec![display("DP-2", Rect::new(0, 0, 2560, 1440), true)];
        let config = config(vec![
            machine("this-one", vec![at("DP-2", Rect::new(0, 1080, 2560, 1440))]),
            machine("up-there", vec![at(PRIMARY, Rect::new(0, 0, 1920, 1080))]),
        ]);
        let desk = Desk::build("this-one", here(), &displays, &config, None);
        let theirs = desk.find("up-there", PRIMARY).unwrap();
        let (machine, placements) = desk.moved(theirs, Point::new(640, 0)).unwrap();
        assert_eq!(machine, "up-there");
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].rect(), Rect::new(640, 0, 1920, 1080));
    }

    #[test]
    fn everything_placed_is_inside_the_bounds() {
        let displays = vec![display("DP-2", Rect::new(0, 0, 2560, 1440), true)];
        let config = config(vec![machine(
            "up-there",
            vec![at(PRIMARY, Rect::new(-1920, -1080, 1920, 1080))],
        )]);
        let desk = Desk::build("this-one", here(), &displays, &config, None);
        let bounds = desk.bounds();
        for screen in &desk.screens {
            assert!(bounds.contains(screen.global.origin()), "{screen:?}");
        }
    }

    #[test]
    fn a_machine_with_nothing_attached_and_nothing_configured_has_an_empty_desk() {
        let desk = Desk::build("this-one", here(), &[], &config(vec![]), None);
        assert!(desk.screens.is_empty());
        assert!(desk.bounds().is_empty());
    }
}
