//! Running as a machine that receives the cursor.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use smkvm_config::status::{Machine, MachineState, Status};
use smkvm_config::{paths, Config};
use smkvm_core::{Client, ClientAction};
use smkvm_input::Monitors;
use smkvm_net::identity::Identity;
use smkvm_net::link::{Link, LinkReader};
use smkvm_net::session::Session;
use smkvm_net::trust::Peer;
use smkvm_proto::{ClientControl, Role, ServerControl};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::clipboard::Sharing;
use crate::{hello, platform};

/// How many heartbeats the server may miss before the link is given up on.
const MISSED_HEARTBEATS: u32 = 3;

/// Connect, stay connected, and reconnect when the link goes.
pub async fn run(identity: Identity, peer: Peer, address: String, config: Config) -> Result<()> {
    // Backoff with a ceiling: a machine that is merely asleep should be picked
    // up promptly when it wakes, without hammering the network meanwhile.
    let mut wait = Duration::from_millis(250);
    const LONGEST: Duration = Duration::from_secs(10);

    // The clipboard outlives any one session: what was copied here is still
    // worth offering once the link is back.
    let mut sharing = Sharing::start(
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
    let status_path = paths::status_file();
    let heartbeat = Duration::from_millis(u64::from(config.network.heartbeat_ms.max(500)));

    loop {
        write_status(&status_path, &config, &peer, MachineState::Away, false);
        let outcome = tokio::select! {
            outcome = session(&identity, &peer, &address, &config, &mut sharing, &status_path, heartbeat) => outcome,
            _ = tokio::signal::ctrl_c() => {
                info!("stopping");
                Status::remove(&status_path);
                return Ok(());
            }
        };
        match outcome {
            Ok(()) => {
                info!("the server closed the link");
                wait = Duration::from_millis(250);
            }
            Err(e) => warn!("{e:#}"),
        }
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = tokio::signal::ctrl_c() => {
                info!("stopping");
                Status::remove(&status_path);
                return Ok(());
            }
        }
        wait = (wait * 2).min(LONGEST);
    }
}

async fn session(
    identity: &Identity,
    peer: &Peer,
    address: &str,
    config: &Config,
    sharing: &mut Sharing,
    status_path: &Path,
    heartbeat: Duration,
) -> Result<()> {
    let mut injector = platform::injector()?;
    let monitors = injector
        .monitors()
        .context("reading this machine's displays")?;

    let session = Session::connect(address, identity, peer)
        .await
        .with_context(|| format!("connecting to {address}"))?;
    let (mut reader, mut writer) = Link::from(session).split();
    let server = hello::as_client(&mut reader, &mut writer, identity, &config.identity.name)
        .await
        .with_context(|| format!("introducing this machine to {}", peer.name))?;
    info!(server = %server.name, "connected");

    let mut client = Client::new(injector);
    writer
        .send(&ClientControl::Monitors { monitors })
        .await
        .context("reporting displays")?;

    let (incoming_tx, mut incoming) = mpsc::channel::<ServerControl>(256);
    let reading = tokio::spawn(read_loop(reader, incoming_tx));

    for (_, msg) in sharing.peer_up(peer.id, Instant::now()) {
        writer.send(&ClientControl::Bulk(msg)).await?;
    }

    let mut refresh = tokio::time::interval(Status::REFRESH_EVERY);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let silence = heartbeat * MISSED_HEARTBEATS;
    let mut had_cursor = client.is_active();
    write_status(
        status_path,
        config,
        peer,
        MachineState::Connected,
        had_cursor,
    );

    let result = loop {
        let step = tokio::select! {
            heard = tokio::time::timeout(silence, incoming.recv()) => match heard {
                Ok(Some(msg)) => Step::Message(msg),
                Ok(None) => break Ok(()),
                Err(_) => break Err(anyhow::anyhow!(
                    "nothing heard from the server for {} s; the link is presumed dead",
                    silence.as_secs()
                )),
            },
            happened = sharing.next() => Step::Clipboard(happened),
            _ = refresh.tick() => Step::Refresh,
        };
        let outcome: Result<()> = match step {
            Step::Message(ServerControl::Bulk(bulk)) => {
                send_all(
                    &mut writer,
                    sharing.peer_said(peer.id, bulk, Instant::now()),
                )
                .await
            }
            Step::Message(ServerControl::Goodbye) => {
                client.handle(ServerControl::Goodbye);
                break Ok(());
            }
            Step::Message(msg) => {
                let mut outcome = Ok(());
                for action in client.handle(msg) {
                    let ClientAction::Send(reply) = action;
                    if let Err(e) = writer.send(&reply).await {
                        outcome = Err(e.into());
                        break;
                    }
                }
                outcome
            }
            Step::Clipboard(happened) => {
                send_all(&mut writer, sharing.on(happened, Instant::now())).await
            }
            Step::Refresh => {
                write_status(
                    status_path,
                    config,
                    peer,
                    MachineState::Connected,
                    client.is_active(),
                );
                Ok(())
            }
        };
        if let Err(e) = outcome {
            break Err(e);
        }
        if client.is_active() != had_cursor {
            had_cursor = client.is_active();
            write_status(
                status_path,
                config,
                peer,
                MachineState::Connected,
                had_cursor,
            );
        }
    };

    reading.abort();
    // Whatever ended the link, nothing may be left held down and the pointer
    // must be this machine's own again: no further message can arrive to do
    // either.
    client.disconnected();
    for (_, msg) in sharing.peer_gone(peer.id, Instant::now()) {
        // The link is gone; there is nobody to send these to.
        let _ = msg;
    }
    write_status(status_path, config, peer, MachineState::Away, false);
    result
}

