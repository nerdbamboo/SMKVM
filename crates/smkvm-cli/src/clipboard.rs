//! Joining the clipboard exchange to this machine's clipboard and to the link.
//!
//! The exchange decides; this carries out. A change on the local clipboard
//! goes in, an offer to put on it comes out; a paste asks for contents, and
//! the answer arrives from another machine some time later. The clipboard
//! backends block -- on the display, on another application, on the person
//! -- so each lives on a thread of its own and talks to the daemon's loop
//! through channels.
//!
//! Files ride along. Where the clipboard would carry a list of paths -- which
//! mean nothing on another machine -- it carries a manifest of names and
//! sizes instead, and a paste pulls the files themselves, piece by piece,
//! into the transfer directory before the pasting application is handed
//! paths that exist.
//!
//! A drag is a copy that never touched the clipboard. Files picked up as the
//! cursor left one machine are announced exactly as a copy would be, with
//! their manifest kept here rather than read from the clipboard; the machine
//! the cursor arrived on is told that this offer is what the cursor carries,
//! pulls the files without waiting for a paste, and drops them.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smkvm_clipboard::files::{local_paths, uri_list};
use smkvm_clipboard::{Available, ClipboardError, Fetch, Read, Watch, Write};
use smkvm_core::exchange::{formats_from_names, Input, Output};
use smkvm_core::transfer::{Input as TransferInput, Output as TransferOutput};
use smkvm_core::{Exchange, Transfer};
use smkvm_layout::DeviceId;
use smkvm_proto::{Bulk, ClipError, ClipFormat, ClipSeq, FileOffer, Role, TransferId};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::transfer::{self as disk, Arriving, Landing, Offered};

/// How long a paste waits for the far machine before giving up.
///
/// Long enough for a large image over a slow link; short enough that a paste
/// into an application that is waiting on it does not look hung for ever.
const FETCH_PATIENCE: Duration = Duration::from_secs(30);

/// How long a paste of files waits for all of them to arrive.
///
/// The pasting application is blocked for the duration, so this is bounded
/// by patience rather than by bandwidth; `transfer.max_bytes` is what keeps
/// the wait reasonable.
const FILES_PATIENCE: Duration = Duration::from_secs(10 * 60);

/// How many manifests this machine keeps serving after copying something
/// else. A paste that began just before the next copy still completes.
const OFFERED_KEPT: usize = 4;

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

/// Something on this machine is pasting files, and needs them on disk.
pub struct PullFiles {
    offer: FileOffer,
    /// Answered with a `text/uri-list` of where they landed.
    reply: std::sync::mpsc::Sender<Result<Vec<u8>, String>>,
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
    PullFiles(PullFiles),
}

