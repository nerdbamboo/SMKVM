//! Which messages may be lost when a machine stops keeping up.
//!
//! A server that stalls every machine behind the slowest one is worse than a
//! server that drops something, so something has to give. The question is what,
//! and the answer is narrow: only the messages a later one puts right.
//!
//! The test below matches on every variant rather than listing a few, so a new
//! message cannot be added without someone deciding which kind it is. Getting
//! that wrong is not a dropped frame -- it is a machine left believing the
//! cursor is somewhere it is not, with nothing on the way to correct it.

use smkvm_layout::{DeviceId, Point};
use smkvm_proto::{Hello, Key, MouseButton, Reject, Role, Scroll, ServerControl, PROTO_VERSION};

fn every_kind() -> Vec<ServerControl> {
    vec![
        ServerControl::Hello(Hello {
            proto: PROTO_VERSION,
            device: DeviceId::from_bytes([1; 32]),
            name: "somewhere".into(),
            role: Role::Server,
        }),
        ServerControl::Rejected {
            reason: Reject::NotPaired,
        },
        ServerControl::Enter {
            at: Point::new(10, 20),
            pressed: vec![Key(0x04)],
            buttons: vec![MouseButton::Left],
        },
        ServerControl::Leave,
        ServerControl::MoveTo { x: 10, y: 20 },
        ServerControl::Button {
            button: MouseButton::Left,
            down: true,
        },
        ServerControl::Wheel(Scroll::new(0, Scroll::NOTCH)),
        ServerControl::KeyEvent {
            key: Key(0x04),
            down: true,
            repeat: false,
        },
        ServerControl::SyncKeys {
            pressed: Vec::new(),
            buttons: Vec::new(),
        },
        ServerControl::ReleaseAll,
        ServerControl::Ping { id: 1 },
        ServerControl::Pong { id: 1 },
        ServerControl::Goodbye,
    ]
}

#[test]
fn only_the_messages_a_later_one_repeats_may_be_lost() {
    for msg in every_kind() {
        // Exhaustive on purpose: a new variant must be classified here before
        // it can reach a machine that is behind.
        let expected = match &msg {
            // The next position supersedes this one, and a stuttered scroll
            // beats one that arrives a second late.
            ServerControl::MoveTo { .. } | ServerControl::Wheel(_) => true,

            // A machine that misses this goes on believing the cursor is
            // elsewhere and discards everything sent afterwards.
            ServerControl::Enter { .. } | ServerControl::Leave => false,
            // A press with no release is a key held down on a machine nobody
            // is looking at.
            ServerControl::Button { .. } | ServerControl::KeyEvent { .. } => false,
            // The two messages whose whole purpose is to put a machine back to
            // a known state.
            ServerControl::SyncKeys { .. } | ServerControl::ReleaseAll => false,
            // Liveness: a lost one is read as a machine that has gone.
            ServerControl::Ping { .. } | ServerControl::Pong { .. } => false,
            // Handshake and shutdown happen once and are never repeated.
            ServerControl::Hello(_) | ServerControl::Rejected { .. } | ServerControl::Goodbye => {
                false
            }
        };
        assert_eq!(
            msg.may_be_dropped(),
            expected,
            "wrong call on whether this may be lost: {msg:?}"
        );
    }
}

#[test]
fn losing_the_cursor_arriving_is_never_acceptable() {
    // Singled out because it is the one that produces a cursor that crossed
    // and then vanished: the server swallows the keyboard on the machine's
    // behalf while the machine itself quietly throws away everything sent.
    let enter = ServerControl::Enter {
        at: Point::new(0, 0),
        pressed: Vec::new(),
        buttons: Vec::new(),
    };
    assert!(!enter.may_be_dropped());
    assert!(!ServerControl::Leave.may_be_dropped());
}
