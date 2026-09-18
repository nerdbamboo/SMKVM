//! Two machines meeting for the first time.
//!
//! Pairing is the one moment neither side knows the other's key, so the
//! handshake exchanges them and a person decides whether to keep them.
//!
//! The code shown at each end is derived from the handshake transcript, which
//! is what makes it worth comparing. Anyone relaying the conversation would be
//! running two different handshakes and could not make both transcripts agree,
//! so the codes would differ. Matching codes mean the two machines are talking
//! to each other and not through anybody.

use tokio::net::{TcpStream, ToSocketAddrs};

use crate::identity::{device_id, Identity};
use crate::stream::{read_framed, split, write_framed, SecureReader, SecureWriter};
use crate::trust::Peer;
use crate::{Error, Result, PAIRING_PARAMS};

const PROLOGUE: &[u8] = b"smkvm pairing v1";

/// How many digits the confirmation code has.
///
/// Six gives a one-in-a-million chance that someone in the middle guesses a
/// code the person will accept, and short enough to read aloud.
const SAS_DIGITS: u32 = 6;

const ACCEPT: u8 = 1;
const REJECT: u8 = 0;

/// The code shown at both ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code(String);

impl Code {
    /// Derive the code from a handshake transcript.
    fn from_handshake(hash: &[u8]) -> Code {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"smkvm pairing code v1");
        hasher.update(hash);
        let bytes = hasher.finalize();
        let n = u32::from_le_bytes(bytes.as_bytes()[..4].try_into().expect("four bytes"));
        let modulus = 10u32.pow(SAS_DIGITS);
        Code(format!(
            "{:0width$}",
            n % modulus,
            width = SAS_DIGITS as usize
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A handshake that has completed but not yet been confirmed by anyone.
///
/// Holding one proves the far machine possesses the key it presented. It does
/// not mean anybody wants to talk to it, which is what [`Pairing::confirm`] is
/// for.
pub struct Pairing {
    reader: SecureReader,
    writer: SecureWriter,
    addr: Option<std::net::SocketAddr>,
    code: Code,
    peer_key: Vec<u8>,
    peer_name: String,
}

impl std::fmt::Debug for Pairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pairing")
            .field("code", &self.code)
            .field("peer_name", &self.peer_name)
            .finish_non_exhaustive()
    }
}

impl Pairing {
    /// The code to show. Pairing goes ahead only if it matches the other end.
    pub fn code(&self) -> &Code {
        &self.code
    }

    /// What the far machine calls itself. Its own claim, and nothing more:
    /// only the key is established by the handshake.
    pub fn peer_name(&self) -> &str {
        &self.peer_name
    }

    pub fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.addr
    }

    /// Offer to pair with a machine that is waiting for one.
    pub async fn initiate(
        addr: impl ToSocketAddrs,
        identity: &Identity,
        my_name: &str,
    ) -> Result<Pairing> {
        let stream = TcpStream::connect(addr).await.map_err(|source| Error::Io {
            path: Default::default(),
            source,
        })?;
        Pairing::start(stream, identity, my_name).await
    }

    pub async fn start(
        mut stream: TcpStream,
        identity: &Identity,
        my_name: &str,
    ) -> Result<Pairing> {
        let _ = stream.set_nodelay(true);
        let mut noise = snow::Builder::new(PAIRING_PARAMS.parse().expect("valid pattern"))
            .prologue(PROLOGUE)
            .local_private_key(identity.private_key())
            .build_initiator()
            .map_err(Error::Crypto)?;
        let mut scratch = vec![0u8; u16::MAX as usize];

        let n = noise
            .write_message(&[], &mut scratch)
            .map_err(Error::Crypto)?;
        write_framed(&mut stream, &scratch[..n]).await?;

        let second = read_framed(&mut stream).await?;
        let n = noise
            .read_message(&second, &mut scratch)
            .map_err(Error::Crypto)?;
        let peer_name = String::from_utf8_lossy(&scratch[..n]).into_owned();

        let n = noise
            .write_message(my_name.as_bytes(), &mut scratch)
            .map_err(Error::Crypto)?;
        write_framed(&mut stream, &scratch[..n]).await?;

        Pairing::finish(stream, noise, peer_name)
    }

    /// Wait for a machine that wants to pair.
    pub async fn accept(
        mut stream: TcpStream,
        identity: &Identity,
        my_name: &str,
    ) -> Result<Pairing> {
        let _ = stream.set_nodelay(true);
        let mut noise = snow::Builder::new(PAIRING_PARAMS.parse().expect("valid pattern"))
            .prologue(PROLOGUE)
            .local_private_key(identity.private_key())
            .build_responder()
            .map_err(Error::Crypto)?;
        let mut scratch = vec![0u8; u16::MAX as usize];

        let first = read_framed(&mut stream).await?;
        noise
            .read_message(&first, &mut scratch)
            .map_err(Error::Crypto)?;

        let n = noise
            .write_message(my_name.as_bytes(), &mut scratch)
            .map_err(Error::Crypto)?;
        write_framed(&mut stream, &scratch[..n]).await?;

        let third = read_framed(&mut stream).await?;
        let n = noise
            .read_message(&third, &mut scratch)
            .map_err(Error::Crypto)?;
        let peer_name = String::from_utf8_lossy(&scratch[..n]).into_owned();

        Pairing::finish(stream, noise, peer_name)
    }

    fn finish(
        stream: TcpStream,
        noise: snow::HandshakeState,
        peer_name: String,
    ) -> Result<Pairing> {
        let code = Code::from_handshake(noise.get_handshake_hash());
        let peer_key = noise.get_remote_static().ok_or(Error::NotPaired)?.to_vec();
        let addr = stream.peer_addr().ok();
        let transport = noise
            .into_stateless_transport_mode()
            .map_err(Error::Crypto)?;
        let (reader, writer) = split(stream, transport);
        Ok(Pairing {
            reader,
            writer,
            addr,
            code,
            peer_key,
            peer_name,
        })
    }

    /// Say yes, and wait to hear the same back.
    ///
    /// Both ends must accept. One person confirming a code the other never saw
    /// is exactly the situation this is meant to prevent.
    pub async fn confirm(mut self) -> Result<Peer> {
        self.writer.send(&[ACCEPT]).await?;
        let reply = self.reader.recv().await?;
        self.writer.shutdown().await;
        if reply.first() != Some(&ACCEPT) {
            return Err(Error::NotPaired);
        }
        Ok(Peer {
            id: device_id(&self.peer_key),
            public_key: self.peer_key,
            name: self.peer_name,
        })
    }

    /// Say no, and tell the other end so it can stop waiting.
    pub async fn reject(mut self) -> Result<()> {
        let _ = self.writer.send(&[REJECT]).await;
        self.writer.shutdown().await;
        Ok(())
    }
}
