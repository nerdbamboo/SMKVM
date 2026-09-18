//! Reading an existing Barrier installation.
//!
//! Two files matter. The GUI keeps its settings in a Qt `QSettings` INI file
//! (`~/.config/Debauchee/Barrier.conf`, or the equivalent registry key on
//! Windows), including a flattened grid of screen names. A server may also
//! have a text configuration in Barrier's own format, which is the only place
//! partial-edge links can be expressed.
//!
//! What is deliberately *not* carried over is trust. Barrier's stored
//! certificate fingerprints were accepted on first sight with nothing tying
//! them to a machine, so importing them would import that assumption too.
//! Names come across; pairing happens again.

use std::collections::BTreeMap;

use crate::ini::Ini;

/// Which way a link goes, in Barrier's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Edge {
    Left,
    Right,
    Up,
    Down,
}

impl Edge {
    fn parse(s: &str) -> Option<Edge> {
        match s {
            "left" => Some(Edge::Left),
            "right" => Some(Edge::Right),
            "up" => Some(Edge::Up),
            "down" => Some(Edge::Down),
            _ => None,
        }
    }
}

/// One entry from a text config's `links` section.
///
/// Barrier's format allows a percentage interval on each side, which the GUI
/// cannot express and therefore silently discards whenever it rewrites the
/// file. Both intervals are captured here so an import can tell whether the
/// user had set one up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub from: String,
    pub edge: Edge,
    pub from_range: Option<(u32, u32)>,
    pub to: String,
    pub to_range: Option<(u32, u32)>,
}

/// A parsed `barrier.conf`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerConfig {
    pub screens: Vec<String>,
    pub links: Vec<Link>,
    pub aliases: BTreeMap<String, Vec<String>>,
    pub options: BTreeMap<String, String>,
}

/// Split `name(1,2)` into its name and interval.
fn split_interval(s: &str) -> (String, Option<(u32, u32)>) {
    let s = s.trim();
    let Some(open) = s.find('(') else {
        return (s.to_string(), None);
    };
    let Some(close) = s.rfind(')') else {
        return (s.to_string(), None);
    };
    let name = s[..open].trim().to_string();
    let inner = &s[open + 1..close];
    let Some((a, b)) = inner.split_once(',') else {
        return (name, None);
    };
    match (a.trim().parse(), b.trim().parse()) {
        (Ok(a), Ok(b)) => (name, Some((a, b))),
        _ => (name, None),
    }
}

impl ServerConfig {
    /// Parse Barrier's text configuration format.
    pub fn parse(text: &str) -> ServerConfig {
        let mut cfg = ServerConfig::default();
        let mut section = String::new();
        // The name a `name:` line most recently introduced, which the indented
        // lines beneath it belong to.
        let mut subject: Option<String> = None;

        for raw in text.lines() {
            let line = match raw.split_once('#') {
                Some((before, _)) => before,
                None => raw,
            }
            .trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix("section:") {
                section = name.trim().to_ascii_lowercase();
                subject = None;
                continue;
            }
            if line.eq_ignore_ascii_case("end") {
                section.clear();
                subject = None;
                continue;
            }

            match section.as_str() {
                "screens" => {
                    if let Some(name) = line.strip_suffix(':') {
                        let name = name.trim().to_string();
                        if !name.is_empty() && !cfg.screens.contains(&name) {
                            cfg.screens.push(name.clone());
                        }
                        subject = Some(name);
                    }
                    // Per-screen options are Barrier-specific tweaks with no
                    // equivalent here; they are reported rather than mapped.
                }
                "links" => {
                    if let Some(name) = line.strip_suffix(':') {
                        subject = Some(name.trim().to_string());
                        continue;
                    }
                    let (Some(from), Some((lhs, rhs))) = (subject.clone(), line.split_once('='))
                    else {
                        continue;
                    };
                    let (edge, from_range) = split_interval(lhs);
                    let (to, to_range) = split_interval(rhs);
                    if let Some(edge) = Edge::parse(&edge.to_ascii_lowercase()) {
                        cfg.links.push(Link {
                            from,
                            edge,
                            from_range,
                            to,
                            to_range,
                        });
                    }
                }
                "aliases" => {
                    if let Some(name) = line.strip_suffix(':') {
                        subject = Some(name.trim().to_string());
                    } else if let Some(name) = &subject {
                        cfg.aliases
                            .entry(name.clone())
                            .or_default()
                            .push(line.to_string());
                    }
                }
                "options" => {
                    if let Some((k, v)) = line.split_once('=') {
                        cfg.options
                            .insert(k.trim().to_string(), v.trim().to_string());
                    }
                }
                _ => {}
            }
        }
        cfg
    }
}

/// A screen as Barrier's GUI grid holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridScreen {
    pub name: String,
    /// Cell in the GUI's grid. Barrier gives each machine exactly one cell,
    /// which is the limitation that makes multi-monitor arrangements awkward.
    pub column: u32,
    pub row: u32,
}

