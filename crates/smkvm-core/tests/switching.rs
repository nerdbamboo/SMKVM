//! Handing the cursor between machines.
//!
//! Everything here runs with no display server, no sockets and no real clock:
//! events go in with a timestamp and actions come out. That is what makes the
//! awkward moments testable — a modifier held across a crossing, a machine that
//! vanishes while it has the cursor, a screen edge with nothing usable beyond
//! it.

use std::time::{Duration, Instant};

use smkvm_core::{Action, Event, LocalAction, PointerMode, Server, Settings};
use smkvm_layout::{DeviceId, EdgeOverflow, Layout, Monitor, Point, Rect};
use smkvm_proto::{Key, MouseButton, ServerControl, SuspendReason};

const A: Key = Key(0x04);

fn dev(n: u8) -> DeviceId {
    DeviceId::from_bytes([n; 32])
}

/// The server's screen on the left, one client's on the right.
fn two_machines(settings: Settings) -> (Server, DeviceId, DeviceId) {
    let (local, client) = (dev(1), dev(2));
    let mut layout = Layout::new(settings.edge_overflow);
    layout.report_monitors(
        local,
        "server",
        vec![Monitor::new("m0", Rect::new(0, 0, 1920, 1080))],
    );
    layout.report_monitors(
        client,
        "client",
        vec![Monitor::new("m0", Rect::new(0, 0, 1920, 1080))],
    );
    layout.place(local, &"m0".into(), Point::new(0, 0));
    layout.place(client, &"m0".into(), Point::new(1920, 0));

    let mut server = Server::new(local, "server", layout, settings);
    // Until a machine finishes its handshake it has no session, however much
    // the layout knows about it.
    server.handle(
        Event::ClientUp {
            device: client,
            name: "client".into(),
        },
        Instant::now(),
    );
    (server, local, client)
}

fn now() -> Instant {
    Instant::now()
}

/// Move the pointer onto the middle of the server's own screen.
fn centre(server: &mut Server) {
    server.handle(Event::PointerBy { dx: 960, dy: 540 }, now());
    assert_eq!(server.cursor(), Point::new(960, 540));
}

fn sent_to(actions: &[Action], device: DeviceId) -> Vec<ServerControl> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send { to, msg } if *to == device => Some(msg.clone()),
            _ => None,
        })
        .collect()
}

