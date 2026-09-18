//! Typed messages over an encrypted connection.
//!
//! Message boundaries come from the protocol's own length prefix rather than
//! from the encryption underneath, so a message larger than one encrypted
//! record still arrives as one message and nothing above has to know that
//! records exist.

use serde::de::DeserializeOwned;
use serde::Serialize;
use smkvm_layout::DeviceId;
use smkvm_proto::FrameDecoder;

use crate::session::Session;
use crate::stream::SecureStream;
use crate::{Error, Result};

/// One authenticated connection, carrying typed messages.
#[derive(Debug)]
pub struct Link {
    stream: SecureStream,
    decoder: FrameDecoder,
    peer: DeviceId,
    peer_name: String,
}

impl From<Session> for Link {
    fn from(session: Session) -> Link {
        let peer = session.peer();
        let peer_name = session.peer_name().to_string();
        Link {
            stream: session.into_stream(),
            decoder: FrameDecoder::new(),
            peer,
            peer_name,
        }
    }
}

impl Link {
    pub fn peer(&self) -> DeviceId {
        self.peer
    }

    pub fn peer_name(&self) -> &str {
        &self.peer_name
    }

    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let frame = smkvm_proto::encode(msg).map_err(Error::Proto)?;
        self.stream.send(&frame).await
    }

    /// Wait for the next message.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        loop {
            match self.decoder.next_frame() {
                Ok(Some(frame)) => return smkvm_proto::decode(frame).map_err(Error::Proto),
                Ok(None) => {}
                Err(e) => return Err(Error::Proto(e)),
            }
            let bytes = self.stream.recv().await?;
            if bytes.is_empty() {
                // An empty record carries nothing; waiting on it forever would
                // be worse than going round again.
                continue;
            }
            self.decoder.extend(&bytes);
        }
    }

    pub async fn close(mut self) {
        self.stream.shutdown().await;
    }
}
