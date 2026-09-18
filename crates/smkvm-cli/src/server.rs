//! Running as the machine that owns the keyboard and mouse.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use smkvm_core::{Action, Event, LocalAction, Placement, PointerMode, Server, Settings};
use smkvm_input::Inject;
use smkvm_layout::DeviceId;
use smkvm_net::identity::Identity;
use smkvm_net::link::{Link, LinkReader, LinkWriter};
use smkvm_net::session::Session;
use smkvm_net::trust::Trust;
use smkvm_proto::{ClientControl, ServerControl};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, Sender};
use tracing::{info, warn};

use crate::platform;

/// A machine that has connected, and where to send to it.
struct Attached {
    outbound: Outbound,
}

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
    /// The machine is not attached; there was nowhere to send it.
    Nowhere,
}

/// The two queues a machine is written through.
///
/// They are separate because the two kinds of message fail differently.
/// Pointer motion is a stream where the next supersedes the last, so under
/// pressure the right answer is to lose one and carry on. Control is a sequence
/// of state changes that nothing repeats: lose a `Leave` or an `Enter` and the
/// machine and the server disagree about where the cursor is, with nothing on
/// the way to correct either of them -- which is a cursor that crossed and then
/// vanished.
///
/// One queue would force a single policy on both, and it is motion, arriving
/// thousands of times a minute, that decides when a shared queue is full. So
/// the message that must not be lost would be the one lost, every time.
///
/// Control may overtake motion, which is harmless in both directions: a
/// position that arrives after a `Leave` is discarded by a machine that knows
/// it no longer has the cursor, and one that arrives after an `Enter` is
/// corrected by the next position a moment later. `Enter` carries its own.
struct Outbound {
    control: Sender<ServerControl>,
    motion: Sender<ServerControl>,
}

impl Outbound {
    fn send(&self, msg: ServerControl) -> Sent {
        if msg.may_be_dropped() {
            match self.motion.try_send(msg) {
                Ok(()) => Sent::Queued,
                Err(_) => Sent::Dropped,
            }
        } else {
            match self.control.try_send(msg) {
                Ok(()) => Sent::Queued,
                Err(_) => Sent::Overflowed,
            }
        }
    }
}

/// Something a client's reader task noticed.
enum FromClient {
    Message(DeviceId, ClientControl),
    Gone(DeviceId),
}

pub async fn run(
    identity: Identity,
    trust: Trust,
    config: smkvm_config::Config,
    layout: smkvm_layout::Layout,
) -> Result<()> {
    let settings = Settings {
        switch_delay: Duration::from_millis(u64::from(config.behavior.switch_delay_ms)),
        switch_double_tap: Duration::from_millis(u64::from(config.behavior.switch_double_tap_ms)),
        edge_overflow: config.behavior.edge_overflow,
    };

    let (events_tx, mut events) = mpsc::channel::<Event>(4096);
    let (from_clients_tx, mut from_clients) = mpsc::channel::<FromClient>(256);
    let (arrivals_tx, mut arrivals) =
        mpsc::channel::<(DeviceId, String, LinkReader, LinkWriter)>(8);

    let mut injector = platform::injector()?;
    let capture = platform::start_capture(events_tx.clone())?;

    let mut server = Server::new(
        identity.id(),
        config.identity.name.clone(),
        layout,
        settings,
    );

    // The arrangement from the configuration, applied as machines appear.
    // Anything it does not mention is placed automatically, so a half-written
    // layout still leaves every screen reachable.
    let placements: Vec<Placement> = config
        .screen
        .iter()
        .flat_map(|screen| {
            screen.monitor.iter().map(|monitor| Placement {
                machine: screen.name.clone(),
                monitor: monitor.id.clone(),
                global: monitor.rect(),
            })
        })
        .collect();
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

    let identity = Arc::new(identity);
    let trust = Arc::new(trust);
    let addrs = bind_addresses(&config);
    for addr in &addrs {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("listening on {addr}"))?;
        info!(%addr, "waiting for machines");
        tokio::spawn(accept_loop(
            listener,
            identity.clone(),
            trust.clone(),
            arrivals_tx.clone(),
        ));
    }
    if addrs.is_empty() {
        anyhow::bail!("network.listen names no address to listen on");
    }

    let mut attached: HashMap<DeviceId, Attached> = HashMap::new();
    // The edge-hold timing needs time to pass even when nothing is happening.
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let event = tokio::select! {
            Some(event) = events.recv() => event,
            Some(from) = from_clients.recv() => match from {
                FromClient::Gone(device) => {
                    attached.remove(&device);
                    warn!(device = %device.short(), "machine disconnected");
                    Event::ClientDown { device }
                }
                FromClient::Message(device, msg) => match msg {
                    ClientControl::Monitors { monitors } => Event::ClientMonitors { device, monitors },
                    ClientControl::Suspended { reason } => Event::ClientSuspended { device, reason },
                    ClientControl::Resumed => Event::ClientResumed { device },
                    // Liveness and acknowledgements need no decision.
                    _ => continue,
                },
            },
            Some((device, name, reader, writer)) = arrivals.recv() => {
                let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE);
                let (motion_tx, motion_rx) = mpsc::channel(MOTION_QUEUE);
                attached.insert(device, Attached {
                    outbound: Outbound { control: control_tx, motion: motion_tx },
                });
                tokio::spawn(client_reader(device, reader, from_clients_tx.clone()));
                tokio::spawn(client_writer(writer, control_rx, motion_rx));
                info!(device = %device.short(), %name, "machine connected");
                Event::ClientUp { device, name }
            }
            _ = tick.tick() => Event::Tick,
        };

        for action in server.handle(event, Instant::now()) {
            match action {
                Action::Send { to, msg } => {
                    let sent = match attached.get(&to) {
                        Some(client) => client.outbound.send(msg),
                        None => Sent::Nowhere,
                    };
                    match sent {
                        Sent::Queued | Sent::Nowhere => {}
                        Sent::Dropped => {
                            warn!(device = %to.short(), "machine is not keeping up")
                        }
                        // A thousand unread state changes is a machine that has
                        // stopped, not one that is busy. Letting the link go
                        // says so, and brings the cursor home -- which is a
                        // state the rest of the system knows how to be in.
                        Sent::Overflowed => {
                            warn!(
                                device = %to.short(),
                                "machine is too far behind to be told where the cursor is; \
                                 letting the link go"
                            );
                            attached.remove(&to);
                        }
                    }
                }
                Action::Local(LocalAction::SetPointerMode(mode)) => {
                    let captured = mode == PointerMode::Captured;
                    capture.set_swallow(captured);
                    let moved = if captured {
                        injector.hide_cursor()
                    } else {
                        injector.show_cursor()
                    };
                    // This machine is the one whose keyboard and mouse are
                    // being taken away, so a pointer that will not go where it
                    // is put is the one failure the person here cannot work
                    // around.
                    if let Err(e) = moved {
                        warn!(captured, "the local pointer would not move: {e}");
                    }
                }
                Action::Local(LocalAction::WarpCursor { x, y }) => {
                    if let Err(e) = injector.move_to(x, y) {
                        warn!("the local pointer would not go to {x},{y}: {e}");
                    }
                    let _ = injector.flush();
                }
                Action::Local(LocalAction::ReleaseAll) => {}
            }
        }
    }
}

