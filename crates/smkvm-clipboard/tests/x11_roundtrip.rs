//! Offering the clipboard and reading it back, against a real X server.
//!
//! Each test gets a private virtual server, so nothing touches whatever
//! session the machine is running. Two connections stand in for two
//! applications: one holds the selection, the other pastes.

#![cfg(all(unix, not(target_os = "macos")))]

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use smkvm_clipboard::platform::x11::{X11Clipboard, X11Owner};
use smkvm_clipboard::{ClipboardError, Fetch, Read as _, Result};
use smkvm_proto::ClipFormat;

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
    let base = 150 + (std::process::id() % 40) * 4;
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
                // Without this the server resets when its last client goes,
                // and the readiness probe below is a client that lets go.
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

/// Hands out whatever it was built with.
struct Canned(Vec<(ClipFormat, Vec<u8>)>);

impl Fetch for Canned {
    fn fetch(&self, format: &ClipFormat) -> Result<Vec<u8>> {
        self.0
            .iter()
            .find(|(f, _)| f == format)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| ClipboardError::Refused(format.clone()))
    }
}

/// Offer `contents`, then read every format back through a second connection.
fn round_trip(contents: Vec<(ClipFormat, Vec<u8>)>) -> Option<Vec<(ClipFormat, Vec<u8>)>> {
    let server = start_server()?;
    let display = server.display.clone();

    let formats: Vec<ClipFormat> = contents.iter().map(|(f, _)| f.clone()).collect();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel::<()>();

    // The owner has to keep answering while the other side asks, so it runs on
    // its own thread: in X11 the application that copied is the one that
    // serves every paste.
    let owner_display = display.clone();
    let owner = std::thread::spawn(move || {
        let clipboard = X11Clipboard::open_display(Some(&owner_display)).expect("owner connects");
        let mut owner = X11Owner::take(clipboard, &formats, Box::new(Canned(contents)))
            .expect("takes the clipboard");
        ready_tx.send(()).expect("reader is waiting");
        // Polling rather than waiting on the display: once the last request
        // has been answered no further event arrives, and a blocking wait
        // would never come back to notice the decision to stop.
        while owner.owns_clipboard() {
            if done_rx.try_recv().is_ok() {
                break;
            }
            match owner.serve_pending() {
                Ok(true) => std::thread::sleep(Duration::from_millis(2)),
                Ok(false) | Err(_) => break,
            }
        }
    });

    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the owner took the clipboard");
    let mut reader = X11Clipboard::open_display(Some(&display)).expect("reader connects");
    let available = reader.available().expect("looks at the clipboard");
    let mut got = Vec::new();
    for format in &available.formats {
        let bytes = reader.read(format).expect("reads the clipboard");
        got.push((format.clone(), bytes));
    }
    let _ = done_tx.send(());
    drop(reader);
    let _ = owner.join();
    drop(server);
    Some(got)
}

#[test]
fn text_offered_by_one_connection_is_read_by_another() {
    let Some(got) = round_trip(vec![(
        ClipFormat::Text,
        b"hello from the other machine".to_vec(),
    )]) else {
        eprintln!("skipping: could not start Xvfb");
        return;
    };
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, ClipFormat::Text);
    assert_eq!(got[0].1, b"hello from the other machine");
}

#[test]
fn several_formats_are_all_offered_and_all_readable() {
    let html = b"<b>hello</b>".to_vec();
    let text = b"hello".to_vec();
    let Some(got) = round_trip(vec![
        (ClipFormat::Text, text.clone()),
        (ClipFormat::Html, html.clone()),
    ]) else {
        eprintln!("skipping: could not start Xvfb");
        return;
    };
    let find = |want: ClipFormat| {
        got.iter()
            .find(|(f, _)| *f == want)
            .map(|(_, b)| b.clone())
            .unwrap_or_else(|| panic!("{want:?} was not offered"))
    };
    assert_eq!(find(ClipFormat::Text), text);
    assert_eq!(find(ClipFormat::Html), html);
}

#[test]
fn something_far_too_large_for_one_message_still_arrives_whole() {
    // The case a fixed size limit would quietly drop: a screenshot-sized
    // payload, which has to be sent a piece at a time.
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let Some(got) = round_trip(vec![(ClipFormat::Png, big.clone())]) else {
        eprintln!("skipping: could not start Xvfb");
        return;
    };
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1.len(), big.len(), "arrived short");
    assert_eq!(got[0].1, big, "arrived corrupted");
}
