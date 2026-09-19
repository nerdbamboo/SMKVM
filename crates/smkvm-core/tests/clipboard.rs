//! The clipboard travelling between machines, with the server as the hub.
//!
//! No sockets and no clipboards: offers, requests and chunks are handed from
//! one exchange to another by the test, which is what lets the awkward parts
//! -- a paste on a machine two hops from the copy, a machine vanishing with a
//! fetch in flight, a copy that supersedes an offer mid-transfer -- be pinned
//! down exactly.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use smkvm_core::exchange::{formats_from_names, Input, Output};
use smkvm_core::Exchange;
use smkvm_layout::DeviceId;
use smkvm_proto::{Bulk, ClipError, ClipFormat, ClipSeq, MAX_CHUNK_DATA};

fn dev(n: u8) -> DeviceId {
    DeviceId::from_bytes([n; 32])
}

const SERVER: u8 = 1;
const A: u8 = 2;
const B: u8 = 3;

const ALL: &[ClipFormat] = &[
    ClipFormat::Text,
    ClipFormat::Html,
    ClipFormat::Png,
    ClipFormat::Uris,
];

const LIMIT: u64 = 64 * 1024 * 1024;

/// What one machine did that was not a message to another.
#[derive(Debug, Default)]
struct Local {
    offered: Vec<(ClipSeq, Vec<ClipFormat>)>,
    released: u32,
    reads: Vec<(ClipSeq, ClipFormat)>,
    delivered: Vec<(u64, Result<Vec<u8>, ClipError>)>,
}

/// A server and two clients, with every message between them delivered in
/// order until nothing is left to deliver.
struct Desk {
    machines: Vec<(DeviceId, Exchange, Local)>,
    /// Messages in flight: (from, to, message).
    wire: VecDeque<(DeviceId, DeviceId, Bulk)>,
    /// What each machine's clipboard would hand over when read.
    contents: Vec<(DeviceId, ClipFormat, Vec<u8>)>,
    now: Instant,
}

impl Desk {
    fn new() -> Desk {
        let mut desk = Desk {
            machines: [SERVER, A, B]
                .iter()
                .map(|n| {
                    (
                        dev(*n),
                        Exchange::new(dev(*n), ALL.to_vec(), LIMIT),
                        Local::default(),
                    )
                })
                .collect(),
            wire: VecDeque::new(),
            contents: Vec::new(),
            now: Instant::now(),
        };
        // The clients connect to the server and the server sees them arrive.
        for client in [A, B] {
            desk.feed(SERVER, Input::PeerUp(dev(client)));
            desk.feed(client, Input::PeerUp(dev(SERVER)));
        }
        desk
    }

    fn feed(&mut self, who: u8, input: Input) {
        let now = self.now;
        let (id, exchange, local) = self
            .machines
            .iter_mut()
            .find(|(d, _, _)| *d == dev(who))
            .expect("a known machine");
        let id = *id;
        for output in exchange.handle(input, now) {
            match output {
                Output::Send { to, msg } => self.wire.push_back((id, to, msg)),
                Output::OfferLocally { seq, formats } => local.offered.push((seq, formats)),
                Output::ReleaseLocally => local.released += 1,
                Output::ReadLocal { seq, format } => local.reads.push((seq, format)),
                Output::Deliver { ticket, result } => local.delivered.push((ticket, result)),
            }
        }
        self.settle();
    }

