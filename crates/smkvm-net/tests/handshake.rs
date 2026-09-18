//! Pairing and connecting, over real sockets.
//!
//! These run against loopback TCP rather than an in-memory pipe, so the
//! handshakes, the framing and the record chunking are exercised as they will
//! be between machines.

use smkvm_layout::Point;
use smkvm_net::identity::{device_id, Identity};
use smkvm_net::link::Link;
use smkvm_net::pairing::Pairing;
use smkvm_net::session::Session;
use smkvm_net::trust::{Peer, Trust};
use smkvm_net::Error;
use smkvm_proto::{ClientControl, Key, ServerControl};
use tokio::net::TcpListener;

async fn listener() -> (TcpListener, std::net::SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    (l, addr)
}

/// Run a pairing handshake between two machines, stopping before anyone has
/// confirmed anything.
async fn shake(a: &Identity, b: &Identity) -> (Pairing, Pairing) {
    let (l, addr) = listener().await;
    let responder = async {
        let (sock, _) = l.accept().await.unwrap();
        Pairing::accept(sock, b, "machine-b").await.unwrap()
    };
    let initiator = async { Pairing::initiate(addr, a, "machine-a").await.unwrap() };
    tokio::join!(initiator, responder)
}

/// Pair two machines fully, returning what each should store about the other.
async fn pair(a: &Identity, b: &Identity) -> (Peer, Peer) {
    let (pa, pb) = shake(a, b).await;
    assert_eq!(pa.code(), pb.code());
    let (ra, rb) = tokio::join!(pa.confirm(), pb.confirm());
    (ra.unwrap(), rb.unwrap())
}

#[tokio::test]
async fn two_machines_pair_and_see_the_same_code() {
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (pa, pb) = shake(&a, &b).await;

    assert_eq!(pa.code(), pb.code(), "the codes must match to be compared");
    assert_eq!(pa.code().as_str().len(), 6);
    assert!(pa.code().as_str().chars().all(|c| c.is_ascii_digit()));
    assert_eq!(pa.peer_name(), "machine-b");
    assert_eq!(pb.peer_name(), "machine-a");

    let (ra, rb) = tokio::join!(pa.confirm(), pb.confirm());
    let (peer_b, peer_a) = (ra.unwrap(), rb.unwrap());
    assert_eq!(peer_b.id, b.id(), "each stores the other's real identity");
    assert_eq!(peer_a.id, a.id());
    assert_eq!(peer_b.public_key, b.public_key());
}

#[tokio::test]
async fn different_pairs_get_different_codes() {
    let (a, b, c) = (
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
    );
    let (first, _) = shake(&a, &b).await;
    let (second, _) = shake(&a, &c).await;
    assert_ne!(first.code(), second.code());
}

#[tokio::test]
async fn someone_in_the_middle_cannot_make_the_codes_agree() {
    // The whole point of comparing codes. A relay has to run two separate
    // handshakes, and the code comes from the transcript, so the two ends see
    // different numbers and the person notices.
    let (a, b, middle) = (
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
    );

    let (l, addr) = listener().await;
    let (l2, addr2) = listener().await;

    let victim_a = async { Pairing::initiate(addr, &a, "machine-a").await.unwrap() };
    let relay = async {
        // The relay answers A, then turns round and pairs with B itself.
        let (sock, _) = l.accept().await.unwrap();
        let to_a = Pairing::accept(sock, &middle, "machine-b").await.unwrap();
        let to_b = Pairing::initiate(addr2, &middle, "machine-a")
            .await
            .unwrap();
        (to_a, to_b)
    };
    let victim_b = async {
        let (sock, _) = l2.accept().await.unwrap();
        Pairing::accept(sock, &b, "machine-b").await.unwrap()
    };
    let (pa, _relay, pb) = tokio::join!(victim_a, relay, victim_b);

    assert_ne!(
        pa.code(),
        pb.code(),
        "a relay managed to show both ends the same code"
    );
}