fn local_actions(actions: &[Action]) -> Vec<LocalAction> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Local(l) => Some(l.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_cursor_starts_at_home() {
    let (server, local, _) = two_machines(Settings::default());
    assert_eq!(server.active(), local);
    assert_eq!(server.pointer_mode(), PointerMode::Local);
}

#[test]
fn moving_around_the_local_screen_tells_nobody() {
    let (mut server, _, _) = two_machines(Settings::default());
    let actions = server.handle(Event::PointerBy { dx: 500, dy: 300 }, now());
    assert!(
        actions.is_empty(),
        "the operating system is already moving the pointer: {actions:?}"
    );
    assert_eq!(server.cursor(), Point::new(500, 300));
}

#[test]
fn crossing_carries_the_input_state_with_it() {
    let (mut server, _, client) = two_machines(Settings::default());
    centre(&mut server);

    // A chord is in progress when the cursor leaves.
    server.handle(
        Event::Key {
            key: Key::LEFT_CTRL,
            down: true,
            repeat: false,
        },
        now(),
    );
    server.handle(
        Event::Button {
            button: MouseButton::Left,
            down: true,
        },
        now(),
    );

    let actions = server.handle(Event::PointerBy { dx: 1000, dy: 0 }, now());
    assert_eq!(server.active(), client);
    assert_eq!(server.pointer_mode(), PointerMode::Captured);

    assert_eq!(
        sent_to(&actions, client),
        vec![ServerControl::Enter {
            at: Point::new(40, 540),
            pressed: vec![Key::LEFT_CTRL],
            buttons: vec![MouseButton::Left],
        }],
        "what is held must arrive with the cursor, or the chord breaks"
    );
    assert_eq!(
        local_actions(&actions),
        vec![LocalAction::SetPointerMode(PointerMode::Captured)]
    );
}

#[test]
fn coming_back_releases_the_client_and_restores_local_control() {
    let (mut server, local, client) = two_machines(Settings::default());
    centre(&mut server);
    server.handle(Event::PointerBy { dx: 1000, dy: 0 }, now());
    assert_eq!(server.active(), client);

    let actions = server.handle(Event::PointerBy { dx: -200, dy: 0 }, now());
    assert_eq!(server.active(), local);
    assert_eq!(server.pointer_mode(), PointerMode::Local);
    assert_eq!(
        sent_to(&actions, client),
        vec![ServerControl::Leave],
        "the machine being left must be told to let go"
    );
    assert!(local_actions(&actions).contains(&LocalAction::SetPointerMode(PointerMode::Local)));
    assert!(local_actions(&actions)
        .iter()
        .any(|a| matches!(a, LocalAction::WarpCursor { .. })));
}

#[test]
fn keys_reach_only_the_machine_holding_the_cursor() {
    let (mut server, _, client) = two_machines(Settings::default());
    centre(&mut server);

    // While the cursor is here, the local machine already sees the key.
    let actions = server.handle(
        Event::Key {
            key: A,
            down: true,
            repeat: false,
        },
        now(),
    );
    assert!(actions.is_empty());
    server.handle(
        Event::Key {
            key: A,
            down: false,
            repeat: false,
        },
        now(),
    );

    server.handle(Event::PointerBy { dx: 1000, dy: 0 }, now());
    let actions = server.handle(
        Event::Key {
            key: A,
            down: true,
            repeat: false,
        },
        now(),
    );
    assert_eq!(
        sent_to(&actions, client),
        vec![ServerControl::KeyEvent {
            key: A,
            down: true,
            repeat: false
        }]
    );
}

#[test]
fn an_edge_with_no_session_beyond_it_is_a_wall() {
    // The layout knows about the machine, but nothing has connected. Sending
    // the cursor there would make it vanish onto a screen that cannot move it.
    let (local, absent) = (dev(1), dev(3));
    let mut layout = Layout::new(EdgeOverflow::Clamp);
    layout.report_monitors(
        local,
        "server",
        vec![Monitor::new("m0", Rect::new(0, 0, 1920, 1080))],
    );
    layout.report_monitors(
        absent,
        "not here",
        vec![Monitor::new("m0", Rect::new(0, 0, 1920, 1080))],
    );
    layout.place(local, &"m0".into(), Point::new(0, 0));
    layout.place(absent, &"m0".into(), Point::new(1920, 0));
    let mut server = Server::new(local, "server", layout, Settings::default());

    server.handle(Event::PointerBy { dx: 960, dy: 540 }, now());
    let actions = server.handle(Event::PointerBy { dx: 5000, dy: 0 }, now());

    assert_eq!(server.active(), local, "the cursor stayed put");
    assert!(sent_to(&actions, absent).is_empty());
    // It still travels as far as the screen goes.
    assert_eq!(server.cursor(), Point::new(1919, 540));
}

#[test]
fn a_machine_that_cannot_act_gives_the_cursor_back() {
    // This is the Windows secure-desktop case: the client is connected and
    // fine, but nothing it is told to do will reach the screen. Continuing to
    // send it positions is what makes the pointer judder.
    let (mut server, local, client) = two_machines(Settings::default());
    centre(&mut server);
    server.handle(Event::PointerBy { dx: 1000, dy: 0 }, now());
    assert_eq!(server.active(), client);

    let actions = server.handle(
        Event::ClientSuspended {
            device: client,
            reason: SuspendReason::SecureDesktop,
        },
        now(),
    );
    assert_eq!(server.active(), local);
    assert_eq!(server.pointer_mode(), PointerMode::Local);
    assert!(local_actions(&actions).contains(&LocalAction::SetPointerMode(PointerMode::Local)));

    // And it stays unreachable until it says otherwise.
    server.handle(Event::PointerBy { dx: 5000, dy: 0 }, now());
    assert_eq!(server.active(), local);

    server.handle(Event::ClientResumed { device: client }, now());
    server.handle(Event::PointerBy { dx: 5000, dy: 0 }, now());
    assert_eq!(server.active(), client, "usable again once it recovers");
}

#[test]
fn a_machine_that_disappears_gives_the_cursor_back() {
    let (mut server, local, client) = two_machines(Settings::default());
    centre(&mut server);
    server.handle(Event::PointerBy { dx: 1000, dy: 0 }, now());

    let actions = server.handle(Event::ClientDown { device: client }, now());
    assert_eq!(server.active(), local);
    assert!(local_actions(&actions)
        .iter()
        .any(|a| matches!(a, LocalAction::WarpCursor { .. })));

    // The cursor must not be able to wander onto a machine that is gone.
    server.handle(Event::PointerBy { dx: 5000, dy: 0 }, now());
    assert_eq!(server.active(), local);
}

#[test]
fn a_switch_delay_makes_the_cursor_wait_at_the_edge() {
    let settings = Settings {
        switch_delay: Duration::from_millis(250),
        ..Settings::default()
    };
    let (mut server, local, client) = two_machines(settings);
    centre(&mut server);

    let start = Instant::now();
    let actions = server.handle(Event::PointerBy { dx: 1000, dy: 0 }, start);
    assert_eq!(server.active(), local, "not yet");
    assert!(sent_to(&actions, client).is_empty());
    assert_eq!(server.cursor(), Point::new(1919, 540), "waits at the edge");

    // Still too soon.
    server.handle(Event::Tick, start + Duration::from_millis(100));
    assert_eq!(server.active(), local);

    let actions = server.handle(Event::Tick, start + Duration::from_millis(300));
    assert_eq!(server.active(), client);
    assert!(matches!(
        sent_to(&actions, client).first(),
        Some(ServerControl::Enter { .. })
    ));
}

#[test]
fn a_double_tap_crosses_straight_away_on_the_second_strike() {
    let settings = Settings {
        switch_double_tap: Duration::from_millis(250),
        ..Settings::default()
    };
    let (mut server, local, client) = two_machines(settings);
    centre(&mut server);

    let start = Instant::now();
    server.handle(Event::PointerBy { dx: 1000, dy: 0 }, start);
    assert_eq!(server.active(), local, "one strike is not enough");

    // Come away from the edge and strike it again within the window.
    server.handle(
        Event::PointerBy { dx: -400, dy: 0 },
        start + Duration::from_millis(50),
    );
    server.handle(
        Event::PointerBy { dx: 1000, dy: 0 },
        start + Duration::from_millis(100),
    );
    assert_eq!(server.active(), client);
}

#[test]
fn a_strike_after_the_window_has_passed_does_not_count() {
    let settings = Settings {
        switch_double_tap: Duration::from_millis(250),
        ..Settings::default()
    };
    let (mut server, local, _) = two_machines(settings);
    centre(&mut server);

    let start = Instant::now();
    server.handle(Event::PointerBy { dx: 1000, dy: 0 }, start);
    server.handle(
        Event::PointerBy { dx: -400, dy: 0 },
        start + Duration::from_secs(1),
    );
    server.handle(
        Event::PointerBy { dx: 1000, dy: 0 },
        start + Duration::from_secs(2),
    );
    assert_eq!(server.active(), local, "too late to count as a double tap");
}

#[test]
fn an_absolute_position_only_resynchronises() {
    // Pointer acceleration means raw motion and real movement differ, so the
    // tracked position is corrected from the real one. That correction must
    // never itself look like movement.
    let (mut server, local, client) = two_machines(Settings::default());
    centre(&mut server);

    let actions = server.handle(Event::PointerAt { x: 1919, y: 540 }, now());
    assert!(actions.is_empty());
    assert_eq!(server.cursor(), Point::new(1919, 540));
    assert_eq!(server.active(), local, "a resync never crosses");

    // Pushing further from the edge is what crosses.
    server.handle(Event::PointerBy { dx: 10, dy: 0 }, now());
    assert_eq!(server.active(), client);
}

#[test]
fn a_display_change_leaves_the_cursor_somewhere_real() {
    let (mut server, _, client) = two_machines(Settings::default());
    centre(&mut server);
    server.handle(Event::PointerBy { dx: 1000, dy: 0 }, now());
    assert_eq!(server.active(), client);

    // The client's screen shrinks out from under the cursor.
    server.handle(
        Event::ClientMonitors {
            device: client,
            monitors: vec![Monitor::new("m0", Rect::new(0, 0, 640, 480))],
        },
        now(),
    );

    assert!(
        server.layout().locate(server.cursor()).is_some(),
        "the cursor ended up at {:?}, which is on no monitor",
        server.cursor()
    );
}

#[test]
fn a_machine_arriving_is_told_to_let_go_of_everything() {
    // It may have been holding keys when its last session ended.
    let (mut server, _, _) = two_machines(Settings::default());
    let newcomer = dev(7);
    let actions = server.handle(
        Event::ClientUp {
            device: newcomer,
            name: "laptop".into(),
        },
        now(),
    );
    assert_eq!(sent_to(&actions, newcomer), vec![ServerControl::ReleaseAll]);
}

#[test]
fn the_configured_arrangement_is_used_where_it_applies() {
    use smkvm_core::Placement;
    use smkvm_layout::Rect;

    let (local, client) = (dev(1), dev(2));
    let mut layout = Layout::new(EdgeOverflow::Clamp);
    layout.report_monitors(
        local,
        "server",
        vec![Monitor {
            id: "\\\\?\\DISPLAY#LONG#PATH".into(),
            local: Rect::new(0, 0, 1920, 1080),
            scale: 1.0,
            primary: true,
            label: None,
        }],
    );
    let mut server = Server::new(local, "server", layout, Settings::default());

    // The machine underneath is placed by name; the one above is named by
    // `primary`, because its own identifier is a device path nobody would
    // want to write down.
    server.set_placements(vec![
        Placement {
            machine: "server".into(),
            monitor: Placement::PRIMARY.into(),
            global: Rect::new(640, 0, 1920, 1080),
        },
        Placement {
            machine: "desk".into(),
            monitor: "DP-2".into(),
            global: Rect::new(0, 1080, 2560, 1440),
        },
        Placement {
            machine: "desk".into(),
            monitor: "DP-0".into(),
            global: Rect::new(2560, 1080, 2560, 1440),
        },
    ]);

    server.handle(
        Event::ClientUp {
            device: client,
            name: "desk".into(),
        },
        now(),
    );
    server.handle(
        Event::ClientMonitors {
            device: client,
            monitors: vec![
                Monitor::new("DP-2", Rect::new(0, 0, 2560, 1440)),
                Monitor::new("DP-0", Rect::new(2560, 0, 2560, 1440)),
            ],
        },
        now(),
    );

    let at = |device, monitor: &str| {
        server
            .layout()
            .placement(device, &monitor.into())
            .unwrap_or_else(|| panic!("{monitor} was never placed"))
    };
    assert_eq!(
        at(local, "\\\\?\\DISPLAY#LONG#PATH"),
        Rect::new(640, 0, 1920, 1080)
    );
    assert_eq!(at(client, "DP-2"), Rect::new(0, 1080, 2560, 1440));
    assert_eq!(at(client, "DP-0"), Rect::new(2560, 1080, 2560, 1440));

    // And the arrangement behaves: straight up from the left-hand desktop
    // monitor reaches the machine above it.
    let m = server
        .layout()
        .resolve(Point::new(1500, 1085), 0, -10)
        .unwrap();
    assert_eq!(m.located.device, local);
}

#[test]
fn a_machine_the_arrangement_does_not_mention_is_still_placed() {
    use smkvm_core::Placement;
    use smkvm_layout::Rect;

    let (local, stranger) = (dev(1), dev(5));
    let mut layout = Layout::new(EdgeOverflow::Clamp);
    layout.report_monitors(
        local,
        "server",
        vec![Monitor::new("m0", Rect::new(0, 0, 1920, 1080))],
    );
    let mut server = Server::new(local, "server", layout, Settings::default());
    server.set_placements(vec![Placement {
        machine: "server".into(),
        monitor: "m0".into(),
        global: Rect::new(0, 0, 1920, 1080),
    }]);

    server.handle(
        Event::ClientUp {
            device: stranger,
            name: "laptop".into(),
        },
        now(),
    );
    server.handle(
        Event::ClientMonitors {
            device: stranger,
            monitors: vec![Monitor::new("eDP-1", Rect::new(0, 0, 1920, 1200))],
        },
        now(),
    );

    // A half-written layout must still leave every screen reachable.
    assert!(server.layout().unplaced().is_empty());
    assert!(server.layout().cells().iter().any(|c| c.device == stranger));
}

/// The arrangement this project was built for, with the second machine along
/// the top configured but not switched on.
fn desk_with_one_machine_missing() -> (Server, DeviceId, DeviceId) {
    use smkvm_core::Placement;
    use smkvm_layout::Rect;

    let (win, desk) = (dev(1), dev(3));
    let mut layout = Layout::new(EdgeOverflow::Clamp);
    layout.report_monitors(
        win,
        "WIN-STUDY",
        vec![Monitor {
            id: "primary".into(),
            local: Rect::new(0, 0, 1920, 1080),
            scale: 1.0,
            primary: true,
            label: None,
        }],
    );
    let mut server = Server::new(win, "WIN-STUDY", layout, Settings::default());
    server.set_placements(vec![
        Placement {
            machine: "WIN-STUDY".into(),
            monitor: Placement::PRIMARY.into(),
            global: Rect::new(640, 0, 1920, 1080),
        },
        // Configured, and never connects.
        Placement {
            machine: "WIN-LAPTOP".into(),
            monitor: Placement::PRIMARY.into(),
            global: Rect::new(2560, 0, 1920, 1080),
        },
        Placement {
            machine: "ubuntu-box".into(),
            monitor: "DP-2".into(),
            global: Rect::new(0, 1080, 2560, 1440),
        },
        Placement {
            machine: "ubuntu-box".into(),
            monitor: "DP-0".into(),
            global: Rect::new(2560, 1080, 2560, 1440),
        },
    ]);
    server.handle(
        Event::ClientUp {
            device: desk,
            name: "ubuntu-box".into(),
        },
        now(),
    );
    server.handle(
        Event::ClientMonitors {
            device: desk,
            monitors: vec![
                Monitor::new("DP-2", Rect::new(0, 0, 2560, 1440)),
                Monitor::new("DP-0", Rect::new(2560, 0, 2560, 1440)),
            ],
        },
        now(),
    );
    (server, win, desk)
}

#[test]
fn a_machine_that_is_not_here_leaves_a_wall_rather_than_a_gap() {
    // The right-hand desktop monitor has a machine configured above it that
    // is switched off. Pushing up there must stop, not slide the cursor
    // sideways onto the machine above the *other* monitor -- which from the
    // person's seat looks like the pointer leaping across the desk.
    let (server, win, desk) = desk_with_one_machine_missing();

    let m = server
        .layout()
        .resolve(Point::new(4000, 1085), 0, -50)
        .unwrap();
    assert_eq!(
        m.located.device, desk,
        "the cursor left the desktop for a machine that is not there"
    );
    assert!(m.adjusted, "it stopped at the edge");

    // Further right still, past the absent machine's slot entirely.
    let m = server
        .layout()
        .resolve(Point::new(5000, 1085), 0, -50)
        .unwrap();
    assert_eq!(m.located.device, desk);

    // And the machine that *is* there remains reachable from beneath it.
    let m = server
        .layout()
        .resolve(Point::new(1500, 1085), 0, -50)
        .unwrap();
    assert_eq!(m.located.device, win);
    assert!(!m.adjusted, "straight up needs no adjusting");
}

#[test]
fn the_wall_appears_when_a_machine_goes_and_lifts_when_it_returns() {
    let (mut server, win, desk) = desk_with_one_machine_missing();
    let absent = dev(2);

    // With the second machine present, its space is its own.
    server.handle(
        Event::ClientUp {
            device: absent,
            name: "WIN-LAPTOP".into(),
        },
        now(),
    );
    server.handle(
        Event::ClientMonitors {
            device: absent,
            monitors: vec![smkvm_layout::Monitor {
                id: "primary".into(),
                local: smkvm_layout::Rect::new(0, 0, 1920, 1080),
                scale: 1.0,
                primary: true,
                label: None,
            }],
        },
        now(),
    );
    let m = server
        .layout()
        .resolve(Point::new(4000, 1085), 0, -50)
        .unwrap();
    assert_eq!(m.located.device, absent, "it should be reachable now");

    // And when it goes away again the wall comes back, rather than the cursor
    // starting to leap to the machine beside it.
    server.handle(Event::ClientDown { device: absent }, now());
    let m = server
        .layout()
        .resolve(Point::new(4000, 1085), 0, -50)
        .unwrap();
    assert_eq!(m.located.device, desk);
    assert_ne!(m.located.device, win);
}
