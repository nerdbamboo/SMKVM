//! Running as the machine that owns the keyboard and mouse.
//!
//! Besides moving the cursor, the daemon keeps three promises to whatever is
//! watching it. It says what it is connected to, in `status.toml`, whenever
//! that changes and every few seconds regardless. It notices when the
//! configuration file changes and takes the change into use without being
//! restarted, so arranging screens in the window is felt at once. And it
//! notices when a machine stops answering, so a link that has quietly died
//! does not leave a pointer hidden or a machine believed connected.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use smkvm_config::status::{Machine, MachineState, PlacedMonitor, Status};
use smkvm_config::{paths, Config};
use smkvm_core::{
    Action, ClientHealth, Event, LocalAction, Placement, PointerMode, Server, Settings,
};
use smkvm_input::Inject;
use smkvm_layout::DeviceId;
use smkvm_net::identity::Identity;
use smkvm_net::link::{Link, LinkReader, LinkWriter};
use smkvm_net::session::Session;
use smkvm_net::trust::Trust;
use smkvm_proto::{Bulk, ClientControl, Role, ServerControl};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, error::TrySendError, Sender};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::clipboard::Sharing;
use crate::{hello, platform};

/// How often the configuration file is looked at and the status refreshed.
const HOUSEKEEPING: Duration = Duration::from_secs(2);

/// How long two status writes are kept apart when things are changing fast.
const STATUS_SETTLE: Duration = Duration::from_millis(250);

/// How many heartbeats a machine may miss before it is given up on.
const MISSED_HEARTBEATS: u32 = 3;

/// How full each queue to a machine is allowed to get.
///
/// Motion is bounded tightly because a machine that has fallen behind on
/// positions is better served by the newest one than by a minute of history.
/// Control is bounded loosely, and reaching the limit is a fault rather than a
/// busy moment: it takes a thousand state changes nobody consumed.
const MOTION_QUEUE: usize = 256;
const CONTROL_QUEUE: usize = 1024;

/// What became of a message handed to a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    Queued,
    /// Thrown away to keep up. Only ever motion, which the next one replaces.
    Dropped,
    /// There was no room for something that cannot be thrown away.
    Overflowed,
    /// Nobody is reading the far end any more.
    Closed,
}

/// The two queues a machine is written through.
///
/// They are separate because the two kinds of message fail differently.
/// Pointer motion is a stream where the next supersedes the last, so under
/// pressure the right answer is to lose one and carry on. Control is a sequence
/// of state changes that nothing repeats: lose a `Leave` or an `Enter` and the
/// machine and the server disagree about where the cursor is, with nothing on
/// the way to correct either of them -- which is a cursor that crossed and then
/// vanished. Clipboard traffic is control: a lost chunk stalls a paste until it
/// times out, and a lost offer loses the clipboard.
///
/// One queue would force a single policy on both, and it is motion, arriving
/// thousands of times a minute, that decides when a shared queue is full. So
/// the message that must not be lost would be the one lost, every time.
///
/// Control may overtake motion, which is harmless in both directions: a
/// position that arrives after a `Leave` is discarded by a machine that knows
/// it no longer has the cursor, and one that arrives after an `Enter` is
/// corrected by the next position a moment later. `Enter` carries its own.
/// A clipboard message overtaking a position changes nothing about where the
/// cursor is.
struct Outbound {
    control: Sender<ServerControl>,
    motion: Sender<ServerControl>,
}

impl Outbound {
    fn new() -> (
        Outbound,
        mpsc::Receiver<ServerControl>,
        mpsc::Receiver<ServerControl>,
    ) {
        let (control, control_rx) = mpsc::channel(CONTROL_QUEUE);
        let (motion, motion_rx) = mpsc::channel(MOTION_QUEUE);
        (Outbound { control, motion }, control_rx, motion_rx)
    }

    fn send(&self, msg: ServerControl) -> Sent {
        if msg.may_be_dropped() {
            match self.motion.try_send(msg) {
                Ok(()) => Sent::Queued,
                Err(TrySendError::Full(_)) => Sent::Dropped,
                Err(TrySendError::Closed(_)) => Sent::Closed,
            }
        } else {
            match self.control.try_send(msg) {
                Ok(()) => Sent::Queued,
                Err(TrySendError::Full(_)) => Sent::Overflowed,
                Err(TrySendError::Closed(_)) => Sent::Closed,
            }
        }
    }
}