/// Everything worth keeping from a Barrier installation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Import {
    /// This machine's screen name.
    pub screen_name: Option<String>,
    /// True when this machine was configured as the server.
    pub is_server: bool,
    pub server_host: Option<String>,
    pub port: Option<u16>,
    pub switch_delay_ms: Option<u32>,
    pub switch_double_tap_ms: Option<u32>,
    pub clipboard: Option<bool>,
    pub clipboard_limit: Option<u64>,
    pub drag_drop: Option<bool>,
    pub grid: Vec<GridScreen>,
    pub grid_columns: u32,
    pub grid_rows: u32,
    /// Settings that were found but intentionally not carried across, each
    /// with the reason. Surfaced to the user rather than dropped in silence.
    pub notes: Vec<String>,
}

impl Import {
    /// Read the GUI's settings file.
    pub fn from_qt_settings(text: &str) -> Import {
        let ini = Ini::parse(text);
        let mut out = Import {
            screen_name: ini
                .get_any("General", "screenName")
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            is_server: ini
                .get_bool("General", "groupServerChecked")
                .unwrap_or(false),
            server_host: ini
                .get_any("General", "serverHostname")
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            port: ini
                .get_u32("General", "port")
                .and_then(|p| u16::try_from(p).ok()),
            switch_delay_ms: None,
            switch_double_tap_ms: None,
            clipboard: ini.get_bool("internalConfig", "clipboardSharing"),
            clipboard_limit: ini
                .get_u32("internalConfig", "clipboardSharingSize")
                .map(u64::from),
            drag_drop: ini.get_bool("internalConfig", "enableDragAndDrop"),
            grid: Vec::new(),
            grid_columns: ini.get_u32("internalConfig", "numColumns").unwrap_or(0),
            grid_rows: ini.get_u32("internalConfig", "numRows").unwrap_or(0),
            notes: Vec::new(),
        };

        // Switch timings only apply when their enabling flag is set.
        if ini
            .get_bool("internalConfig", "hasSwitchDelay")
            .unwrap_or(false)
        {
            out.switch_delay_ms = ini.get_u32("internalConfig", "switchDelay");
        }
        if ini
            .get_bool("internalConfig", "hasSwitchDoubleTap")
            .unwrap_or(false)
        {
            out.switch_double_tap_ms = ini.get_u32("internalConfig", "switchDoubleTap");
        }

        // The grid is a flattened array; only entries with a name are real.
        let mut named: BTreeMap<u32, String> = BTreeMap::new();
        for (idx, field, value) in ini.array("internalConfig", "screens") {
            if field == "name" && !value.is_empty() {
                named.insert(idx, value.to_string());
            }
        }
        if out.grid_columns > 0 {
            for (idx, name) in named {
                // Qt's array indices are one-based.
                let zero = idx.saturating_sub(1);
                out.grid.push(GridScreen {
                    name,
                    column: zero % out.grid_columns,
                    row: zero / out.grid_columns,
                });
            }
        } else {
            for (_, name) in named {
                out.grid.push(GridScreen {
                    name,
                    column: 0,
                    row: 0,
                });
            }
        }

        if ini.get_bool("General", "cryptoEnabled").unwrap_or(false)
            && !ini
                .get_bool("General", "requireClientCertificate")
                .unwrap_or(false)
        {
            out.notes.push(
                "Barrier had TLS on but was not checking client certificates, so any host that \
                 could reach the port could connect. Devices are paired explicitly here."
                    .into(),
            );
        }
        if ini
            .get_any("General", "interface")
            .is_none_or(str::is_empty)
        {
            out.notes.push(
                "Barrier was listening on every interface. Set network.listen to the addresses \
                 you actually use."
                    .into(),
            );
        }
        if ini
            .get_bool("General", "useInternalConfig")
            .unwrap_or(false)
        {
            out.notes.push(
                "The GUI was managing the screen layout, which means any hand-edited \
                 barrier.conf was being overwritten."
                    .into(),
            );
        }
        out.notes.push(
            "Stored TLS fingerprints were not imported: Barrier trusted them on first sight. \
             Each machine is paired again."
                .into(),
        );
        out
    }

    /// Fold in a server's text configuration, which carries the screen names
    /// and any partial-edge links the GUI could not express.
    pub fn merge_server_config(&mut self, cfg: &ServerConfig) {
        for name in &cfg.screens {
            if !self.grid.iter().any(|g| &g.name == name) {
                self.grid.push(GridScreen {
                    name: name.clone(),
                    column: 0,
                    row: 0,
                });
            }
        }
        if cfg
            .links
            .iter()
            .any(|l| l.from_range.is_some() || l.to_range.is_some())
        {
            self.notes.push(
                "The text config used partial-edge links to work around one-rectangle-per-machine. \
                 Monitors are placed individually here, so those are no longer needed."
                    .into(),
            );
        }
        for (k, v) in &cfg.options {
            match k.as_str() {
                "switchDelay" => self.switch_delay_ms = v.parse().ok(),
                "switchDoubleTap" => self.switch_double_tap_ms = v.parse().ok(),
                "clipboardSharing" => self.clipboard = Some(v == "true"),
                "screenSaverSync" | "relativeMouseMoves" | "win32KeepForeground" => {}
                other => self.notes.push(format!(
                    "Barrier option `{other} = {v}` has no equivalent here."
                )),
            }
        }
    }
}
