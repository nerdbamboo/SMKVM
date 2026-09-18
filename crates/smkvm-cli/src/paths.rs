//! Where things are kept.
//!
//! Configuration is meant to be read and edited; state is this machine's own
//! secrets and bookkeeping. Keeping them apart means a configuration file can
//! be copied between machines without carrying an identity along with it.

use std::path::PathBuf;

/// Directory for the configuration file.
pub fn config_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("smkvm")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("smkvm")
    }
}

/// Directory for this machine's key and its list of paired machines.
pub fn state_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("smkvm")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("smkvm")
    }
}

pub fn config_file() -> PathBuf {
    config_dir().join("smkvm.toml")
}

pub fn identity_file() -> PathBuf {
    state_dir().join("device.toml")
}

pub fn peers_file() -> PathBuf {
    state_dir().join("peers.toml")
}

pub fn log_file() -> PathBuf {
    state_dir().join("smkvm.log")
}

/// This machine's name, when the configuration does not give one.
pub fn default_name() -> String {
    std::env::var("SMKVM_NAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "smkvm".into())
}
