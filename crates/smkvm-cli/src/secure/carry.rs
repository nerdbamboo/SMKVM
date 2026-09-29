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
    /// It is already machine-wide and is the same file. Nothing to do,
    /// and nothing to say beyond a line.
    Keep,
    /// It is already machine-wide and is a *different* file from the
    /// person's. Left alone, as `Keep` is, but said loudly.
    ///
    /// This is the case worth separating out, and the identity is why. A
    /// person who installed `--system`, later went back to the login
    /// task and paired the machine again, now has a new key in their
    /// profile and the old one machine-wide. Keeping the old one is
    /// still the right default -- overwriting an identity is becoming a
    /// different machine -- but the consequence here is that the service
    /// starts, connects, and is turned away in the handshake, because
    /// every other machine's paired list names the new key. Reported as
    /// one more neutral line among three, that is a sentence nobody
    /// reads before an evening of wondering why the link will not come
    /// up.
    Differs,
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
                "kept the {} already at {}; it is the same as yours",
                self.name,
                self.to.display()
            ),
            Carry::Differs => format!(
                "WARNING: {} at {} is NOT the same as the one at {}. The service will use \
                 the first. {}",
                self.name,
                self.to.display(),
                self.from.display(),
                if self.name == SECRET {
                    "That is this machine's identity, so the service will introduce itself \
                     with a key the other machines may no longer know -- they would refuse \
                     the handshake with nothing to explain it. If you have paired this \
                     machine again since the last --system install, delete the first file \
                     and install again."
                } else {
                    "Delete the first file and install again to use yours instead."
                }
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
    same: &dyn Fn(&Path, &Path) -> bool,
) -> Vec<Step> {
    from.iter()
        .map(|(name, source)| {
            let target = to.join(name);
            let carry = if exists(&target) {
                // Existence alone cannot tell the ordinary case -- a
                // second install of the same machine -- from the one
                // that breaks the link. Both are left alone; only one of
                // them is worth a person's attention.
                if !exists(source) || same(source, &target) {
                    Carry::Keep
                } else {
                    Carry::Differs
                }
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

/// The files that are machine-wide and are not the person's.
pub fn differing(steps: &[Step]) -> Vec<&'static str> {
    steps
        .iter()
        .filter(|step| step.carry == Carry::Differs)
        .map(|step| step.name)
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

    /// Every file is the same as every other, which is the ordinary
    /// case: nothing has been re-paired.
    fn identical(_: &Path, _: &Path) -> bool {
        true
    }

    fn all_different(_: &Path, _: &Path) -> bool {
        false
    }

    #[test]
    fn a_paired_machine_carries_all_three_across() {
        let steps = plan(&sources(), &to(), &|_| false, &identical);
        assert_eq!(steps.len(), 3);
        // Nothing is machine-wide yet, so everything in the profile moves.
        let steps = plan(&sources(), &to(), &|p| under(p, r"C:\profile"), &identical);
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
        let steps = plan(
            &sources(),
            &to(),
            &|p| p == already || under(p, r"C:\profile"),
            &identical,
        );
        let identity = steps.iter().find(|s| s.name == IDENTITY_NAME).unwrap();
        assert_eq!(identity.carry, Carry::Keep);
        // Quietly, because with the two files identical this is the
        // ordinary second install and there is nothing to act on. The
        // loud version is `an_identity_that_is_not_the_same_identity_
        // is_said_loudly`.
        assert!(identity.said().contains("the same as yours"));
        assert!(!identity.said().contains("WARNING"));
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
        let steps = plan(
            &sources(),
            &to(),
            &|p| under(p, r"C:\ProgramData"),
            &identical,
        );
        assert!(steps.iter().all(|s| s.carry == Carry::Keep));
        assert!(what_is_missing(&steps).is_empty());
    }

    #[test]
    fn a_machine_with_nothing_to_carry_is_told_before_the_service_fails() {
        // The real failure was a service that started, found no
        // configuration and exited. Saying so while somebody is watching
        // the install is the whole point of this function.
        let steps = plan(&sources(), &to(), &|_| false, &identical);
        assert!(steps.iter().all(|s| s.carry == Carry::Absent));
        assert_eq!(what_is_missing(&steps), CARRIED_OVER.to_vec());
        assert!(steps[0].said().contains("no smkvm.toml to copy"));
    }

    #[test]
    fn the_identity_is_the_only_one_marked_secret() {
        let steps = plan(&sources(), &to(), &|_| true, &identical);
        let secret: Vec<_> = steps.iter().filter(|s| s.secret).map(|s| s.name).collect();
        assert_eq!(secret, vec![IDENTITY_NAME]);
    }

    #[test]
    fn nothing_is_ever_copied_out_of_the_machine_directory_into_itself() {
        let steps = plan(&sources(), &to(), &|_| true, &identical);
        assert!(steps.iter().all(|s| s.from != s.to));
    }

    #[test]
    fn an_identity_that_is_not_the_same_identity_is_said_loudly() {
        // Paired again under the login task after an earlier --system
        // install: the profile has the new key, ProgramData has the old
        // one, and the old one is what the service would introduce
        // itself with. Every other machine's paired list names the new
        // one, so the handshake is refused with nothing to explain it.
        let steps = plan(&sources(), &to(), &|_| true, &all_different);
        assert!(steps.iter().all(|s| s.carry == Carry::Differs));
        assert_eq!(differing(&steps), CARRIED_OVER.to_vec());
        let identity = steps.iter().find(|s| s.name == IDENTITY_NAME).unwrap();
        let said = identity.said();
        assert!(said.contains("WARNING"), "{said}");
        assert!(said.contains("NOT the same"), "{said}");
        assert!(said.contains("refuse the handshake"), "{said}");
        // Both paths are named, so it is actionable without guessing.
        assert!(said.contains(r"C:\ProgramData"), "{said}");
        assert!(said.contains(r"C:\profile"), "{said}");
    }

    #[test]
    fn a_second_install_of_the_same_machine_is_quiet() {
        // The common case must stay a single calm line, or the loud one
        // above is just more noise to skim past.
        let steps = plan(&sources(), &to(), &|_| true, &identical);
        assert!(steps.iter().all(|s| s.carry == Carry::Keep));
        assert!(differing(&steps).is_empty());
        assert!(!steps[0].said().contains("WARNING"));
    }

    #[test]
    fn a_machine_wide_file_with_no_counterpart_is_not_a_difference() {
        // Nothing in the profile to compare against -- a machine set up
        // by hand, or a person who has since removed their own copy.
        // Leaving it alone is right and there is nothing to warn about.
        let steps = plan(
            &sources(),
            &to(),
            &|p| under(p, r"C:\ProgramData"),
            &all_different,
        );
        assert!(steps.iter().all(|s| s.carry == Carry::Keep));
    }
}
