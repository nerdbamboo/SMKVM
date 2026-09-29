//! Where things are kept.
//!
//! Configuration is meant to be read and edited; state is this machine's own
//! secrets and bookkeeping. Keeping them apart means a configuration file can
//! be copied between machines without carrying an identity along with it.
//!
//! ## Whose files, which is a question with two answers
//!
//! Ordinarily these are the person's: their roaming profile for the
//! configuration, their local profile for the identity, the paired machines
//! and the log. That is right for a daemon the person starts, or that their
//! login starts for them.
//!
//! It is wrong, and wrong in a way that stops the program dead, for the
//! Windows service. A service runs as LocalSystem, whose `APPDATA` is
//! `C:\WINDOWS\system32\config\systemprofile\AppData\Roaming` -- so the
//! first thing the service did on a real machine was look there, find no
//! configuration, and exit. (It said so clearly and exited non-zero, which
//! is the only reason that took one look rather than an afternoon.)
//!
//! The answer is not for the service to read out of somebody's profile.
//! Which somebody? At boot, before anyone has logged in -- which is exactly
//! the case `--system` exists to cover -- there is no somebody, and a
//! service running as the system account reading `C:\Users\<name>\AppData`
//! is a thing that should make anyone reading it uneasy. So the service has
//! its own machine-wide place, `%ProgramData%\smkvm`, and
//! `smkvm service install --system` copies the person's files there once, so
//! that a machine which is already paired stays paired.
//!
//! [`Scope`] is that choice, and every path below is a function of it. The
//! choosing is [`scope`], set once by the service before it starts the
//! daemon; everything else in the program keeps calling the same functions
//! it always did.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

/// Whose files these are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// The person's own profile. Every way of running this except the
    /// Windows service.
    #[default]
    Person,
    /// Machine-wide, for a service that runs as the system account and has
    /// no person to belong to.
    Machine,
}

const PERSON: u8 = 0;
const MACHINE: u8 = 1;
static SCOPE: AtomicU8 = AtomicU8::new(PERSON);

/// Say that this process is the machine's rather than a person's.
///
/// Called once, by the service, before the daemon starts and before
/// anything has read a path. Not reversible, because a process that
/// changed its mind halfway would write half its state to each place.
pub fn use_machine_scope() {
    SCOPE.store(MACHINE, Ordering::Release);
}

pub fn scope() -> Scope {
    match SCOPE.load(Ordering::Acquire) {
        MACHINE => Scope::Machine,
        _ => Scope::Person,
    }
}

/// The directories the platform gives us to build paths out of.
///
/// Separated from reading the environment so that the choosing below is a
/// function of its arguments and can be tested for Windows on a machine
/// that is not one -- which matters here more than usual, because the bug
/// this exists to fix was a path being resolved in the wrong profile and
/// nothing on a Linux machine could have noticed.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    /// Windows `APPDATA`, the roaming profile.
    pub roaming: Option<PathBuf>,
    /// Windows `LOCALAPPDATA`.
    pub local: Option<PathBuf>,
    /// Windows `ProgramData`, which belongs to the machine.
    pub program_data: Option<PathBuf>,
    pub xdg_config: Option<PathBuf>,
    pub xdg_data: Option<PathBuf>,
    pub home: Option<PathBuf>,
}

impl Roots {
    pub fn from_env() -> Roots {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        Roots {
            roaming: var("APPDATA"),
            local: var("LOCALAPPDATA"),
            // `ProgramData` is the documented variable; `ALLUSERSPROFILE`
            // is the older name for the same directory and is still set,
            // which is worth falling back to rather than guessing at
            // `C:\ProgramData` -- a machine with Windows somewhere else
            // would be a machine this wrote outside.
            program_data: var("ProgramData").or_else(|| var("ALLUSERSPROFILE")),
            xdg_config: var("XDG_CONFIG_HOME"),
            xdg_data: var("XDG_DATA_HOME"),
            home: var("HOME"),
        }
    }
}

