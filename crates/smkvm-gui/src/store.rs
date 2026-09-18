//! Reading and writing what the daemon reads and writes.
//!
//! The window is one more reader of the same files, never a second source of
//! truth: the arrangement it draws is the one in `smkvm.toml`, the machines it
//! lists are the ones in `peers.toml`, and what is connected comes from the
//! report the running daemon leaves behind.
//!
//! Saving only rewrites the keys the window owns. The file belongs to the
//! person -- they may have put comments in it, and the order of what is there
//! is theirs -- so reserialising the whole parsed configuration would drop
//! every comment and rewrite every line, and moving one screen would produce a
//! diff nobody could read.

use std::path::{Path, PathBuf};

use smkvm_config::status::Status;
use smkvm_config::{paths, Config, MonitorPlacement, Screen};
use smkvm_input::Monitors;
use smkvm_layout::{EdgeOverflow, Monitor};
use smkvm_net::identity::Identity;
use smkvm_net::trust::Trust;
use smkvm_proto::Role;
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] smkvm_config::ConfigError),
    #[error(transparent)]
    Net(#[from] smkvm_net::Error),
    #[error("reading this machine's displays: {0}")]
    Displays(#[from] smkvm_input::InputError),
    #[error("{path} is no longer valid TOML, so nothing was written. Check it by hand.")]
    NotToml { path: PathBuf },
    #[error("the {what} part of {path} is written in a form this window cannot edit, so it has been left alone. Change it by hand.")]
    Reshaped { path: PathBuf, what: &'static str },
    #[error("writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Everything on disk, as of the last time it was read.
pub struct Store {
    config_path: PathBuf,
    pub config: Config,
    pub trust: Trust,
    pub identity: Identity,
    /// This machine's displays, straight from the operating system. The one
    /// part of the desk that needs no daemon running to be known.
    pub displays: Vec<Monitor>,
    /// What the daemon says it is connected to, when one is running and has
    /// said so recently enough to be describing now.
    pub status: Option<Status>,
}

impl Store {
    pub fn open(displays: Vec<Monitor>) -> Result<Store, Error> {
        let config_path = paths::config_file();
        // No configuration is the state a machine starts in, not a failure.
        // Nothing is written until something is actually changed.
        let config = match config_path.exists() {
            true => Config::load(&config_path)?,
            false => Config::fresh(paths::default_name(), Role::Server),
        };
        Ok(Store {
            config_path,
            config,
            trust: Trust::load(&paths::peers_file())?,
            identity: Identity::load_or_create(&paths::identity_file())?,
            displays,
            status: current_status(),
        })
    }

    /// Ask the operating system what this machine has, and the daemon what it
    /// is connected to.
    pub fn refresh(&mut self, displays: &mut dyn Monitors) {
        if let Ok(displays) = displays.monitors() {
            self.displays = displays;
        }
        self.status = current_status();
    }

    pub fn this_machine(&self) -> &str {
        &self.config.identity.name
    }

    /// Put the configuration's own machine in the list of screens if it is not
    /// there, so the desk has somewhere to record where its displays sit.
    pub fn screen_mut(&mut self, machine: &str) -> &mut Screen {
        if let Some(i) = self.config.screen.iter().position(|s| s.name == machine) {
            return &mut self.config.screen[i];
        }
        self.config.screen.push(Screen {
            name: machine.to_string(),
            device: None,
            grid: None,
            monitor: Vec::new(),
        });
        self.config.screen.last_mut().expect("just pushed one")
    }

    /// A machine that has been forgotten gives its space back.
    ///
    /// Leaving its screens in the configuration would leave the cursor walking
    /// into a wall where a machine used to be, for ever and for no reason.
    pub fn drop_machine(&mut self, machine: &str) {
        self.config.screen.retain(|s| s.name != machine);
    }

    pub fn save_config(&self) -> Result<(), Error> {
        let text = match std::fs::read_to_string(&self.config_path) {
            // Re-read rather than kept from startup: someone may have edited
            // the file in the meantime, and their work should survive this.
            Ok(text) => apply(&text, &self.config).map_err(|trouble| match trouble {
                Trouble::NotToml => Error::NotToml {
                    path: self.config_path.clone(),
                },
                Trouble::Reshaped(what) => Error::Reshaped {
                    path: self.config_path.clone(),
                    what,
                },
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.config.to_toml()?,
            Err(source) => {
                return Err(Error::Io {
                    path: self.config_path.clone(),
                    source,
                })
            }
        };
        write_atomically(&self.config_path, &text)
    }

    pub fn save_trust(&self) -> Result<(), Error> {
        Ok(self.trust.save(&paths::peers_file())?)
    }
}

/// The daemon's report, if one is running.
///
/// A report older than the daemon's refresh interval describes a machine that
/// has stopped, so it is worth nothing: saying a machine is connected when
/// nothing is running is worse than admitting nothing is known.
fn current_status() -> Option<Status> {
    Status::current(&paths::status_file())
}

/// Write through a neighbouring file and move it into place, so a daemon
/// starting at the wrong moment never reads half a configuration.
fn write_atomically(path: &Path, text: &str) -> Result<(), Error> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| Error::Io { path, source }
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io(dir))?;
    }
    let beside = path.with_extension("toml.new");
    std::fs::write(&beside, text).map_err(io(&beside))?;
    std::fs::rename(&beside, path).map_err(io(path))
}

/// What stopped the file being edited in place. Either can only happen if it
/// changed underneath a running window, or was written by hand in a shape the
/// window does not produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trouble {
    NotToml,
    Reshaped(&'static str),
}

/// Make the file's text say what `config` says, for the keys the window owns,
/// and leave every other byte of it alone.
fn apply(text: &str, config: &Config) -> Result<String, Trouble> {
    let mut doc: DocumentMut = text.parse().map_err(|_| Trouble::NotToml)?;

    let behavior = section(&mut doc, "behavior").ok_or(Trouble::Reshaped("behavior"))?;
    behavior["switch_delay_ms"] = value(i64::from(config.behavior.switch_delay_ms));
    // Spelled out rather than handed to a serialiser, because how these words
    // are written is part of the file's format. The test that parses the
    // result back is what keeps the spelling honest.
    behavior["edge_overflow"] = value(match config.behavior.edge_overflow {
        EdgeOverflow::Clamp => "clamp",
        EdgeOverflow::Block => "block",
    });

    let clipboard = section(&mut doc, "clipboard").ok_or(Trouble::Reshaped("clipboard"))?;
    clipboard["enabled"] = value(config.clipboard.enabled);

    apply_screens(&mut doc, &config.screen).ok_or(Trouble::Reshaped("screen"))?;
    Ok(doc.to_string())
}

fn section<'a>(doc: &'a mut DocumentMut, name: &str) -> Option<&'a mut Table> {
    doc.entry(name)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
}

