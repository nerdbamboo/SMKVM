//! Carrying the clipboard between machines.
//!
//! Every machine watches its own clipboard. A copy produces an *offer* -- a
//! description of what is available, nothing more -- which travels to the
//! others, and each of them puts that offer on its own clipboard. Only when
//! something pastes are the contents fetched, one chunk at a time, from
//! wherever they actually are. A screenshot that is never pasted costs one
//! small message.
//!
//! The server is the hub. A client speaks only to the server; an offer from
//! one client reaches the others by way of the server, which holds the offer
//! itself and fetches on their behalf when they paste. That keeps a client
//! from ever needing to know how many other machines there are.
//!
//! Like the rest of this crate this is a state machine: inputs go in, outputs
//! come out, and nothing here touches a socket or a clipboard. The same code
//! runs on both ends -- a client is simply a hub with one peer.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use smkvm_layout::DeviceId;
use smkvm_proto::{Bulk, ClipEntry, ClipError, ClipFormat, ClipOffer, ClipSeq, MAX_CHUNK_DATA};

/// Things that happen to the exchange.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// A machine's link is up and can carry clipboard traffic.
    PeerUp(DeviceId),
    PeerGone(DeviceId),
    /// Something was copied on this machine, and these formats are available.
    LocalChanged(Vec<ClipFormat>),
    /// A clipboard message arrived from a machine.
    FromPeer {
        from: DeviceId,
        msg: Bulk,
    },
    /// Something on this machine is pasting an offer we hold from elsewhere.
    ///
    /// `ticket` is the caller's; it comes back on [`Output::Deliver`].
    Wanted {
        ticket: u64,
        seq: ClipSeq,
        format: ClipFormat,
    },
    /// The local clipboard has been read, in answer to [`Output::ReadLocal`].
    ReadDone {
        seq: ClipSeq,
        format: ClipFormat,
        result: Result<Vec<u8>, ClipError>,
    },
}

/// What should be done about it.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Send {
        to: DeviceId,
        msg: Bulk,
    },
    /// Put this offer on the local clipboard. Its contents are fetched through
    /// [`Input::Wanted`] when something pastes.
    OfferLocally {
        seq: ClipSeq,
        formats: Vec<ClipFormat>,
    },
    /// Stop offering: whatever was offered can no longer be fetched.
    ReleaseLocally,
    /// Read this format of the local clipboard and answer with
    /// [`Input::ReadDone`].
    ReadLocal {
        seq: ClipSeq,
        format: ClipFormat,
    },
    /// The answer to an [`Input::Wanted`].
    Deliver {
        ticket: u64,
        result: Result<Vec<u8>, ClipError>,
    },
}

/// Which spelling of a format the configuration uses.
///
/// Anything unrecognised is ignored rather than refused: a format this build
/// cannot carry is not a reason to carry nothing.
pub fn formats_from_names(names: &[String]) -> Vec<ClipFormat> {
    let mut out = Vec::new();
    for name in names {
        let format = match name.trim().to_ascii_lowercase().as_str() {
            "text" | "plain" => ClipFormat::Text,
            "html" => ClipFormat::Html,
            "image" | "png" | "images" => ClipFormat::Png,
            "files" | "uris" | "file" => ClipFormat::Uris,
            _ => continue,
        };
        if !out.contains(&format) {
            out.push(format);
        }
    }
    out
}

/// An offer this machine currently holds on its own clipboard, from elsewhere.
#[derive(Debug, Clone, PartialEq)]
struct Held {
    seq: ClipSeq,
    /// The machine to fetch from. Not necessarily the one that copied: a
    /// client fetches everything through the server.
    from: DeviceId,
    formats: Vec<ClipFormat>,
    /// When the offer was put on the local clipboard, so a change notice that
    /// is merely our own offer arriving can be told from a real copy.
    placed: Instant,
}

/// Contents on their way from another machine.
#[derive(Debug)]
struct Fetching {
    from: DeviceId,
    buf: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
enum Waiter {
    /// Something on this machine pasting.
    Ticket(u64),
    /// Another machine asking for a chunk.
    Relay { to: DeviceId, offset: u64 },
}

#[derive(Debug, Clone, PartialEq)]
struct Waiting {
    seq: ClipSeq,
    format: ClipFormat,
    who: Waiter,
}

/// How long after placing an offer a change notice is taken to be that very
/// offer coming back round, rather than somebody copying something new.
///
/// The platform backends already suppress their own notices where they can;
/// this catches the ones they cannot, and is short enough that a real copy a
/// moment later still gets through.
const ECHO_WINDOW: Duration = Duration::from_millis(750);

pub struct Exchange {
    me: DeviceId,
    peers: BTreeSet<DeviceId>,
    counter: u64,
    allowed: Vec<ClipFormat>,
    max_bytes: u64,
    enabled: bool,