/// A machine that has connected, and where to send to it.
struct Attached {
    name: String,
    outbound: Outbound,
    /// Which connection this is. A machine that connects again gets a new
    /// one, and anything the old connection's tasks say afterwards is not
    /// about the machine that is here now.
    generation: u64,
    reader: JoinHandle<()>,
    last_heard: Instant,
}

/// Something a client's reader task noticed.
enum FromClient {
    Message {
        device: DeviceId,
        generation: u64,
        msg: ClientControl,
    },
    Gone {
        device: DeviceId,
        generation: u64,
    },
}

/// A machine that has finished its handshake and introduced itself.
struct Arrival {
    device: DeviceId,
    name: String,
    reader: LinkReader,
    writer: LinkWriter,
}

/// The switching behaviour the configuration asks for.
pub fn settings_of(config: &Config) -> Settings {
    Settings {
        switch_delay: Duration::from_millis(u64::from(config.behavior.switch_delay_ms)),
        switch_double_tap: Duration::from_millis(u64::from(config.behavior.switch_double_tap_ms)),
        edge_overflow: config.behavior.edge_overflow,
    }
}

/// The arrangement the configuration asks for.
pub fn placements_of(config: &Config) -> Vec<Placement> {
    config
        .screen
        .iter()
        .flat_map(|screen| {
            screen.monitor.iter().map(|monitor| Placement {
                machine: screen.name.clone(),
                monitor: monitor.id.clone(),
                global: monitor.rect(),
            })
        })
        .collect()
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

struct Daemon {
    me: DeviceId,
    server: Server,
    config: Config,
    config_path: PathBuf,
    config_stamp: Option<(SystemTime, u64)>,
    attached: HashMap<DeviceId, Attached>,
    generation: u64,
    injector: Box<dyn platform::InjectAndReport>,
    capture: platform::CaptureHandle,
    sharing: Sharing,
    from_clients: Sender<FromClient>,
    heartbeat: Duration,
    status_path: PathBuf,
    status_dirty: bool,
    status_written: Option<Instant>,
    last_active: DeviceId,
}

pub async fn run(
    identity: Identity,
    trust: Trust,
    config: Config,
    config_path: PathBuf,
    layout: smkvm_layout::Layout,
) -> Result<()> {
    let (events_tx, mut events) = mpsc::channel::<Event>(4096);
    let (from_clients_tx, mut from_clients) = mpsc::channel::<FromClient>(256);
    let (arrivals_tx, mut arrivals) = mpsc::channel::<Arrival>(8);

    let mut injector = platform::injector()?;
    let capture = platform::start_capture(events_tx.clone())?;

    let mut server = Server::new(
        identity.id(),
        config.identity.name.clone(),
        layout,
        settings_of(&config),
    );
    let placements = placements_of(&config);
    if !placements.is_empty() {
        info!(
            count = placements.len(),
            "using the arrangement from the configuration"
        );
    }
    server.set_placements(placements);
    // This machine's own displays, so it has a place on the desktop.
    let monitors = injector
        .monitors()
        .context("reading this machine's displays")?;
    info!(count = monitors.len(), "this machine's displays");
    server.handle(
        Event::ClientMonitors {
            device: identity.id(),
            monitors,
        },
        Instant::now(),
    );

    let sharing = Sharing::start(
        identity.id(),
        &config.clipboard,
        match platform::clipboard() {
            Ok(backends) => Some(backends),
            Err(e) => {
                warn!("the clipboard on this machine cannot be shared: {e:#}");
                None
            }
        },
    );

    let heartbeat = Duration::from_millis(u64::from(config.network.heartbeat_ms.max(500)));
    let mut daemon = Daemon {
        me: identity.id(),
        last_active: identity.id(),
        server,
        config_stamp: stamp(&config_path),
        config,
        config_path,
        attached: HashMap::new(),
        generation: 0,
        injector,
        capture,
        sharing,
        from_clients: from_clients_tx,
        heartbeat,
        status_path: paths::status_file(),
        status_dirty: true,
        status_written: None,
    };

    let identity = Arc::new(identity);
    let trust = Arc::new(trust);
    let addrs = bind_addresses(&daemon.config);
    if addrs.is_empty() {
        anyhow::bail!("network.listen names no address to listen on");
    }
    for addr in &addrs {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("listening on {addr}"))?;
        info!(%addr, "waiting for machines");
        tokio::spawn(accept_loop(
            listener,
            identity.clone(),
            trust.clone(),
            daemon.config.identity.name.clone(),
            arrivals_tx.clone(),
        ));
    }
    daemon.write_status();

    // The edge-hold timing needs time to pass even when nothing is happening.
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = tokio::time::interval(daemon.heartbeat);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut housekeeping = tokio::time::interval(HOUSEKEEPING);
    housekeeping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(event) = events.recv() => daemon.handle(event),
            Some(from) = from_clients.recv() => match from {
                FromClient::Gone { device, generation } => daemon.gone(device, generation),
                FromClient::Message { device, generation, msg } => daemon.heard(device, generation, msg),
            },
            Some(arrival) = arrivals.recv() => daemon.arrived(arrival),
            _ = tick.tick() => daemon.handle(Event::Tick),
            _ = heartbeat.tick() => daemon.heartbeat(),
            _ = housekeeping.tick() => daemon.housekeeping(),
            happened = daemon.sharing.next() => {
                let sends = daemon.sharing.on(happened, Instant::now());
                daemon.send_bulk(sends);
            }
            _ = tokio::signal::ctrl_c() => {
                info!("stopping");
                break;
            }
        }
        daemon.maybe_write_status();
    }

    // Everything connected is told, so nothing stays held down anywhere and
    // no pointer stays out of the way.
    for (_, client) in daemon.attached.drain() {
        let _ = client.outbound.send(ServerControl::Goodbye);
        client.reader.abort();
    }
    Status::remove(&daemon.status_path);
    // Returning drops the capture, which uninstalls the hooks: that is what
    // gives the keyboard and mouse back if the cursor was elsewhere.
    Ok(())
}

