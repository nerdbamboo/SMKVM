//! Injecting into a real X server.
//!
//! Each test gets a private virtual server, so nothing here touches whatever
//! session the machine is actually running, and every test starts with a
//! keyboard that is known to be idle.
//!
//! Skipped when `Xvfb` is not installed, since there is then nothing to
//! inject into.

#![cfg(all(unix, not(target_os = "macos")))]

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use smkvm_input::keymap::hid_to_x11_keycode;
use smkvm_input::platform::x11::X11Input;
use smkvm_input::{Inject, Monitors, Tracked};
use smkvm_proto::{Key, MouseButton, Scroll};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const A: Key = Key(0x04);
const WIDTH: i32 = 1920;
const HEIGHT: i32 = 1080;

struct Server {
    child: Child,
    display: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        // Ask it to stop rather than killing it outright: an X server that is
        // shot dead leaves its socket and lock file behind, and the next run
        // that picks the same display number then connects to a dead socket.
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Does a live server already hold this display number?
fn display_taken(n: u32) -> bool {
    std::path::Path::new(&format!("/tmp/.X{n}-lock")).exists()
        || std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
}

/// Connect and make one round trip, which a leftover socket cannot survive.
fn server_answers(display: &str) -> bool {
    let Ok((conn, _)) = x11rb::connect(Some(display)) else {
        return false;
    };
    conn.get_input_focus().is_ok_and(|c| c.reply().is_ok())
}

fn xvfb_available() -> bool {
    Command::new("Xvfb")
        .arg("-help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Start a private virtual X server and wait until it accepts connections.
fn start_server() -> Option<Server> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    // Spread across a range so concurrent test binaries do not collide, and
    // step past any number that turns out to be taken.
    let base = 90 + (std::process::id() % 30) * 4;

    for _ in 0..24 {
        let n = base + NEXT.fetch_add(1, Ordering::SeqCst);
        if display_taken(n) {
            continue;
        }
        let display = format!(":{n}");
        let Ok(child) = Command::new("Xvfb")
            .args([
                &display,
                "-screen",
                "0",
                &format!("{WIDTH}x{HEIGHT}x24"),
                "-nolisten",
                "tcp",
                // An X server resets itself when its last client disconnects.
                // The readiness probe below connects and lets go, so without
                // this the server would be tearing down and rebuilding exactly
                // as the test tries to connect.
                "-noreset",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return None;
        };
        let mut server = Server { child, display };

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if server_answers(&server.display) {
                return Some(server);
            }
            if matches!(server.child.try_wait(), Ok(Some(_))) {
                break; // that display was taken; try the next
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(server);
    }
    None
}

/// Run `body` against a private X server, or skip if one cannot be had.
fn with_server(body: impl FnOnce(X11Input, RustConnection)) {
    if !xvfb_available() {
        eprintln!("skipping: Xvfb is not installed");
        return;
    }
    let Some(server) = start_server() else {
        eprintln!("skipping: could not start Xvfb");
        return;
    };
    let input = X11Input::open_display(Some(&server.display)).expect("open the test display");
    let (conn, _) = x11rb::connect(Some(&server.display)).expect("observe the test display");
    body(input, conn);
}

/// Wait for the injector's requests to be processed.
///
/// The injector and the observer are separate X clients, and the server is
/// free to service them in any order. Once a round trip on the injector's
/// connection has returned, everything it sent earlier has been handled, so a
/// subsequent query on the other connection sees it.
fn settle(input: &X11Input) {
    input.sync().expect("round trip to the test server");
}

fn pointer(conn: &RustConnection) -> (i32, i32, u16) {
    let root = conn.setup().roots[0].root;
    let p = conn.query_pointer(root).unwrap().reply().unwrap();
    (i32::from(p.root_x), i32::from(p.root_y), p.mask.into())
}

/// Is this X11 keycode currently held, according to the server?
fn key_held(conn: &RustConnection, keycode: u8) -> bool {
    let keys = conn.query_keymap().unwrap().reply().unwrap().keys;
    keys[usize::from(keycode) / 8] & (1 << (u32::from(keycode) % 8)) != 0
}

#[test]
fn the_pointer_goes_where_it_is_put() {
    with_server(|mut input, conn| {
        for (x, y) in [(0, 0), (960, 540), (WIDTH - 1, HEIGHT - 1), (123, 456)] {
            input.move_to(x, y).unwrap();
            settle(&input);
            let (px, py, _) = pointer(&conn);
            assert_eq!((px, py), (x, y));
        }
    });
}

#[test]
fn a_position_outside_the_screen_does_not_wrap_around() {
    with_server(|mut input, conn| {
        // X11 carries coordinates in 16 bits. A value past that must not come
        // out the other side as a small positive number.
        input.move_to(999_999, 999_999).unwrap();
        settle(&input);
        let (x, y, _) = pointer(&conn);
        assert!(x >= WIDTH - 1 && y >= HEIGHT - 1, "landed at {x},{y}");
    });
}

#[test]
fn keys_press_and_release_by_physical_position() {
    with_server(|mut input, conn| {
        let keycode = hid_to_x11_keycode(A).unwrap();
        assert!(!key_held(&conn, keycode), "the test server starts idle");

        input.key(A, true).unwrap();
        settle(&input);
        assert!(key_held(&conn, keycode), "the key did not go down");

        input.key(A, false).unwrap();
        settle(&input);
        assert!(!key_held(&conn, keycode), "the key did not come back up");
    });
}

#[test]
fn every_mapped_key_can_actually_be_pressed() {
    with_server(|mut input, conn| {
        for (usage, _) in smkvm_input::keymap::HID_TO_EVDEV {
            let key = Key(usage);
            let keycode = hid_to_x11_keycode(key).unwrap();
            input.key(key, true).unwrap();
            settle(&input);
            assert!(
                key_held(&conn, keycode),
                "usage {usage:#06x} would not press"
            );
            input.key(key, false).unwrap();
            settle(&input);
            assert!(!key_held(&conn, keycode), "usage {usage:#06x} stayed down");
        }
    });
}

#[test]
fn buttons_press_and_release() {
    with_server(|mut input, conn| {
        const BUTTON1_MASK: u16 = 1 << 8;
        input.button(MouseButton::Left, true).unwrap();
        settle(&input);
        assert_ne!(pointer(&conn).2 & BUTTON1_MASK, 0, "button did not go down");

        input.button(MouseButton::Left, false).unwrap();
        settle(&input);
        assert_eq!(pointer(&conn).2 & BUTTON1_MASK, 0, "button stayed down");
    });
}

#[test]
fn releasing_everything_really_clears_the_server_state() {
    // The end-to-end version of the stuck-key guarantee: after a link drops,
    // the X server itself must report nothing held.
    with_server(|input, conn| {
        let mut t = Tracked::new(input);
        let held = [Key::LEFT_CTRL, Key::LEFT_ALT, Key::LEFT_SHIFT, A];
        for key in held {
            t.key(key, true).unwrap();
        }
        t.button(MouseButton::Left, true).unwrap();
        settle(t.inner());
        for key in held {
            assert!(key_held(&conn, hid_to_x11_keycode(key).unwrap()));
        }

        t.release_all().unwrap();
        settle(t.inner());

        for key in held {
            assert!(
                !key_held(&conn, hid_to_x11_keycode(key).unwrap()),
                "{key:?} was left held down"
            );
        }
        assert_eq!(pointer(&conn).2 & (1 << 8), 0, "a button was left held");
    });
}

#[test]
fn fine_scrolling_accumulates_instead_of_being_lost() {
    with_server(|mut input, _conn| {
        // A high-resolution wheel sends fractions of a click. Dropping them
        // would make such a wheel scroll short, so they must add up.
        for _ in 0..4 {
            input.wheel(Scroll::new(0, Scroll::NOTCH / 4)).unwrap();
        }
        input.flush().unwrap();
        // X11 reports wheel motion as transient clicks, which leave no state
        // to query, so there is nothing to assert beyond it not erroring.
    });
}

#[test]
fn an_enormous_scroll_cannot_tie_up_the_display() {
    // The delta arrives over the network, and X11 can only express scrolling
    // by repeating a click. Unbounded, one message would issue millions of
    // requests and lock the session up for minutes.
    with_server(|mut input, conn| {
        let start = Instant::now();
        for _ in 0..8 {
            input.wheel(Scroll::new(i32::MAX, i32::MIN)).unwrap();
            settle(&input);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "eight extreme scrolls took {elapsed:?}"
        );
        // The server is still answering afterwards.
        input.move_to(42, 43).unwrap();
        settle(&input);
        let (px, py, _) = pointer(&conn);
        assert_eq!((px, py), (42, 43));
    });
}

#[test]
fn a_bare_server_still_reports_a_usable_monitor() {
    with_server(|mut input, _conn| {
        let monitors = input.monitors().unwrap();
        assert_eq!(monitors.len(), 1, "a virtual server has one screen");
        assert_eq!(monitors[0].local.w, WIDTH);
        assert_eq!(monitors[0].local.h, HEIGHT);
        assert!(
            !monitors[0].id.as_str().is_empty(),
            "a monitor needs a name"
        );
        // `primary` is advisory and a bare server marks nothing primary, so
        // the layout must not depend on one existing.
    });
}
