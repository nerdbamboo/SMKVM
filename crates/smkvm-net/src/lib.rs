//! Device identity, pairing, and the encrypted link between machines.
//!
//! Barrier's published vulnerabilities came down to one thing: a machine that
//! could reach the port was, in practice, allowed to take part. Its TLS was
//! optional, its stored fingerprints were accepted on first sight, and its
//! server would act on clipboard and file messages from a peer it had never
//! authenticated.
//!
//! Here a link carries nothing until both ends have proved which machine they
//! are. Authentication is not a check performed on a connection; it is the
//! handshake itself. Each machine holds a long-lived key pair, records the
//! public half of every machine it has been paired with, and a handshake
//! simply does not complete with anyone else. There is no code path in which
//! an unknown peer is talked to and then rejected, because there is no
//! verification step to get wrong.
//!
//! Pairing is the one moment two machines meet without already knowing each
//! other. It runs a handshake that exchanges keys, derives a short code from
//! the transcript, and shows it at both ends. Because the code comes from the
//! transcript, anyone sitting in the middle produces a different one, so
//! matching codes mean the two machines really are talking to each other.

#![forbid(unsafe_code)]

pub mod identity;
pub mod link;
pub mod pairing;
pub mod session;
pub mod stream;
pub mod trust;

use std::path::PathBuf;

/// The pattern for an ordinary session between two machines that already know
/// each other. Both static keys are settled in advance, so the handshake
/// authenticates both ends by construction: it cannot complete with anyone
/// whose key is not the expected one.
pub const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// The pattern for pairing, where neither side knows the other's key yet.
/// Both are exchanged during the handshake and confirmed by the human.
pub const PAIRING_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Largest plaintext one encrypted message may carry.
///
/// A Noise message is at most 65535 bytes including its 16-byte tag. Larger
/// payloads are split across several, so nothing above this layer has to care.
pub const MAX_NOISE_PAYLOAD: usize = 65535 - 16;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cryptography: {0}")]
    Crypto(snow::Error),
    #[error("{path} is not a usable identity: {why}")]
    BadIdentity { path: PathBuf, why: String },
    #[error("{path} is not a usable list of paired machines: {why}")]
    BadTrustStore { path: PathBuf, why: String },
    /// The handshake did not complete, which for these patterns means the peer
    /// is not who this machine expected.
    #[error("the machine at the other end is not one this machine is paired with")]
    NotPaired,
    #[error("the peer went away mid-handshake")]
    Disconnected,
    #[error("a message of {len} bytes exceeds the {max} byte limit")]
    MessageTooLarge { len: usize, max: usize },
    #[error(transparent)]
    Proto(smkvm_proto::ProtoError),
}
