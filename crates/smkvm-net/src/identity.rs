//! Who a machine is.
//!
//! A device's identity is a long-lived X25519 key pair generated once and kept
//! thereafter. Its [`DeviceId`] is a hash of the public half, which is what
//! every other machine records when it pairs. Nothing about the identity
//! depends on a name, an address, or a certificate authority, so a machine can
//! be renamed or moved to a different network and still be the same machine.

use std::path::{Path, PathBuf};

use smkvm_layout::DeviceId;

use crate::{Error, Result, NOISE_PARAMS};

/// A machine's own key pair.
pub struct Identity {
    private: Vec<u8>,
    public: Vec<u8>,
    id: DeviceId,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let the private half reach a log.
        f.debug_struct("Identity").field("id", &self.id).finish()
    }
}

/// Derive the public identifier for a static key.
pub fn device_id(public_key: &[u8]) -> DeviceId {
    let mut hasher = blake3::Hasher::new();
    // Domain-separated so this hash can never be confused with another use of
    // the same key material.
    hasher.update(b"smkvm device id v1");
    hasher.update(public_key);
    DeviceId::from_bytes(*hasher.finalize().as_bytes())
}

impl Identity {
    /// Make a new identity. Used once per machine, on first run.
    pub fn generate() -> Result<Identity> {
        let builder = snow::Builder::new(NOISE_PARAMS.parse().expect("the pattern is valid"));
        let keypair = builder.generate_keypair().map_err(Error::Crypto)?;
        Ok(Identity {
            id: device_id(&keypair.public),
            private: keypair.private,
            public: keypair.public,
        })
    }

    pub fn id(&self) -> DeviceId {
        self.id
    }

    pub fn public_key(&self) -> &[u8] {
        &self.public
    }

    pub(crate) fn private_key(&self) -> &[u8] {
        &self.private
    }

    /// Load an identity, creating one if the file is not there yet.
    ///
    /// The key is written with owner-only permissions. A machine's identity is
    /// what lets it be trusted by the others, so anyone who can read it can
    /// impersonate this machine.
    pub fn load_or_create(path: &Path) -> Result<Identity> {
        match std::fs::read_to_string(path) {
            Ok(text) => Identity::from_text(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let identity = Identity::generate()?;
                identity.save(path)?;
                Ok(identity)
            }
            Err(source) => Err(Error::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    fn from_text(text: &str, path: &Path) -> Result<Identity> {
        let mut private = None;
        for line in text.lines() {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("private_key") {
                let value = value.trim_start().trim_start_matches('=').trim();
                private = Some(decode_hex(value.trim_matches('"')).ok_or_else(|| {
                    Error::BadIdentity {
                        path: path.to_path_buf(),
                        why: "private_key is not hex".into(),
                    }
                })?);
            }
        }
        let private = private.ok_or_else(|| Error::BadIdentity {
            path: path.to_path_buf(),
            why: "no private_key".into(),
        })?;
        if private.len() != 32 {
            return Err(Error::BadIdentity {
                path: path.to_path_buf(),
                why: format!("private_key is {} bytes, expected 32", private.len()),
            });
        }
        let public = x25519_public(&private)?;
        Ok(Identity {
            id: device_id(&public),
            private,
            public,
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let text = format!(
            "# SMKVM device identity. Anyone who can read this file can act as\n\
             # this machine, so keep it to yourself.\n\
             device_id = \"{}\"\n\
             private_key = \"{}\"\n",
            self.id,
            encode_hex(&self.private)
        );
        write_private(path, text.as_bytes())
    }
}

/// The public half of an X25519 private key.
///
/// Derived rather than stored, so a hand-edited identity file cannot pair a
/// private key with a public one that does not belong to it.
fn x25519_public(private: &[u8]) -> Result<Vec<u8>> {
    use snow::params::DHChoice;
    use snow::resolvers::{CryptoResolver, DefaultResolver};

    let mut dh = DefaultResolver
        .resolve_dh(&DHChoice::Curve25519)
        .ok_or_else(|| Error::BadIdentity {
            path: PathBuf::new(),
            why: "this build has no curve25519".into(),
        })?;
    dh.set(private);
    Ok(dh.pubkey().to_vec())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(bytes).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

pub(crate) fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
        .collect()
}
