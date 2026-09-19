//! Moving files between machines, a chunk at a time.
//!
//! Files ride on the clipboard. Copying files puts a *manifest* -- names and
//! sizes, nothing more -- where the list of files would have gone, and the
//! clipboard exchange carries it like any other format. Only when something
//! pastes are the files themselves pulled, one chunk per request, from the
//! machine that has them, and written where they land. A folder copied and
//! never pasted costs one small message.
//!
//! Like the exchange this is a hub-and-spoke: a client speaks only to the
//! server, and the server passes requests on to the machine named in the
//! transfer's id and chunks back to whoever asked. Nothing here touches a
//! file or a socket; that is the caller's, in answer to the outputs.

use std::collections::{BTreeSet, HashMap, HashSet};

use smkvm_layout::DeviceId;
use smkvm_proto::{Bulk, TransferId};

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    PeerUp(DeviceId),
    PeerGone(DeviceId),
    /// A file message from a machine.
    FromPeer {
        from: DeviceId,
        msg: Bulk,
    },
    /// This machine wants the chunk of file `index` of transfer `id` that
    /// begins at `offset`. The answer comes as [`Output::Chunk`].
    Pull {
        id: TransferId,
        index: u32,
        offset: u64,
    },
    /// This machine has lost interest in a transfer it was pulling.
    Drop(TransferId),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Send {
        to: DeviceId,
        msg: Bulk,
    },
    /// Read this chunk of a file this machine offered, and send it to `to`
    /// as a [`Bulk::FileChunk`] -- or a [`Bulk::FileAbort`] if it cannot.
    ReadFile {
        to: DeviceId,
        id: TransferId,
        index: u32,
        offset: u64,
    },
    /// A chunk this machine asked for has arrived.
    Chunk {
        id: TransferId,
        index: u32,
        offset: u64,
        data: Vec<u8>,
        last: bool,
    },
    /// A transfer this machine was pulling cannot be completed.
    Failed(TransferId),
}

pub struct Transfer {
    me: DeviceId,
    /// The one machine a client talks to. A hub has none and talks to the
    /// machine each transfer's id names.
    hub: Option<DeviceId>,
    peers: BTreeSet<DeviceId>,
    /// Requests passed on for another machine, remembered so the chunk that
    /// answers each can be passed back to the one that asked.
    relays: HashMap<(TransferId, u32, u64), DeviceId>,
    /// Transfers this machine is pulling, and where each is coming from.
    pulling: HashMap<TransferId, DeviceId>,
    /// The chunks outstanding on each.
    awaiting: HashSet<(TransferId, u32, u64)>,
}

impl Transfer {
    /// `hub` is the server, for a client; a server has none.
    pub fn new(me: DeviceId, hub: Option<DeviceId>) -> Transfer {
        Transfer {
            me,
            hub,
            peers: BTreeSet::new(),
            relays: HashMap::new(),
            pulling: HashMap::new(),
            awaiting: HashSet::new(),
        }
    }

    pub fn set_hub(&mut self, hub: Option<DeviceId>) {
        self.hub = hub;
    }

    pub fn handle(&mut self, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        match input {
            Input::PeerUp(peer) => {
                self.peers.insert(peer);
            }
            Input::PeerGone(peer) => self.peer_gone(peer, &mut out),
            Input::FromPeer { from, msg } => self.peer_said(from, msg, &mut out),
            Input::Pull { id, index, offset } => self.pull(id, index, offset, &mut out),
            Input::Drop(id) => {
                self.pulling.remove(&id);
                self.awaiting.retain(|(i, _, _)| *i != id);
            }
        }
        out
    }

    /// Where a request about this transfer goes from here.
    fn source_of(&self, id: TransferId) -> Option<DeviceId> {
        match self.hub {
            Some(hub) => Some(hub),
            None if self.peers.contains(&id.device) => Some(id.device),
            None => None,
        }
    }

    fn pull(&mut self, id: TransferId, index: u32, offset: u64, out: &mut Vec<Output>) {
        let Some(source) = self.source_of(id) else {
            out.push(Output::Failed(id));
            return;
        };
        self.pulling.insert(id, source);
        self.awaiting.insert((id, index, offset));
        out.push(Output::Send {
            to: source,
            msg: Bulk::FileRequest { id, index, offset },
        });
    }

