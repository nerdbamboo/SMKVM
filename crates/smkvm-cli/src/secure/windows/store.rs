//! The machine's own copy of its files, and who may read them.
//!
//! `%ProgramData%\smkvm`, made by `service install --system` and read by
//! the service. What goes in it is decided by [`crate::secure::carry`];
//! this is the making, the copying, the access lists -- and the reading
//! of those lists back, which is the part that earns the rest.
//!
//! ## What the key is, so that the care below reads as proportionate
//!
//! `device.toml` is this machine's private key in the Noise handshake. It
//! is the whole of what the server uses to decide this is the machine it
//! paired with. Anyone holding a copy can connect to the server as this
//! machine and be handed the cursor -- and therefore the keystrokes that
//! follow it, including the ones typed into a password field on what the
//! person believes is their own client. Pairing exists to make that
//! impossible for anyone who has not been introduced by comparing six
//! digits on two screens.
//!
//! It is also the one thing here that a rollback does not undo. Every
//! other fault in this feature was recoverable by going back to the
//! scheduled task; a key that was readable stays copied, and the only
//! remedy is pairing every machine again, which is the cost this whole
//! design exists to avoid.
//!
//! ## Closed by construction, rather than tightened afterwards
//!
//! The first version of this granted ordinary accounts read on the
//! directory and then took it away again from each secret. That was
//! wrong in three ways at once, and they were three faces of one
//! mistake -- the default was open:
//!
//! * a key the *service* generated later, on the documented path where
//!   nothing was there to carry over, was never tightened at all,
//!   because the only code that tightened anything ran at install time.
//!   It inherited the directory's grant and stayed readable for ever;
//! * a key that *was* carried over was readable between being written
//!   and being tightened, which is a race a local process wins without
//!   effort against a one-shot, user-initiated event;
//! * anything added in future would have been open unless somebody
//!   remembered.
//!
//! So the directory now grants the system account and administrators and
//! nobody else, inheritably. Anything created in it later -- by the
//! service, by a future version of this program, by hand -- is closed
//! without anyone having to think of it. Ordinary accounts get read on
//! the directory *itself*, not propagated to its contents, so that the
//! readable corner below can be reached and listed.
//!
//! ## The readable corner
//!
//! `smkvm status` and the window are run by the person at the desk, and
//! would stop working the moment somebody chose `--system` if everything
//! here were closed. So there is one subdirectory,
//! `paths::READABLE_CORNER`, which does grant ordinary accounts read and
//! does propagate it, and the daemon writes its report there and nothing
//! else. A subdirectory rather than a permission on the report, because
//! the report does not exist when these lists are set -- and a file
//! created later inherits what the directory says, which is the whole
//! lesson above.
//!
//! ## Ownership
//!
//! `%ProgramData%` lets ordinary accounts create folders, and grants
//! `CREATOR OWNER` full control of what they create. So any local
//! account can make `C:\ProgramData\smkvm` before we do and be its
//! owner. `/inheritance:r` removes inherited entries and `/grant:r`
//! rewrites only the entries it names, so a stranger's explicit entry
//! survives both -- and an owner holds `WRITE_DAC` implicitly anyway, so
//! stripping it would only postpone them putting it back.
//!
//! This is the pipe-name squatting from the first review against a
//! different object, and `acl.rs` already wrote the sentence for it:
//! *the owner of an object can always rewrite its access control list.*
//! So the owner is set to the system account before anything else, and a
//! directory that already existed is a decision rather than a shrug.
//!
//! ## And then it is read back
//!
//! Setting a list and believing the exit code is the same shape as the
//! silent refused injection this project started with. `icacls` prints
//! "Successfully processed 0 files; Failed processing 1 files" and has
//! been known to exit zero while doing it. Every list set here is read
//! back with `icacls /save`, which emits SDDL -- machine-readable and,
//! unlike the names in its ordinary output, not translated -- and
//! checked by [`acl::granted_to_anyone_but`]. If anyone but the system
//! account and administrators can reach the key, the install fails and
//! the service is never registered.
//!
//! The check somebody will actually run is
//! `type %ProgramData%\smkvm\device.toml` from an ordinary account, and
//! it must be refused. This is that check, made by the program, before
//! there is anything to read.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use smkvm_config::paths;

use crate::secure::acl;
use crate::secure::carry::{self, Carry, Step};

/// LocalSystem.
const SYSTEM: &str = "*S-1-5-18";
/// The built-in Administrators group.
const ADMINISTRATORS: &str = "*S-1-5-32-544";
/// The built-in Users group, which is every ordinary account.
const USERS: &str = "*S-1-5-32-545";