    /// What this machine's own clipboard is offering, if anything.
    local: Option<(ClipSeq, Vec<ClipFormat>)>,
    /// The offer from elsewhere that this machine's clipboard currently holds.
    remote: Option<Held>,
    /// Contents that can be served straight away: our own clipboard once read,
    /// and anything fetched from elsewhere. Only the current offers' entries
    /// are kept, so this never grows past a couple of pastes' worth.
    cache: HashMap<(ClipSeq, ClipFormat), Vec<u8>>,
    reading: BTreeSet<(ClipSeq, ClipFormat)>,
    fetching: HashMap<(ClipSeq, ClipFormat), Fetching>,
    waiting: Vec<Waiting>,
}

impl Exchange {
    pub fn new(me: DeviceId, allowed: Vec<ClipFormat>, max_bytes: u64) -> Exchange {
        Exchange {
            me,
            peers: BTreeSet::new(),
            counter: 0,
            allowed,
            max_bytes,
            enabled: true,
            local: None,
            remote: None,
            cache: HashMap::new(),
            reading: BTreeSet::new(),
            fetching: HashMap::new(),
            waiting: Vec::new(),
        }
    }

    /// Turn sharing on or off without losing track of the machines.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_allowed(&mut self, allowed: Vec<ClipFormat>) {
        self.allowed = allowed;
    }

    pub fn set_max_bytes(&mut self, max_bytes: u64) {
        self.max_bytes = max_bytes;
    }

    /// The offer this machine is currently making, if any.
    pub fn local_offer(&self) -> Option<ClipSeq> {
        self.local.as_ref().map(|(seq, _)| *seq)
    }

