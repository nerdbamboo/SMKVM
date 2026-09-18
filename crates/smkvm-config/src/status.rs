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
//!
//! It also carries where every screen sits. The daemon is the only thing that
//! knows where a machine the configuration does not mention has been put, and
//! a window that could not show that machine would have no way to offer to
//! move it.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use smkvm_layout::{DeviceId, Rect};
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

/// One screen, where the daemon has it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacedMonitor {
    /// The owning machine's own name for the output.
    pub id: String,
    /// Position and size on the global desktop, as `[x, y, w, h]`.
    pub global: [i32; 4],
    /// What the monitor calls itself, where the machine knows.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub primary: bool,
}

impl PlacedMonitor {
    pub fn rect(&self) -> Rect {
        Rect::new(
            self.global[0],
            self.global[1],
            self.global[2],
            self.global[3],
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub name: String,
    #[serde(default)]
    pub device: Option<DeviceId>,
    pub state: MachineState,
    /// Whether the cursor is on this machine right now.
    #[serde(default)]
    pub active: bool,
    /// Its screens, where the daemon has arranged them. Includes the daemon's
    /// own machine, so a reader sees the same desk the cursor moves over.
    #[serde(default)]
    pub monitor: Vec<PlacedMonitor>,
}

impl Machine {
    pub fn new(name: impl Into<String>, device: Option<DeviceId>, state: MachineState) -> Machine {
        Machine {
            name: name.into(),
            device,
            state,
            active: false,
            monitor: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// Seconds since the epoch, when this was written.
    pub updated: u64,
    pub role: Role,
    /// What the daemon calls the machine it is running on.
    pub name: String,
    /// The daemon's process, for anyone who needs to find it.
    #[serde(default)]
    pub pid: Option<u32>,
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

    /// How often a daemon with nothing to report should write anyway, so its
    /// report stays believed.
    pub const REFRESH_EVERY: Duration = Duration::from_secs(5);

    pub fn new(role: Role, name: impl Into<String>, machines: Vec<Machine>) -> Status {
        Status {
            updated: now(),
            role,
            name: name.into(),
            pid: Some(std::process::id()),
            machines,
        }
    }

    /// Was this written recently enough to describe the present?
    pub fn is_current(&self) -> bool {
        let age = now().saturating_sub(self.updated);
        age <= Self::FRESH_FOR.as_secs()
    }

    /// How long ago it was written.
    pub fn age(&self) -> Duration {
        Duration::from_secs(now().saturating_sub(self.updated))
    }

    /// The machine the cursor is on, if the report says.
    pub fn active(&self) -> Option<&Machine> {
        self.machines.iter().find(|m| m.active)
    }

    pub fn machine(&self, name: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.name == name)
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

    /// The report at `path`, if one is there and recent enough to believe.
    pub fn current(path: &Path) -> Option<Status> {
        Status::load(path).ok().flatten().filter(Status::is_current)
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

    /// Take the report away, for a daemon that is stopping on purpose.
    ///
    /// A stale file says the daemon died; no file says it was stopped. A
    /// reader can tell the two apart, and the second is not worth worrying
    /// about.
    pub fn remove(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_survives_the_trip_through_toml() {
        let mut machine = Machine::new("over-there", None, MachineState::Connected);
        machine.active = true;
        machine.monitor.push(PlacedMonitor {
            id: "DP-1".into(),
            global: [1920, 0, 2560, 1440],
            label: Some("HCS27".into()),
            primary: true,
        });
        let status = Status::new(Role::Server, "this-one", vec![machine.clone()]);
        let text = toml::to_string_pretty(&status).unwrap();
        let read: Status = toml::from_str(&text).unwrap();
        assert_eq!(read, status);
        assert_eq!(read.active().map(|m| m.name.as_str()), Some("over-there"));
        assert_eq!(
            read.machine("over-there").unwrap().monitor[0].rect(),
            Rect::new(1920, 0, 2560, 1440)
        );
    }

    #[test]
    fn a_report_from_before_geometry_was_carried_still_reads() {
        let old = r#"
updated = 1
role = "server"
name = "this-one"

[[machine]]
name = "over-there"
state = "connected"
"#;
        let read: Status = toml::from_str(old).unwrap();
        assert_eq!(read.machines.len(), 1);
        assert!(!read.machines[0].active);
        assert!(read.machines[0].monitor.is_empty());
        assert_eq!(read.pid, None);
        assert!(
            !read.is_current(),
            "a report from 1970 says nothing about now"
        );
    }
}
