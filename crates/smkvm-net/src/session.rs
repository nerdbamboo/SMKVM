//! Connecting two machines that already know each other.
//!
//! The handshake pattern settles both static keys in advance, so completing it
//! *is* the authentication. An initiator's first message is encrypted to the
//! responder's key, which only the real responder can read; and the responder
//! learns the initiator's key from that same message, before it has said
//! anything back.
//!
//! That ordering matters. The responder checks its list of paired machines
//! while still silent, so an unknown peer never gets a reply, let alone a
//! session. There is no window in which a stranger is talked to and only
//! afterwards turned away.

use smkvm_layout::DeviceId;
use tokio::net::{TcpStream, ToSocketAddrs};

use crate::identity::{device_id, Identity};
use crate::stream::{read_framed, write_framed, SecureStream};
use crate::trust::{Peer, Trust};
use crate::{Error, Result, NOISE_PARAMS};

/// Mixed into the handshake so two builds that disagree about the protocol
/// fail to connect rather than proceeding to misunderstand each other.
const PROLOGUE: &[u8] = b"smkvm session v1";

/// An authenticated, encrypted connection to one machine.
#[derive(Debug)]
pub struct Session {
    stream: SecureStream,
    peer: DeviceId,
    peer_name: String,
}

impl Session {
    pub fn peer(&self) -> DeviceId {
        self.peer
    }

    pub fn peer_name(&self) -> &str {
        &self.peer_name
    }

    pub fn stream(&mut self) -> &mut SecureStream {
        &mut self.stream
    }

    pub fn into_stream(self) -> SecureStream {
        self.stream
    }

    /// Open a session with a machine this one is paired with.
    pub async fn connect(
        addr: impl ToSocketAddrs,
        identity: &Identity,
        peer: &Peer,
    ) -> Result<Session> {
        let stream = TcpStream::connect(addr).await.map_err(|source| Error::Io {
            path: Default::default(),
            source,
        })?;
        Session::start(stream, identity, peer).await
    }

    /// Run the handshake as initiator over an existing connection.
    pub async fn start(mut stream: TcpStream, identity: &Identity, peer: &Peer) -> Result<Session> {
        // Nagle would hold small messages back waiting for company, which on
        // the path every pointer movement takes is felt directly as lag.
        let _ = stream.set_nodelay(true);

        let mut noise = snow::Builder::new(NOISE_PARAMS.parse().expect("valid pattern"))
            .prologue(PROLOGUE)
            .local_private_key(identity.private_key())
            .remote_public_key(&peer.public_key)
            .build_initiator()
            .map_err(Error::Crypto)?;

        let mut scratch = vec![0u8; u16::MAX as usize];
        let n = noise
            .write_message(&[], &mut scratch)
            .map_err(Error::Crypto)?;
        write_framed(&mut stream, &scratch[..n]).await?;

        let reply = read_framed(&mut stream).await?;
        // Only the machine holding the expected private key could have
        // produced something that decrypts here.
        noise
            .read_message(&reply, &mut scratch)
            .map_err(|_| Error::NotPaired)?;

        let noise = noise.into_transport_mode().map_err(Error::Crypto)?;
        Ok(Session {
            stream: SecureStream::new(stream, noise),
            peer: peer.id,
            peer_name: peer.name.clone(),
        })
    }

    /// Run the handshake as responder, admitting only a paired machine.
    pub async fn accept(
        mut stream: TcpStream,
        identity: &Identity,
        trust: &Trust,
    ) -> Result<Session> {
        let _ = stream.set_nodelay(true);

        let mut noise = snow::Builder::new(NOISE_PARAMS.parse().expect("valid pattern"))
            .prologue(PROLOGUE)
            .local_private_key(identity.private_key())
            .build_responder()
            .map_err(Error::Crypto)?;

        let mut scratch = vec![0u8; u16::MAX as usize];
        let first = read_framed(&mut stream).await?;
        noise
            .read_message(&first, &mut scratch)
            .map_err(|_| Error::NotPaired)?;

        // Still nothing has been said back. Decide now whether this machine is
        // one to talk to at all.
        let key = noise.get_remote_static().ok_or(Error::NotPaired)?.to_vec();
        let id = device_id(&key);
        let Some(known) = trust.get(id) else {
            return Err(Error::NotPaired);
        };
        let peer_name = known.name.clone();

        let n = noise
            .write_message(&[], &mut scratch)
            .map_err(Error::Crypto)?;
        write_framed(&mut stream, &scratch[..n]).await?;

        let noise = noise.into_transport_mode().map_err(Error::Crypto)?;
        Ok(Session {
            stream: SecureStream::new(stream, noise),
            peer: id,
            peer_name,
        })
    }
}
