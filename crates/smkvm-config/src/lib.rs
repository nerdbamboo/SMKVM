//! SMKVM's configuration file, and migration from an existing Barrier setup.
//!
//! Everything that describes a particular set of machines lives here, in TOML,
//! never in code. Which machine is the server, how many there are, how their
//! monitors are arranged: all of it is data, so adding a computer or swapping
//! one out is an edit, not a rebuild.

#![forbid(unsafe_code)]

pub mod barrier;
pub mod ini;
pub mod paths;
pub mod status;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use smkvm_layout::{DeviceId, EdgeOverflow, Rect};
use smkvm_proto::Role;

/// Default listening port.
///
/// Deliberately not Barrier's 24800, so both can run at once during a
/// changeover and going back is always possible.
pub const DEFAULT_PORT: u16 = 24810;

/// Current configuration schema version.
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid configuration: {source}")]
    Parse {
        path: PathBuf,
        // Boxed because the parser's error carries a span and a copy of the
        // input, and every function that can fail this way would otherwise
        // return a value that size whether it fails or not.
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("configuration could not be written: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("configuration version {found} is newer than this build understands ({known})")]
    TooNew { found: u32, known: u32 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    /// How this machine appears to the others. Free to change: identity is
    /// carried by the device key, not the name.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Network {
    /// Whether this machine owns the keyboard and mouse or receives from one.
    /// Changing role is a config edit and nothing more.
    pub role: Role,
    /// For a client: where the server is, as `host` or `host:port`.
    #[serde(default)]
    pub server: Option<String>,
    /// For a server: which addresses to accept connections on.
    ///
    /// Empty means every interface, which is how Barrier shipped and is why
    /// anything on the same network could reach it. Listing addresses keeps
    /// the surface to the networks actually in use.
    #[serde(default)]
    pub listen: Vec<String>,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Prefer a direct local address over a VPN one when a peer offers both.
    #[serde(default = "yes")]
    pub prefer_lan: bool,
    /// How long to wait for a heartbeat before treating the link as dead.
    #[serde(default = "default_heartbeat")]
    pub heartbeat_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Behavior {
    /// How long the cursor must rest against an edge before it crosses. Zero
    /// crosses immediately.
    #[serde(default)]
    pub switch_delay_ms: u32,
    /// When set, an edge is only crossed if it is struck twice within this
    /// window.
    #[serde(default)]
    pub switch_double_tap_ms: u32,
    /// What happens at an edge with nothing directly beyond it.
    #[serde(default)]
    pub edge_overflow: EdgeOverflow,
    /// Release every key and button on the far machine when a link drops.
    /// Without this a modifier held at the wrong moment stays down.
    #[serde(default = "yes")]
    pub release_keys_on_disconnect: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clipboard {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Which flavours to share. Images travel as PNG.
    #[serde(default = "default_formats")]
    pub formats: Vec<String>,
    /// Refuse to transfer more than this in one go.
    ///
    /// Contents are fetched only when something is pasted, so a large limit
    /// costs nothing until it is used. Exceeding it is reported, never a
    /// silent drop.
    #[serde(default = "default_clipboard_max")]
    pub max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transfer {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Where received files land.
    #[serde(default = "default_quarantine")]
    pub directory: String,
    /// Ask before accepting a transfer larger than this.
    #[serde(default = "default_prompt_over")]
    pub prompt_over_bytes: u64,
}

/// Where one monitor sits on the global desktop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorPlacement {
    /// The owning device's stable name for this output, e.g. `DP-2`.
    pub id: String,
    /// Position and size on the global desktop, as `[x, y, w, h]`.
    pub global: [i32; 4],
}

impl MonitorPlacement {
    pub fn rect(&self) -> Rect {
        Rect::new(
            self.global[0],
            self.global[1],
            self.global[2],
            self.global[3],
        )
    }
}

/// A machine in the layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    pub name: String,
    /// Stable identity, filled in once the machine has been paired. Absent for
    /// a machine that is known by name but has not connected yet, which is the
    /// state an import leaves things in.
    #[serde(default)]
    pub device: Option<DeviceId>,
    /// Where the machine sat in an imported Barrier grid, as `[column, row]`.
    /// Only a hint for first placement; real geometry arrives from the machine.
    #[serde(default)]
    pub grid: Option<[u32; 2]>,
    #[serde(default)]
    pub monitor: Vec<MonitorPlacement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    pub identity: Identity,
    pub network: Network,
    #[serde(default)]
    pub behavior: Behavior,
    #[serde(default)]
    pub clipboard: Clipboard,
    #[serde(default)]
    pub transfer: Transfer,
    /// The machines in the layout. Server-side only; a client learns the
    /// layout from the server.
    #[serde(default)]
    pub screen: Vec<Screen>,
}

fn yes() -> bool {
    true
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_version() -> u32 {
    CONFIG_VERSION
}
fn default_heartbeat() -> u32 {
    3_000
}
fn default_formats() -> Vec<String> {
    ["text", "html", "image", "files"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}
fn default_clipboard_max() -> u64 {
    128 * 1024 * 1024
}
fn default_prompt_over() -> u64 {
    256 * 1024 * 1024
}
fn default_quarantine() -> String {
    "~/Downloads/SMKVM".into()
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            switch_delay_ms: 0,
            switch_double_tap_ms: 0,
            edge_overflow: EdgeOverflow::default(),
            release_keys_on_disconnect: true,
        }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self {
            enabled: true,
            formats: default_formats(),
            max_bytes: default_clipboard_max(),
        }
    }
}

impl Default for Transfer {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: default_quarantine(),
            prompt_over_bytes: default_prompt_over(),
        }
    }
}

impl Config {
    /// A usable configuration for a machine with no prior setup.
    pub fn fresh(name: impl Into<String>, role: Role) -> Config {
        Config {
            version: CONFIG_VERSION,
            identity: Identity { name: name.into() },
            network: Network {
                role,
                server: None,
                listen: Vec::new(),
                port: DEFAULT_PORT,
                prefer_lan: true,
                heartbeat_ms: default_heartbeat(),
            },
            behavior: Behavior::default(),
            clipboard: Clipboard::default(),
            transfer: Transfer::default(),
            screen: Vec::new(),
        }
    }

    pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
        let cfg: Config = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
        if cfg.version > CONFIG_VERSION {
            return Err(ConfigError::TooNew {
                found: cfg.version,
                known: CONFIG_VERSION,
            });
        }
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Config::parse(&text, path)
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Build a configuration from what an existing Barrier setup was doing.
    ///
    /// Machine names, roles, timings and preferences come across. Layout does
    /// not: Barrier only knew one rectangle per machine, so the grid is kept
    /// as a hint and real placement waits until each machine reports its
    /// monitors.
    pub fn from_barrier(import: &barrier::Import) -> Config {
        let role = if import.is_server {
            Role::Server
        } else {
            Role::Client
        };
        let name = import
            .screen_name
            .clone()
            .unwrap_or_else(|| "smkvm".to_string());

        let mut cfg = Config::fresh(name, role);
        // Barrier's port is left behind on purpose: running on a different one
        // means both can be up at once while switching over.
        cfg.network.server = import.server_host.clone();
        cfg.behavior.switch_delay_ms = import.switch_delay_ms.unwrap_or(0);
        cfg.behavior.switch_double_tap_ms = import.switch_double_tap_ms.unwrap_or(0);
        if let Some(enabled) = import.clipboard {
            cfg.clipboard.enabled = enabled;
        }
        if let Some(limit) = import.clipboard_limit {
            cfg.clipboard.max_bytes = limit.max(default_clipboard_max());
        }
        if let Some(enabled) = import.drag_drop {
            cfg.transfer.enabled = enabled;
        }
        cfg.screen = import
            .grid
            .iter()
            .map(|g| Screen {
                name: g.name.clone(),
                device: None,
                grid: Some([g.column, g.row]),
                monitor: Vec::new(),
            })
            .collect();
        cfg
    }
}