impl Daemon {
    /// Put an event through the state machine and carry out what it says.
    ///
    /// Carrying it out can produce further events -- a machine too far behind
    /// to be told something is let go, which is a departure -- so this runs
    /// until nothing is left.
    fn handle(&mut self, event: Event) {
        let mut queue = vec![event];
        while let Some(event) = queue.pop() {
            if !matches!(
                event,
                Event::Tick
                    | Event::PointerAt { .. }
                    | Event::PointerBy { .. }
                    | Event::Button { .. }
                    | Event::Wheel(_)
                    | Event::Key { .. }
            ) {
                self.status_dirty = true;
            }
            for action in self.server.handle(event, Instant::now()) {
                self.act(action, &mut queue);
            }
        }
        if self.server.active() != self.last_active {
            self.last_active = self.server.active();
            self.status_dirty = true;
        }
    }

    fn act(&mut self, action: Action, queue: &mut Vec<Event>) {
        match action {
            Action::Send { to, msg } => self.send(to, msg, queue),
            Action::Local(LocalAction::SetPointerMode(mode)) => {
                let captured = mode == PointerMode::Captured;
                self.capture.set_swallow(captured);
                let result = if captured {
                    self.injector.hide_cursor()
                } else {
                    self.injector.show_cursor()
                };
                // This machine is the one whose keyboard and mouse are being
                // taken away, so a pointer that will not go where it is put is
                // the one failure the person here cannot work around.
                if let Err(e) = result {
                    warn!(captured, "the local pointer would not move: {e}");
                }
            }
            Action::Local(LocalAction::WarpCursor { x, y }) => {
                if let Err(e) = self.injector.move_to(x, y) {
                    warn!("the local pointer would not go to {x},{y}: {e}");
                }
                let _ = self.injector.flush();
            }
            Action::Local(LocalAction::ReleaseAll) => {}
        }
    }

    fn send(&mut self, to: DeviceId, msg: ServerControl, queue: &mut Vec<Event>) {
        let Some(client) = self.attached.get(&to) else {
            return;
        };
        match client.outbound.send(msg) {
            Sent::Queued => {}
            // Motion gives way to keep up; the next position puts it right.
            Sent::Dropped => warn!(machine = %client.name, "machine is not keeping up"),
            // A thousand unread state changes is a machine that has stopped,
            // not one that is busy. Letting the link go says so, and brings
            // the cursor home -- a state the rest of the system knows how to
            // be in.
            Sent::Overflowed => {
                self.detach(to, "too far behind to be told where the cursor is", queue)
            }
            Sent::Closed => self.detach(to, "its link has closed", queue),
        }
    }

    fn send_bulk(&mut self, sends: Vec<(DeviceId, Bulk)>) {
        let mut queue = Vec::new();
        for (to, msg) in sends {
            self.send(to, ServerControl::Bulk(msg), &mut queue);
        }
        for event in queue {
            self.handle(event);
        }
    }

