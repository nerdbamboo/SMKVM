//! An encrypted connection between two machines, readable and writable at once.
//!
//! Everything above this is ordinary framed messages; everything below is a
//! TCP connection. In between, each write becomes one or more encrypted
//! records, length-prefixed so the far side can find their edges.
//!
//! The two directions are independent. Each has its own cipher state and its
//! own counter, so reading and writing can happen in separate tasks without
//! either waiting on the other — which matters because the link carries both
//! pointer movement, which must never queue behind anything, and bulk data,
//! which is large enough to queue behind.
//!
//! Record boundaries deliberately do not survive the crossing. The protocol
//! above has its own framing, and letting it rely on these boundaries as well
//! would mean two separate things had to agree about where a message ends.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::{Error, Result, MAX_NOISE_PAYLOAD};

/// Length prefix on every record, handshake and transport alike.
const LENGTH_PREFIX: usize = 2;

/// Read one length-prefixed message during a handshake.
pub(crate) async fn read_framed(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = [0u8; LENGTH_PREFIX];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|_| Error::Disconnected)?;
    let mut body = vec![0u8; usize::from(u16::from_le_bytes(header))];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|_| Error::Disconnected)?;
    Ok(body)
}

/// Write one length-prefixed message during a handshake.
pub(crate) async fn write_framed(stream: &mut TcpStream, body: &[u8]) -> Result<()> {
    let len = u16::try_from(body.len()).map_err(|_| Error::MessageTooLarge {
        len: body.len(),
        max: u16::MAX as usize,
    })?;
    stream
        .write_all(&len.to_le_bytes())
        .await
        .map_err(|_| Error::Disconnected)?;
    stream
        .write_all(body)
        .await
        .map_err(|_| Error::Disconnected)?;
    stream.flush().await.map_err(|_| Error::Disconnected)?;
    Ok(())
}

/// The shared cipher state, and the peer's established identity.
pub(crate) struct Cipher(pub(crate) snow::StatelessTransportState);

/// The receiving half.
pub struct SecureReader {
    reader: OwnedReadHalf,
    cipher: Arc<Cipher>,
    /// Counts the records read. The sender counts the same way, and TCP keeps
    /// them in order, so the two stay in step. A record that arrives out of
    /// order or altered simply fails to decrypt, and the link is finished --
    /// there is no resynchronising, by design.
    nonce: u64,
    scratch: Vec<u8>,
}

/// The sending half.
pub struct SecureWriter {
    writer: OwnedWriteHalf,
    cipher: Arc<Cipher>,
    nonce: u64,
    scratch: Vec<u8>,
}

impl std::fmt::Debug for SecureReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureReader").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for SecureWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureWriter").finish_non_exhaustive()
    }
}

pub(crate) fn split(
    stream: TcpStream,
    transport: snow::StatelessTransportState,
) -> (SecureReader, SecureWriter) {
    let cipher = Arc::new(Cipher(transport));
    let (reader, writer) = stream.into_split();
    (
        SecureReader {
            reader,
            cipher: cipher.clone(),
            nonce: 0,
            scratch: vec![0u8; u16::MAX as usize],
        },
        SecureWriter {
            writer,
            cipher,
            nonce: 0,
            scratch: vec![0u8; u16::MAX as usize],
        },
    )
}

impl SecureReader {
    /// The far machine's long-lived public key, as the handshake established
    /// it. Not something the peer asserted: the handshake could not have
    /// completed without the matching private key.
    pub fn peer_static_key(&self) -> Option<Vec<u8>> {
        self.cipher.0.get_remote_static().map(<[u8]>::to_vec)
    }

    /// Receive and decrypt the next record.
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        let mut header = [0u8; LENGTH_PREFIX];
        self.reader
            .read_exact(&mut header)
            .await
            .map_err(|_| Error::Disconnected)?;
        let mut record = vec![0u8; usize::from(u16::from_le_bytes(header))];
        self.reader
            .read_exact(&mut record)
            .await
            .map_err(|_| Error::Disconnected)?;

        let n = self
            .cipher
            .0
            .read_message(self.nonce, &record, &mut self.scratch)
            .map_err(Error::Crypto)?;
        self.nonce += 1;
        Ok(self.scratch[..n].to_vec())
    }
}

impl SecureWriter {
    /// Encrypt and send. Payloads too large for one record are split; the far
    /// side sees a byte stream either way.
    pub async fn send(&mut self, plaintext: &[u8]) -> Result<()> {
        let mut chunks = plaintext.chunks(MAX_NOISE_PAYLOAD);
        // An empty payload still deserves one record, so that a caller sending
        // nothing does not silently send nothing at all.
        let empty = [][..].into();
        let mut once = std::iter::once(empty);
        let iter: &mut dyn Iterator<Item = &[u8]> = if plaintext.is_empty() {
            &mut once
        } else {
            &mut chunks
        };

        for chunk in iter {
            let n = self
                .cipher
                .0
                .write_message(self.nonce, chunk, &mut self.scratch)
                .map_err(Error::Crypto)?;
            self.nonce += 1;
            let len = u16::try_from(n).map_err(|_| Error::MessageTooLarge {
                len: n,
                max: u16::MAX as usize,
            })?;
            self.writer
                .write_all(&len.to_le_bytes())
                .await
                .map_err(|_| Error::Disconnected)?;
            self.writer
                .write_all(&self.scratch[..n])
                .await
                .map_err(|_| Error::Disconnected)?;
        }
        self.writer.flush().await.map_err(|_| Error::Disconnected)?;
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        let _ = self.writer.shutdown().await;
    }
}
