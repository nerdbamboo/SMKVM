//! Introducing this machine to another, from the window.
//!
//! The handshake wants a runtime and the window is an event loop, so the two
//! talk through a channel: the work happens off to one side, and the single
//! decision a person has to make comes back here and waits until they make it.
//!
//! Nothing is recorded until both ends have said yes. A code that is only
//! confirmed at one end is exactly the situation the code exists to catch.

use std::future::Future;
use std::sync::mpsc::{self, Receiver};

use smkvm_config::paths;
use smkvm_net::identity::Identity;
use smkvm_net::pairing::Pairing as Handshake;
use smkvm_net::trust::Peer;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// How far along an introduction has got.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Step {
    #[default]
    Idle,
    /// Waiting for the other machine to reach out to this one.
    Listening(u16),
    Reaching(String),
    /// Both ends are showing a code, and nothing goes further until somebody
    /// says whether they are the same.
    Compare {
        code: String,
        peer: String,
    },
    Paired(String),
    Refused,
    Failed(String),
}

pub struct Pairing {
    runtime: tokio::runtime::Runtime,
    pub step: Step,
    incoming: Option<Receiver<Message>>,
    working: Option<tokio::task::JoinHandle<()>>,
    /// The other end is holding the line until this is answered.
    answer: Option<oneshot::Sender<bool>>,
}

enum Message {
    Compare {
        code: String,
        peer: String,
        answer: oneshot::Sender<bool>,
    },
    Paired(Box<Peer>),
    Refused,
    Failed(String),
}

impl Pairing {
    pub fn new() -> std::io::Result<Pairing> {
        Ok(Pairing {
            // One worker: this runs a single handshake at a time and spends
            // nearly all of it waiting for a person.
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()?,
            step: Step::Idle,
            incoming: None,
            working: None,
            answer: None,
        })
    }

    /// Wait for the other machine to reach out to this one.
    pub fn listen(&mut self, ctx: &egui::Context, me: String, port: u16) {
        self.begin(ctx, Step::Listening(port), async move {
            let identity = Identity::load_or_create(&paths::identity_file())?;
            let listener = TcpListener::bind(("0.0.0.0", port)).await.map_err(io)?;
            let (socket, _) = listener.accept().await.map_err(io)?;
            Handshake::accept(socket, &identity, &me).await
        });
    }

    /// Reach out to a machine that is waiting.
    pub fn reach(&mut self, ctx: &egui::Context, me: String, host: String) {
        let address = host.clone();
        self.begin(ctx, Step::Reaching(host), async move {
            let identity = Identity::load_or_create(&paths::identity_file())?;
            Handshake::initiate(&address, &identity, &me).await
        });
    }

    /// Take in whatever the handshake has said since last time. Returns a
    /// machine to record when one has just been agreed at both ends.
    pub fn poll(&mut self) -> Option<Peer> {
        let said: Vec<Message> = self.incoming.as_ref()?.try_iter().collect();
        let mut paired = None;
        for message in said {
            match message {
                Message::Compare { code, peer, answer } => {
                    self.answer = Some(answer);
                    self.step = Step::Compare { code, peer };
                }
                Message::Paired(peer) => {
                    self.step = Step::Paired(peer.name.clone());
                    paired = Some(*peer);
                }
                Message::Refused => self.step = Step::Refused,
                Message::Failed(why) => self.step = Step::Failed(why),
            }
        }
        paired
    }

    /// Say whether the two codes were the same.
    pub fn answer(&mut self, same: bool) {
        if let Some(answer) = self.answer.take() {
            let _ = answer.send(same);
        }
    }

    /// Stop whatever is going on and forget it.
    ///
    /// The handshake is dropped rather than answered, which is what a machine
    /// waiting on the other end reads as the offer being withdrawn.
    pub fn cancel(&mut self) {
        if let Some(working) = self.working.take() {
            working.abort();
        }
        self.answer = None;
        self.incoming = None;
        self.step = Step::Idle;
    }

    fn begin(
        &mut self,
        ctx: &egui::Context,
        step: Step,
        handshake: impl Future<Output = smkvm_net::Result<Handshake>> + Send + 'static,
    ) {
        self.cancel();
        let (tell, incoming) = mpsc::channel();
        let ctx = ctx.clone();
        self.incoming = Some(incoming);
        self.step = step;
        self.working = Some(self.runtime.spawn(async move {
            if let Err(why) = introduce(handshake, &tell, &ctx).await {
                let _ = tell.send(Message::Failed(why));
                ctx.request_repaint();
            }
        }));
    }
}

async fn introduce(
    handshake: impl Future<Output = smkvm_net::Result<Handshake>>,
    tell: &mpsc::Sender<Message>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let pairing = handshake.await.map_err(|e| e.to_string())?;
    let (answer, decided) = oneshot::channel();
    tell.send(Message::Compare {
        code: pairing.code().to_string(),
        peer: pairing.peer_name().to_string(),
        answer,
    })
    .map_err(|_| "the window stopped listening".to_string())?;
    ctx.request_repaint();

    // A window that goes away without answering is a refusal, not a yes.
    if !decided.await.unwrap_or(false) {
        pairing.reject().await.map_err(|e| e.to_string())?;
        let _ = tell.send(Message::Refused);
        ctx.request_repaint();
        return Ok(());
    }

    let peer = pairing.confirm().await.map_err(|e| e.to_string())?;
    let _ = tell.send(Message::Paired(Box::new(peer)));
    ctx.request_repaint();
    Ok(())
}

fn io(source: std::io::Error) -> smkvm_net::Error {
    smkvm_net::Error::Io {
        path: Default::default(),
        source,
    }
}

/// Give a bare address the port SMKVM listens on.
///
/// An address that already names one is left alone. A bare IPv6 address is
/// bracketed first, because its own colons would otherwise be read as one.
pub fn with_port(host: &str, port: u16) -> String {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.contains("]:") {
            true => host.to_string(),
            false => format!("{host}:{port}"),
        };
    }
    if host.matches(':').count() > 1 {
        return format!("[{host}]:{port}");
    }
    match host.rsplit_once(':') {
        Some((head, "")) => format!("{head}:{port}"),
        Some((_, tail)) if tail.parse::<u16>().is_ok() => host.to_string(),
        _ => format!("{host}:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_gets_the_usual_port() {
        assert_eq!(with_port("desk", 24810), "desk:24810");
        assert_eq!(with_port("  desk  ", 24810), "desk:24810");
        assert_eq!(with_port("192.168.1.5", 24810), "192.168.1.5:24810");
    }

    #[test]
    fn a_port_somebody_typed_is_left_alone() {
        assert_eq!(with_port("desk:5000", 24810), "desk:5000");
        assert_eq!(with_port("192.168.1.5:5000", 24810), "192.168.1.5:5000");
    }

    #[test]
    fn an_address_that_trails_off_still_gets_a_port() {
        assert_eq!(with_port("desk:", 24810), "desk:24810");
    }

    #[test]
    fn the_colons_in_an_ipv6_address_are_its_own() {
        assert_eq!(with_port("::1", 24810), "[::1]:24810");
        assert_eq!(with_port("fe80::1", 24810), "[fe80::1]:24810");
        assert_eq!(with_port("[::1]", 24810), "[::1]:24810");
        assert_eq!(with_port("[::1]:5000", 24810), "[::1]:5000");
    }
}