    /// Let a machine's link go, and treat it as departed.
    fn detach(&mut self, device: DeviceId, why: &str, queue: &mut Vec<Event>) {
        let Some(client) = self.attached.remove(&device) else {
            return;
        };
        warn!(machine = %client.name, "letting the link go: {why}");
        // Dropping the sender ends the writer task, which closes the socket;
        // the reader is stopped here so it cannot report the close as a
        // second departure.
        client.reader.abort();
        queue.push(Event::ClientDown { device });
        let sends = self.sharing.peer_gone(device, Instant::now());
        for (to, msg) in sends {
            self.send(to, ServerControl::Bulk(msg), queue);
        }
        self.status_dirty = true;
    }

    fn arrived(&mut self, arrival: Arrival) {
        let Arrival {
            device,
            name,
            reader,
            writer,
        } = arrival;
        if self.attached.contains_key(&device) {
            // The same machine, again. Either its old link died without
            // saying so or a second copy of it has started; either way the
            // old one cannot be told anything useful any more, and left in
            // place it would keep whatever it did last -- a hidden pointer
            // included -- for ever.
            let mut queue = Vec::new();
            self.detach(device, "the same machine connected again", &mut queue);
            for event in queue {
                self.handle(event);
            }
        }
        self.generation += 1;
        let generation = self.generation;
        let (outbound, control_rx, motion_rx) = Outbound::new();
        let reader = tokio::spawn(client_reader(
            device,
            generation,
            reader,
            self.from_clients.clone(),
        ));
        tokio::spawn(client_writer(writer, control_rx, motion_rx));
        info!(machine = %name, device = %device.short(), "machine connected");
        self.attached.insert(
            device,
            Attached {
                name: name.clone(),
                outbound,
                generation,
                reader,
                last_heard: Instant::now(),
            },
        );
        self.handle(Event::ClientUp { device, name });
        let sends = self.sharing.peer_up(device, Instant::now());
        self.send_bulk(sends);
    }

    fn current(&self, device: DeviceId, generation: u64) -> bool {
        self.attached
            .get(&device)
            .is_some_and(|c| c.generation == generation)
    }

    fn gone(&mut self, device: DeviceId, generation: u64) {
        if !self.current(device, generation) {
            // An old connection's last word, about a machine that has since
            // connected again.
            return;
        }
        let mut queue = Vec::new();
        self.detach(device, "it disconnected", &mut queue);
        for event in queue {
            self.handle(event);
        }
    }

    fn heard(&mut self, device: DeviceId, generation: u64, msg: ClientControl) {
        if !self.current(device, generation) {
            return;
        }
        if let Some(client) = self.attached.get_mut(&device) {
            client.last_heard = Instant::now();
        }
        match msg {
            ClientControl::Monitors { monitors } => {
                self.handle(Event::ClientMonitors { device, monitors })
            }
            ClientControl::Suspended { reason } => {
                self.handle(Event::ClientSuspended { device, reason })
            }
            ClientControl::Resumed => self.handle(Event::ClientResumed { device }),
            ClientControl::Bulk(bulk) => {
                let sends = self.sharing.peer_said(device, bulk, Instant::now());
                self.send_bulk(sends);
            }
            ClientControl::Ping { id } => {
                let mut queue = Vec::new();
                self.send(device, ServerControl::Pong { id }, &mut queue);
                for event in queue {
                    self.handle(event);
                }
            }
            ClientControl::Goodbye => self.gone(device, generation),
            // Liveness and acknowledgements need no decision; being heard
            // from at all was the point.
            ClientControl::Pong { .. } | ClientControl::KeyStateReport { .. } => {}
            // Introductions happened before this machine was attached.
            ClientControl::Hello(_) => {}
        }
    }

    /// Ask every machine whether it is still there, and give up on any that
    /// has not answered for a while.
    fn heartbeat(&mut self) {
        let limit = self.heartbeat * MISSED_HEARTBEATS;
        let silent: Vec<DeviceId> = self
            .attached
            .iter()
            .filter(|(_, c)| c.last_heard.elapsed() > limit)
            .map(|(d, _)| *d)
            .collect();
        let mut queue = Vec::new();
        for device in silent {
            self.detach(
                device,
                &format!("nothing heard from it for {} s", limit.as_secs()),
                &mut queue,
            );
        }
        let live: Vec<DeviceId> = self.attached.keys().copied().collect();
        for (n, device) in live.into_iter().enumerate() {
            self.send(device, ServerControl::Ping { id: n as u32 }, &mut queue);
        }
        for event in queue {
            self.handle(event);
        }
    }