/// Fetches an offer's contents from the machine that holds them.
///
/// Called on the clipboard backend's thread when something pastes, so it
/// blocks there until the daemon's loop has fetched the answer over the link.
struct RemoteFetch {
    seq: ClipSeq,
    ask: mpsc::Sender<Wanted>,
    files: mpsc::Sender<PullFiles>,
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
        let bytes = match answer.recv_timeout(FETCH_PATIENCE) {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(why)) => return Err(ClipboardError::Display(describe(&why))),
            Err(_) => {
                return Err(ClipboardError::Display(
                    "the other machine did not hand the clipboard over in time".into(),
                ))
            }
        };
        if *format != ClipFormat::Uris {
            return Ok(bytes);
        }

        // What arrived is a manifest. The files are still on the other
        // machine, and the application pasting wants paths that exist here.
        let offer: FileOffer = smkvm_proto::decode(&bytes).map_err(|_| {
            ClipboardError::Display("the file list from the other machine is unreadable".into())
        })?;
        let (reply, landed) = std::sync::mpsc::channel();
        self.files
            .blocking_send(PullFiles { offer, reply })
            .map_err(|_| ClipboardError::Display("the daemon has stopped".into()))?;
        match landed.recv_timeout(FILES_PATIENCE) {
            Ok(Ok(uris)) => Ok(uris),
            Ok(Err(why)) => Err(ClipboardError::Display(why)),
            Err(_) => Err(ClipboardError::Display(
                "the files did not all arrive in time".into(),
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

/// What the configuration says about files.
#[derive(Debug, Clone)]
struct FileSettings {
    enabled: bool,
    directory: PathBuf,
    max_bytes: u64,
}

impl FileSettings {
    fn from(cfg: &smkvm_config::Transfer) -> FileSettings {
        FileSettings {
            enabled: cfg.enabled,
            directory: smkvm_config::paths::expand_home(&cfg.directory),
            max_bytes: cfg.max_bytes,
        }
    }
}

/// Who is waiting for files being pulled: an application pasting, or a drag
/// that will drop them once they are here.
enum Delivery {
    Paste(std::sync::mpsc::Sender<Result<Vec<u8>, String>>),
    Drop,
}

/// Files being pulled for a paste or a drop on this machine.
struct Incoming {
    id: TransferId,
    offer: FileOffer,
    landing: Landing,
    /// The entry being written, or the next to start.
    index: usize,
    current: Option<Arriving>,
    delivery: Delivery,
    started: Instant,
}

/// Hands over exactly what it was made with. For files that have landed from
/// a drop: their list goes on the clipboard so a paste places them too.
struct Fixed(Vec<u8>);

impl Fetch for Fixed {
    fn fetch(&self, format: &ClipFormat) -> smkvm_clipboard::Result<Vec<u8>> {
        match format {
            ClipFormat::Uris => Ok(self.0.clone()),
            other => Err(ClipboardError::Refused(other.clone())),
        }
    }
}

/// The clipboard side of a daemon, whichever role it plays.
pub struct Sharing {
    me: DeviceId,
    role: Role,
    exchange: Exchange,
    transfer: Transfer,
    files: FileSettings,
    live: Option<Live>,
    changes: mpsc::Receiver<Available>,
    /// Whether the watcher is still there to report changes.
    watching: bool,
    wanted_tx: mpsc::Sender<Wanted>,
    wanted: mpsc::Receiver<Wanted>,
    pulls_tx: mpsc::Sender<PullFiles>,
    pulls: mpsc::Receiver<PullFiles>,
    reads_tx: mpsc::Sender<ReadDone>,
    reads: mpsc::Receiver<ReadDone>,
    tickets: HashMap<u64, std::sync::mpsc::Sender<Result<Vec<u8>, ClipError>>>,
    next_ticket: u64,
    /// Manifests this machine has offered, newest last, and where the files are.
    offered: VecDeque<Offered>,
    next_transfer: u64,
    incoming: Option<Incoming>,
    /// Manifests offered for a drag. Nothing put them on the clipboard, so a
    /// request for them is answered from here.
    synthetic: HashMap<ClipSeq, Vec<u8>>,
    /// Requests whose answer is a manifest to drop here, not to paste.
    drop_tickets: HashSet<u64>,
    /// A drag the cursor arrived with, whose offer has not arrived yet.
    pending_drop: Option<ClipSeq>,
    /// Files that have just landed from a drop, for the daemon to take.
    landed: Option<Vec<PathBuf>>,
}

impl Sharing {
    /// Start sharing, with whatever this platform can provide.
    ///
    /// Without backends the exchange still runs: a server with no clipboard of
    /// its own still carries offers between its clients.
    pub fn start(
        me: DeviceId,
        role: Role,
        cfg: &smkvm_config::Clipboard,
        files: &smkvm_config::Transfer,
        backends: Option<Backends>,
    ) -> Sharing {
        let (changes_tx, changes) = mpsc::channel(16);
        let (wanted_tx, wanted) = mpsc::channel(16);
        let (pulls_tx, pulls) = mpsc::channel(4);
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
            me,
            role,
            exchange,
            transfer: Transfer::new(me, None),
            files: FileSettings::from(files),
            watching: live.is_some(),
            live,
            changes,
            wanted_tx,
            wanted,
            pulls_tx,
            pulls,
            reads_tx,
            reads,
            tickets: HashMap::new(),
            next_ticket: 1,
            offered: VecDeque::new(),
            next_transfer: 1,
            incoming: None,
            synthetic: HashMap::new(),
            drop_tickets: HashSet::new(),
            pending_drop: None,
            landed: None,
        }
    }

    pub fn files_enabled(&self) -> bool {
        self.files.enabled
    }

    /// Files were picked up from a drag as the cursor left this machine.
    ///
    /// Offered to every other machine exactly as a copy would be; the
    /// manifest is kept here since nothing put it on the clipboard. Returns
    /// the sequence the offer travels under, for telling the machine the
    /// cursor arrived on that this is what it is carrying.
    pub fn drag_started(
        &mut self,
        paths: Vec<PathBuf>,
        now: Instant,
    ) -> (Option<ClipSeq>, Vec<(DeviceId, Bulk)>) {
        if !self.files.enabled {
            return (None, Vec::new());
        }
        let id = TransferId {
            device: self.me,
            counter: self.next_transfer,
        };
        self.next_transfer += 1;
        let offered = match disk::describe(id, &paths) {
            Ok(offered) => offered,
            Err(e) => {
                warn!("could not describe the dragged files: {e}");
                return (None, Vec::new());
            }
        };
        let Ok(bytes) = smkvm_proto::encode_bare(&offered.offer) else {
            return (None, Vec::new());
        };
        info!(
            files = offered.offer.files.len(),
            bytes = offered.offer.total_bytes,
            "files picked up from a drag; offering them to the other machines"
        );
        self.offered.push_back(offered);
        while self.offered.len() > OFFERED_KEPT {
            self.offered.pop_front();
        }
        let (seq, outputs) = self.exchange.announce(vec![ClipFormat::Uris], now);
        if let Some(seq) = seq {
            self.synthetic.insert(seq, bytes);
            let oldest = seq.counter.saturating_sub(OFFERED_KEPT as u64);
            self.synthetic.retain(|s, _| s.counter > oldest);
        }
        let sends = self.apply(outputs);
        (seq, sends)
    }

    /// The cursor arrived here carrying the offer `seq`: pull its files now,
    /// without waiting for a paste, and drop them.
    pub fn drop_here(&mut self, seq: ClipSeq, now: Instant) -> Vec<(DeviceId, Bulk)> {
        if !self.files.enabled {
            debug!(
                ?seq,
                "the cursor arrived carrying files, but transfer is off here"
            );
            return Vec::new();
        }
        if self.exchange.held_offer() != Some(seq) {
            // The notice can only follow the offer on the link, but the
            // server passes each on separately; the offer is moments away.
            self.pending_drop = Some(seq);
            return Vec::new();
        }
        self.pending_drop = None;
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.drop_tickets.insert(ticket);
        info!(
            ?seq,
            "the cursor arrived carrying files; fetching them to drop here"
        );
        let outputs = self.exchange.handle(
            Input::Wanted {
                ticket,
                seq,
                format: ClipFormat::Uris,
            },
            now,
        );
        self.apply(outputs)
    }

    /// Files that have just landed from a drop, once.
    pub fn take_landed(&mut self) -> Option<Vec<PathBuf>> {
        self.landed.take()
    }

    /// Take a changed configuration into use.
    pub fn reconfigure(&mut self, cfg: &smkvm_config::Clipboard, files: &smkvm_config::Transfer) {
        self.exchange.set_enabled(cfg.enabled);
        self.exchange.set_allowed(formats_from_names(&cfg.formats));
        self.exchange.set_max_bytes(cfg.max_bytes);
        self.files = FileSettings::from(files);
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
                Some(pull) = self.pulls.recv() => return Happened::PullFiles(pull),
            }
        }
    }

    /// Act on it. Returns what has to go over the link, and to whom.
    pub fn on(&mut self, happened: Happened, now: Instant) -> Vec<(DeviceId, Bulk)> {
        let input = match happened {
            Happened::Changed(available) => {
                debug!(formats = ?available.formats, "the clipboard here changed");
                let mut formats = available.formats;
                if !self.files.enabled {
                    formats.retain(|f| *f != ClipFormat::Uris);
                }
                Input::LocalChanged(formats)
            }
            Happened::Wanted(wanted) => {
                debug!(seq = ?wanted.seq, format = ?wanted.format, "something here is pasting");
                let ticket = self.next_ticket;
                self.next_ticket += 1;
                self.tickets.insert(ticket, wanted.reply);
                Input::Wanted {
                    ticket,
                    seq: wanted.seq,
                    format: wanted.format,
                }
            }
            Happened::Read(read) => {
                // A drag's manifest was made here and is already one; only a
                // list read off the clipboard has to be turned into one.
                let result =
                    if read.format == ClipFormat::Uris && !self.synthetic.contains_key(&read.seq) {
                        read.result.and_then(|bytes| self.manifest_for(&bytes))
                    } else {
                        read.result
                    };
                Input::ReadDone {
                    seq: read.seq,
                    format: read.format,
                    result,
                }
            }
            Happened::PullFiles(pull) => {
                return self.begin_pull(pull.offer, Delivery::Paste(pull.reply))
            }
        };
        let outputs = self.exchange.handle(input, now);
        self.apply(outputs)
    }

    pub fn peer_up(&mut self, peer: DeviceId, now: Instant) -> Vec<(DeviceId, Bulk)> {
        if self.role == Role::Client {
            // A client's one peer is the server, through which every file
            // is fetched.
            self.transfer.set_hub(Some(peer));
        }
        let outputs = self.transfer.handle(TransferInput::PeerUp(peer));
        let mut sends = self.apply_transfer(outputs);
        let outputs = self.exchange.handle(Input::PeerUp(peer), now);
        sends.extend(self.apply(outputs));
        sends
    }

    pub fn peer_gone(&mut self, peer: DeviceId, now: Instant) -> Vec<(DeviceId, Bulk)> {
        let outputs = self.transfer.handle(TransferInput::PeerGone(peer));
        let mut sends = self.apply_transfer(outputs);
        let outputs = self.exchange.handle(Input::PeerGone(peer), now);
        sends.extend(self.apply(outputs));
        sends
    }

    pub fn peer_said(&mut self, from: DeviceId, msg: Bulk, now: Instant) -> Vec<(DeviceId, Bulk)> {
        match &msg {
            Bulk::ClipOffer(offer) => {
                debug!(seq = ?offer.seq, entries = offer.entries.len(), "another machine copied")
            }
            Bulk::ClipRequest {
                seq,
                format,
                offset,
            } => {
                debug!(?seq, ?format, offset, "another machine is pasting")
            }
            Bulk::ClipUnavailable {
                seq,
                format,
                reason,
            } => {
                debug!(
                    ?seq,
                    ?format,
                    ?reason,
                    "another machine would not hand the clipboard over"
                )
            }
            Bulk::FileRequest { id, index, offset } => {
                debug!(
                    ?id,
                    index, offset, "another machine wants a piece of a file"
                )
            }
            Bulk::FileAbort { id, reason } => debug!(?id, reason, "a transfer was called off"),
            _ => {}
        }
        if matches!(
            msg,
            Bulk::FileOffer(_)
                | Bulk::FileRequest { .. }
                | Bulk::FileChunk { .. }
                | Bulk::FileDone { .. }
                | Bulk::FileAbort { .. }
        ) {
            let outputs = self.transfer.handle(TransferInput::FromPeer { from, msg });
            return self.apply_transfer(outputs);
        }
        let outputs = self.exchange.handle(Input::FromPeer { from, msg }, now);
        let mut sends = self.apply(outputs);
        // A drag notice that got here before its offer waits for exactly this.
        if let Some(seq) = self.pending_drop {
            if self.exchange.held_offer() == Some(seq) {
                sends.extend(self.drop_here(seq, now));
            }
        }
        sends
    }

    fn apply(&mut self, outputs: Vec<Output>) -> Vec<(DeviceId, Bulk)> {
        let mut sends = Vec::new();
        for output in outputs {
            match output {
                Output::Send { to, msg } => sends.push((to, msg)),
                Output::OfferLocally { seq, mut formats } => {
                    let Some(live) = self.live.as_mut() else {
                        continue;
                    };
                    if !self.files.enabled {
                        formats.retain(|f| *f != ClipFormat::Uris);
                        if formats.is_empty() {
                            continue;
                        }
                    }
                    let source = Box::new(RemoteFetch {
                        seq,
                        ask: self.wanted_tx.clone(),
                        files: self.pulls_tx.clone(),
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
                Output::Deliver { ticket, result } if self.drop_tickets.remove(&ticket) => {
                    match result {
                        Ok(bytes) => match smkvm_proto::decode::<FileOffer>(&bytes) {
                            Ok(offer) => sends.extend(self.begin_pull(offer, Delivery::Drop)),
                            Err(_) => warn!("the dragged files' list is unreadable"),
                        },
                        Err(why) => {
                            warn!("the dragged files could not be fetched: {}", describe(&why))
                        }
                    }
                }
                Output::Deliver { ticket, result } => {
                    if let Err(why) = &result {
                        debug!(
                            ticket,
                            ?why,
                            local = ?self.exchange.local_offer(),
                            held = ?self.exchange.held_offer(),
                            "a paste here could not be served"
                        );
                    }
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
        if format == ClipFormat::Uris {
            if let Some(bytes) = self.synthetic.get(&seq) {
                // A drag's manifest: made here, never on the clipboard.
                let _ = self.reads_tx.try_send(ReadDone {
                    seq,
                    format,
                    result: Ok(bytes.clone()),
                });
                return;
            }
        }
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

    /// Turn the list of paths an application copied here into the manifest
    /// the other machines are given, and remember where the files are.
    fn manifest_for(&mut self, uri_list: &[u8]) -> Result<Vec<u8>, ClipError> {
        let paths = local_paths(uri_list);
        if paths.is_empty() {
            warn!("the copied file list names nothing on this machine");
            return Err(ClipError::OwnerRefused);
        }
        let id = TransferId {
            device: self.me,
            counter: self.next_transfer,
        };
        self.next_transfer += 1;
        let offered = match disk::describe(id, &paths) {
            Ok(offered) => offered,
            Err(e) => {
                warn!("could not describe the copied files: {e}");
                return Err(ClipError::OwnerRefused);
            }
        };
        info!(
            files = offered.offer.files.len(),
            bytes = offered.offer.total_bytes,
            "offering copied files to the other machines"
        );
        // Bare, because this travels as the contents of a clipboard format,
        // not as a frame of its own; `decode` on the other side expects that.
        let bytes =
            smkvm_proto::encode_bare(&offered.offer).map_err(|_| ClipError::OwnerRefused)?;
        self.offered.push_back(offered);
        while self.offered.len() > OFFERED_KEPT {
            self.offered.pop_front();
        }
        Ok(bytes)
    }

    fn apply_transfer(&mut self, outputs: Vec<TransferOutput>) -> Vec<(DeviceId, Bulk)> {
        let mut sends = Vec::new();
        for output in outputs {
            match output {
                TransferOutput::Send { to, msg } => sends.push((to, msg)),
                TransferOutput::ReadFile {
                    to,
                    id,
                    index,
                    offset,
                } => sends.push((to, self.piece_of(id, index, offset))),
                TransferOutput::Chunk {
                    id,
                    index,
                    offset,
                    data,
                    last,
                } => sends.extend(self.piece_arrived(id, index, offset, data, last)),
                TransferOutput::Failed(id) => {
                    self.finish_pull(id, Err("the machine holding the files went away".into()))
                }
            }
        }
        sends
    }

    /// Read a piece of a file this machine offered, for another machine.
    fn piece_of(&self, id: TransferId, index: u32, offset: u64) -> Bulk {
        let refuse = |reason: &str| Bulk::FileAbort {
            id,
            reason: reason.to_owned(),
        };
        let Some(offered) = self.offered.iter().find(|o| o.offer.id == id) else {
            return refuse("those files are no longer on offer");
        };
        let Some((entry, path)) = offered
            .offer
            .files
            .get(index as usize)
            .zip(offered.paths.get(index as usize))
        else {
            return refuse("no such file in that copy");
        };
        if entry.is_dir {
            return refuse("that entry is a folder");
        }
        match disk::read_piece(path, offset) {
            Ok((data, last)) => Bulk::FileChunk {
                id,
                index,
                offset,
                data,
                last,
            },
            Err(e) => {
                warn!(path = %path.display(), "could not read a copied file: {e}");
                refuse("the file could not be read")
            }
        }
    }

    /// Files from elsewhere are wanted here -- pasted, or dropped: decide
    /// where they go and ask for the first piece.
    fn begin_pull(&mut self, offer: FileOffer, delivery: Delivery) -> Vec<(DeviceId, Bulk)> {
        let refuse = |delivery: Delivery, why: String| {
            warn!("{why}");
            if let Delivery::Paste(reply) = delivery {
                let _ = reply.send(Err(why));
            }
        };
        if !self.files.enabled {
            refuse(delivery, "file transfer is turned off here".into());
            return Vec::new();
        }
        if self.incoming.is_some() {
            refuse(
                delivery,
                "another set of files is still arriving; try again when it has".into(),
            );
            return Vec::new();
        }
        let landing = match disk::plan_landing(&offer, &self.files.directory, self.files.max_bytes)
        {
            Ok(landing) => landing,
            Err(why) => {
                refuse(delivery, format!("refusing the files: {why}"));
                return Vec::new();
            }
        };
        info!(
            files = offer.files.len(),
            bytes = offer.total_bytes,
            into = %self.files.directory.display(),
            how = match delivery {
                Delivery::Paste(_) => "pasted",
                Delivery::Drop => "dropped",
            },
            "files are wanted here; fetching them"
        );
        self.incoming = Some(Incoming {
            id: offer.id,
            offer,
            landing,
            index: 0,
            current: None,
            delivery,
            started: Instant::now(),
        });
        self.advance_pull()
    }

    /// Create whatever comes next -- folders and empty files need no bytes --
    /// and ask for the first piece of the next file that does.
    fn advance_pull(&mut self) -> Vec<(DeviceId, Bulk)> {
        let Some(incoming) = self.incoming.as_mut() else {
            return Vec::new();
        };
        while incoming.index < incoming.offer.files.len() {
            let entry = &incoming.offer.files[incoming.index];
            let path = &incoming.landing.paths[incoming.index];
            if entry.is_dir {
                if let Err(e) = std::fs::create_dir_all(path) {
                    let why = format!("could not create {}: {e}", path.display());
                    let id = incoming.id;
                    return self.fail_pull(id, why);
                }
                incoming.index += 1;
                continue;
            }
            match Arriving::create(path) {
                Ok(arriving) if entry.bytes == 0 => {
                    if let Err(e) = arriving.finish() {
                        let why = format!("could not write {}: {e}", path.display());
                        let id = incoming.id;
                        return self.fail_pull(id, why);
                    }
                    incoming.index += 1;
                }
                Ok(arriving) => {
                    incoming.current = Some(arriving);
                    let id = incoming.id;
                    let index = incoming.index as u32;
                    let outputs = self.transfer.handle(TransferInput::Pull {
                        id,
                        index,
                        offset: 0,
                    });
                    return self.apply_transfer(outputs);
                }
                Err(e) => {
                    let why = format!("could not create {}: {e}", path.display());
                    let id = incoming.id;
                    return self.fail_pull(id, why);
                }
            }
        }
        // Everything has landed.
        let incoming = self.incoming.take().expect("checked above");
        info!(
            files = incoming.offer.files.len(),
            bytes = incoming.offer.total_bytes,
            took_ms = incoming.started.elapsed().as_millis() as u64,
            into = %self.files.directory.display(),
            "the files have all arrived"
        );
        let list = uri_list(&incoming.landing.top_level);
        match incoming.delivery {
            Delivery::Paste(reply) => {
                let _ = reply.send(Ok(list));
            }
            Delivery::Drop => {
                // Whether or not the drop itself can be made where the
                // pointer is, the files are here now, and a paste places
                // them: so their list goes on the clipboard too.
                if let Some(live) = self.live.as_mut() {
                    if let Err(e) = live.write.offer(&[ClipFormat::Uris], Box::new(Fixed(list))) {
                        warn!("could not put the landed files on the clipboard: {e}");
                    }
                }
                self.landed = Some(incoming.landing.top_level);
            }
        }
        Vec::new()
    }

    fn piece_arrived(
        &mut self,
        id: TransferId,
        index: u32,
        offset: u64,
        data: Vec<u8>,
        last: bool,
    ) -> Vec<(DeviceId, Bulk)> {
        let Some(incoming) = self.incoming.as_mut() else {
            return Vec::new();
        };
        if incoming.id != id || incoming.index as u32 != index {
            return Vec::new();
        }
        let Some(current) = incoming.current.as_mut() else {
            return Vec::new();
        };
        if let Err(e) = current.append(offset, &data) {
            return self.fail_pull(id, format!("could not write the arriving file: {e}"));
        }
        let expected = incoming.offer.files[incoming.index].bytes;
        if current.written() > expected {
            return self.fail_pull(id, "a file arrived larger than it was said to be".into());
        }
        if !last {
            let offset = current.written();
            let outputs = self
                .transfer
                .handle(TransferInput::Pull { id, index, offset });
            return self.apply_transfer(outputs);
        }
        if current.written() != expected {
            return self.fail_pull(id, "a file arrived shorter than it was said to be".into());
        }
        if let Some(done) = incoming.current.take() {
            if let Err(e) = done.finish() {
                return self.fail_pull(id, format!("could not finish writing a file: {e}"));
            }
        }
        incoming.index += 1;
        self.advance_pull()
    }

    fn fail_pull(&mut self, id: TransferId, why: String) -> Vec<(DeviceId, Bulk)> {
        warn!("{why}");
        self.finish_pull(id, Err(why));
        let outputs = self.transfer.handle(TransferInput::Drop(id));
        self.apply_transfer(outputs)
    }

    fn finish_pull(&mut self, id: TransferId, result: Result<Vec<u8>, String>) {
        if self.incoming.as_ref().is_some_and(|i| i.id == id) {
            let incoming = self.incoming.take().expect("checked");
            // Whatever was half written is not left looking like a file.
            if incoming.current.is_some() {
                let _ = std::fs::remove_file(&incoming.landing.paths[incoming.index]);
            }
            if let Delivery::Paste(reply) = incoming.delivery {
                let _ = reply.send(result);
            }
        }
    }
}
