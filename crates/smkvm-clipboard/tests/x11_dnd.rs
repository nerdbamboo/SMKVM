//! Catching a drag in progress, against a real X server and a stand-in for
//! the application doing the dragging.
//!
//! The stand-in speaks XDND the way a toolkit does: it owns `XdndSelection`,
//! tells the window under the pointer what it carries when the pointer moves,
//! drops when the button comes up, and hands the file list over when asked
//! for it. The catcher is driven exactly as the daemon drives it.

#![cfg(all(unix, not(target_os = "macos")))]

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use smkvm_clipboard::platform::x11::{DndCatcher, X11Clipboard};
use smkvm_clipboard::{CatchDrag, Drive};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, ClientMessageData, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, SelectionNotifyEvent, Window, WindowClass, CLIENT_MESSAGE_EVENT,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

struct Server {
    child: Child,
    display: String,
}

impl Drop for Server {
    fn drop(&mut self) {
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

fn display_taken(n: u32) -> bool {
    std::path::Path::new(&format!("/tmp/.X{n}-lock")).exists()
        || std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
}

fn start_server() -> Option<Server> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    // A hundred display numbers of its own, so two test binaries running
    // at once cannot land on the same one: twenty buckets four apart,
    // and the search below walks up to twenty-four from wherever it
    // starts.
    let base = 300 + (std::process::id() % 20) * 4;
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
                "800x600x24",
                "-nolisten",
                "tcp",
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
            if X11Clipboard::open_display(Some(&server.display)).is_ok() {
                return Some(server);
            }
            if matches!(server.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(server);
    }
    None
}

fn atom(conn: &RustConnection, name: &str) -> Atom {
    conn.intern_atom(false, name.as_bytes())
        .unwrap()
        .reply()
        .unwrap()
        .atom
}

/// What the stand-in application saw.
#[derive(Debug, Default)]
struct Seen {
    status: u32,
    finished: bool,
    served: bool,
}

/// The application dragging files. Owns the drag selection on its own
/// connection and answers for it from a thread; the main thread, standing in
/// for its motion handling, tells the window under the pointer about the drag
/// as the catcher nudges the pointer.
struct Dragger {
    conn: RustConnection,
    root: Window,
    window: Window,
    enter: Atom,
    position: Atom,
    drop: Atom,
    uri_list: Atom,
    action_copy: Atom,
}

impl Dragger {
    fn new(display: &str) -> Dragger {
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let window = conn.generate_id().unwrap();
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap();
        conn.flush().unwrap();
        Dragger {
            enter: atom(&conn, "XdndEnter"),
            position: atom(&conn, "XdndPosition"),
            drop: atom(&conn, "XdndDrop"),
            uri_list: atom(&conn, "text/uri-list"),
            action_copy: atom(&conn, "XdndActionCopy"),
            conn,
            root,
            window,
        }
    }

    /// The top-level window under the pointer, as a toolkit finds it.
    fn under_pointer(&self) -> Option<Window> {
        let reply = self.conn.query_pointer(self.root).unwrap().reply().unwrap();
        (reply.child != NONE).then_some(reply.child)
    }

    fn tell(&self, to: Window, type_: Atom, data: [u32; 5]) {
        let event = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: to,
            type_,
            data: ClientMessageData::from(data),
        };
        self.conn
            .send_event(false, to, EventMask::NO_EVENT, event)
            .unwrap();
        self.conn.flush().unwrap();
    }

    /// The pointer moved: whatever is under it is told about the drag.
    fn moved(&self, x: i32, y: i32) {
        let Some(target) = self.under_pointer() else {
            return;
        };
        self.tell(
            target,
            self.enter,
            [self.window, 5 << 24, self.uri_list, 0, 0],
        );
        self.tell(
            target,
            self.position,
            [
                self.window,
                0,
                ((x as u32) << 16) | (y as u32 & 0xFFFF),
                CURRENT_TIME,
                self.action_copy,
            ],
        );
    }

    /// The button came up: drop on whatever is under the pointer.
    fn released(&self) {
        let Some(target) = self.under_pointer() else {
            return;
        };
        self.tell(target, self.drop, [self.window, 0, CURRENT_TIME, 0, 0]);
    }
}

/// Answer a request for the drag's file list, as the dragging application
/// would.
fn serve(
    conn: &RustConnection,
    request: &x11rb::protocol::xproto::SelectionRequestEvent,
    contents: &[u8],
) {
    conn.change_property8(
        PropMode::REPLACE,
        request.requestor,
        request.property,
        request.target,
        contents,
    )
    .unwrap();
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: request.selection,
        target: request.target,
        property: request.property,
    };
    conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify)
        .unwrap();
    conn.flush().unwrap();
}