#[tokio::test]
async fn paired_machines_can_talk() {
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (peer_b, peer_a) = pair(&a, &b).await;

    let mut trust = Trust::new();
    trust.add(peer_a.public_key.clone(), "machine-a");

    let (l, addr) = listener().await;
    let server = async {
        let (sock, _) = l.accept().await.unwrap();
        let session = Session::accept(sock, &b, &trust).await.unwrap();
        assert_eq!(session.peer(), a.id());
        let mut link = Link::from(session);
        let msg: ServerControl = link.recv().await.unwrap();
        link.send(&ClientControl::Pong { id: 99 }).await.unwrap();
        msg
    };
    let client = async {
        let session = Session::connect(addr, &a, &peer_b).await.unwrap();
        assert_eq!(session.peer(), b.id());
        let mut link = Link::from(session);
        link.send(&ServerControl::Enter {
            at: Point::new(12, 34),
            pressed: vec![Key::LEFT_CTRL],
            buttons: vec![],
        })
        .await
        .unwrap();
        link.recv::<ClientControl>().await.unwrap()
    };
    let (got, reply) = tokio::join!(server, client);

    assert_eq!(
        got,
        ServerControl::Enter {
            at: Point::new(12, 34),
            pressed: vec![Key::LEFT_CTRL],
            buttons: vec![],
        }
    );
    assert_eq!(reply, ClientControl::Pong { id: 99 });
}

#[tokio::test]
async fn a_machine_that_was_never_paired_is_refused() {
    let (a, b, stranger) = (
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
    );
    let (peer_b, peer_a) = pair(&a, &b).await;

    // b trusts only a.
    let mut trust = Trust::new();
    trust.add(peer_a.public_key.clone(), "machine-a");

    let (l, addr) = listener().await;
    let server = async {
        let (sock, _) = l.accept().await.unwrap();
        Session::accept(sock, &b, &trust).await
    };
    let intruder = async { Session::connect(addr, &stranger, &peer_b).await };
    let (server_result, intruder_result) = tokio::join!(server, intruder);

    assert!(
        matches!(server_result, Err(Error::NotPaired)),
        "an unpaired machine got in: {server_result:?}"
    );
    assert!(intruder_result.is_err(), "the intruder got a session");
}

#[tokio::test]
async fn a_machine_cannot_be_impersonated_by_name() {
    // Trust is keyed on the key, not on anything the peer says about itself.
    let (a, b, impostor) = (
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
    );
    let (peer_b, peer_a) = pair(&a, &b).await;

    let mut trust = Trust::new();
    trust.add(peer_a.public_key.clone(), "machine-a");
    // The impostor is listed under a's name but with its own key.
    let mut wrong = Trust::new();
    wrong.add(impostor.public_key().to_vec(), "machine-a");
    assert_ne!(
        trust.peers().next().unwrap().id,
        wrong.peers().next().unwrap().id,
        "the name has no bearing on identity"
    );

    let (l, addr) = listener().await;
    let server = async {
        let (sock, _) = l.accept().await.unwrap();
        Session::accept(sock, &b, &trust).await
    };
    let client = async { Session::connect(addr, &impostor, &peer_b).await };
    let (server_result, _) = tokio::join!(server, client);
    assert!(matches!(server_result, Err(Error::NotPaired)));
}

#[tokio::test]
async fn connecting_to_the_wrong_machine_fails() {
    // The initiator encrypts to the key it expects, so reaching a different
    // machine cannot produce a session even if that machine would have you.
    let (a, b, other) = (
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
        Identity::generate().unwrap(),
    );
    let (peer_b, peer_a) = pair(&a, &b).await;

    let mut trust = Trust::new();
    trust.add(peer_a.public_key.clone(), "machine-a");

    let (l, addr) = listener().await;
    // `other` answers, but the client is expecting `b`.
    let server = async {
        let (sock, _) = l.accept().await.unwrap();
        Session::accept(sock, &other, &trust).await
    };
    let client = async { Session::connect(addr, &a, &peer_b).await };
    let (_, client_result) = tokio::join!(server, client);
    assert!(client_result.is_err(), "connected to the wrong machine");
}

