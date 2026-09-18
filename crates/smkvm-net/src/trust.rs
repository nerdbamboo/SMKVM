//! The machines this one has been paired with.
//!
//! Only a machine listed here can complete a handshake. The list is written by
//! pairing, which requires a human to confirm a code at both ends, so nothing
//! reaches it by merely having connected once.

use std::collections::BTreeMap;
use std::path::Path;

use smkvm_layout::DeviceId;

use crate::identity::{decode_hex, device_id, encode_hex};
use crate::{Error, Result};

/// A machine that has been paired with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub id: DeviceId,
    /// The static public key the handshake must see.
    pub public_key: Vec<u8>,
    /// What to call it. Cosmetic, and safe to change.
    pub name: String,
}

/// Every machine this one will talk to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trust {
    peers: BTreeMap<DeviceId, Peer>,
}

impl Trust {
    pub fn new() -> Trust {
        Trust::default()
    }

    pub fn peers(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values()
    }

    pub fn get(&self, id: DeviceId) -> Option<&Peer> {
        self.peers.get(&id)
    }

    pub fn contains(&self, id: DeviceId) -> bool {
        self.peers.contains_key(&id)
    }

    pub fn by_key(&self, public_key: &[u8]) -> Option<&Peer> {
        self.peers.get(&device_id(public_key))
    }

    /// Record a machine. The identifier is derived from the key rather than
    /// supplied alongside it, so the two can never disagree.
    pub fn add(&mut self, public_key: Vec<u8>, name: impl Into<String>) -> DeviceId {
        let id = device_id(&public_key);
        self.peers.insert(
            id,
            Peer {
                id,
                public_key,
                name: name.into(),
            },
        );
        id
    }

    pub fn remove(&mut self, id: DeviceId) -> Option<Peer> {
        self.peers.remove(&id)
    }

    pub fn rename(&mut self, id: DeviceId, name: impl Into<String>) -> bool {
        match self.peers.get_mut(&id) {
            Some(peer) => {
                peer.name = name.into();
                true
            }
            None => false,
        }
    }

    pub fn load(path: &Path) -> Result<Trust> {
        match std::fs::read_to_string(path) {
            Ok(text) => Trust::parse(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Trust::new()),
            Err(source) => Err(Error::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn parse(text: &str, path: &Path) -> Result<Trust> {
        let doc: toml::Value = text
            .parse()
            .map_err(|e: toml::de::Error| Error::BadTrustStore {
                path: path.to_path_buf(),
                why: e.to_string(),
            })?;
        let mut trust = Trust::new();
        let Some(list) = doc.get("peer").and_then(|v| v.as_array()) else {
            return Ok(trust);
        };
        for entry in list {
            let bad = |why: &str| Error::BadTrustStore {
                path: path.to_path_buf(),
                why: why.to_string(),
            };
            let key = entry
                .get("public_key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| bad("a peer has no public_key"))?;
            let key = decode_hex(key).ok_or_else(|| bad("public_key is not hex"))?;
            if key.len() != 32 {
                return Err(bad("public_key is not 32 bytes"));
            }
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            // A stored identifier is only ever a comment: the real one comes
            // from the key, so a tampered file cannot make a key answer to a
            // name it does not own.
            let id = trust.add(key, name);
            if let Some(stated) = entry.get("device_id").and_then(|v| v.as_str()) {
                if stated != id.to_hex() {
                    return Err(bad(&format!(
                        "device_id {stated} does not match its public_key"
                    )));
                }
            }
        }
        Ok(trust)
    }

    pub fn to_toml(&self) -> String {
        let mut out =
            String::from("# Machines this one has been paired with. Only these can connect.\n");
        for peer in self.peers.values() {
            out.push_str("\n[[peer]]\n");
            out.push_str(&format!("name = {:?}\n", peer.name));
            out.push_str(&format!("device_id = \"{}\"\n", peer.id));
            out.push_str(&format!(
                "public_key = \"{}\"\n",
                encode_hex(&peer.public_key)
            ));
        }
        out
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(path, self.to_toml()).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}
