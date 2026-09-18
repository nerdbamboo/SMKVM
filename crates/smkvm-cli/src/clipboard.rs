//! Joining the clipboard exchange to this machine's clipboard and to the link.
//!
//! The exchange decides; this carries out. A change on the local clipboard
//! goes in, an offer to put on it comes out; a paste asks for contents, and
//! the answer arrives from another machine some time later. The clipboard
//! backends block -- on the display, on another application, on the person
//! -- so each lives on a thread of its own and talks to the daemon's loop
//! through channels.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smkvm_clipboard::{Available, ClipboardError, Fetch, Read, Watch, Write};
use smkvm_core::exchange::{formats_from_names, Input, Output};
use smkvm_core::Exchange;
use smkvm_layout::DeviceId;
use smkvm_proto::{Bulk, ClipError, ClipFormat, ClipSeq};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// How long a paste waits for the far machine before giving up.
///
/// Long enough for a large image over a slow link; short enough that a paste
/// into an application that is waiting on it does not look hung for ever.
const FETCH_PATIENCE: Duration = Duration::from_secs(30);

/// The three things a platform provides.
pub struct Backends {
    pub watch: Box<dyn Watch + Send>,
    pub read: Box<dyn Read + Send>,
    pub write: Box<dyn Write + Send>,
}

/// Something on this machine is pasting, and needs the contents.
pub struct Wanted {
    seq: ClipSeq,
    format: ClipFormat,
    reply: std::sync::mpsc::Sender<Result<Vec<u8>, ClipError>>,
}

/// The local clipboard has been read.
pub struct ReadDone {
    seq: ClipSeq,
    format: ClipFormat,
    result: Result<Vec<u8>, ClipError>,
}

/// What the clipboard side of the daemon noticed.
pub enum Happened {
    Changed(Available),
    Wanted(Wanted),
    Read(ReadDone),
}

/// Fetches an offer's contents from the machine that holds them.
///
/// Called on the clipboard backend's thread when something pastes, so it
/// blocks there until the daemon's loop has fetched the answer over the link.
struct RemoteFetch {
    seq: ClipSeq,
    ask: mpsc::Sender<Wanted>,
}

impl Fetch for RemoteFetch {
    fn fetch(&self, format: &ClipFormat) -> smkvm_clipboard::Result<Vec<u8>> {
        let (reply, answer) = std::sync::mpsc::channel();
        self.ask
            .blocking_send(Wanted {
                seq: self.seq,
                format: format.clone(),
                reply,
            })
            .map_err(|_| ClipboardError::Display("the daemon has stopped".into()))?;
        match answer.recv_timeout(FETCH_PATIENCE) {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(why)) => Err(ClipboardError::Display(describe(&why))),
            Err(_) => Err(ClipboardError::Display(
                "the other machine did not hand the clipboard over in time".into(),
            )),
        }
    }
}

fn describe(why: &ClipError) -> String {
    match why {
        ClipError::Stale => "that copy has since been replaced".into(),
        ClipError::OwnerRefused => "the application that copied it would not hand it over".into(),
        ClipError::TooLarge { bytes, limit } => {
            format!("it is {bytes} bytes and the limit is {limit}; raise clipboard.max_bytes")
        }
        ClipError::Cancelled => "the machine holding it went away".into(),
    }
}

fn from_backend(e: ClipboardError) -> ClipError {
    match e {
        ClipboardError::NothingUsable => ClipError::Stale,
        ClipboardError::Busy
        | ClipboardError::Refused(_)
        | ClipboardError::Display(_)
        | ClipboardError::Unsupported(_) => ClipError::OwnerRefused,
    }
}

struct Live {
    write: Box<dyn Write + Send>,
    read: Arc<Mutex<Box<dyn Read + Send>>>,
}

/// The clipboard side of a daemon, whichever role it plays.
pub struct Sharing {
    exchange: Exchange,
    live: Option<Live>,
    changes: mpsc::Receiver<Available>,
    /// Whether the watcher is still there to report changes.
    watching: bool,
    wanted_tx: mpsc::Sender<Wanted>,
    wanted: mpsc::Receiver<Wanted>,
    reads_tx: mpsc::Sender<ReadDone>,
    reads: mpsc::Receiver<ReadDone>,
    tickets: HashMap<u64, std::sync::mpsc::Sender<Result<Vec<u8>, ClipError>>>,
    next_ticket: u64,
}