    fn housekeeping(&mut self) {
        let now = stamp(&self.config_path);
        if now != self.config_stamp {
            self.config_stamp = now;
            self.reload_config();
        }
        if self
            .status_written
            .is_none_or(|t| t.elapsed() >= Status::REFRESH_EVERY)
        {
            self.write_status();
        }
    }

    /// Take a changed configuration file into use.
    fn reload_config(&mut self) {
        let fresh = match Config::load(&self.config_path) {
            Ok(fresh) => fresh,
            Err(e) => {
                warn!("the configuration changed but could not be read, so the old one stays: {e}");
                return;
            }
        };
        if fresh.network != self.config.network {
            warn!("network settings changed; those take effect when smkvm is restarted");
        }
        if fresh.identity != self.config.identity {
            warn!("this machine's name changed; that takes effect when smkvm is restarted");
        }
        let actions = self
            .server
            .reconfigure(placements_of(&fresh), settings_of(&fresh));
        let mut queue = Vec::new();
        for action in actions {
            self.act(action, &mut queue);
        }
        for event in queue {
            self.handle(event);
        }
        self.sharing.reconfigure(&fresh.clipboard);
        self.config = fresh;
        self.status_dirty = true;
        info!("configuration reloaded");
    }

    fn maybe_write_status(&mut self) {
        if !self.status_dirty {
            return;
        }
        let settled = self
            .status_written
            .is_none_or(|t| t.elapsed() >= STATUS_SETTLE);
        if settled {
            self.write_status();
        }
    }

    fn write_status(&mut self) {
        let status = self.report();
        if let Err(e) = status.save(&self.status_path) {
            warn!("could not write the status report: {e}");
        }
        self.status_dirty = false;
        self.status_written = Some(Instant::now());
    }

    /// What the daemon knows, for anything that wants to show it.
    fn report(&self) -> Status {
        let layout = self.server.layout();
        let active = self.server.active();
        let machines = layout
            .devices()
            .iter()
            .map(|device| {
                let state = if device.id == self.me {
                    MachineState::Connected
                } else {
                    match self.server.client_health(device.id) {
                        Some(ClientHealth::Ready) => MachineState::Connected,
                        Some(ClientHealth::Suspended) => MachineState::Suspended,
                        None => MachineState::Away,
                    }
                };
                let mut machine = Machine::new(device.name.clone(), Some(device.id), state);
                machine.active = device.id == active;
                machine.monitor = device
                    .monitors
                    .iter()
                    .filter_map(|m| {
                        let global = layout.placement(device.id, &m.id)?;
                        Some(PlacedMonitor {
                            id: m.id.as_str().to_string(),
                            global: [global.x, global.y, global.w, global.h],
                            label: m.label.clone(),
                            primary: m.primary,
                        })
                    })
                    .collect();
                machine
            })
            .collect();
        Status::new(Role::Server, self.config.identity.name.clone(), machines)
    }
}

fn bind_addresses(config: &Config) -> Vec<String> {
    let port = config.network.port;
    if config.network.listen.is_empty() {
        // Listening everywhere is how a machine ends up reachable from a
        // network nobody meant to share it with, so it is never the default.
        warn!("network.listen is empty, so only the loopback address is used");
        return vec![format!("127.0.0.1:{port}")];
    }
    config
        .network
        .listen
        .iter()
        .map(|a| {
            if a.contains(':') && !a.starts_with('[') {
                format!("[{a}]:{port}")
            } else {
                format!("{a}:{port}")
            }
        })
        .collect()
}

async fn accept_loop(
    listener: TcpListener,
    identity: Arc<Identity>,
    trust: Arc<Trust>,
    name: String,
    arrivals: Sender<Arrival>,
) {
    loop {
        let Ok((socket, from)) = listener.accept().await else {
            return;
        };
        let identity = identity.clone();
        let trust = trust.clone();
        let arrivals = arrivals.clone();
        let name = name.clone();
        tokio::spawn(async move {
            let session = match Session::accept(socket, &identity, &trust).await {
                Ok(session) => session,
                Err(e) => {
                    warn!(%from, "refused: {e}");
                    return;
                }
            };
            let device = session.peer();
            let peer_name = session.peer_name().to_string();
            let (mut reader, mut writer) = Link::from(session).split();
            if let Err(e) = hello::as_server(&mut reader, &mut writer, &identity, &name).await {
                warn!(machine = %peer_name, %from, "{e:#}");
                return;
            }
            let _ = arrivals
                .send(Arrival {
                    device,
                    name: peer_name,
                    reader,
                    writer,
                })
                .await;
        });
    }
}

