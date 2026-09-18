//! What the running daemon is connected to.
//!
//! Written by the daemon whenever anything changes and refreshed on a timer;
//! read by anything that wants to show the state. A file rather than a socket
//! means there is nothing to connect to, nothing to fail when the daemon is
//! not running, and nothing platform-specific.
//!
//! It carries the time it was written because a file outlives the process that
//! made it. A daemon that was killed leaves its last word behind, and a reader
//! that took that at face value would show machines as connected long after
//! everything had stopped. Anything older than [`Status::FRESH_FOR`] says only
//! what used to be true.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use smkvm_layout::DeviceId;
use smkvm_proto::Role;

use crate::ConfigError;

/// How a machine stands at the moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineState {
    /// Connected, and able to act on what it is sent.
    Connected,
    /// Connected, but temporarily unable to put input on its screen. On
    /// Windows this is a prompt or the lock screen holding the desktop.
    Suspended,
    /// Known and configured, but not here.
    Away,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub name: String,
    #[serde(default)]
    pub device: Option<DeviceId>,
    pub state: MachineState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// Seconds since the epoch, when this was written.
    pub updated: u64,
    pub role: Role,
    /// What the daemon calls the machine it is running on.
    pub name: String,
    #[serde(default, rename = "machine")]
    pub machines: Vec<Machine>,
}

impl Status {
    /// How long a report is worth believing.
    ///
    /// Comfortably longer than the refresh interval, so an ordinarily busy
    /// daemon is never mistaken for a stopped one, and short enough that a
    /// stopped one stops being believed while somebody is still looking.
    pub const FRESH_FOR: Duration = Duration::from_secs(15);

    pub fn new(role: Role, name: impl Into<String>, machines: Vec<Machine>) -> Status {
        Status {
            updated: now(),
            role,
            name: name.into(),
            machines,
        }
    }

    /// Was this written recently enough to describe the present?
    pub fn is_current(&self) -> bool {
        let age = now().saturating_sub(self.updated);
        age <= Self::FRESH_FOR.as_secs()
    }

    pub fn load(path: &Path) -> Result<Option<Status>, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(toml::from_str(&text).map_err(|source| {
                ConfigError::Parse {
                    path: path.to_path_buf(),
                    source: Box::new(source),
                }
            })?)),
            // Nothing there means no daemon has run, which is a state rather
            // than a failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| ConfigError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let text = toml::to_string_pretty(self)?;
        // Written beside the real file and moved into place, so a reader never
        // catches it half-written and decides the daemon has gone.
        let temporary = path.with_extension("toml.new");
        std::fs::write(&temporary, text).map_err(|source| ConfigError::Io {
            path: temporary.clone(),
            source,
        })?;
        std::fs::rename(&temporary, path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
