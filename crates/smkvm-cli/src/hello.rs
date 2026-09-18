//! The first thing two machines say once the link is encrypted.
//!
//! The handshake underneath proves *which* machine is at the other end. It
//! says nothing about which build, and two builds that disagree about the
//! wire format would otherwise proceed to misunderstand each other -- the
//! symptoms of which look like anything at all except a version mismatch.
//! So each side states its protocol version first, and a mismatch is refused
//! with a message that says so.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use smkvm_net::identity::Identity;
use smkvm_net::link::{LinkReader, LinkWriter};
use smkvm_proto::{ClientControl, Hello, Reject, Role, ServerControl, PROTO_VERSION};

/// How long to wait for the other side to introduce itself.
const PATIENCE: Duration = Duration::from_secs(5);

fn hello(identity: &Identity, name: &str, role: Role) -> Hello {
    Hello {
        proto: PROTO_VERSION,
        device: identity.id(),
        name: name.to_string(),
        role,
    }
}

/// As the client: introduce this machine and hear the server's introduction.
pub async fn as_client(
    reader: &mut LinkReader,
    writer: &mut LinkWriter,
    identity: &Identity,
    name: &str,
) -> Result<Hello> {
    writer
        .send(&ClientControl::Hello(hello(identity, name, Role::Client)))
        .await
        .context("introducing this machine")?;
    let answer = tokio::time::timeout(PATIENCE, reader.recv::<ServerControl>())
        .await
        .context("the server did not introduce itself in time")?
        .context("waiting for the server to introduce itself")?;
    match answer {
        ServerControl::Hello(theirs) if theirs.proto == PROTO_VERSION => Ok(theirs),
        ServerControl::Hello(theirs) => bail!(
            "the server speaks protocol version {}, this build speaks {}; update the older one",
            theirs.proto,
            PROTO_VERSION
        ),
        ServerControl::Rejected {
            reason: Reject::ProtocolVersion { theirs, ours },
        } => bail!(
            "the server refused: it speaks protocol version {theirs}, this build speaks {ours}; \
             update the older one"
        ),
        ServerControl::Rejected { reason } => bail!("the server refused: {reason:?}"),
        _ => {
            bail!("the server did not introduce itself, so it is running an older build; update it")
        }
    }
}

/// As the server: hear the client's introduction and answer it, or refuse.
pub async fn as_server(
    reader: &mut LinkReader,
    writer: &mut LinkWriter,
    identity: &Identity,
    name: &str,
) -> Result<Hello> {
    let first = tokio::time::timeout(PATIENCE, reader.recv::<ClientControl>())
        .await
        .context("the machine did not introduce itself in time")?
        .context("waiting for the machine to introduce itself")?;
    let theirs = match first {
        ClientControl::Hello(theirs) => theirs,
        _ => bail!(
            "the machine did not introduce itself, so it is running an older build; update it"
        ),
    };
    if theirs.proto != PROTO_VERSION {
        let _ = writer
            .send(&ServerControl::Rejected {
                reason: Reject::ProtocolVersion {
                    theirs: theirs.proto,
                    ours: PROTO_VERSION,
                },
            })
            .await;
        writer.close().await;
        bail!(
            "{} speaks protocol version {}, this build speaks {}; update the older one",
            theirs.name,
            theirs.proto,
            PROTO_VERSION
        );
    }
    writer
        .send(&ServerControl::Hello(hello(identity, name, Role::Server)))
        .await
        .context("introducing this machine")?;
    Ok(theirs)
}