fn bind_addresses(config: &smkvm_config::Config) -> Vec<String> {
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
    arrivals: Sender<(DeviceId, String, LinkReader, LinkWriter)>,
) {
    loop {
        let Ok((socket, from)) = listener.accept().await else {
            return;
        };
        let identity = identity.clone();
        let trust = trust.clone();
        let arrivals = arrivals.clone();
        tokio::spawn(async move {
            match Session::accept(socket, &identity, &trust).await {
                Ok(session) => {
                    let peer = session.peer();
                    let name = session.peer_name().to_string();
                    let (reader, writer) = Link::from(session).split();
                    let _ = arrivals.send((peer, name, reader, writer)).await;
                }
                Err(e) => warn!(%from, "refused: {e}"),
            }
        });
    }
}

async fn client_reader(device: DeviceId, mut reader: LinkReader, out: Sender<FromClient>) {
    while let Ok(msg) = reader.recv::<ClientControl>().await {
        if out.send(FromClient::Message(device, msg)).await.is_err() {
            return;
        }
    }
    let _ = out.send(FromClient::Gone(device)).await;
}

/// Write to one machine, taking state changes ahead of pointer motion.
///
/// The bias is what makes the two queues worth having: a `Leave` behind a
/// hundred queued positions arrives a hundred positions late, which on a link
/// that is struggling is exactly when it is needed soonest. Motion is never
/// starved in practice, because control messages happen at the rate a person
/// crosses between screens.
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
    }

    #[test]
    fn a_machine_that_has_stopped_reading_loses_its_link_rather_than_a_state_change() {
        let (out, _control_rx, _motion_rx) = outbound(1, 8);
        assert_eq!(out.send(enter()), Sent::Queued);
        assert_eq!(
            out.send(ServerControl::Leave),
            Sent::Overflowed,
            "there is no quiet way to lose this one"
        );
    }

    #[test]
    fn motion_and_control_do_not_share_a_queue() {
        // Filling one must not consume room in the other, in either direction.
        let (out, _control_rx, _motion_rx) = outbound(2, 2);
        assert_eq!(out.send(enter()), Sent::Queued);
        assert_eq!(out.send(ServerControl::Leave), Sent::Queued);
        for _ in 0..2 {
            assert_eq!(out.send(ServerControl::MoveTo { x: 0, y: 0 }), Sent::Queued);
        }
        assert_eq!(
            out.send(ServerControl::MoveTo { x: 0, y: 0 }),
            Sent::Dropped
        );
        assert_eq!(out.send(ServerControl::ReleaseAll), Sent::Overflowed);
    }
}
