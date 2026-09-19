//! An offer replaced while the last one is still being fetched.
//!
//! On the real machines one copy on Windows arrives as two offers a few
//! milliseconds apart, and something on the desktop asks for the first one
//! at once. The owner thread is then fetching for the first offer when the
//! second arrives. Whatever happens to the first, the second must be served.

#![cfg(all(unix, not(target_os = "macos")))]

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smkvm_clipboard::platform::x11::{X11Clipboard, X11Writer};
use smkvm_clipboard::{ClipboardError, Fetch, Read as _, Result, Write as _};
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
    let base = 550 + (std::process::id() % 40) * 4;
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

/// Answers only once told to, the way a fetch over the link answers only
/// when the far machine does -- and then keeps giving that answer, since a
/// requestor refused one target asks for the next.
/// What the gate eventually answers with, once it does.
type Answer = Arc<Mutex<Option<std::result::Result<Vec<u8>, String>>>>;

struct Gated {
    answer: Answer,
    asked: mpsc::Sender<()>,
}

impl Fetch for Gated {
    fn fetch(&self, _format: &ClipFormat) -> Result<Vec<u8>> {
        let _ = self.asked.send(());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(answer) = self.answer.lock().unwrap().as_ref() {
                return answer.clone().map_err(ClipboardError::Display);
            }
            if Instant::now() > deadline {
                return Err(ClipboardError::Display("gate never opened".into()));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

struct Canned(Vec<u8>);

impl Fetch for Canned {
    fn fetch(&self, _format: &ClipFormat) -> Result<Vec<u8>> {
        Ok(self.0.clone())
    }
}

/// Offers go to the owner thread by channel, so a paste sent straight after
/// one could reach the display before the owner has taken the selection --
/// and an ownerless selection is refused at once by the server. Wait for the
/// owner thread to publish (or withdraw) its window first.
fn wait_owner(writer: &X11Writer, owning: bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if (writer.owner_window().load(Ordering::Relaxed) != 0) == owning {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "the owner thread did not {}",
        if owning {
            "take the selection"
        } else {
            "let go"
        }
    );
}

/// Paste on a thread, so a paste that never comes back is a test failure
/// rather than a hang.
fn paste_within(display: &str, patience: Duration) -> Option<Result<Vec<u8>>> {
    let display = display.to_owned();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = X11Clipboard::open_display(Some(&display)).expect("reader connects");
        let _ = tx.send(reader.read(&ClipFormat::Text));
    });
    rx.recv_timeout(patience).ok()
}

#[test]
fn the_second_offer_is_served_whatever_became_of_the_first() {
    let Some(server) = start_server() else {
        eprintln!("skipping: could not start Xvfb");
        return;
    };
    let display = server.display.clone();
    let mut writer = X11Writer::open_display(Some(&display)).expect("writer connects");

    // The first offer, whose fetch will hang until released.
    let answer: Answer = Arc::new(Mutex::new(None));
    let (asked_tx, asked_rx) = mpsc::channel();
    writer
        .offer(
            &[ClipFormat::Text],
            Box::new(Gated {
                answer: answer.clone(),
                asked: asked_tx,
            }),
        )
        .expect("first offer");
    wait_owner(&writer, true);

    // Something pastes it, and the owner is now waiting on the fetch.
    let first_display = display.clone();
    let first = std::thread::spawn(move || {
        let mut reader = X11Clipboard::open_display(Some(&first_display)).expect("connects");
        reader.read(&ClipFormat::Text)
    });
    asked_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the owner asked the source for the first offer");

    // The second offer lands while the first is still being fetched.
    writer
        .offer(&[ClipFormat::Text], Box::new(Canned(b"second".to_vec())))
        .expect("second offer");

    // The first fetch comes back a failure, as a stale fetch does.
    *answer.lock().unwrap() = Some(Err("replaced".to_owned()));
    // Refused one target, a requestor asks for the next; by then the second
    // offer may already be what is answering. Either outcome is honest: the
    // first paste is refused, or it gets what the clipboard now holds.
    let first = first.join().expect("the first reader finished");
    assert!(
        first.is_err() || matches!(&first, Ok(bytes) if bytes == b"second"),
        "the first paste got the stale content: {first:?}"
    );
    eprintln!(
        "owner window after replacement: {:#x}",
        writer.owner_window().load(Ordering::Relaxed)
    );

    // And a paste now gets the second offer, promptly.
    let got = paste_within(&display, Duration::from_secs(8))
        .expect("the owner answered at all")
        .expect("and had the second offer to give");
    assert_eq!(got, b"second".to_vec());
    drop(writer);
    drop(server);
}

#[test]
fn letting_go_and_taking_again_still_answers() {
    // Giving a selection up sends its owner a SelectionClear. Taken again
    // straight after, that event arrives on the new ownership and must not
    // be mistaken for somebody else copying.
    let Some(server) = start_server() else {
        eprintln!("skipping: could not start Xvfb");
        return;
    };
    let display = server.display.clone();
    let mut writer = X11Writer::open_display(Some(&display)).expect("writer connects");
    writer
        .offer(&[ClipFormat::Text], Box::new(Canned(b"one".to_vec())))
        .expect("first offer");
    wait_owner(&writer, true);
    let got = paste_within(&display, Duration::from_secs(8))
        .expect("answered")
        .expect("served");
    assert_eq!(got, b"one".to_vec());

    writer.release().expect("released");
    wait_owner(&writer, false);
    writer
        .offer(&[ClipFormat::Text], Box::new(Canned(b"two".to_vec())))
        .expect("second offer");
    wait_owner(&writer, true);
    let got = paste_within(&display, Duration::from_secs(8))
        .expect("answered after taking again")
        .expect("served after taking again");
    assert_eq!(got, b"two".to_vec());
    drop(writer);
    drop(server);
}
