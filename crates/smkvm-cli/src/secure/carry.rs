//! Getting the machine's files to where the service can read them.
//!
//! A service running as the system account has no profile worth reading,
//! so `--system` gives it `%ProgramData%\smkvm` instead. That is only half
//! an answer: an empty directory is a machine that has no configuration,
//! no identity and no paired peers, which is a machine that has to be
//! introduced to every other one again by comparing six digits on two
//! screens. Against the login task, which just works, that is a
//! regression nobody would accept.
//!
//! So `install --system` carries three files across once. Which three,
//! and what to do when one is already there or missing, is decided here,
//! where it can be tested; the copying and the access lists are the
//! platform's business and live in `windows::store`.
//!
//! The rule for a file that is already in the machine directory is to
//! leave it, and it is worth saying why rather than the more obvious
//! "overwrite so the newest wins". `device.toml` is this machine's
//! identity in the handshake. Replacing it is not an update, it is
//! becoming a different machine -- every other machine's `peers.toml`
//! still names the old key, so the link stops working and the only way
//! back is pairing again. An install that silently did that to a working
//! machine would be the worst kind of helpful. Leaving it means a second
//! `install --system` is safe to run, which is the thing somebody
//! reaching for it is most likely to do.

use std::path::{Path, PathBuf};

/// What to do about one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carry {
    /// It is in the person's profile and not machine-wide: copy it.
    Copy,
    /// It is already machine-wide. Left exactly as it is.
    Keep,
    /// It is in neither place. Nothing to copy, and for some files that
    /// is a reason to say something at install time rather than let the
    /// service fail later.
    Absent,
}

/// One file's name, where it is coming from, where it is going, and what
/// is to be done with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub name: &'static str,
    pub from: PathBuf,
    pub to: PathBuf,
    pub carry: Carry,
    /// Whether the file is a secret, which decides the access list put on
    /// it afterwards.
    pub secret: bool,
}

impl Step {
    /// What to print, in the person's terms.
    pub fn said(&self) -> String {
        match self.carry {
            Carry::Copy => format!("copied {} to {}", self.name, self.to.display()),
            Carry::Keep => format!(
                "kept the {} already at {} -- delete it and install again to replace it",
                self.name,
                self.to.display()
            ),
            Carry::Absent => format!(
                "no {} to copy; there is none at {}",
                self.name,
                self.from.display()
            ),
        }
    }
}

/// The identity file, which is the one with a private key in it.
pub const SECRET: &str = smkvm_config::paths::IDENTITY_NAME;

/// Work out what carrying the three files means, given where each one is.
///
/// `exists` answers "is there a file at this path", which is the only
/// thing about the filesystem this needs to know -- which is what makes
/// the rest of it testable.
pub fn plan(
    from: &[(&'static str, PathBuf)],
    to: &Path,
    exists: &dyn Fn(&Path) -> bool,
) -> Vec<Step> {
    from.iter()
        .map(|(name, source)| {
            let target = to.join(name);
            let carry = if exists(&target) {
                Carry::Keep
            } else if exists(source) {
                Carry::Copy
            } else {
                Carry::Absent
            };
            Step {
                name,
                from: source.clone(),
                to: target,
                carry,
                secret: *name == SECRET,
            }
        })
        .collect()
}

/// Would the service be able to start, given what the plan leaves behind?
///
/// The configuration is the one it cannot do without -- that is exactly
/// the failure this whole change is fixing, and it is much better said
/// while a person is watching an install than in a log after a service
/// has exited.
pub fn what_is_missing(steps: &[Step]) -> Vec<&'static str> {
    steps
        .iter()
        .filter(|step| step.carry == Carry::Absent)
        .map(|step| step.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smkvm_config::paths::{CARRIED_OVER, CONFIG_NAME, IDENTITY_NAME, PEERS_NAME};

    fn sources() -> Vec<(&'static str, PathBuf)> {
        CARRIED_OVER
            .iter()
            .map(|name| (*name, PathBuf::from(r"C:\profile").join(name)))
            .collect()
    }

    fn to() -> PathBuf {
        PathBuf::from(r"C:\ProgramData\smkvm")
    }

    /// Windows paths have no components on a Linux build -- the whole
    /// thing is one -- so `Path::starts_with` cannot be used to fake a
    /// filesystem here. Matching on the text is what these tests mean
    /// anyway: "is this file in that place".
    fn under(path: &Path, place: &str) -> bool {
        path.to_string_lossy().starts_with(place)
    }

    #[test]
    fn a_paired_machine_carries_all_three_across() {
        let steps = plan(&sources(), &to(), &|_| false);
        assert_eq!(steps.len(), 3);
        // Nothing is machine-wide yet, so everything in the profile moves.
        let steps = plan(&sources(), &to(), &|p| under(p, r"C:\profile"));
        assert!(steps.iter().all(|s| s.carry == Carry::Copy));
        assert!(what_is_missing(&steps).is_empty());
        assert_eq!(
            steps[0].to,
            PathBuf::from(r"C:\ProgramData\smkvm").join(CONFIG_NAME)
        );
    }

    #[test]
    fn an_identity_already_machine_wide_is_never_replaced() {
        // Replacing it is not an update, it is becoming a different
        // machine: every other machine's peers list still names the old
        // key, so the link stops and the only way back is pairing again.
        let already = to().join(IDENTITY_NAME);
        let steps = plan(&sources(), &to(), &|p| {
            p == already || under(p, r"C:\profile")
        });
        let identity = steps.iter().find(|s| s.name == IDENTITY_NAME).unwrap();
        assert_eq!(identity.carry, Carry::Keep);
        assert!(identity.said().contains("delete it and install again"));
        // And the others still go, so a machine part-way through is
        // finished rather than left.
        assert_eq!(
            steps
                .iter()
                .filter(|s| s.carry == Carry::Copy)
                .map(|s| s.name)
                .collect::<Vec<_>>(),
            vec![CONFIG_NAME, PEERS_NAME]
        );
    }

    #[test]
    fn installing_twice_changes_nothing_the_second_time() {
        let steps = plan(&sources(), &to(), &|p| under(p, r"C:\ProgramData"));
        assert!(steps.iter().all(|s| s.carry == Carry::Keep));
        assert!(what_is_missing(&steps).is_empty());
    }

    #[test]
    fn a_machine_with_nothing_to_carry_is_told_before_the_service_fails() {
        // The real failure was a service that started, found no
        // configuration and exited. Saying so while somebody is watching
        // the install is the whole point of this function.
        let steps = plan(&sources(), &to(), &|_| false);
        assert!(steps.iter().all(|s| s.carry == Carry::Absent));
        assert_eq!(what_is_missing(&steps), CARRIED_OVER.to_vec());
        assert!(steps[0].said().contains("no smkvm.toml to copy"));
    }

    #[test]
    fn the_identity_is_the_only_one_marked_secret() {
        let steps = plan(&sources(), &to(), &|_| true);
        let secret: Vec<_> = steps.iter().filter(|s| s.secret).map(|s| s.name).collect();
        assert_eq!(secret, vec![IDENTITY_NAME]);
    }

    #[test]
    fn nothing_is_ever_copied_out_of_the_machine_directory_into_itself() {
        let steps = plan(&sources(), &to(), &|_| true);
        assert!(steps.iter().all(|s| s.from != s.to));
    }
}