    /// Deliver everything in flight, and answer every local read, until the
    /// desk is quiet.
    fn settle(&mut self) {
        loop {
            if let Some((from, to, msg)) = self.wire.pop_front() {
                let now = self.now;
                let (_, exchange, local) = self
                    .machines
                    .iter_mut()
                    .find(|(d, _, _)| *d == to)
                    .expect("a known machine");
                for output in exchange.handle(Input::FromPeer { from, msg }, now) {
                    match output {
                        Output::Send { to: next, msg } => self.wire.push_back((to, next, msg)),
                        Output::OfferLocally { seq, formats } => local.offered.push((seq, formats)),
                        Output::ReleaseLocally => local.released += 1,
                        Output::ReadLocal { seq, format } => local.reads.push((seq, format)),
                        Output::Deliver { ticket, result } => {
                            local.delivered.push((ticket, result))
                        }
                    }
                }
                continue;
            }
            // Reads are answered from what each machine's clipboard holds.
            let pending = self
                .machines
                .iter_mut()
                .find_map(|(d, _, local)| local.reads.pop().map(|r| (*d, r)));
            let Some((who, (seq, format))) = pending else {
                return;
            };
            let result = self
                .contents
                .iter()
                .find(|(d, f, _)| *d == who && *f == format)
                .map(|(_, _, bytes)| bytes.clone())
                .ok_or(ClipError::OwnerRefused);
            let now = self.now;
            let (_, exchange, local) = self
                .machines
                .iter_mut()
                .find(|(d, _, _)| *d == who)
                .expect("a known machine");
            for output in exchange.handle(
                Input::ReadDone {
                    seq,
                    format,
                    result,
                },
                now,
            ) {
                match output {
                    Output::Send { to, msg } => self.wire.push_back((who, to, msg)),
                    Output::OfferLocally { seq, formats } => local.offered.push((seq, formats)),
                    Output::ReleaseLocally => local.released += 1,
                    Output::ReadLocal { seq, format } => local.reads.push((seq, format)),
                    Output::Deliver { ticket, result } => local.delivered.push((ticket, result)),
                }
            }
        }
    }

    fn local(&self, who: u8) -> &Local {
        &self
            .machines
            .iter()
            .find(|(d, _, _)| *d == dev(who))
            .expect("a known machine")
            .2
    }

    fn local_mut(&mut self, who: u8) -> &mut Local {
        &mut self
            .machines
            .iter_mut()
            .find(|(d, _, _)| *d == dev(who))
            .expect("a known machine")
            .2
    }

    fn exchange(&self, who: u8) -> &Exchange {
        &self
            .machines
            .iter()
            .find(|(d, _, _)| *d == dev(who))
            .expect("a known machine")
            .1
    }

    /// `who` copies `contents`.
    fn copy(&mut self, who: u8, contents: Vec<(ClipFormat, Vec<u8>)>) {
        self.contents.retain(|(d, _, _)| *d != dev(who));
        let formats: Vec<ClipFormat> = contents.iter().map(|(f, _)| f.clone()).collect();
        for (format, bytes) in contents {
            self.contents.push((dev(who), format, bytes));
        }
        // The copy happened after any offer that was placed earlier.
        self.now += Duration::from_secs(2);
        self.feed(who, Input::LocalChanged(formats));
    }

    /// The offer `who` currently holds from elsewhere.
    fn held(&self, who: u8) -> ClipSeq {
        self.exchange(who)
            .held_offer()
            .unwrap_or_else(|| panic!("{who} holds nothing"))
    }

    /// Something on `who` pastes `format`, and this is what it gets.
    fn paste(&mut self, who: u8, format: ClipFormat) -> Result<Vec<u8>, ClipError> {
        let seq = self.held(who);
        let ticket = 7000 + u64::from(who);
        self.feed(
            who,
            Input::Wanted {
                ticket,
                seq,
                format,
            },
        );
        let delivered = &mut self.local_mut(who).delivered;
        let at = delivered
            .iter()
            .position(|(t, _)| *t == ticket)
            .expect("something was delivered");
        delivered.remove(at).1
    }
}

#[test]
fn a_copy_on_a_client_is_offered_on_every_other_machine() {
    let mut desk = Desk::new();
    desk.copy(A, vec![(ClipFormat::Text, b"hello".to_vec())]);

    let seq = desk.exchange(A).local_offer().expect("A is offering");
    assert_eq!(seq.device, dev(A));
    assert_eq!(desk.held(SERVER), seq, "the server holds A's offer");
    assert_eq!(desk.held(B), seq, "and passed it on to B");
    assert_eq!(
        desk.local(SERVER).offered.last().unwrap().1,
        vec![ClipFormat::Text]
    );
    assert_eq!(
        desk.local(B).offered.last().unwrap().1,
        vec![ClipFormat::Text]
    );
    assert!(
        desk.local(A).offered.is_empty(),
        "the machine that copied does not get its own clipboard back"
    );
}