#[test]
fn a_drag_in_progress_is_caught_with_its_files_and_ended_on_the_catcher() {
    let Some(server) = start_server() else {
        eprintln!("skipping: could not start Xvfb");
        return;
    };
    let display = server.display.clone();

    // The stand-in application, on this thread: it owns the drag selection
    // and answers for it, since X delivers requests about a window to the
    // connection that made the window.
    let dragger = Dragger::new(&display);
    let selection = atom(&dragger.conn, "XdndSelection");
    let status = atom(&dragger.conn, "XdndStatus");
    let finished = atom(&dragger.conn, "XdndFinished");
    dragger
        .conn
        .set_selection_owner(dragger.window, selection, CURRENT_TIME)
        .unwrap();
    // Somewhere the catcher's window can be wholly on screen.
    dragger
        .conn
        .warp_pointer(NONE, dragger.root, 0, 0, 0, 0, 300, 300)
        .unwrap();
    dragger.conn.flush().unwrap();
    let contents =
        b"file:///home/user/photos/a%20b.jpg\r\nfile:///home/user/notes.txt\r\n".to_vec();

    // The catcher, on a thread, driven as the daemon drives it; what it asks
    // of the pointer comes here, where the stand-in reacts to it.
    let (drives_tx, drives_rx) = mpsc::channel();
    let catching = std::thread::spawn({
        let display = display.clone();
        move || {
            let mut catcher = DndCatcher::open_display(Some(&display)).expect("catcher connects");
            let mut driven = Vec::new();
            let paths = catcher.catch(&mut |drive| {
                driven.push(drive);
                let _ = drives_tx.send(drive);
            });
            (paths, driven)
        }
    });

    let mut seen = Seen::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !catching.is_finished() && Instant::now() < deadline {
        while let Ok(drive) = drives_rx.try_recv() {
            match drive {
                Drive::MoveTo(x, y) => dragger.moved(x, y),
                Drive::ReleaseLeft => dragger.released(),
            }
        }
        match dragger.conn.poll_for_event().unwrap() {
            Some(Event::SelectionRequest(request)) if request.target == dragger.uri_list => {
                serve(&dragger.conn, &request, &contents);
                seen.served = true;
            }
            Some(Event::ClientMessage(m)) if m.type_ == status => {
                seen.status = m.data.as_data32()[1];
            }
            Some(Event::ClientMessage(m)) if m.type_ == finished => {
                seen.finished = true;
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    let (paths, driven) = catching.join().expect("the catcher thread finishes");
    // Anything the application was told after the catcher returned. The
    // message was flushed before the catcher gave up its files, so this waits
    // for it to arrive rather than for a fixed stretch of clock: a machine
    // running the whole suite at once can leave this thread unscheduled for
    // longer than any stretch worth writing down, and did.
    let settle = Instant::now() + Duration::from_secs(5);
    while !seen.finished && Instant::now() < settle {
        match dragger.conn.poll_for_event().unwrap() {
            Some(Event::ClientMessage(m)) if m.type_ == finished => seen.finished = true,
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }

    assert_eq!(
        paths,
        Some(vec![
            std::path::PathBuf::from("/home/user/photos/a b.jpg"),
            std::path::PathBuf::from("/home/user/notes.txt"),
        ]),
        "driven: {driven:?}, seen: {seen:?}"
    );
    assert_eq!(
        driven.iter().filter(|d| **d == Drive::ReleaseLeft).count(),
        1,
        "the button is let go exactly once, once the drag is confirmed: {driven:?}"
    );
    assert!(
        driven.iter().any(|d| matches!(d, Drive::MoveTo(..))),
        "the pointer was nudged so the application would look: {driven:?}"
    );
    assert_eq!(
        seen.status & 1,
        1,
        "the catcher accepted the drag: {seen:?}"
    );
    assert!(seen.served, "the file list was asked for: {seen:?}");
    assert!(
        seen.finished,
        "the application was told the drop is finished: {seen:?}"
    );
    drop(server);
}

#[test]
fn with_nothing_being_dragged_the_button_is_left_alone() {
    let Some(server) = start_server() else {
        eprintln!("skipping: could not start Xvfb");
        return;
    };
    let display = server.display.clone();
    let mut catcher = DndCatcher::open_display(Some(&display)).expect("catcher connects");
    let mut driven = Vec::new();
    let started = Instant::now();
    let paths = catcher.catch(&mut |drive| driven.push(drive));
    assert_eq!(paths, None);
    assert!(
        !driven.contains(&Drive::ReleaseLeft),
        "no drag, so nothing to let go of: {driven:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "giving up takes a fraction of a second, not a wait"
    );
    drop(catcher);
    drop(server);
}
