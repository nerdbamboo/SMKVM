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
use crate::stream::{SecureReader, SecureWriter};
use crate::{Error, Result};

/// The receiving half of a link.
#[derive(Debug)]
pub struct LinkReader {
    stream: SecureReader,
    decoder: FrameDecoder,
}

/// The sending half of a link.
#[derive(Debug)]
pub struct LinkWriter {
    stream: SecureWriter,
}

/// One authenticated connection, carrying typed messages.
///
/// Splitting it is what lets a machine read and write at the same time, which
/// the server must: pointer movement has to keep flowing while a large paste
/// or a file transfer is still going out.
#[derive(Debug)]
pub struct Link {
    reader: LinkReader,
    writer: LinkWriter,
    peer: DeviceId,
    peer_name: String,
}

impl From<Session> for Link {
    fn from(session: Session) -> Link {
        let peer = session.peer();
        let peer_name = session.peer_name().to_string();
        let (reader, writer) = session.split();
        Link {
            reader: LinkReader {
                stream: reader,
                decoder: FrameDecoder::new(),
            },
            writer: LinkWriter { stream: writer },
            peer,
            peer_name,
        }
    }
}

impl LinkWriter {
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let frame = smkvm_proto::encode(msg).map_err(Error::Proto)?;
        self.stream.send(&frame).await
    }

    pub async fn close(&mut self) {
        self.stream.shutdown().await;
    }
}

impl LinkReader {
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
                continue;
            }
            self.decoder.extend(&bytes);
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
        self.writer.send(msg).await
    }

    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        self.reader.recv().await
    }

    /// Separate the directions so each can be driven on its own.
    pub fn split(self) -> (LinkReader, LinkWriter) {
        (self.reader, self.writer)
    }

    pub async fn close(mut self) {
        self.writer.close().await;
    }
}