    fn peer_said(&mut self, from: DeviceId, msg: Bulk, out: &mut Vec<Output>) {
        match msg {
            Bulk::FileRequest { id, index, offset } => {
                if id.device == self.me {
                    out.push(Output::ReadFile {
                        to: from,
                        id,
                        index,
                        offset,
                    });
                    return;
                }
                // Not ours: pass it on, if the machine that has it is here.
                match self.source_of(id).filter(|s| *s != from) {
                    Some(owner) => {
                        self.relays.insert((id, index, offset), from);
                        out.push(Output::Send {
                            to: owner,
                            msg: Bulk::FileRequest { id, index, offset },
                        });
                    }
                    None => out.push(Output::Send {
                        to: from,
                        msg: Bulk::FileAbort {
                            id,
                            reason: "the machine holding the files is not connected here".into(),
                        },
                    }),
                }
            }
            Bulk::FileChunk {
                id,
                index,
                offset,
                data,
                last,
            } => {
                if let Some(to) = self.relays.remove(&(id, index, offset)) {
                    out.push(Output::Send {
                        to,
                        msg: Bulk::FileChunk {
                            id,
                            index,
                            offset,
                            data,
                            last,
                        },
                    });
                    return;
                }
                // Only a chunk this machine asked for, from where it asked.
                if self.pulling.get(&id) == Some(&from)
                    && self.awaiting.remove(&(id, index, offset))
                {
                    out.push(Output::Chunk {
                        id,
                        index,
                        offset,
                        data,
                        last,
                    });
                }
            }
            Bulk::FileAbort { id, reason } => {
                // Everyone waiting on this transfer through here hears of it.
                let waiting: Vec<DeviceId> = {
                    let mut v: Vec<DeviceId> = self
                        .relays
                        .iter()
                        .filter(|((i, _, _), _)| *i == id)
                        .map(|(_, to)| *to)
                        .collect();
                    v.sort();
                    v.dedup();
                    v
                };
                self.relays.retain(|(i, _, _), _| *i != id);
                for to in waiting {
                    out.push(Output::Send {
                        to,
                        msg: Bulk::FileAbort {
                            id,
                            reason: reason.clone(),
                        },
                    });
                }
                if self.pulling.get(&id) == Some(&from) {
                    self.pulling.remove(&id);
                    self.awaiting.retain(|(i, _, _)| *i != id);
                    out.push(Output::Failed(id));
                }
            }
            // Offers travel inside the clipboard exchange; the rest is not
            // this machine's concern.
            _ => {}
        }
    }