fn apply_screens(doc: &mut DocumentMut, screens: &[Screen]) -> Option<()> {
    // A configuration with no machines in it says `screen = []`, which is the
    // same thing a list of tables says with nothing in it. It only has to
    // change shape once there is a machine to put there.
    let empty_list = doc
        .get("screen")
        .and_then(|item| item.as_array())
        .is_some_and(|list| list.is_empty());
    if screens.is_empty() && (empty_list || doc.get("screen").is_none()) {
        return Some(());
    }
    if empty_list {
        doc.insert("screen", Item::ArrayOfTables(ArrayOfTables::new()));
    }

    let list = doc
        .entry("screen")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()?;

    list.retain(|entry| {
        screens
            .iter()
            .any(|s| named(entry) == Some(s.name.as_str()))
    });
    for screen in screens {
        // Bound first: the iterator holds a borrow of the list until the end
        // of the statement it appears in.
        let found = list
            .iter()
            .position(|e| named(e) == Some(screen.name.as_str()));
        match found {
            Some(i) => write_screen(list.get_mut(i)?, screen),
            None => {
                let mut entry = Table::new();
                write_screen(&mut entry, screen);
                list.push(entry);
            }
        }
    }
    Some(())
}

fn named(entry: &Table) -> Option<&str> {
    entry.get("name")?.as_str()
}

/// `grid` is left as it was found: it is a hint from an import about where a
/// machine sat in Barrier's arrangement, and nothing here has an opinion on it.
fn write_screen(entry: &mut Table, screen: &Screen) {
    entry["name"] = value(screen.name.as_str());
    if let Some(device) = screen.device {
        entry["device"] = value(device.to_hex());
    }
    let mut monitors = ArrayOfTables::new();
    for placement in &screen.monitor {
        let mut m = Table::new();
        m["id"] = value(placement.id.as_str());
        m["global"] = value(Array::from_iter(
            placement.global.iter().map(|n| i64::from(*n)),
        ));
        monitors.push(m);
    }
    entry.insert("monitor", Item::ArrayOfTables(monitors));
}