#[test]
fn a_paste_two_hops_from_the_copy_fetches_through_the_server() {
    let mut desk = Desk::new();
    desk.copy(A, vec![(ClipFormat::Text, b"hello".to_vec())]);

    assert_eq!(desk.paste(B, ClipFormat::Text), Ok(b"hello".to_vec()));
    assert_eq!(
        desk.local(A).reads.len(),
        0,
        "reads are answered as they are asked, so none is left over"
    );
    // A second paste is served from what the server already fetched.
    let before = desk.wire.len();
    assert_eq!(desk.paste(B, ClipFormat::Text), Ok(b"hello".to_vec()));
    assert_eq!(desk.wire.len(), before);
}

#[test]
fn a_copy_on_the_server_reaches_both_clients_and_pastes_on_either() {
    let mut desk = Desk::new();
    desk.copy(SERVER, vec![(ClipFormat::Html, b"<b>x</b>".to_vec())]);
    assert_eq!(desk.paste(A, ClipFormat::Html), Ok(b"<b>x</b>".to_vec()));
    assert_eq!(desk.paste(B, ClipFormat::Html), Ok(b"<b>x</b>".to_vec()));
}

#[test]
fn something_larger_than_one_chunk_arrives_whole_and_one_chunk_at_a_time() {
    let mut desk = Desk::new();
    let big: Vec<u8> = (0..(MAX_CHUNK_DATA * 3 + 17) as u32)
        .map(|i| (i % 251) as u8)
        .collect();
    desk.copy(A, vec![(ClipFormat::Png, big.clone())]);

    // Watch the wire as B fetches: after every chunk the next request goes
    // out, and never more than one chunk is outstanding.
    let seq = desk.held(B);
    let mut exchange_b = Exchange::new(dev(B), ALL.to_vec(), LIMIT);
    exchange_b.handle(Input::PeerUp(dev(SERVER)), desk.now);
    let offer = Bulk::ClipOffer(smkvm_proto::ClipOffer {
        seq,
        entries: vec![smkvm_proto::ClipEntry {
            format: ClipFormat::Png,
            bytes: None,
            hash: None,
        }],
    });
    exchange_b.handle(
        Input::FromPeer {
            from: dev(SERVER),
            msg: offer,
        },
        desk.now,
    );
    let first = exchange_b.handle(
        Input::Wanted {
            ticket: 1,
            seq,
            format: ClipFormat::Png,
        },
        desk.now,
    );
    assert_eq!(
        first,
        vec![Output::Send {
            to: dev(SERVER),
            msg: Bulk::ClipRequest {
                seq,
                format: ClipFormat::Png,
                offset: 0
            }
        }]
    );
    let mut offset = 0usize;
    loop {
        let end = (offset + MAX_CHUNK_DATA).min(big.len());
        let last = end >= big.len();
        let out = exchange_b.handle(
            Input::FromPeer {
                from: dev(SERVER),
                msg: Bulk::ClipChunk {
                    seq,
                    format: ClipFormat::Png,
                    offset: offset as u64,
                    data: big[offset..end].to_vec(),
                    last,
                },
            },
            desk.now,
        );
        if last {
            assert_eq!(
                out,
                vec![Output::Deliver {
                    ticket: 1,
                    result: Ok(big.clone())
                }]
            );
            break;
        }
        assert_eq!(
            out,
            vec![Output::Send {
                to: dev(SERVER),
                msg: Bulk::ClipRequest {
                    seq,
                    format: ClipFormat::Png,
                    offset: end as u64
                }
            }],
            "exactly one more chunk is asked for"
        );
        offset = end;
    }

    // And end to end through the hub it arrives intact.
    assert_eq!(desk.paste(B, ClipFormat::Png), Ok(big));
}

