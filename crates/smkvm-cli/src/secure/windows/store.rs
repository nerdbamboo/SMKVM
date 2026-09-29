//! The machine's own copy of its files, and who may read them.
//!
//! `%ProgramData%\smkvm`, made by `service install --system` and read by
//! the service. What goes in it is decided by [`crate::secure::carry`];
//! this is the making, the copying and -- the part that matters -- the
//! access list.
//!
//! ## Why the default access list will not do
//!
//! `%ProgramData%` grants every authenticated user read access, and new
//! files under it inherit that. So a directory made here and left alone
//! would put `device.toml` -- this machine's private key -- where any
//! account on the machine could read it.
//!
//! What that key is: the machine's identity in the Noise handshake. It is
//! the whole of what the server uses to decide this is the machine it
//! paired with. Anyone holding a copy can connect to the server as this
//! machine and be handed the cursor, which means being handed the
//! keystrokes that follow it -- including the ones typed into a password
//! field on what the person believes is their own client. Pairing exists
//! to make that impossible for anyone who has not been introduced by
//! comparing six digits on two screens; a world-readable key hands it to
//! anyone with a local account and a copy of this program.
//!
//! `peers.toml` is the other half and needs a different argument. Its
//! contents are public keys, which are not secret; what matters is that
//! nobody may *write* it, because a peer added there is a machine this
//! one will accept a session from. On a server that is a way in.
//!
//! So: the directory grants the system account and administrators
//! everything and ordinary users read only, and the identity file is
//! given a list of its own with no ordinary users on it at all.
//!
//! ## The status report is deliberately on the readable side
//!
//! It is written into this directory too, and it stays readable, because
//! `smkvm status` and the window are run by the person at the desk and
//! would otherwise stop working the moment somebody chose `--system`.
//! What it discloses is which machines are connected and where their
//! screens are, to somebody sitting at one of them. That is a choice
//! rather than an accident of which directory it landed in.
//!
//! ## Why `icacls` and why by number
//!
//! The same reasoning as the scheduled task going through PowerShell:
//! this runs once, by hand, with a person watching and an exit code to
//! read. The groups are named by their well-known identifiers rather than
//! as "Administrators" and "Users" because those names are translated --
//! on the Korean and Japanese machines this is meant for they are
//! something else entirely, and a rule that silently fails to apply is
//! how a private key ends up world-readable while the install prints
//! success. That lesson is already in this repository once, in the
//! service status that was parsed out of translated text.

use std::ffi::OsStr;
use std::path::Path;

use anyhow::{bail, Context, Result};
use smkvm_config::paths;

use crate::secure::carry::{self, Carry, Step};

/// LocalSystem.
const SYSTEM: &str = "*S-1-5-18";
/// The built-in Administrators group.
const ADMINISTRATORS: &str = "*S-1-5-32-544";
/// The built-in Users group, which is every ordinary account.
const USERS: &str = "*S-1-5-32-545";

fn icacls(arguments: &[&OsStr]) -> Result<()> {
    let out = std::process::Command::new("icacls.exe")
        .args(arguments)
        .output()
        .context("running icacls.exe")?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stdout);
        let complained = String::from_utf8_lossy(&out.stderr);
        bail!(
            "icacls refused: {}",
            if complained.trim().is_empty() {
                said.trim()
            } else {
                complained.trim()
            }
        );
    }
    Ok(())
}

/// Make the machine-wide directory and put an explicit list on it.
///
/// `/inheritance:r` first, because the point is to stop inheriting
/// `%ProgramData%`'s own grants; `/grant:r` then replaces rather than
/// adds, so running this twice says the same thing rather than
/// accumulating. `(OI)(CI)` makes the grants apply to the files inside,
/// which is what puts them on the copies made next.
fn make_directory(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    icacls(&[
        dir.as_os_str(),
        OsStr::new("/inheritance:r"),
        OsStr::new("/grant:r"),
        OsStr::new(&format!("{SYSTEM}:(OI)(CI)F")),
        OsStr::new("/grant:r"),
        OsStr::new(&format!("{ADMINISTRATORS}:(OI)(CI)F")),
        // Read and execute, not write. Reading is what `smkvm status` and
        // the window need; writing is what would let any account add a
        // machine to the paired list.
        OsStr::new("/grant:r"),
        OsStr::new(&format!("{USERS}:(OI)(CI)RX")),
    ])
    .with_context(|| format!("setting who may read {}", dir.display()))
}