enum Step {
    Message(ServerControl),
    Clipboard(crate::clipboard::Happened),
    Refresh,
}

async fn send_all(
    writer: &mut smkvm_net::link::LinkWriter,
    sends: Vec<(smkvm_layout::DeviceId, smkvm_proto::Bulk)>,
) -> Result<()> {
    for (_, msg) in sends {
        writer.send(&ClientControl::Bulk(msg)).await?;
    }
    Ok(())
}

async fn read_loop(mut reader: LinkReader, out: mpsc::Sender<ServerControl>) {
    loop {
        match reader.recv::<ServerControl>().await {
            Ok(msg) => {
                if out.send(msg).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                warn!("the link failed: {e}");
                return;
            }
        }
    }
}

/// Say how this machine stands: connected to the server or not, and whether
/// the cursor is here.
fn write_status(path: &Path, config: &Config, server: &Peer, state: MachineState, active: bool) {
    let mut here = Machine::new(config.identity.name.clone(), None, MachineState::Connected);
    here.active = active;
    let mut them = Machine::new(server.name.clone(), Some(server.id), state);
    them.active = state == MachineState::Connected && !active;
    let status = Status::new(Role::Client, config.identity.name.clone(), vec![here, them]);
    if let Err(e) = status.save(path) {
        warn!("could not write the status report: {e}");
    }
}

/// The machine to connect to, from the list of paired ones.
pub fn choose_server(trust: &smkvm_net::trust::Trust, config: &Config) -> Result<Peer> {
    let peers: Vec<&Peer> = trust.peers().collect();
    match peers.len() {
        0 => bail!("no machines are paired yet. Run `smkvm pair <server>` first."),
        1 => Ok(peers[0].clone()),
        _ => {
            // The configuration may name the server by the host it connects
            // to, which is not a machine name; so the one paired machine whose
            // name matches is preferred and otherwise the first is taken.
            let named = config.network.server.as_deref().unwrap_or("");
            peers
                .iter()
                .find(|p| p.name == named)
                .or(peers.first())
                .map(|p| (*p).clone())
                .context("which paired machine is the server? Name it in the configuration.")
        }
    }
}