#[test]
fn a_new_copy_makes_the_old_offer_stale() {
    let mut desk = Desk::new();
    desk.copy(A, vec![(ClipFormat::Text, b"first".to_vec())]);
    let old = desk.held(B);
    desk.copy(A, vec![(ClipFormat::Text, b"second".to_vec())]);
    assert_ne!(desk.held(B), old);

    // Somebody pasting the old one is told so rather than given the new one.
    desk.feed(
        B,
        Input::Wanted {
            ticket: 99,
            seq: old,
            format: ClipFormat::Text,
        },
    );
    assert_eq!(
        desk.local(B).delivered.last(),
        Some(&(99, Err(ClipError::Stale)))
    );
    assert_eq!(desk.paste(B, ClipFormat::Text), Ok(b"second".to_vec()));
}

#[test]
fn a_copy_elsewhere_replaces_what_this_machine_was_offering() {
    let mut desk = Desk::new();
    desk.copy(A, vec![(ClipFormat::Text, b"from a".to_vec())]);
    desk.copy(B, vec![(ClipFormat::Text, b"from b".to_vec())]);

    assert_eq!(desk.exchange(A).local_offer(), None, "A's offer is over");
    assert_eq!(desk.held(A).device, dev(B));
    assert_eq!(desk.paste(A, ClipFormat::Text), Ok(b"from b".to_vec()));
}

#[test]
fn the_offer_coming_back_as_a_change_is_not_a_new_copy() {
    // Putting an offer on the clipboard changes the clipboard, and the watcher
    // may well say so. Taking that as a copy would send the offer straight
    // back where it came from, and round again.
    let mut desk = Desk::new();
    desk.copy(A, vec![(ClipFormat::Text, b"hello".to_vec())]);
    let seq = desk.held(B);

    desk.now += Duration::from_millis(50);
    desk.feed(B, Input::LocalChanged(vec![ClipFormat::Text]));

    assert_eq!(
        desk.exchange(B).local_offer(),
        None,
        "B did not start offering"
    );
    assert_eq!(desk.held(B), seq, "and still holds A's offer");
}

#[test]
fn a_real_copy_a_little_later_is_taken_as_one() {
    let mut desk = Desk::new();
    desk.copy(A, vec![(ClipFormat::Text, b"hello".to_vec())]);
    desk.copy(B, vec![(ClipFormat::Text, b"other".to_vec())]);
    assert!(desk.exchange(B).local_offer().is_some());
    assert_eq!(desk.paste(A, ClipFormat::Text), Ok(b"other".to_vec()));
}

#[test]
fn a_machine_arriving_is_given_the_current_offer() {
    let mut desk = Desk::new();
    desk.copy(A, vec![(ClipFormat::Text, b"hello".to_vec())]);
    // B goes and comes back.
    desk.feed(SERVER, Input::PeerGone(dev(B)));
    desk.feed(B, Input::PeerGone(dev(SERVER)));
    assert_eq!(
        desk.local(B).released,
        1,
        "with nowhere to fetch from, B lets go"
    );
    desk.feed(SERVER, Input::PeerUp(dev(B)));
    desk.feed(B, Input::PeerUp(dev(SERVER)));
    assert_eq!(desk.held(B).device, dev(A));
    assert_eq!(desk.paste(B, ClipFormat::Text), Ok(b"hello".to_vec()));
}

#[test]
fn a_machine_that_vanishes_mid_fetch_fails_the_paste_rather_than_hanging_it() {
    let mut desk = Desk::new();
    desk.copy(A, vec![(ClipFormat::Text, b"hello".to_vec())]);
    let seq = desk.held(SERVER);

    // The server starts fetching from A on its own behalf, but A's answer
    // never comes: A is gone.
    let mut server = Exchange::new(dev(SERVER), ALL.to_vec(), LIMIT);
    server.handle(Input::PeerUp(dev(A)), desk.now);
    server.handle(
        Input::FromPeer {
            from: dev(A),
            msg: Bulk::ClipOffer(smkvm_proto::ClipOffer {
                seq,
                entries: vec![smkvm_proto::ClipEntry {
                    format: ClipFormat::Text,
                    bytes: None,
                    hash: None,
                }],
            }),
        },
        desk.now,
    );
    let asked = server.handle(
        Input::Wanted {
            ticket: 5,
            seq,
            format: ClipFormat::Text,
        },
        desk.now,
    );
    assert!(matches!(asked[..], [Output::Send { .. }]));
    let gone = server.handle(Input::PeerGone(dev(A)), desk.now);
    assert!(gone.contains(&Output::Deliver {
        ticket: 5,
        result: Err(ClipError::Cancelled)
    }));
    assert!(gone.contains(&Output::ReleaseLocally));
}

