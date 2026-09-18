//! Running as a machine that receives the cursor.

use std::time::Duration;

use anyhow::{Context, Result};
use smkvm_core::{Client, ClientAction};
use smkvm_input::Monitors;
use smkvm_net::identity::Identity;
use smkvm_net::link::Link;
use smkvm_net::session::Session;
use smkvm_net::trust::Peer;
use smkvm_proto::{ClientControl, ServerControl};
use tracing::{info, warn};

use crate::platform;

/// Connect, stay connected, and reconnect when the link goes.
pub async fn run(identity: Identity, peer: Peer, address: String) -> Result<()> {
    // Backoff with a ceiling: a machine that is merely asleep should be picked
    // up promptly when it wakes, without hammering the network meanwhile.
    let mut wait = Duration::from_millis(250);
    const LONGEST: Duration = Duration::from_secs(10);

    loop {
        match session(&identity, &peer, &address).await {
            Ok(()) => {
                info!("the server closed the link");
                wait = Duration::from_millis(250);
            }
            Err(e) => warn!("{e}"),
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(LONGEST);
    }
}

async fn session(identity: &Identity, peer: &Peer, address: &str) -> Result<()> {
    let mut injector = platform::injector()?;
    let monitors = injector
        .monitors()
        .context("reading this machine's displays")?;

    let session = Session::connect(address, identity, peer)
        .await
        .with_context(|| format!("connecting to {address}"))?;
    info!(server = session.peer_name(), "connected");

    let (mut reader, mut writer) = Link::from(session).split();
    let mut client = Client::new(injector);

    writer
        .send(&ClientControl::Monitors { monitors })
        .await
        .context("reporting displays")?;

    let result = loop {
        let msg = match reader.recv::<ServerControl>().await {
            Ok(msg) => msg,
            Err(e) => break Err(e),
        };
        let mut failed = None;
        for action in client.handle(msg) {
            let ClientAction::Send(reply) = action;
            if let Err(e) = writer.send(&reply).await {
                failed = Some(e);
                break;
            }
        }
        if let Some(e) = failed {
            break Err(e);
        }
    };

    // Whatever ended the link, nothing may be left held down: no further
    // message can arrive to release it.
    client.disconnected();
    result.map_err(|e| anyhow::anyhow!("{e}"))
}