#[tokio::test]
async fn one_end_refusing_leaves_nothing_paired() {
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (pa, pb) = shake(&a, &b).await;
    let (confirmed, _) = tokio::join!(pa.confirm(), pb.reject());
    assert!(
        matches!(confirmed, Err(Error::NotPaired)),
        "one side accepted while the other refused: {confirmed:?}"
    );
}

#[tokio::test]
async fn a_message_larger_than_one_record_arrives_whole() {
    // Encryption works in records well under the protocol's frame limit, so a
    // large message is split. Nothing above may have to know that.
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (peer_b, peer_a) = pair(&a, &b).await;
    let mut trust = Trust::new();
    trust.add(peer_a.public_key.clone(), "machine-a");

    let big = ServerControl::SyncKeys {
        pressed: (0..40_000)
            .map(|i| Key(0x8000 | (i as u16 & 0x7FFF)))
            .collect(),
        buttons: vec![],
    };
    let expected = big.clone();

    let (l, addr) = listener().await;
    let server = async {
        let (sock, _) = l.accept().await.unwrap();
        let mut link = Link::from(Session::accept(sock, &b, &trust).await.unwrap());
        link.recv::<ServerControl>().await.unwrap()
    };
    let client = async {
        let mut link = Link::from(Session::connect(addr, &a, &peer_b).await.unwrap());
        link.send(&big).await.unwrap();
        // Keep the connection up until the far side has read it.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    let (got, ()) = tokio::join!(server, client);
    assert_eq!(got, expected);
}

#[test]
fn an_identity_survives_a_restart() {
    let dir = std::env::temp_dir().join(format!("smkvm-identity-{}", std::process::id()));
    let path = dir.join("device.toml");
    let _ = std::fs::remove_dir_all(&dir);

    let first = Identity::load_or_create(&path).unwrap();
    let again = Identity::load_or_create(&path).unwrap();
    assert_eq!(first.id(), again.id(), "the machine changed identity");
    assert_eq!(first.public_key(), again.public_key());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the private key is readable by others");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_trust_store_round_trips() {
    let mut trust = Trust::new();
    let key = Identity::generate().unwrap().public_key().to_vec();
    let id = trust.add(key.clone(), "desk");
    assert_eq!(id, device_id(&key));

    let text = trust.to_toml();
    let back = Trust::parse(&text, std::path::Path::new("<test>")).unwrap();
    assert_eq!(back, trust);
    assert!(back.contains(id));
    assert_eq!(back.by_key(&key).unwrap().name, "desk");
}

#[test]
fn a_trust_store_whose_identifier_does_not_match_its_key_is_refused() {
    // The identifier in the file is a comment; the key is what counts. A file
    // that disagrees with itself has been meddled with.
    let key = Identity::generate().unwrap().public_key().to_vec();
    let mut trust = Trust::new();
    trust.add(key, "desk");
    let text = trust
        .to_toml()
        .replace(&trust.peers().next().unwrap().id.to_hex(), &"a".repeat(64));

    let result = Trust::parse(&text, std::path::Path::new("<test>"));
    assert!(
        matches!(result, Err(Error::BadTrustStore { .. })),
        "a tampered store was accepted: {result:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_machine_that_answers_and_then_says_nothing_does_not_hold_the_link() {
    // A firewall that drops packets rather than refusing them, or a peer that
    // accepts a connection and stalls, would otherwise leave the caller
    // waiting for ever -- and reconnect logic never runs, because the first
    // attempt has not finished failing.
    let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (peer_b, _) = pair(&a, &b).await;

    let (l, addr) = listener().await;
    let silent = async {
        let (socket, _) = l.accept().await.unwrap();
        // Hold it open and say nothing at all.
        std::future::pending::<()>().await;
        drop(socket);
    };
    let caller = async { Session::connect(addr, &a, &peer_b).await };

    tokio::select! {
        result = caller => {
            assert!(result.is_err(), "a silent peer produced a session");
        }
        _ = silent => unreachable!("the silent side never finishes"),
    }
}
