//! An encrypted byte stream between two machines.
//!
//! Everything above this is ordinary framed messages; everything below is a
//! TCP connection. In between, each write becomes one or more Noise transport
//! messages, length-prefixed so the far side can find their edges.
//!
//! Message boundaries deliberately do not survive the crossing. The protocol
//! above has its own framing, and letting it rely on these boundaries too
//! would mean two things had to agree about where a message ends.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{Error, Result, MAX_NOISE_PAYLOAD};

/// Length prefix on every message on the wire, handshake and transport alike.
const LENGTH_PREFIX: usize = 2;

/// Read one length-prefixed message.
pub(crate) async fn read_framed(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = [0u8; LENGTH_PREFIX];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|_| Error::Disconnected)?;
    let len = usize::from(u16::from_le_bytes(header));
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|_| Error::Disconnected)?;
    Ok(body)
}

/// Write one length-prefixed message.
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

/// A TCP connection carrying Noise transport messages.
pub struct SecureStream {
    stream: TcpStream,
    noise: snow::TransportState,
    scratch: Vec<u8>,
}

impl std::fmt::Debug for SecureStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureStream")
            .field("peer", &self.stream.peer_addr().ok())
            .finish()
    }
}

impl SecureStream {
    pub(crate) fn new(stream: TcpStream, noise: snow::TransportState) -> Self {
        Self {
            stream,
            noise,
            scratch: vec![0u8; u16::MAX as usize],
        }
    }

    /// The far machine's long-lived public key, as the handshake established
    /// it. Not something the peer asserted: the handshake could not have
    /// completed if it did not hold the matching private key.
    pub fn peer_static_key(&self) -> Option<Vec<u8>> {
        self.noise.get_remote_static().map(<[u8]>::to_vec)
    }

    pub fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.stream.peer_addr().ok()
    }

    /// Encrypt and send. Large payloads are split; the far side sees a byte
    /// stream either way.
    pub async fn send(&mut self, plaintext: &[u8]) -> Result<()> {
        for chunk in plaintext.chunks(MAX_NOISE_PAYLOAD).chain(
            // An empty payload still deserves one message, so that a caller
            // sending nothing does not silently send nothing at all.
            plaintext.is_empty().then_some(&[][..]),
        ) {
            let n = self
                .noise
                .write_message(chunk, &mut self.scratch)
                .map_err(Error::Crypto)?;
            write_framed(&mut self.stream, &self.scratch[..n]).await?;
        }
        Ok(())
    }

    /// Receive and decrypt the next message.
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        let message = read_framed(&mut self.stream).await?;
        let n = self
            .noise
            .read_message(&message, &mut self.scratch)
            .map_err(Error::Crypto)?;
        Ok(self.scratch[..n].to_vec())
    }

    pub async fn shutdown(&mut self) {
        let _ = self.stream.shutdown().await;
    }
}
