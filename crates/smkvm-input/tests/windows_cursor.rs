//! Moving a real Windows pointer out of the way, and getting it back.
//!
//! The rest of the parking behaviour is exercised against the recording
//! backend, which is enough to describe the rule but cannot say whether this
//! platform actually honours it. That question is the whole of the bug this
//! guards against: a backend that parks the pointer and has no way to put it
//! back leaves the person staring at a screen with the pointer in the far
//! corner, and nothing in the protocol will ever mention it again.
//!
//! Skipped where there is no desktop to move a pointer on, which is every
//! session a service or an ssh login gets.

#![cfg(windows)]

use std::time::{Duration, Instant};

use smkvm_input::platform::windows::{cursor_position, WindowsInput};
use smkvm_input::Inject as _;

/// Wait for the system to apply an injected move.
///
/// `SendInput` hands the event to the input thread and returns; the pointer has
/// not necessarily arrived yet. Polling rather than sleeping keeps this quick
/// when the machine is idle and patient when it is not.
fn settle_at(wanted: (i32, i32)) -> (i32, i32) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let at = cursor_position().unwrap_or(wanted);
        if at == wanted || Instant::now() > deadline {
            return at;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Can this session move a pointer at all?
fn has_a_desktop(input: &mut WindowsInput) -> bool {
    let probe = (300, 200);
    if input.move_to(probe.0, probe.1).is_err() {
        return false;
    }
    settle_at(probe) == probe
}

#[test]
fn a_parked_pointer_comes_back_to_where_it_was() {
    let mut input = WindowsInput::new();
    if !has_a_desktop(&mut input) {
        eprintln!("no desktop to move a pointer on; skipping");
        return;
    }

    let home = (400, 300);
    input.move_to(home.0, home.1).unwrap();
    let before = settle_at(home);
    assert_eq!(before, home, "the pointer would not go where it was put");

    input.hide_cursor().unwrap();
    let parked = cursor_position().unwrap();
    assert_ne!(
        parked, before,
        "parking left the pointer in the middle of what the person is reading"
    );

    input.show_cursor().unwrap();
    assert_eq!(
        settle_at(before),
        before,
        "the pointer never came back from {parked:?}"
    );
}

#[test]
fn arriving_after_a_park_keeps_the_position_it_arrived_at() {
    // The order the client uses: place the pointer where the cursor came in,
    // then ask for it back. The asking must not undo the placing.
    let mut input = WindowsInput::new();
    if !has_a_desktop(&mut input) {
        eprintln!("no desktop to move a pointer on; skipping");
        return;
    }

    input.move_to(400, 300).unwrap();
    settle_at((400, 300));
    input.hide_cursor().unwrap();

    let arrived = (120, 640);
    input.move_to(arrived.0, arrived.1).unwrap();
    input.show_cursor().unwrap();

    assert_eq!(
        settle_at(arrived),
        arrived,
        "the pointer was dragged away from where the cursor arrived"
    );
}

/// Move the pointer the way something that is not this program would, so the
/// capture hook has something it is supposed to report.
///
/// Without this the test below cannot tell a hook that correctly ignored the
/// parking from a hook that was not watching at all.
fn move_like_a_stranger(x: i32, y: i32) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    };

    // SAFETY: reading system metrics takes no pointers, and the slice handed
    // to SendInput is valid for the call with the size the API expects.
    unsafe {
        let (w, h) = (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        );
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: (i64::from(x) * 65535 / i64::from(w - 1)) as i32,
                    dy: (i64::from(y) * 65535 / i64::from(h - 1)) as i32,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    // The whole point: no marker, so the hook treats it as the
                    // person's own mouse.
                    dwExtraInfo: 0,
                },
            },
        };
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

#[test]
fn parking_the_pointer_is_not_read_back_as_the_person_moving_it() {
    // Everything this program injects is stamped so the capture hook can tell
    // it apart from the real mouse, and the server's authoritative cursor is
    // kept from what that hook reports. So whatever parking does, the hook
    // must not see it: a park reported as real input would drag the cursor to
    // the corner on the next movement.
    use smkvm_input::platform::windows::capture::{Capture, Captured};

    let mut input = WindowsInput::new();
    if !has_a_desktop(&mut input) {
        eprintln!("no desktop to move a pointer on; skipping");
        return;
    }
    let Ok((_capture, events)) = Capture::start() else {
        eprintln!("the system would not install the hooks; skipping");
        return;
    };

    let drain = |events: &std::sync::mpsc::Receiver<Captured>| {
        std::thread::sleep(Duration::from_millis(250));
        std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>()
    };
    let saw_a_move_to = |seen: &[Captured], at: (i32, i32)| {
        seen.iter()
            .any(|e| matches!(e, Captured::PointerAt { x, y } if (*x, *y) == at))
    };

    // First establish that the hook is watching at all, or the assertion below
    // would hold just as well with the hooks uninstalled.
    input.move_to(400, 300).unwrap();
    settle_at((400, 300));
    drain(&events);
    let control = (500, 400);
    move_like_a_stranger(control.0, control.1);
    let seen = drain(&events);
    if !saw_a_move_to(&seen, control) {
        eprintln!("the hook is not reporting movement on this machine; skipping");
        let _ = input.show_cursor();
        return;
    }

    input.hide_cursor().unwrap();
    let corner = cursor_position().unwrap();
    let seen = drain(&events);
    assert!(
        !saw_a_move_to(&seen, corner),
        "the park came back through the hook as though the person had moved \
         the mouse to {corner:?}: {seen:?}"
    );

    let _ = input.show_cursor();
}