/// Directory for the configuration file, given everything that decides it.
pub fn config_dir_in(scope: Scope, roots: &Roots, windows: bool) -> PathBuf {
    if windows {
        match scope {
            Scope::Machine => machine_dir(roots),
            Scope::Person => roots
                .roaming
                .clone()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("smkvm"),
        }
    } else {
        // There is no service on Linux -- `service install` writes a login
        // item, which runs as the person -- so there is nothing for a
        // machine scope to mean here, and asking for one gets the
        // person's rather than a path nobody would find.
        roots
            .xdg_config
            .clone()
            .or_else(|| roots.home.as_ref().map(|h| h.join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("smkvm")
    }
}

/// Directory for this machine's key, its paired machines and its log.
pub fn state_dir_in(scope: Scope, roots: &Roots, windows: bool) -> PathBuf {
    if windows {
        match scope {
            // One directory rather than two: the split between roaming and
            // local configuration is about what follows a person between
            // machines, and nothing machine-wide follows anybody anywhere.
            Scope::Machine => machine_dir(roots),
            Scope::Person => roots
                .local
                .clone()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("smkvm"),
        }
    } else {
        roots
            .xdg_data
            .clone()
            .or_else(|| roots.home.as_ref().map(|h| h.join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("smkvm")
    }
}

fn machine_dir(roots: &Roots) -> PathBuf {
    roots
        .program_data
        .clone()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("smkvm")
}

/// The machine-wide directory, whatever this process's own scope is.
///
/// Needed by `service install --system`, which runs as the person and has
/// to put files where the service will look for them.
pub fn machine_config_dir() -> PathBuf {
    config_dir_in(Scope::Machine, &Roots::from_env(), cfg!(windows))
}

pub fn machine_state_dir() -> PathBuf {
    state_dir_in(Scope::Machine, &Roots::from_env(), cfg!(windows))
}

/// Directory for the configuration file.
pub fn config_dir() -> PathBuf {
    config_dir_in(scope(), &Roots::from_env(), cfg!(windows))
}

/// Directory for this machine's key and its list of paired machines.
pub fn state_dir() -> PathBuf {
    state_dir_in(scope(), &Roots::from_env(), cfg!(windows))
}

/// The three files that make a machine what it is: what it does, who it
/// is, and who it trusts.
///
/// Named in one place because `service install --system` has to carry
/// exactly these across and nothing else, and a list that lives in the
/// copying code is a list that goes out of step with this file.
pub const CARRIED_OVER: [&str; 3] = [CONFIG_NAME, IDENTITY_NAME, PEERS_NAME];

pub const CONFIG_NAME: &str = "smkvm.toml";
pub const IDENTITY_NAME: &str = "device.toml";
pub const PEERS_NAME: &str = "peers.toml";
pub const STATUS_NAME: &str = "status.toml";

pub fn config_file() -> PathBuf {
    config_dir().join(CONFIG_NAME)
}

pub fn identity_file() -> PathBuf {
    state_dir().join(IDENTITY_NAME)
}

pub fn peers_file() -> PathBuf {
    state_dir().join(PEERS_NAME)
}

pub fn log_file() -> PathBuf {
    state_dir().join("smkvm.log")
}

/// Where the running daemon reports what it is connected to.
///
/// A file rather than a socket: anything that wants to show the state only
/// needs to read it, on any platform, with nothing to connect to and
/// nothing to fail when the daemon is not running.
pub fn status_file() -> PathBuf {
    state_dir().join(STATUS_NAME)
}

/// Every place a report might be, in the order to believe them.
///
/// The writer and the reader are not the same process and need not be the
/// same scope: the service writes machine-wide, and `smkvm status` is run
/// by the person at the desk. Rather than make the person pass a flag to
/// ask about a service they may not know is there, a reader looks in both
/// places. The machine's comes first because a service, when there is one,
/// is the daemon; a report left in a profile by a hand-started daemon is
/// the fallback.
///
/// This is also the argument for the status file being the one thing in
/// the machine directory an ordinary person may read. It says which
/// machines are connected and where their screens are -- nothing that is
/// worth keeping from somebody sitting at the keyboard it describes -- and
/// keeping it unreadable would mean `smkvm status` and the window both
/// stopped working the moment somebody chose `--system`.
pub fn status_candidates() -> Vec<PathBuf> {
    let roots = Roots::from_env();
    let windows = cfg!(windows);
    let mut places = Vec::with_capacity(2);
    if windows {
        places.push(state_dir_in(Scope::Machine, &roots, windows).join(STATUS_NAME));
    }
    let person = state_dir_in(Scope::Person, &roots, windows).join(STATUS_NAME);
    if !places.contains(&person) {
        places.push(person);
    }
    places
}

/// The report to read, which is the first of [`status_candidates`] that is
/// there at all.
pub fn status_file_to_read() -> PathBuf {
    let places = status_candidates();
    places
        .iter()
        .find(|path| path.exists())
        .cloned()
        .unwrap_or_else(|| places.into_iter().next().unwrap_or_else(status_file))
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
///
/// Note for service mode: the system account's home is the system profile,
/// so a `transfer.directory` of `~/Downloads/SMKVM` resolves somewhere the
/// person cannot see. That is a real gap and it is recorded in
/// `docs/NOTES.md` rather than papered over here, because guessing which
/// person's Downloads a service meant is the same bad question as guessing
/// whose configuration to read.
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

/// Is `path` inside `root`? Used to say, in a message, whether a file the
/// service wants is one the installer put there.
pub fn is_within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    fn windows_roots() -> Roots {
        Roots {
            roaming: Some(PathBuf::from(r"C:\Users\someone\AppData\Roaming")),
            local: Some(PathBuf::from(r"C:\Users\someone\AppData\Local")),
            program_data: Some(PathBuf::from(r"C:\ProgramData")),
            ..Default::default()
        }
    }

    /// What LocalSystem's environment actually looks like, which is the
    /// whole reason this module has a scope at all.
    fn system_account_roots() -> Roots {
        let profile = r"C:\WINDOWS\system32\config\systemprofile\AppData";
        Roots {
            roaming: Some(PathBuf::from(format!(r"{profile}\Roaming"))),
            local: Some(PathBuf::from(format!(r"{profile}\Local"))),
            program_data: Some(PathBuf::from(r"C:\ProgramData")),
            ..Default::default()
        }
    }

    /// `join` uses the separator of the machine doing the building, not
    /// of the machine the path describes, so these compare against a
    /// path built the same way rather than against a typed-out string.
    /// What is being asserted is which root was chosen, which is the
    /// whole of what this module decides.
    fn under(root: &str) -> PathBuf {
        PathBuf::from(root).join("smkvm")
    }

    #[test]
    fn a_person_gets_their_own_profile() {
        let roots = windows_roots();
        assert_eq!(
            config_dir_in(Scope::Person, &roots, true),
            under(r"C:\Users\someone\AppData\Roaming")
        );
        assert_eq!(
            state_dir_in(Scope::Person, &roots, true),
            under(r"C:\Users\someone\AppData\Local")
        );
    }

    #[test]
    fn the_service_never_reads_a_profile_even_though_it_has_one() {
        // The failure on the real machine, pinned. The system account has
        // an APPDATA and it resolves perfectly well; it is simply the
        // wrong place, and nothing but this distinction stops the daemon
        // looking there and finding nothing.
        let roots = system_account_roots();
        for dir in [
            config_dir_in(Scope::Machine, &roots, true),
            state_dir_in(Scope::Machine, &roots, true),
        ] {
            assert_eq!(dir, under(r"C:\ProgramData"));
            assert!(
                !dir.to_string_lossy().contains("systemprofile"),
                "the service is reading out of the system profile again: {dir:?}"
            );
            assert!(
                !dir.to_string_lossy().contains(r"\Users\"),
                "the service is reading out of somebody's profile: {dir:?}"
            );
        }
    }

    #[test]
    fn the_machine_directory_is_the_same_one_from_either_side() {
        // `service install --system` runs as the person and has to put
        // files exactly where the service will look for them. If these two
        // ever disagree the copy lands somewhere nothing reads.
        let person_installing = windows_roots();
        let the_service = system_account_roots();
        assert_eq!(
            config_dir_in(Scope::Machine, &person_installing, true),
            config_dir_in(Scope::Machine, &the_service, true)
        );
        assert_eq!(
            state_dir_in(Scope::Machine, &person_installing, true),
            state_dir_in(Scope::Machine, &the_service, true)
        );
    }

    #[test]
    fn linux_has_no_machine_scope_to_get_wrong() {
        // There is no service on Linux; `service install` writes a login
        // item that runs as the person. Asking for a machine scope there
        // gets the person's paths rather than an invented directory.
        let roots = Roots {
            home: Some(PathBuf::from("/home/someone")),
            ..Default::default()
        };
        assert_eq!(
            config_dir_in(Scope::Machine, &roots, false),
            config_dir_in(Scope::Person, &roots, false)
        );
        assert_eq!(
            config_dir_in(Scope::Person, &roots, false),
            PathBuf::from("/home/someone/.config/smkvm")
        );
    }

    #[test]
    fn program_data_falls_back_to_its_older_name_rather_than_to_a_guess() {
        let roots = Roots {
            program_data: None,
            ..windows_roots()
        };
        // With neither variable there is nothing honest to say, so it is
        // the working directory -- visible and wrong -- rather than
        // `C:\ProgramData` on a machine whose Windows is somewhere else.
        assert_eq!(config_dir_in(Scope::Machine, &roots, true), under("."));
    }

    #[test]
    fn the_three_carried_files_are_the_three_that_matter() {
        assert!(CARRIED_OVER.contains(&CONFIG_NAME));
        assert!(CARRIED_OVER.contains(&IDENTITY_NAME));
        assert!(CARRIED_OVER.contains(&PEERS_NAME));
        // Not the status report: it is written by whichever daemon is
        // running and copying a stale one would announce a daemon that is
        // not there.
        assert!(!CARRIED_OVER.contains(&STATUS_NAME));
        assert_eq!(CARRIED_OVER.len(), 3);
    }

    #[test]
    fn a_reader_looks_machine_wide_first_and_in_a_profile_second() {
        let places = status_candidates();
        assert!(!places.is_empty());
        if cfg!(windows) {
            assert_eq!(places.len(), 2);
            assert!(places[0].to_string_lossy().contains("smkvm"));
        } else {
            assert_eq!(places.len(), 1);
        }
    }

    #[test]
    fn the_scope_starts_as_the_person_and_is_what_it_is_set_to() {
        // Not `use_machine_scope` here: it is process-wide and one-way by
        // design, and setting it would change every other test in this
        // binary. The default is what matters -- anything that forgets to
        // choose gets the person's files, which is what everything except
        // the service is.
        assert_eq!(Scope::default(), Scope::Person);
    }
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