/// Take the ordinary users off one file, leaving the system account and
/// administrators.
fn keep_to_ourselves(file: &Path) -> Result<()> {
    icacls(&[
        file.as_os_str(),
        OsStr::new("/inheritance:r"),
        OsStr::new("/grant:r"),
        OsStr::new(&format!("{SYSTEM}:F")),
        OsStr::new("/grant:r"),
        OsStr::new(&format!("{ADMINISTRATORS}:F")),
    ])
    .with_context(|| format!("keeping {} to the system account", file.display()))
}

/// What happened, for the person watching.
pub struct Carried {
    pub steps: Vec<Step>,
    pub missing: Vec<&'static str>,
    pub directory: std::path::PathBuf,
}

/// Prepare `%ProgramData%\smkvm` for the service, carrying the person's
/// files across.
///
/// Run as the person, from an administrator prompt, before the service is
/// registered -- so that a machine which cannot be prepared is one that
/// never gets a service pointed at it.
pub fn prepare() -> Result<Carried> {
    let dir = paths::machine_config_dir();
    if dir == paths::config_dir() {
        bail!(
            "the machine-wide directory and this account's are the same place ({}). \
             ProgramData could not be found, so there is nowhere for a service to read \
             from that is not somebody's profile",
            dir.display()
        );
    }
    make_directory(&dir)?;

    // Out of this account's own places, whatever they are -- the person
    // running the install is the one whose machine this is paired as.
    let from = vec![
        (paths::CONFIG_NAME, paths::config_file()),
        (paths::IDENTITY_NAME, paths::identity_file()),
        (paths::PEERS_NAME, paths::peers_file()),
    ];
    let steps = carry::plan(&from, &dir, &|path| path.exists());

    for step in &steps {
        if step.carry == Carry::Copy {
            std::fs::copy(&step.from, &step.to).with_context(|| {
                format!("copying {} to {}", step.from.display(), step.to.display())
            })?;
        }
        // Applied whether it was copied or was already there: a file put
        // in this directory by hand, or left by an earlier version of
        // this code, must not stay readable just because this run did not
        // create it.
        if step.secret && step.to.exists() {
            keep_to_ourselves(&step.to)?;
        }
    }

    let missing = carry::what_is_missing(&steps);
    Ok(Carried {
        steps,
        missing,
        directory: dir,
    })
}

/// What `uninstall` should say about the copies, which it does not delete.
///
/// Deleting `device.toml` is the one irreversible thing in this whole
/// arrangement: it is the machine's identity, every other machine's
/// paired list names it, and there is no way back but pairing again on
/// both ends. An uninstall that silently did that -- to somebody who may
/// only be switching back to the login task for an afternoon -- would be
/// much worse than a file left behind, particularly as the file is
/// readable only by the system account and administrators. So it stays,
/// and the person is told exactly where it is and that removing the
/// directory is theirs to do.
pub fn what_is_left_behind() -> Option<String> {
    let dir = paths::machine_config_dir();
    let anything = paths::CARRIED_OVER
        .iter()
        .any(|name| dir.join(name).exists());
    anything.then(|| {
        format!(
            "The machine-wide copies are still in {}. They are left on purpose: \
             {} is this machine's identity, and deleting it means pairing with every \
             other machine again. Remove the directory by hand if that is what you want.",
            dir.display(),
            paths::IDENTITY_NAME
        )
    })
}
