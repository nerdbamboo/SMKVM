//! A server and a client wired together.
//!
//! Messages are handed straight from one to the other rather than through a
//! socket, and the client injects into a recording backend rather than a
//! screen. What is left is the behaviour: the cursor crosses, input lands on
//! the far machine, and what is held gets let go of at the right moments.

use std::time::Instant;

use smkvm_core::{Action, Client, ClientAction, Event, Server, Settings};
use smkvm_input::platform::loopback::{Event as Injected, Loopback};
use smkvm_input::Inject;
use smkvm_layout::{DeviceId, Layout, Monitor, Point, Rect};
use smkvm_proto::{ClientControl, Key, MouseButton, Scroll, SuspendReason};

const A: Key = Key(0x04);

fn dev(n: u8) -> DeviceId {
    DeviceId::from_bytes([n; 32])
}

/// The server's screen on the left, the client's on the right, joined at
/// global x = 1920.
struct Wired {
    server: Server,
    client: Client<Loopback>,
    client_id: DeviceId,
}

impl Wired {
    fn new() -> Self {
        let (local, client_id) = (dev(1), dev(2));
        let mut layout = Layout::new(Default::default());
        layout.report_monitors(
            local,
            "server",
            vec![Monitor::new("m0", Rect::new(0, 0, 1920, 1080))],
        );
        layout.place(local, &"m0".into(), Point::new(0, 0));

        let mut wired = Wired {
            server: Server::new(local, "server", layout, Settings::default()),
            client: Client::new(Loopback::single_screen()),
            client_id,
        };

        // The client connects and says what it has, exactly as it would over a
        // real link.
        wired.feed(Event::ClientUp {
            device: client_id,
            name: "client".into(),
        });
        wired.feed(Event::ClientMonitors {
            device: client_id,
            monitors: vec![Monitor::new("m0", Rect::new(0, 0, 1920, 1080))],
        });
        // Put it to the right of the server rather than wherever automatic
        // placement chose, so the geometry in the tests is predictable.
        let layout = wired.server.layout_mut();
        layout.place(client_id, &"m0".into(), Point::new(1920, 0));
        wired.client.input_mut().inner_mut().clear();
        wired
    }

    /// Run one event through the server, delivering anything it produces.
    fn feed(&mut self, event: Event) {
        let actions = self.server.handle(event, Instant::now());
        for action in actions {
            let Action::Send { to, msg } = action else {
                continue;
            };
            if to != self.client_id {
                continue;
            }
            for reply in self.client.handle(msg) {
                let ClientAction::Send(reply) = reply;
                match reply {
                    ClientControl::Suspended { reason } => self.later(Event::ClientSuspended {
                        device: self.client_id,
                        reason,
                    }),
                    ClientControl::Resumed => self.later(Event::ClientResumed {
                        device: self.client_id,
                    }),
                    _ => {}
                }
            }
        }
    }

    /// Feed something back to the server without recursing into delivery.
    fn later(&mut self, event: Event) {
        let _ = self.server.handle(event, Instant::now());
    }

    fn cross_to_client(&mut self) {
        self.feed(Event::PointerBy { dx: 960, dy: 540 });
        self.feed(Event::PointerBy { dx: 1000, dy: 0 });
        assert_eq!(self.server.active(), self.client_id, "did not cross");
    }

    fn injected(&self) -> Vec<Injected> {
        self.client.input().inner().actions()
    }

    fn clear(&mut self) {
        self.client.input_mut().inner_mut().clear();
    }
}

#[test]
fn the_cursor_crosses_and_lands_on_the_other_machine() {
    let mut w = Wired::new();
    w.cross_to_client();

    assert!(w.client.is_active());
    assert_eq!(
        w.injected().first(),
        Some(&Injected::MoveTo { x: 40, y: 540 }),
        "arriving must put the pointer where the server says: {:?}",
        w.injected()
    );
}

#[test]
fn typing_reaches_the_machine_holding_the_cursor() {
    let mut w = Wired::new();
    w.cross_to_client();
    w.clear();

    w.feed(Event::Key {
        key: A,
        down: true,
        repeat: false,
    });
    w.feed(Event::Key {
        key: A,
        down: false,
        repeat: false,
    });
    w.feed(Event::Button {
        button: MouseButton::Left,
        down: true,
    });
    w.feed(Event::Button {
        button: MouseButton::Left,
        down: false,
    });
    w.feed(Event::Wheel(Scroll::new(0, -Scroll::NOTCH)));
    w.feed(Event::PointerBy { dx: 10, dy: 10 });

    let injected = w.injected();
    assert!(injected.contains(&Injected::Key { key: A, down: true }));
    assert!(injected.contains(&Injected::Key {
        key: A,
        down: false
    }));
    assert!(injected.contains(&Injected::Button {
        button: MouseButton::Left,
        down: true
    }));
    assert!(injected.contains(&Injected::Wheel {
        dx: 0,
        dy: -Scroll::NOTCH
    }));
    assert!(injected
        .iter()
        .any(|e| matches!(e, Injected::MoveTo { .. })));
}

