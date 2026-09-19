//! Migrating a real Barrier installation.
//!
//! The fixtures are a genuine `Barrier.conf` (addresses replaced) and a
//! server-side text config of the kind a multi-monitor user ends up
//! hand-writing. What matters is that the settings worth keeping come across,
//! that trust does not, and that the user is told what was left behind.

use smkvm_config::barrier::{Edge, Import, ServerConfig};
use smkvm_config::{Config, DEFAULT_PORT};
use smkvm_proto::Role;

const QT_SETTINGS: &str = include_str!("fixtures/Barrier.conf");
const SERVER_CONFIG: &str = include_str!("fixtures/barrier.conf");

fn import() -> Import {
    Import::from_qt_settings(QT_SETTINGS)
}

#[test]
fn the_machines_own_settings_come_across() {
    let i = import();
    assert_eq!(i.screen_name.as_deref(), Some("ubuntu-box"));
    assert!(!i.is_server, "this machine was configured as a client");
    assert_eq!(i.server_host.as_deref(), Some("192.0.2.10"));
    assert_eq!(i.port, Some(24800));
    assert_eq!(i.clipboard, Some(true));
    assert_eq!(i.drag_drop, Some(true));
}

#[test]
fn switch_timings_are_only_taken_when_they_were_enabled() {
    // The fixture has hasSwitchDelay=false with switchDelay=250 left over.
    // Importing the number regardless would introduce a delay the user had
    // switched off.
    let i = import();
    assert_eq!(i.switch_delay_ms, None);
    assert_eq!(i.switch_double_tap_ms, None);
}

#[test]
fn the_screen_grid_is_recovered_with_positions() {
    let i = import();
    let names: Vec<_> = i.grid.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, ["ubuntu-box", "WIN-STUDY"]);

    // numColumns=5, and Qt's indices are one-based: entries 8 and 9 are
    // columns 2 and 3 of row 1, i.e. side by side.
    assert_eq!(i.grid_columns, 5);
    assert_eq!((i.grid[0].column, i.grid[0].row), (2, 1));
    assert_eq!((i.grid[1].column, i.grid[1].row), (3, 1));
}

#[test]
fn the_user_is_told_what_was_not_carried_over() {
    let notes = import().notes.join("\n");
    assert!(
        notes.contains("paired again"),
        "must say fingerprints are not inherited:\n{notes}"
    );
    assert!(
        notes.contains("every interface"),
        "must flag the wide-open bind:\n{notes}"
    );
    assert!(
        notes.contains("client certificates"),
        "must flag that clients were never verified:\n{notes}"
    );
}

#[test]
fn a_text_config_with_partial_edges_parses() {
    let cfg = ServerConfig::parse(SERVER_CONFIG);
    assert_eq!(cfg.screens.len(), 3);
    assert_eq!(cfg.aliases["ubuntu-box"], vec!["linux-desk"]);
    assert_eq!(cfg.options["switchDelay"], "250");

    // The intervals the GUI cannot show, on both sides of the link.
    let up: Vec<_> = cfg
        .links
        .iter()
        .filter(|l| l.from == "ubuntu-box" && l.edge == Edge::Up)
        .collect();
    assert_eq!(up.len(), 2);
    assert_eq!(up[0].from_range, Some((0, 50)));
    assert_eq!(up[0].to, "WIN-STUDY");
    assert_eq!(up[1].from_range, Some((50, 100)));

    let down = cfg
        .links
        .iter()
        .find(|l| l.from == "WIN-STUDY" && l.edge == Edge::Down)
        .unwrap();
    assert_eq!(down.to, "ubuntu-box");
    assert_eq!(down.to_range, Some((0, 50)));
}

#[test]
fn merging_a_text_config_adds_its_screens_and_explains_the_workaround() {
    let mut i = import();
    i.merge_server_config(&ServerConfig::parse(SERVER_CONFIG));

    let names: Vec<_> = i.grid.iter().map(|g| g.name.as_str()).collect();
    assert!(names.contains(&"WIN-LAPTOP"), "third machine picked up");

    let notes = i.notes.join("\n");
    assert!(
        notes.contains("partial-edge links"),
        "must explain why they are no longer needed:\n{notes}"
    );
    // Options with a real equivalent are applied, not reported as unknown.
    assert_eq!(i.switch_delay_ms, Some(250));
    assert!(!notes.contains("screenSaverSync"));
}

#[test]
fn the_resulting_config_is_usable_and_round_trips() {
    let mut i = import();
    i.merge_server_config(&ServerConfig::parse(SERVER_CONFIG));
    let cfg = Config::from_barrier(&i);

    assert_eq!(cfg.identity.name, "ubuntu-box");
    assert_eq!(cfg.network.role, Role::Client);
    assert_eq!(cfg.network.server.as_deref(), Some("192.0.2.10"));
    assert_eq!(
        cfg.network.port, DEFAULT_PORT,
        "a different port lets both run side by side during a changeover"
    );
    assert_eq!(cfg.screen.len(), 3);
    assert!(
        cfg.screen.iter().all(|s| s.device.is_none()),
        "no machine is trusted until it has been paired"
    );

    let text = cfg.to_toml().unwrap();
    let back = Config::parse(&text, std::path::Path::new("<test>")).unwrap();
    assert_eq!(back, cfg);
}

#[test]
fn a_fresh_config_round_trips_through_toml() {
    let cfg = Config::fresh("workstation", Role::Server);
    let text = cfg.to_toml().unwrap();
    let back = Config::parse(&text, std::path::Path::new("<test>")).unwrap();
    assert_eq!(back, cfg);
}

#[test]
fn a_minimal_config_file_fills_in_the_rest() {
    let text = r#"
        [identity]
        name = "desk"

        [network]
        role = "server"
    "#;
    let cfg = Config::parse(text, std::path::Path::new("<test>")).unwrap();
    assert_eq!(cfg.network.port, DEFAULT_PORT);
    assert!(cfg.behavior.release_keys_on_disconnect);
    assert!(cfg.clipboard.enabled);
    assert!(cfg.network.listen.is_empty());
}

#[test]
fn a_config_from_a_newer_build_is_refused_rather_than_misread() {
    let text = r#"
        version = 9999
        [identity]
        name = "desk"
        [network]
        role = "server"
    "#;
    assert!(matches!(
        Config::parse(text, std::path::Path::new("<test>")),
        Err(smkvm_config::ConfigError::TooNew { found: 9999, .. })
    ));
}