fn icacls(arguments: &[&OsStr]) -> Result<String> {
    let out = std::process::Command::new("icacls.exe")
        .args(arguments)
        .output()
        .context("running icacls.exe")?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let complained = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(
            "icacls refused: {}",
            if complained.is_empty() {
                &said
            } else {
                &complained
            }
        );
    }
    // It reports partial failure in its printing rather than in its exit
    // code, so the printing is read too. The number is not parsed out of
    // a translated sentence -- the presence of a non-zero count would be,
    // and that is not something to depend on -- this is only a first
    // refusal; the list is read back properly below whatever this says.
    if said.contains("Failed processing 1") || said.contains("Failed processing 2") {
        bail!("icacls reported a failure while exiting successfully: {said}");
    }
    Ok(said)
}

/// Make the machine-wide directory, owned by the system account and
/// closed to everyone but it and administrators.
///
/// `create_dir` rather than `create_dir_all` for the last component, so
/// that finding it already there is something this function decides about
/// rather than something it steps over. It is not refused -- an uninstall
/// leaves the directory behind on purpose, so meeting it again is
/// ordinary -- but it does mean somebody else may own it, which is why
/// the owner is taken first and the list rebuilt from nothing.
fn make_directory(dir: &Path) -> Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("making {}", parent.display()))?;
    }
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            tracing::debug!(
                directory = %dir.display(),
                "the machine-wide directory was already there; its owner and list are \
                 being taken over rather than trusted"
            );
        }
        Err(e) => return Err(e).with_context(|| format!("making {}", dir.display())),
    }

    // First, and before any grant. Whoever created the directory owns it
    // and can rewrite any list put on it afterwards, so until this
    // succeeds nothing below means anything.
    icacls(&[
        dir.as_os_str(),
        OsStr::new("/setowner"),
        OsStr::new(SYSTEM),
        OsStr::new("/t"),
        OsStr::new("/c"),
    ])
    .with_context(|| format!("taking ownership of {}", dir.display()))?;

    // `/inheritance:r` drops what ProgramData grants -- which includes
    // read for every authenticated account. `/grant:r` then replaces
    // rather than adds, so running this twice says the same thing.
    //
    // `(OI)(CI)` on the two that may hold the key, so files made here
    // later inherit them; nothing at all for ordinary accounts with
    // those flags, which is what makes a later file closed by default.
    // `remove` for ordinary accounts first, because `grant:r` rewrites
    // only what it names and an explicit entry left by whoever created
    // the directory would otherwise survive.
    icacls(&[
        dir.as_os_str(),
        OsStr::new("/inheritance:r"),
        OsStr::new("/remove"),
        OsStr::new(USERS),
        OsStr::new("/grant:r"),
        OsStr::new(&format!("{SYSTEM}:(OI)(CI)F")),
        OsStr::new("/grant:r"),
        OsStr::new(&format!("{ADMINISTRATORS}:(OI)(CI)F")),
    ])
    .with_context(|| format!("setting who may read {}", dir.display()))?;

    // Read on the directory object alone, with no inheritance flags, so
    // an ordinary account can list it and walk into the readable corner
    // and can read nothing that is in it.
    icacls(&[
        dir.as_os_str(),
        OsStr::new("/grant:r"),
        OsStr::new(&format!("{USERS}:(RX)")),
    ])
    .with_context(|| format!("letting ordinary accounts reach {}", dir.display()))?;
    Ok(())
}

/// The one subdirectory an ordinary account may read the contents of.
fn make_readable_corner(dir: &Path) -> Result<PathBuf> {
    let corner = dir.join(paths::READABLE_CORNER);
    if let Err(e) = std::fs::create_dir(&corner) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e).with_context(|| format!("making {}", corner.display()));
        }
    }
    icacls(&[
        corner.as_os_str(),
        OsStr::new("/inheritance:r"),
        OsStr::new("/grant:r"),
        OsStr::new(&format!("{SYSTEM}:(OI)(CI)F")),
        OsStr::new("/grant:r"),
        OsStr::new(&format!("{ADMINISTRATORS}:(OI)(CI)F")),
        // Inherited this time, so the report the daemon writes later is
        // readable. Nothing but the report is ever put here.
        OsStr::new("/grant:r"),
        OsStr::new(&format!("{USERS}:(OI)(CI)RX")),
    ])
    .with_context(|| format!("letting ordinary accounts read {}", corner.display()))?;
    Ok(corner)
}