#[test]
fn a_chord_begun_here_continues_there() {
    let mut w = Wired::new();
    w.feed(Event::PointerBy { dx: 960, dy: 540 });

    // Ctrl goes down on the server, then the cursor leaves mid-chord.
    w.feed(Event::Key {
        key: Key::LEFT_CTRL,
        down: true,
        repeat: false,
    });
    w.clear();
    w.feed(Event::PointerBy { dx: 1000, dy: 0 });

    assert_eq!(
        w.client.input().held_keys(),
        vec![Key::LEFT_CTRL],
        "the modifier must arrive with the cursor"
    );
    // And the key that completes the chord lands modified.
    w.feed(Event::Key {
        key: A,
        down: true,
        repeat: false,
    });
    assert_eq!(w.client.input().held_keys(), vec![A, Key::LEFT_CTRL]);
}

#[test]
fn leaving_makes_the_client_let_go_of_everything() {
    let mut w = Wired::new();
    w.cross_to_client();
    w.feed(Event::Key {
        key: Key::LEFT_ALT,
        down: true,
        repeat: false,
    });
    w.feed(Event::Button {
        button: MouseButton::Left,
        down: true,
    });
    assert!(w.client.input().is_holding_anything());

    w.feed(Event::PointerBy { dx: -2000, dy: 0 });

    assert!(!w.client.is_active());
    assert!(
        !w.client.input().is_holding_anything(),
        "the machine left behind is still holding {:?}",
        w.client.input().held_keys()
    );
}

#[test]
fn a_dropped_link_mid_chord_does_not_strand_a_key() {
    // Nothing can arrive to release what is held, so the client must do it
    // on its own. This is the commonest way a key gets stranded.
    let mut w = Wired::new();
    w.cross_to_client();
    w.feed(Event::Key {
        key: Key::LEFT_ALT,
        down: true,
        repeat: false,
    });
    w.feed(Event::Key {
        key: Key(0x2B),
        down: true,
        repeat: false,
    });
    assert_eq!(w.client.input().held_keys().len(), 2);

    w.client.disconnected();
    w.feed(Event::ClientDown {
        device: w.client_id,
    });

    assert!(!w.client.input().is_holding_anything());
    assert!(!w.client.is_active());
    // And the server has the cursor back.
    assert_ne!(w.server.active(), w.client_id);
}

#[test]
fn a_client_that_cannot_act_says_so_and_the_cursor_comes_home() {
    // The Windows secure-desktop case, end to end.
    let mut w = Wired::new();
    w.cross_to_client();

    let replies = w.client.suspend(SuspendReason::SecureDesktop);
    assert_eq!(
        replies,
        vec![ClientAction::Send(ClientControl::Suspended {
            reason: SuspendReason::SecureDesktop
        })]
    );
    w.feed(Event::ClientSuspended {
        device: w.client_id,
        reason: SuspendReason::SecureDesktop,
    });

    assert_ne!(w.server.active(), w.client_id, "the cursor came home");
    assert!(!w.client.input().is_holding_anything());

    // Pushing at the edge does not send it back while the prompt is up.
    w.feed(Event::PointerBy { dx: 5000, dy: 0 });
    assert_ne!(w.server.active(), w.client_id);

    // Once the prompt is gone it is reachable again.
    w.client.resume();
    w.feed(Event::ClientResumed {
        device: w.client_id,
    });
    w.feed(Event::PointerBy { dx: 5000, dy: 0 });
    assert_eq!(w.server.active(), w.client_id);
}

#[test]
fn input_aimed_at_a_suspended_client_is_not_injected() {
    let mut w = Wired::new();
    w.cross_to_client();
    w.client.suspend(SuspendReason::Locked);
    w.clear();

    // Even if the server were to keep sending, nothing may reach the screen.
    w.client
        .handle(smkvm_proto::ServerControl::MoveTo { x: 5, y: 5 });
    w.client.handle(smkvm_proto::ServerControl::KeyEvent {
        key: A,
        down: true,
        repeat: false,
    });
    assert!(w.injected().is_empty(), "{:?}", w.injected());

    // But being told to let go is always obeyed.
    w.client.input_mut().inner_mut().key(A, true).unwrap();
    w.client.handle(smkvm_proto::ServerControl::ReleaseAll);
    assert!(!w.client.input().is_holding_anything());
}

#[test]
fn a_ping_is_answered() {
    let mut w = Wired::new();
    assert_eq!(
        w.client.handle(smkvm_proto::ServerControl::Ping { id: 7 }),
        vec![ClientAction::Send(ClientControl::Pong { id: 7 })]
    );
}
