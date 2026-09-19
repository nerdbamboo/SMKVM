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

/// Where the running daemon reports what it is connected to.
///
/// A file rather than a socket: anything that wants to show the state only
/// needs to read it, on any platform, with nothing to connect to and nothing
/// to fail when the daemon is not running.
pub fn status_file() -> PathBuf {
    state_dir().join("status.toml")
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

/// Resolve a path from the configuration, where a leading `~` stands for
/// the home directory.
///
/// Only the bare `~` and `~/` forms are recognised: `~user` is another
/// person's home, which this has no business writing into.
pub fn expand_home(path: &str) -> PathBuf {
    let home = || {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
    };
    if path == "~" {
        return home().unwrap_or_else(|| PathBuf::from("."));
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = home() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod home_tests {
    use super::expand_home;

    #[test]
    fn a_tilde_becomes_the_home_directory() {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .expect("a home to test against");
        assert_eq!(expand_home("~"), std::path::PathBuf::from(&home));
        assert_eq!(
            expand_home("~/Downloads/SMKVM"),
            std::path::PathBuf::from(&home).join("Downloads/SMKVM")
        );
    }

    #[test]
    fn anything_else_is_left_alone() {
        assert_eq!(expand_home("/srv/in"), std::path::PathBuf::from("/srv/in"));
        assert_eq!(
            expand_home("~someone/x"),
            std::path::PathBuf::from("~someone/x")
        );
        assert_eq!(
            expand_home("rel/ative"),
            std::path::PathBuf::from("rel/ative")
        );
    }
}