    fn peer_gone(&mut self, peer: DeviceId, out: &mut Vec<Output>) {
        self.peers.remove(&peer);
        // Requests passed on to it will never be answered.
        let orphaned: Vec<(TransferId, DeviceId)> = self
            .relays
            .iter()
            .filter(|((id, _, _), _)| id.device == peer)
            .map(|((id, _, _), to)| (*id, *to))
            .collect();
        self.relays
            .retain(|(id, _, _), to| id.device != peer && *to != peer);
        let mut told = BTreeSet::new();
        for (id, to) in orphaned {
            if told.insert((id, to)) {
                out.push(Output::Send {
                    to,
                    msg: Bulk::FileAbort {
                        id,
                        reason: "the machine holding the files went away".into(),
                    },
                });
            }
        }
        // Pulls coming through it are over.
        let lost: Vec<TransferId> = self
            .pulling
            .iter()
            .filter(|(_, from)| **from == peer)
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            self.pulling.remove(&id);
            self.awaiting.retain(|(i, _, _)| *i != id);
            out.push(Output::Failed(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(n: u8) -> DeviceId {
        DeviceId::from_bytes([n; 32])
    }
    const SERVER: u8 = 1;
    const A: u8 = 2;
    const B: u8 = 3;

    fn id(owner: u8) -> TransferId {
        TransferId {
            device: dev(owner),
            counter: 7,
        }
    }

    fn hub_with(peers: &[u8]) -> Transfer {
        let mut hub = Transfer::new(dev(SERVER), None);
        for p in peers {
            hub.handle(Input::PeerUp(dev(*p)));
        }
        hub
    }

    #[test]
    fn a_client_asks_the_server_whoever_has_the_file() {
        let mut client = Transfer::new(dev(B), Some(dev(SERVER)));
        client.handle(Input::PeerUp(dev(SERVER)));
        let out = client.handle(Input::Pull {
            id: id(A),
            index: 0,
            offset: 0,
        });
        assert_eq!(
            out,
            vec![Output::Send {
                to: dev(SERVER),
                msg: Bulk::FileRequest {
                    id: id(A),
                    index: 0,
                    offset: 0
                }
            }]
        );
    }

    #[test]
    fn the_server_passes_a_request_to_the_owner_and_the_chunk_back() {
        let mut hub = hub_with(&[A, B]);
        let out = hub.handle(Input::FromPeer {
            from: dev(B),
            msg: Bulk::FileRequest {
                id: id(A),
                index: 2,
                offset: 4096,
            },
        });
        assert_eq!(
            out,
            vec![Output::Send {
                to: dev(A),
                msg: Bulk::FileRequest {
                    id: id(A),
                    index: 2,
                    offset: 4096
                }
            }]
        );
        let out = hub.handle(Input::FromPeer {
            from: dev(A),
            msg: Bulk::FileChunk {
                id: id(A),
                index: 2,
                offset: 4096,
                data: vec![9; 10],
                last: true,
            },
        });
        assert_eq!(
            out,
            vec![Output::Send {
                to: dev(B),
                msg: Bulk::FileChunk {
                    id: id(A),
                    index: 2,
                    offset: 4096,
                    data: vec![9; 10],
                    last: true
                }
            }]
        );
        // Answered once; a second copy of the chunk goes nowhere.
        let out = hub.handle(Input::FromPeer {
            from: dev(A),
            msg: Bulk::FileChunk {
                id: id(A),
                index: 2,
                offset: 4096,
                data: vec![9; 10],
                last: true,
            },
        });
        assert!(out.is_empty());
    }

    #[test]
    fn the_owner_is_asked_to_read_its_own_file() {
        let mut owner = Transfer::new(dev(A), Some(dev(SERVER)));
        let out = owner.handle(Input::FromPeer {
            from: dev(SERVER),
            msg: Bulk::FileRequest {
                id: id(A),
                index: 0,
                offset: 0,
            },
        });
        assert_eq!(
            out,
            vec![Output::ReadFile {
                to: dev(SERVER),
                id: id(A),
                index: 0,
                offset: 0
            }]
        );
    }

    #[test]
    fn a_chunk_this_machine_asked_for_is_handed_over_and_an_unasked_one_is_not() {
        let mut client = Transfer::new(dev(B), Some(dev(SERVER)));
        client.handle(Input::PeerUp(dev(SERVER)));
        client.handle(Input::Pull {
            id: id(A),
            index: 0,
            offset: 0,
        });
        let chunk = |offset: u64| Bulk::FileChunk {
            id: id(A),
            index: 0,
            offset,
            data: vec![1, 2, 3],
            last: false,
        };
        assert_eq!(
            client.handle(Input::FromPeer {
                from: dev(SERVER),
                msg: chunk(0)
            }),
            vec![Output::Chunk {
                id: id(A),
                index: 0,
                offset: 0,
                data: vec![1, 2, 3],
                last: false
            }]
        );
        // Wrong offset, wrong sender: neither is delivered.
        assert!(client
            .handle(Input::FromPeer {
                from: dev(SERVER),
                msg: chunk(3)
            })
            .is_empty());
        client.handle(Input::Pull {
            id: id(A),
            index: 0,
            offset: 3,
        });
        assert!(client
            .handle(Input::FromPeer {
                from: dev(A),
                msg: chunk(3)
            })
            .is_empty());
    }

    #[test]
    fn a_request_for_a_machine_that_is_not_here_is_refused_at_once() {
        let mut hub = hub_with(&[B]);
        let out = hub.handle(Input::FromPeer {
            from: dev(B),
            msg: Bulk::FileRequest {
                id: id(A),
                index: 0,
                offset: 0,
            },
        });
        assert!(matches!(
            out.as_slice(),
            [Output::Send {
                to,
                msg: Bulk::FileAbort { id: aborted, .. }
            }] if *to == dev(B) && *aborted == id(A)
        ));
    }

    #[test]
    fn the_owner_leaving_aborts_what_was_passed_on_and_fails_what_was_pulled() {
        let mut hub = hub_with(&[A, B]);
        hub.handle(Input::FromPeer {
            from: dev(B),
            msg: Bulk::FileRequest {
                id: id(A),
                index: 0,
                offset: 0,
            },
        });
        hub.handle(Input::FromPeer {
            from: dev(B),
            msg: Bulk::FileRequest {
                id: id(A),
                index: 1,
                offset: 0,
            },
        });
        // The server itself is pasting the same transfer.
        hub.handle(Input::Pull {
            id: id(A),
            index: 0,
            offset: 0,
        });
        let out = hub.handle(Input::PeerGone(dev(A)));
        assert_eq!(
            out.len(),
            2,
            "B is told once, and the server's own pull fails"
        );
        assert!(matches!(
            &out[0],
            Output::Send {
                to,
                msg: Bulk::FileAbort { id: aborted, .. }
            } if *to == dev(B) && *aborted == id(A)
        ));
        assert_eq!(out[1], Output::Failed(id(A)));
    }

    #[test]
    fn an_abort_from_the_owner_reaches_everyone_waiting() {
        let mut hub = hub_with(&[A, B]);
        hub.handle(Input::FromPeer {
            from: dev(B),
            msg: Bulk::FileRequest {
                id: id(A),
                index: 0,
                offset: 0,
            },
        });
        let out = hub.handle(Input::FromPeer {
            from: dev(A),
            msg: Bulk::FileAbort {
                id: id(A),
                reason: "gone".into(),
            },
        });
        assert_eq!(
            out,
            vec![Output::Send {
                to: dev(B),
                msg: Bulk::FileAbort {
                    id: id(A),
                    reason: "gone".into()
                }
            }],
            "the reason travels with it"
        );
        let mut client = Transfer::new(dev(B), Some(dev(SERVER)));
        client.handle(Input::PeerUp(dev(SERVER)));
        client.handle(Input::Pull {
            id: id(A),
            index: 0,
            offset: 0,
        });
        let out = client.handle(Input::FromPeer {
            from: dev(SERVER),
            msg: Bulk::FileAbort {
                id: id(A),
                reason: "gone".into(),
            },
        });
        assert_eq!(out, vec![Output::Failed(id(A))]);
    }
}