impl Sharing {
    /// Start sharing, with whatever this platform can provide.
    ///
    /// Without backends the exchange still runs: a server with no clipboard of
    /// its own still carries offers between its clients.
    pub fn start(
        me: DeviceId,
        cfg: &smkvm_config::Clipboard,
        backends: Option<Backends>,
    ) -> Sharing {
        let (changes_tx, changes) = mpsc::channel(16);
        let (wanted_tx, wanted) = mpsc::channel(16);
        let (reads_tx, reads) = mpsc::channel(16);

        let mut exchange = Exchange::new(me, formats_from_names(&cfg.formats), cfg.max_bytes);
        exchange.set_enabled(cfg.enabled);

        let live = backends.map(|b| {
            let mut watch = b.watch;
            if let Err(e) = std::thread::Builder::new()
                .name("smkvm-clipboard-watch".into())
                .spawn(move || {
                    while let Some(available) = watch.next_change() {
                        if changes_tx.blocking_send(available).is_err() {
                            break;
                        }
                    }
                    debug!("the clipboard can no longer be watched");
                })
            {
                warn!("could not start watching the clipboard: {e}");
            }
            Live {
                write: b.write,
                read: Arc::new(Mutex::new(b.read)),
            }
        });
        if live.is_none() {
            info!("no clipboard on this machine; copies elsewhere are still carried between the others");
        }

        Sharing {
            exchange,
            watching: live.is_some(),
            live,
            changes,
            wanted_tx,
            wanted,
            reads_tx,
            reads,
            tickets: HashMap::new(),
            next_ticket: 1,
        }
    }

    /// Take a changed configuration into use.
    pub fn reconfigure(&mut self, cfg: &smkvm_config::Clipboard) {
        self.exchange.set_enabled(cfg.enabled);
        self.exchange.set_allowed(formats_from_names(&cfg.formats));
        self.exchange.set_max_bytes(cfg.max_bytes);
    }

    /// Wait for the clipboard side to have something to say.
    pub async fn next(&mut self) -> Happened {
        loop {
            tokio::select! {
                change = self.changes.recv(), if self.watching => match change {
                    Some(available) => return Happened::Changed(available),
                    None => self.watching = false,
                },
                Some(wanted) = self.wanted.recv() => return Happened::Wanted(wanted),
                Some(read) = self.reads.recv() => return Happened::Read(read),
            }
        }
    }

    /// Act on it. Returns what has to go over the link, and to whom.
    pub fn on(&mut self, happened: Happened, now: Instant) -> Vec<(DeviceId, Bulk)> {
        let input = match happened {
            Happened::Changed(available) => Input::LocalChanged(available.formats),
            Happened::Wanted(wanted) => {
                let ticket = self.next_ticket;
                self.next_ticket += 1;
                self.tickets.insert(ticket, wanted.reply);
                Input::Wanted {
                    ticket,
                    seq: wanted.seq,
                    format: wanted.format,
                }
            }
            Happened::Read(read) => Input::ReadDone {
                seq: read.seq,
                format: read.format,
                result: read.result,
            },
        };
        let outputs = self.exchange.handle(input, now);
        self.apply(outputs)
    }

    pub fn peer_up(&mut self, peer: DeviceId, now: Instant) -> Vec<(DeviceId, Bulk)> {
        let outputs = self.exchange.handle(Input::PeerUp(peer), now);
        self.apply(outputs)
    }

    pub fn peer_gone(&mut self, peer: DeviceId, now: Instant) -> Vec<(DeviceId, Bulk)> {
        let outputs = self.exchange.handle(Input::PeerGone(peer), now);
        self.apply(outputs)
    }

    pub fn peer_said(&mut self, from: DeviceId, msg: Bulk, now: Instant) -> Vec<(DeviceId, Bulk)> {
        let outputs = self.exchange.handle(Input::FromPeer { from, msg }, now);
        self.apply(outputs)
    }

    fn apply(&mut self, outputs: Vec<Output>) -> Vec<(DeviceId, Bulk)> {
        let mut sends = Vec::new();
        for output in outputs {
            match output {
                Output::Send { to, msg } => sends.push((to, msg)),
                Output::OfferLocally { seq, formats } => {
                    let Some(live) = self.live.as_mut() else {
                        continue;
                    };
                    let source = Box::new(RemoteFetch {
                        seq,
                        ask: self.wanted_tx.clone(),
                    });
                    match live.write.offer(&formats, source) {
                        Ok(()) => debug!(?formats, "offering another machine's clipboard here"),
                        Err(e) => warn!("could not offer the clipboard here: {e}"),
                    }
                }
                Output::ReleaseLocally => {
                    if let Some(live) = self.live.as_mut() {
                        if let Err(e) = live.write.release() {
                            warn!("could not give the clipboard back: {e}");
                        }
                    }
                }
                Output::ReadLocal { seq, format } => self.read_local(seq, format),
                Output::Deliver { ticket, result } => {
                    if let Some(reply) = self.tickets.remove(&ticket) {
                        let _ = reply.send(result);
                    }
                }
            }
        }
        sends
    }

    /// Read the local clipboard off the loop, and report back when done.
    fn read_local(&mut self, seq: ClipSeq, format: ClipFormat) {
        let Some(live) = self.live.as_ref() else {
            // Nothing to read from. Said straight away rather than left to
            // time out on the other machine.
            let _ = self.reads_tx.try_send(ReadDone {
                seq,
                format,
                result: Err(ClipError::OwnerRefused),
            });
            return;
        };
        let reader = live.read.clone();
        let done = self.reads_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = match reader.lock() {
                Ok(mut reader) => reader.read(&format).map_err(|e| {
                    warn!(?format, "could not read the clipboard: {e}");
                    from_backend(e)
                }),
                Err(_) => Err(ClipError::OwnerRefused),
            };
            let _ = done.blocking_send(ReadDone {
                seq,
                format,
                result,
            });
        });
    }
}
