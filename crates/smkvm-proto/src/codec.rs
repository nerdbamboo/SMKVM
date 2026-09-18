//! Framing and encoding.
//!
//! Frames are a little-endian `u32` byte count followed by that many bytes of
//! postcard-encoded message. Nothing here touches a socket: bytes go in, frames
//! come out, so the part of the system that parses hostile input can be tested
//! and fuzzed without a network.
//!
//! That separation is deliberate. Barrier's published vulnerabilities were in
//! exactly this layer — a length taken at face value, a buffer sized from it —
//! so here a length is checked before a single byte is reserved, and the
//! decoder is a pure function over a slice.

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Largest frame this build will accept or produce.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;

/// Largest payload carried by one clipboard or file chunk.
///
/// Well under [`MAX_FRAME_LEN`] so chunk framing overhead can never push a
/// frame over the limit, and small enough that a transfer yields to input
/// frequently.
pub const MAX_CHUNK_DATA: usize = 128 * 1024;

/// Bytes of length prefix in front of every frame.
pub const HEADER_LEN: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("frame of {len} bytes exceeds the {max} byte limit")]
    FrameTooLarge { len: usize, max: usize },
    #[error("message did not decode: {0}")]
    Decode(postcard::Error),
    #[error("message did not encode: {0}")]
    Encode(postcard::Error),
}

/// Encode a message into a length-prefixed frame.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    encode_with_limit(msg, MAX_FRAME_LEN)
}

pub fn encode_with_limit<T: Serialize>(msg: &T, max: usize) -> Result<Vec<u8>, ProtoError> {
    let body = postcard::to_stdvec(msg).map_err(ProtoError::Encode)?;
    if body.len() > max {
        return Err(ProtoError::FrameTooLarge {
            len: body.len(),
            max,
        });
    }
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode one frame body.
pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, ProtoError> {
    postcard::from_bytes(frame).map_err(ProtoError::Decode)
}

/// Reassembles frames from a byte stream.
///
/// Feed it whatever arrives from the socket, in whatever sizes, then pull
/// complete frames until it says there are none. A declared length over the
/// limit is an error before any memory is committed to it.
#[derive(Debug)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// How much of `buf` has been consumed.
    start: usize,
    max: usize,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::with_limit(MAX_FRAME_LEN)
    }

    pub fn with_limit(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            start: 0,
            max,
        }
    }

    /// Bytes received but not yet formed into a complete frame.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Total bytes currently held, consumed and not. Useful for backpressure
    /// and for asserting that a long-lived connection stays bounded.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    pub fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Take the next complete frame, if one has arrived.
    ///
    /// A [`ProtoError::FrameTooLarge`] is terminal: the stream cannot be
    /// resynchronised, so the same error repeats and the caller should drop
    /// the connection.
    pub fn next_frame(&mut self) -> Result<Option<&[u8]>, ProtoError> {
        self.compact();
        let avail = &self.buf[self.start..];
        if avail.len() < HEADER_LEN {
            return Ok(None);
        }
        let len = u32::from_le_bytes([avail[0], avail[1], avail[2], avail[3]]) as usize;
        if len > self.max {
            return Err(ProtoError::FrameTooLarge { len, max: self.max });
        }
        if avail.len() < HEADER_LEN + len {
            return Ok(None);
        }
        let body = self.start + HEADER_LEN;
        self.start = body + len;
        Ok(Some(&self.buf[body..body + len]))
    }

    /// Drop consumed bytes once they are worth reclaiming, so a long-lived
    /// connection does not grow its buffer without bound.
    fn compact(&mut self) {
        const THRESHOLD: usize = 64 * 1024;
        if self.start >= THRESHOLD && self.start * 2 >= self.buf.len() {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}