/// Read a list back and say who is on it besides the two that may be.
///
/// `/save` writes SDDL, which is the same on every machine whatever
/// language it is in. `icacls`'s ordinary printing uses account names,
/// which are translated -- and a check that silently fails to parse on a
/// Korean machine is worse than no check, because it reports success.
fn who_can_reach(path: &Path) -> Result<Vec<String>> {
    let saved = std::env::temp_dir().join(format!("smkvm-acl-{}.sddl", std::process::id()));
    let outcome = icacls(&[
        path.parent().unwrap_or(path).as_os_str(),
        OsStr::new("/save"),
        saved.as_os_str(),
        OsStr::new("/c"),
    ]);
    let read = outcome.and_then(|_| {
        let bytes = std::fs::read(&saved)
            .with_context(|| format!("reading back the list from {}", saved.display()))?;
        // What `icacls /save` writes is UTF-16, little-endian, with a
        // byte-order mark.
        let wide: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        Ok(String::from_utf16_lossy(&wide))
    });
    let _ = std::fs::remove_file(&saved);
    let text = read?;

    let wanted = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let descriptors = acl::descriptors_in(&text);
    let found = descriptors
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(&wanted) || name.ends_with(&wanted));
    let Some((_, sddl)) = found else {
        // A list that could not be read is not a list that is correct.
        // Failing closed is the whole point of doing this at all.
        bail!(
            "the access list on {} could not be read back, so it cannot be confirmed. \
             `icacls \"{}\" /save` wrote nothing this build could parse",
            path.display(),
            path.parent().unwrap_or(path).display()
        );
    };
    Ok(acl::granted_to_anyone_but(sddl, &acl::MAY_HOLD_THE_KEY))
}

/// Confirm that nobody but the system account and administrators can
/// reach the key, and refuse if anyone can.
pub fn confirm_key_is_private(path: &Path) -> Result<()> {
    let strangers = who_can_reach(path)?;
    if !strangers.is_empty() {
        bail!(
            "{} can be reached by {}, which must not be so: it is this machine's private \
             key, and anyone holding a copy can connect to the server as this machine and \
             be handed the cursor, and the keystrokes that follow it. Check with `icacls \
             \"{}\"`; the remedy is to delete {} and install again, and to pair this \
             machine afresh, because a key that has been readable stays copied",
            path.display(),
            strangers.join(", "),
            path.display(),
            path.parent().unwrap_or(path).display()
        );
    }
    Ok(())
}

/// What happened, for the person watching.
pub struct Carried {
    pub steps: Vec<Step>,
    pub missing: Vec<&'static str>,
    pub differing: Vec<&'static str>,
    pub directory: PathBuf,
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
    make_readable_corner(&dir)?;

    // Out of this account's own places, whatever they are -- the person
    // running the install is the one whose machine this is paired as.
    let from = vec![
        (paths::CONFIG_NAME, paths::config_file()),
        (paths::IDENTITY_NAME, paths::identity_file()),
        (paths::PEERS_NAME, paths::peers_file()),
    ];
    let steps = carry::plan(&from, &dir, &|path| path.exists(), &|a, b| match (
        std::fs::read(a),
        std::fs::read(b),
    ) {
        (Ok(a), Ok(b)) => a == b,
        // Unreadable is not the same as identical. Saying "these
        // differ" when one cannot be read is the direction that gets
        // looked at rather than the one that gets skipped.
        _ => false,
    });

    for step in &steps {
        if step.carry == Carry::Copy {
            // It inherits the directory's list, which grants the system
            // account and administrators and nobody else -- so it is
            // closed from the instant it exists. The version of this
            // that copied first and tightened afterwards had a window on
            // exactly this line.
            std::fs::copy(&step.from, &step.to).with_context(|| {
                format!("copying {} to {}", step.from.display(), step.to.display())
            })?;
        }
    }

    // Read back, always, whatever was or was not done just now: a key
    // left by an earlier version of this code, or put there by hand, is
    // exactly as dangerous as one this run created.
    let key = dir.join(paths::IDENTITY_NAME);
    if key.exists() {
        confirm_key_is_private(&key)?;
    }

    let missing = carry::what_is_missing(&steps);
    let differing = carry::differing(&steps);
    Ok(Carried {
        steps,
        missing,
        differing,
        directory: dir,
    })
}

/// Tidy up what the install put there, except the things that cost
/// something to lose.
///
/// The report goes: it is not an identity, deleting it costs nothing, and
/// leaving it is a real fault -- every reader would go on finding a dead
/// file in `%ProgramData%` that shadows the live one a hand-started
/// daemon writes in its profile, and `smkvm status` would say nothing was
/// running while it ran perfectly.
pub fn forget_the_report() {
    let report = paths::status_file_in(
        paths::Scope::Machine,
        &paths::Roots::from_env(),
        cfg!(windows),
    );
    if report.exists() {
        if let Err(e) = std::fs::remove_file(&report) {
            tracing::warn!("could not remove {}: {e}", report.display());
        }
    }
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
