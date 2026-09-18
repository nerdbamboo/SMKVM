//! Running as the machine that owns the keyboard and mouse.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use smkvm_core::{Action, Event, LocalAction, PointerMode, Server, Settings};
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
    outbound: Sender<ServerControl>,
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

    let mut server = Server::new(identity.id(), layout, settings);
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
                let (outbound_tx, outbound_rx) = mpsc::channel(256);
                attached.insert(device, Attached { outbound: outbound_tx });
                tokio::spawn(client_reader(device, reader, from_clients_tx.clone()));
                tokio::spawn(client_writer(writer, outbound_rx));
                info!(device = %device.short(), %name, "machine connected");
                Event::ClientUp { device, name }
            }
            _ = tick.tick() => Event::Tick,
        };

        for action in server.handle(event, Instant::now()) {
            match action {
                Action::Send { to, msg } => {
                    if let Some(client) = attached.get(&to) {
                        // Dropping a message beats stalling every other machine
                        // behind one that has stopped reading.
                        if client.outbound.try_send(msg).is_err() {
                            warn!(device = %to.short(), "machine is not keeping up");
                        }
                    }
                }
                Action::Local(LocalAction::SetPointerMode(mode)) => {
                    capture.set_swallow(mode == PointerMode::Captured);
                }
                Action::Local(LocalAction::WarpCursor { x, y }) => {
                    let _ = injector.move_to(x, y);
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

async fn client_writer(mut writer: LinkWriter, mut outbound: mpsc::Receiver<ServerControl>) {
    while let Some(msg) = outbound.recv().await {
        if writer.send(&msg).await.is_err() {
            return;
        }
    }
    writer.close().await;
}