async fn client_reader(
    device: DeviceId,
    generation: u64,
    mut reader: LinkReader,
    out: Sender<FromClient>,
) {
    while let Ok(msg) = reader.recv::<ClientControl>().await {
        if out
            .send(FromClient::Message {
                device,
                generation,
                msg,
            })
            .await
            .is_err()
        {
            return;
        }
    }
    let _ = out.send(FromClient::Gone { device, generation }).await;
}

/// Write to one machine, taking state changes ahead of pointer motion.
///
/// The bias is what makes the two queues worth having: a `Leave` behind a
/// hundred queued positions arrives a hundred positions late, which on a link
/// that is struggling is exactly when it is needed soonest. Motion is never
/// starved in practice, because control messages happen at the rate a person
/// crosses between screens and pastes.
async fn client_writer(
    mut writer: LinkWriter,
    mut control: mpsc::Receiver<ServerControl>,
    mut motion: mpsc::Receiver<ServerControl>,
) {
    loop {
        let msg = tokio::select! {
            biased;
            Some(msg) = control.recv() => msg,
            Some(msg) = motion.recv() => msg,
            else => break,
        };
        if writer.send(&msg).await.is_err() {
            return;
        }
    }
    writer.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use smkvm_layout::Point;
    use smkvm_proto::{Bulk, ClipFormat, ClipSeq};

    /// An `Outbound` whose far end nobody is reading, which is the situation
    /// all of this is about. The receivers come back so the queues stay open
    /// and keep what they are given.
    type Ends = (
        Outbound,
        mpsc::Receiver<ServerControl>,
        mpsc::Receiver<ServerControl>,
    );

    fn outbound(control: usize, motion: usize) -> Ends {
        let (control_tx, control_rx) = mpsc::channel(control);
        let (motion_tx, motion_rx) = mpsc::channel(motion);
        (
            Outbound {
                control: control_tx,
                motion: motion_tx,
            },
            control_rx,
            motion_rx,
        )
    }

    fn enter() -> ServerControl {
        ServerControl::Enter {
            at: Point::new(0, 0),
            pressed: Vec::new(),
            buttons: Vec::new(),
        }
    }

    fn chunk() -> ServerControl {
        ServerControl::Bulk(Bulk::ClipChunk {
            seq: ClipSeq {
                device: DeviceId::from_bytes([2; 32]),
                counter: 1,
            },
            format: ClipFormat::Text,
            offset: 0,
            data: vec![1, 2, 3],
            last: true,
        })
    }

    #[test]
    fn a_machine_drowning_in_motion_can_still_be_told_the_cursor_arrived() {
        // The failure this exists to prevent. Motion arrives thousands of times
        // a minute and control a handful, so a shared queue is always full of
        // motion at the moment the one message that cannot be lost turns up.
        let (out, _control_rx, _motion_rx) = outbound(8, 2);
        for _ in 0..2 {
            assert_eq!(out.send(ServerControl::MoveTo { x: 1, y: 1 }), Sent::Queued);
        }
        assert_eq!(
            out.send(ServerControl::MoveTo { x: 2, y: 2 }),
            Sent::Dropped,
            "motion should give way once its queue is full"
        );

        assert_eq!(out.send(enter()), Sent::Queued);
        assert_eq!(out.send(ServerControl::Leave), Sent::Queued);
        assert_eq!(out.send(ServerControl::ReleaseAll), Sent::Queued);
        assert_eq!(
            out.send(chunk()),
            Sent::Queued,
            "the clipboard rides with control"
        );
    }

    #[test]
    fn a_thousand_unread_state_changes_is_a_fault_not_a_busy_moment() {
        let (out, _control_rx, _motion_rx) = outbound(2, 8);
        assert_eq!(out.send(enter()), Sent::Queued);
        assert_eq!(out.send(ServerControl::Leave), Sent::Queued);
        assert_eq!(out.send(enter()), Sent::Overflowed);
    }

    #[test]
    fn a_link_nobody_reads_is_reported_as_such_whatever_is_sent() {
        let (out, control_rx, motion_rx) = outbound(8, 8);
        drop(control_rx);
        drop(motion_rx);
        assert_eq!(out.send(ServerControl::MoveTo { x: 1, y: 1 }), Sent::Closed);
        assert_eq!(out.send(enter()), Sent::Closed);
    }
}