#[test]
fn contents_over_the_limit_are_refused_and_said_so() {
    let mut desk = Desk::new();
    for (_, exchange, _) in &mut desk.machines {
        exchange.set_max_bytes(1000);
    }
    desk.copy(A, vec![(ClipFormat::Text, vec![b'x'; 5000])]);
    let got = desk.paste(B, ClipFormat::Text);
    assert_eq!(
        got,
        Err(ClipError::TooLarge {
            bytes: 5000,
            limit: 1000
        })
    );
}

#[test]
fn formats_not_shared_are_left_out_of_the_offer() {
    let mut desk = Desk::new();
    for (_, exchange, _) in &mut desk.machines {
        exchange.set_allowed(vec![ClipFormat::Text]);
    }
    desk.copy(
        A,
        vec![
            (ClipFormat::Text, b"t".to_vec()),
            (ClipFormat::Png, b"p".to_vec()),
        ],
    );
    assert_eq!(
        desk.local(B).offered.last().unwrap().1,
        vec![ClipFormat::Text]
    );
}

#[test]
fn sharing_switched_off_carries_nothing() {
    let mut desk = Desk::new();
    for (_, exchange, _) in &mut desk.machines {
        exchange.set_enabled(false);
    }
    desk.copy(A, vec![(ClipFormat::Text, b"t".to_vec())]);
    assert!(desk.exchange(A).local_offer().is_none());
    assert!(desk.exchange(SERVER).held_offer().is_none());
    assert!(desk.local(B).offered.is_empty());
}

#[test]
fn a_request_for_something_nobody_is_offering_is_answered_not_ignored() {
    let mut server = Exchange::new(dev(SERVER), ALL.to_vec(), LIMIT);
    server.handle(Input::PeerUp(dev(A)), Instant::now());
    let seq = ClipSeq {
        device: dev(B),
        counter: 9,
    };
    let out = server.handle(
        Input::FromPeer {
            from: dev(A),
            msg: Bulk::ClipRequest {
                seq,
                format: ClipFormat::Text,
                offset: 0,
            },
        },
        Instant::now(),
    );
    assert_eq!(
        out,
        vec![Output::Send {
            to: dev(A),
            msg: Bulk::ClipUnavailable {
                seq,
                format: ClipFormat::Text,
                reason: ClipError::Stale
            }
        }]
    );
}

#[test]
fn the_configuration_names_formats_in_plain_words() {
    let names: Vec<String> = ["text", "html", "image", "files", "bogus", "text"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(formats_from_names(&names), ALL.to_vec());
    assert!(formats_from_names(&[]).is_empty());
}

#[test]
fn a_change_notice_with_nothing_usable_in_it_leaves_what_is_held_alone() {
    // What the watcher reports when the clipboard's owner did not answer in
    // time, or when something was copied in a form this cannot carry. Seen on
    // the real machines: our own owner was busy fetching, the watcher gave up
    // asking it, and the empty report made the machine forget the offer it
    // was holding -- so the paste that had been waiting was refused as stale.
    let mut desk = Desk::new();
    desk.copy(A, vec![(ClipFormat::Text, b"hello".to_vec())]);
    let seq = desk.held(B);

    desk.now += Duration::from_secs(5);
    desk.feed(B, Input::LocalChanged(vec![]));
    assert_eq!(desk.held(B), seq, "B still holds A's offer");
    assert_eq!(desk.paste(B, ClipFormat::Text), Ok(b"hello".to_vec()));

    // The same on the machine that copied: its own offer stands.
    desk.now += Duration::from_secs(5);
    desk.feed(A, Input::LocalChanged(vec![]));
    assert_eq!(desk.exchange(A).local_offer(), Some(seq));
    assert_eq!(desk.paste(SERVER, ClipFormat::Text), Ok(b"hello".to_vec()));
}