    /// The offer from elsewhere this machine currently holds, if any.
    pub fn held_offer(&self) -> Option<ClipSeq> {
        self.remote.as_ref().map(|h| h.seq)
    }

    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        match input {
            Input::PeerUp(peer) => self.peer_up(peer, &mut out),
            Input::PeerGone(peer) => self.peer_gone(peer, &mut out),
            Input::LocalChanged(formats) => self.local_changed(formats, now, &mut out),
            Input::FromPeer { from, msg } => self.peer_said(from, msg, now, &mut out),
            Input::Wanted {
                ticket,
                seq,
                format,
            } => self.wanted(ticket, seq, format, &mut out),
            Input::ReadDone {
                seq,
                format,
                result,
            } => self.read_done(seq, format, result, &mut out),
        }
        out
    }

    fn current_offer(&self) -> Option<ClipOffer> {
        if let Some((seq, formats)) = &self.local {
            return Some(offer(*seq, formats));
        }
        self.remote.as_ref().map(|h| offer(h.seq, &h.formats))
    }

    fn peer_up(&mut self, peer: DeviceId, out: &mut Vec<Output>) {
        self.peers.insert(peer);
        if !self.enabled {
            return;
        }
        // A machine arriving gets whatever is on the clipboard now, so it does
        // not have to wait for the next copy to join in.
        let skip = self.remote.as_ref().map(|h| h.from);
        if let Some(current) = self.current_offer() {
            if skip != Some(peer) {
                out.push(Output::Send {
                    to: peer,
                    msg: Bulk::ClipOffer(current),
                });
            }
        }
    }

    fn peer_gone(&mut self, peer: DeviceId, out: &mut Vec<Output>) {
        self.peers.remove(&peer);
        let gone: Vec<(ClipSeq, ClipFormat)> = self
            .fetching
            .iter()
            .filter(|(_, f)| f.from == peer)
            .map(|(k, _)| k.clone())
            .collect();
        for key in gone {
            self.fetching.remove(&key);
            self.fail_waiters(&key.0, &key.1, ClipError::Cancelled, out);
        }
        // Nothing waiting on that machine can be answered any more, and a
        // relay to it has nowhere to go.
        self.waiting
            .retain(|w| !matches!(w.who, Waiter::Relay { to, .. } if to == peer));
        if self.remote.as_ref().is_some_and(|h| h.from == peer) {
            let seq = self.remote.take().map(|h| h.seq);
            if let Some(seq) = seq {
                self.forget(seq, out);
            }
            out.push(Output::ReleaseLocally);
        }
    }

    fn local_changed(&mut self, formats: Vec<ClipFormat>, now: Instant, out: &mut Vec<Output>) {
        if !self.enabled {
            return;
        }
        let formats: Vec<ClipFormat> = formats
            .into_iter()
            .filter(|f| self.allowed.contains(f))
            .collect();
        // A notice with nothing usable in it says nothing about what is
        // held. It is what a watcher reports when the owner did not answer in
        // time -- our own owner, busy fetching, included -- and what a copy
        // of some form this cannot carry looks like. Taking `local` or
        // `remote` away on its strength left peers offering a sequence this
        // machine had forgotten, and their next paste refused as stale.
        if formats.is_empty() {
            return;
        }

        // Our own offer coming back round as a change. The backends catch this
        // where they can; where they cannot, it arrives moments after the
        // offer was placed and says nothing new.
        if let Some(held) = &self.remote {
            let echo = now.duration_since(held.placed) <= ECHO_WINDOW
                && formats.iter().all(|f| held.formats.contains(f));
            if echo {
                return;
            }
        }

        if let Some((old, _)) = self.local.take() {
            self.forget(old, out);
        }
        // Whatever was held from elsewhere has been replaced on the clipboard
        // by the copy that just happened.
        if let Some(held) = self.remote.take() {
            self.forget(held.seq, out);
        }

        self.counter += 1;
        let seq = ClipSeq {
            device: self.me,
            counter: self.counter,
        };
        self.local = Some((seq, formats.clone()));
        let announcement = offer(seq, &formats);
        for peer in &self.peers {
            out.push(Output::Send {
                to: *peer,
                msg: Bulk::ClipOffer(announcement.clone()),
            });
        }
    }

    fn peer_said(&mut self, from: DeviceId, msg: Bulk, now: Instant, out: &mut Vec<Output>) {
        match msg {
            Bulk::ClipOffer(offer) => self.offered(from, offer, now, out),
            Bulk::ClipRequest {
                seq,
                format,
                offset,
            } => self.requested(from, seq, format, offset, out),
            Bulk::ClipChunk {
                seq,
                format,
                offset,
                data,
                last,
            } => self.chunk_arrived(from, seq, format, offset, data, last, out),
            Bulk::ClipUnavailable {
                seq,
                format,
                reason,
            } => {
                if self
                    .fetching
                    .get(&(seq, format.clone()))
                    .is_some_and(|f| f.from == from)
                {
                    self.fetching.remove(&(seq, format.clone()));
                    self.fail_waiters(&seq, &format, reason, out);
                }
            }
            Bulk::ClipCancel { .. } => {}
            // File transfer is not carried yet; a machine that sends it is
            // ahead of this build and nothing here can act on it.
            Bulk::FileOffer(_)
            | Bulk::FileRequest { .. }
            | Bulk::FileChunk { .. }
            | Bulk::FileDone { .. }
            | Bulk::FileAbort { .. } => {}
        }
    }

    fn offered(&mut self, from: DeviceId, offer: ClipOffer, now: Instant, out: &mut Vec<Output>) {
        if !self.enabled {
            return;
        }
        let formats: Vec<ClipFormat> = offer
            .entries
            .iter()
            .map(|e| e.format.clone())
            .filter(|f| self.allowed.contains(f))
            .collect();
        if formats.is_empty() {
            return;
        }
        // Whatever this machine was offering is superseded by the copy that
        // just happened elsewhere, exactly as a local copy would supersede it.
        if let Some((old, _)) = self.local.take() {
            self.forget(old, out);
        }
        if let Some(held) = self.remote.take() {
            if held.seq != offer.seq {
                self.forget(held.seq, out);
            }
        }
        self.remote = Some(Held {
            seq: offer.seq,
            from,
            formats: formats.clone(),
            placed: now,
        });
        out.push(Output::OfferLocally {
            seq: offer.seq,
            formats: formats.clone(),
        });
        // The hub passes it on. A client has no other peers, so this does
        // nothing there.
        let forwarded = ClipOffer {
            seq: offer.seq,
            entries: formats
                .iter()
                .map(|f| ClipEntry {
                    format: f.clone(),
                    bytes: None,
                    hash: None,
                })
                .collect(),
        };
        for peer in self.peers.iter().filter(|p| **p != from) {
            out.push(Output::Send {
                to: *peer,
                msg: Bulk::ClipOffer(forwarded.clone()),
            });
        }
    }

    fn requested(
        &mut self,
        from: DeviceId,
        seq: ClipSeq,
        format: ClipFormat,
        offset: u64,
        out: &mut Vec<Output>,
    ) {
        let key = (seq, format.clone());
        if let Some(data) = self.cache.get(&key) {
            out.push(Output::Send {
                to: from,
                msg: chunk(seq, format, data, offset),
            });
            return;
        }

        let ours = self
            .local
            .as_ref()
            .is_some_and(|(s, formats)| *s == seq && formats.contains(&format));
        if ours {
            self.waiting.push(Waiting {
                seq,
                format: format.clone(),
                who: Waiter::Relay { to: from, offset },
            });
            if self.reading.insert(key) {
                out.push(Output::ReadLocal { seq, format });
            }
            return;
        }

        let held_from = self
            .remote
            .as_ref()
            .filter(|h| h.seq == seq && h.formats.contains(&format) && h.from != from)
            .map(|h| h.from);
        if let Some(owner) = held_from {
            self.waiting.push(Waiting {
                seq,
                format: format.clone(),
                who: Waiter::Relay { to: from, offset },
            });
            self.start_fetch(owner, seq, format, out);
            return;
        }

        out.push(Output::Send {
            to: from,
            msg: Bulk::ClipUnavailable {
                seq,
                format,
                reason: ClipError::Stale,
            },
        });
    }

    fn wanted(&mut self, ticket: u64, seq: ClipSeq, format: ClipFormat, out: &mut Vec<Output>) {
        let key = (seq, format.clone());
        if let Some(data) = self.cache.get(&key) {
            out.push(Output::Deliver {
                ticket,
                result: Ok(data.clone()),
            });
            return;
        }
        let owner = self
            .remote
            .as_ref()
            .filter(|h| h.seq == seq && h.formats.contains(&format))
            .map(|h| h.from);
        let Some(owner) = owner else {
            out.push(Output::Deliver {
                ticket,
                result: Err(ClipError::Stale),
            });
            return;
        };
        self.waiting.push(Waiting {
            seq,
            format: format.clone(),
            who: Waiter::Ticket(ticket),
        });
        self.start_fetch(owner, seq, format, out);
    }

    fn start_fetch(
        &mut self,
        from: DeviceId,
        seq: ClipSeq,
        format: ClipFormat,
        out: &mut Vec<Output>,
    ) {
        let key = (seq, format.clone());
        if self.fetching.contains_key(&key) {
            return;
        }
        self.fetching.insert(
            key,
            Fetching {
                from,
                buf: Vec::new(),
            },
        );
        out.push(Output::Send {
            to: from,
            msg: Bulk::ClipRequest {
                seq,
                format,
                offset: 0,
            },
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn chunk_arrived(
        &mut self,
        from: DeviceId,
        seq: ClipSeq,
        format: ClipFormat,
        offset: u64,
        data: Vec<u8>,
        last: bool,
        out: &mut Vec<Output>,
    ) {
        let key = (seq, format.clone());
        let Some(fetching) = self.fetching.get_mut(&key) else {
            return;
        };
        if fetching.from != from {
            return;
        }
        if offset != fetching.buf.len() as u64 {
            // Out of step. Nothing resynchronises a stream that has lost a
            // piece, so the fetch is given up rather than delivered wrong.
            self.fetching.remove(&key);
            out.push(Output::Send {
                to: from,
                msg: Bulk::ClipCancel {
                    seq,
                    format: format.clone(),
                },
            });
            self.fail_waiters(&seq, &format, ClipError::Cancelled, out);
            return;
        }
        fetching.buf.extend_from_slice(&data);
        let have = fetching.buf.len() as u64;
        if have > self.max_bytes {
            self.fetching.remove(&key);
            out.push(Output::Send {
                to: from,
                msg: Bulk::ClipCancel {
                    seq,
                    format: format.clone(),
                },
            });
            self.fail_waiters(
                &seq,
                &format,
                ClipError::TooLarge {
                    bytes: have,
                    limit: self.max_bytes,
                },
                out,
            );
            return;
        }
        if !last {
            out.push(Output::Send {
                to: from,
                msg: Bulk::ClipRequest {
                    seq,
                    format,
                    offset: have,
                },
            });
            return;
        }
        let done = self.fetching.remove(&key).expect("just looked it up");
        self.serve_waiters(seq, format, done.buf, out);
    }

    fn read_done(
        &mut self,
        seq: ClipSeq,
        format: ClipFormat,
        result: Result<Vec<u8>, ClipError>,
        out: &mut Vec<Output>,
    ) {
        let key = (seq, format.clone());
        self.reading.remove(&key);
        let still_ours = self.local.as_ref().is_some_and(|(s, _)| *s == seq);
        match result {
            Ok(_) if !still_ours => self.fail_waiters(&seq, &format, ClipError::Stale, out),
            Ok(data) if data.len() as u64 > self.max_bytes => self.fail_waiters(
                &seq,
                &format,
                ClipError::TooLarge {
                    bytes: data.len() as u64,
                    limit: self.max_bytes,
                },
                out,
            ),
            Ok(data) => self.serve_waiters(seq, format, data, out),
            Err(e) => self.fail_waiters(&seq, &format, e, out),
        }
    }

    /// Contents have arrived: keep them, and answer everyone waiting.
    fn serve_waiters(
        &mut self,
        seq: ClipSeq,
        format: ClipFormat,
        data: Vec<u8>,
        out: &mut Vec<Output>,
    ) {
        let (ready, rest): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|w| w.seq == seq && w.format == format);
        self.waiting = rest;
        for waiter in ready {
            match waiter.who {
                Waiter::Ticket(ticket) => out.push(Output::Deliver {
                    ticket,
                    result: Ok(data.clone()),
                }),
                Waiter::Relay { to, offset } => out.push(Output::Send {
                    to,
                    msg: chunk(seq, format.clone(), &data, offset),
                }),
            }
        }
        self.cache.insert((seq, format), data);
    }

    fn fail_waiters(
        &mut self,
        seq: &ClipSeq,
        format: &ClipFormat,
        why: ClipError,
        out: &mut Vec<Output>,
    ) {
        let (failed, rest): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|w| w.seq == *seq && w.format == *format);
        self.waiting = rest;
        for waiter in failed {
            match waiter.who {
                Waiter::Ticket(ticket) => out.push(Output::Deliver {
                    ticket,
                    result: Err(why.clone()),
                }),
                Waiter::Relay { to, .. } => out.push(Output::Send {
                    to,
                    msg: Bulk::ClipUnavailable {
                        seq: *seq,
                        format: format.clone(),
                        reason: why.clone(),
                    },
                }),
            }
        }
    }

    /// An offer is over: nothing about it will be asked for or answered again.
    fn forget(&mut self, seq: ClipSeq, out: &mut Vec<Output>) {
        self.cache.retain(|(s, _), _| *s != seq);
        self.reading.retain(|(s, _)| *s != seq);
        let fetches: Vec<(ClipSeq, ClipFormat)> = self
            .fetching
            .keys()
            .filter(|(s, _)| *s == seq)
            .cloned()
            .collect();
        for key in fetches {
            if let Some(f) = self.fetching.remove(&key) {
                out.push(Output::Send {
                    to: f.from,
                    msg: Bulk::ClipCancel {
                        seq,
                        format: key.1.clone(),
                    },
                });
            }
        }
        let stale: Vec<ClipFormat> = self
            .waiting
            .iter()
            .filter(|w| w.seq == seq)
            .map(|w| w.format.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for format in stale {
            self.fail_waiters(&seq, &format, ClipError::Stale, out);
        }
    }
}

fn offer(seq: ClipSeq, formats: &[ClipFormat]) -> ClipOffer {
    ClipOffer {
        seq,
        entries: formats
            .iter()
            .map(|f| ClipEntry {
                format: f.clone(),
                bytes: None,
                hash: None,
            })
            .collect(),
    }
}

/// The one chunk that answers a request for `offset`.
fn chunk(seq: ClipSeq, format: ClipFormat, data: &[u8], offset: u64) -> Bulk {
    let start = (offset as usize).min(data.len());
    let end = (start + MAX_CHUNK_DATA).min(data.len());
    Bulk::ClipChunk {
        seq,
        format,
        offset: start as u64,
        data: data[start..end].to_vec(),
        last: end >= data.len(),
    }
}

impl std::fmt::Debug for Exchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exchange")
            .field("peers", &self.peers.len())
            .field("local", &self.local.as_ref().map(|(s, _)| s))
            .field("held", &self.remote.as_ref().map(|h| h.seq))
            .field("cached", &self.cache.len())
            .field("fetching", &self.fetching.len())
            .field("waiting", &self.waiting.len())
            .finish()
    }
}