/// One monitor's place on the global desktop, as the configuration records it.
pub fn placement(id: &str, rect: smkvm_layout::Rect) -> MonitorPlacement {
    MonitorPlacement {
        id: id.to_string(),
        global: [rect.x, rect.y, rect.w, rect.h],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smkvm_layout::Rect;

    fn parse(text: &str) -> Config {
        Config::parse(text, Path::new("test.toml")).expect("the result should still be readable")
    }

    fn base() -> Config {
        let mut config = Config::fresh("this-one", Role::Server);
        config.behavior.switch_delay_ms = 250;
        config.behavior.edge_overflow = EdgeOverflow::Block;
        config.clipboard.enabled = false;
        config.screen = vec![Screen {
            name: "over-there".into(),
            device: None,
            grid: None,
            monitor: vec![placement("DP-1", Rect::new(1920, 0, 2560, 1440))],
        }];
        config
    }

    #[test]
    fn what_is_written_reads_back_as_what_was_asked_for() {
        let config = base();
        let written = apply(&config.to_toml().unwrap(), &config).unwrap();
        let read = parse(&written);
        assert_eq!(read.behavior.switch_delay_ms, 250);
        assert_eq!(read.behavior.edge_overflow, EdgeOverflow::Block);
        assert!(!read.clipboard.enabled);
        assert_eq!(read.screen, config.screen);
    }

    #[test]
    fn both_spellings_of_the_edge_survive_the_trip() {
        for overflow in [EdgeOverflow::Clamp, EdgeOverflow::Block] {
            let mut config = base();
            config.behavior.edge_overflow = overflow;
            let written = apply(&config.to_toml().unwrap(), &config).unwrap();
            assert_eq!(parse(&written).behavior.edge_overflow, overflow);
        }
    }

    #[test]
    fn comments_and_untouched_keys_are_left_alone() {
        let original = r#"
# The machine that owns the keyboard.
version = 1

[identity]
name = "this-one"  # trailing thought

[network]
role = "server"
port = 24810
# why we listen where we do
listen = ["192.168.1.10"]

[behavior]
switch_delay_ms = 0
release_keys_on_disconnect = true
"#;
        let mut config = parse(original);
        config.behavior.switch_delay_ms = 400;
        let written = apply(original, &config).unwrap();

        assert!(written.contains("# The machine that owns the keyboard."));
        assert!(written.contains("# trailing thought"));
        assert!(written.contains("# why we listen where we do"));
        assert!(written.contains("release_keys_on_disconnect = true"));
        assert!(written.contains("switch_delay_ms = 400"));
        assert_eq!(parse(&written).network.listen, vec!["192.168.1.10"]);
    }

    #[test]
    fn moving_a_screen_changes_only_where_it_is() {
        let mut config = base();
        let before = apply(&config.to_toml().unwrap(), &config).unwrap();
        config.screen[0].monitor[0] = placement("DP-1", Rect::new(1920, 1080, 2560, 1440));
        let after = apply(&before, &config).unwrap();

        let differing = before
            .lines()
            .zip(after.lines())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(before.lines().count(), after.lines().count());
        assert_eq!(differing, 1, "only the line holding the rectangle moved");
        assert_eq!(parse(&after).screen[0].monitor[0].rect().y, 1080);
    }

    #[test]
    fn a_machine_the_desk_no_longer_knows_stops_reserving_space() {
        let mut config = base();
        config.screen.push(Screen {
            name: "retired".into(),
            device: None,
            grid: None,
            monitor: vec![placement("primary", Rect::new(0, 0, 1920, 1080))],
        });
        let with_both = apply(&config.to_toml().unwrap(), &config).unwrap();
        assert!(with_both.contains("retired"));

        config.screen.retain(|s| s.name != "retired");
        let without = apply(&with_both, &config).unwrap();
        assert!(!without.contains("retired"));
        assert_eq!(parse(&without).screen.len(), 1);
    }

    #[test]
    fn a_machine_the_file_has_never_heard_of_is_added() {
        let mut config = base();
        let text = apply(&config.to_toml().unwrap(), &config).unwrap();
        config.screen.push(Screen {
            name: "newcomer".into(),
            device: None,
            grid: None,
            monitor: vec![placement("HDMI-1", Rect::new(-1920, 0, 1920, 1080))],
        });
        let grown = apply(&text, &config).unwrap();
        let read = parse(&grown);
        assert_eq!(read.screen.len(), 2);
        let added = read.screen.iter().find(|s| s.name == "newcomer").unwrap();
        assert_eq!(added.monitor[0].rect(), Rect::new(-1920, 0, 1920, 1080));
    }

    #[test]
    fn a_barrier_import_keeps_its_grid_hint() {
        let original = r#"
version = 1
[identity]
name = "this-one"
[network]
role = "server"

[[screen]]
name = "over-there"
grid = [1, 0]
"#;
        let mut config = parse(original);
        config.screen[0].monitor = vec![placement("DP-1", Rect::new(0, 0, 1920, 1080))];
        let written = apply(original, &config).unwrap();
        assert_eq!(parse(&written).screen[0].grid, Some([1, 0]));
    }

    #[test]
    fn a_configuration_with_no_machines_yet_gains_none() {
        let mut config = Config::fresh("this-one", Role::Server);
        config.screen.clear();
        let written = apply(&config.to_toml().unwrap(), &config).unwrap();
        assert!(!written.contains("[[screen]]"));
        assert!(parse(&written).screen.is_empty());
    }

    #[test]
    fn a_reshaped_file_is_reported_rather_than_flattened() {
        let config = base();
        assert_eq!(
            apply("behavior = 3\n", &config),
            Err(Trouble::Reshaped("behavior"))
        );
        assert_eq!(
            apply("screen = 3\n", &config),
            Err(Trouble::Reshaped("screen"))
        );
        assert_eq!(apply("[[[oops\n", &config), Err(Trouble::NotToml));
    }
}
