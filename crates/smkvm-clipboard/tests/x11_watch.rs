//! Watching the clipboard change hands, against a real X server.
//!
//! The watcher looks at what each new owner offers, and looking leaves
//! events behind on its connection. It has to keep noticing changes after
//! that, however many copies have gone before.

#![cfg(all(unix, not(target_os = "macos")))]

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use smkvm_clipboard::platform::x11::{X11Clipboard, X11Owner};
use smkvm_clipboard::{ClipboardError, Fetch, Result, Watch as _};
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
    // A different range from the round-trip tests, which may run alongside.
    let base = 350 + (std::process::id() % 40) * 4;
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

struct Canned(Vec<u8>);

impl Fetch for Canned {
    fn fetch(&self, format: &ClipFormat) -> Result<Vec<u8>> {
        match format {
            ClipFormat::Text => Ok(self.0.clone()),
            other => Err(ClipboardError::Refused(other.clone())),
        }
    }
}

/// Copy `text` on the given display, and keep serving it until something
/// else copies or the returned handle is dropped.
///
/// Returns once the selection is held. Each copy is its own application with
/// its own connection, as it would be on a real desktop; the one before it
/// finds out it has been superseded the way any application does.
fn copy(display: &str, text: &str) -> mpsc::Sender<()> {
    let display = display.to_owned();
    let text = text.as_bytes().to_vec();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let conn = X11Clipboard::open_display(Some(&display)).expect("owner connects");
        let mut owner =
            X11Owner::take(conn, &[ClipFormat::Text], Box::new(Canned(text))).expect("takes it");
        ready_tx.send(()).expect("the test is waiting");
        while owner.owns_clipboard() {
            if matches!(
                stop_rx.try_recv(),
                Err(mpsc::TryRecvError::Disconnected) | Ok(())
            ) {
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
        .expect("the copy happened");
    stop_tx
}

/// What the watcher reports next, along with the watcher itself -- unless it
/// never answers, in which case it stays with the thread it wedged.
///
/// The watcher blocks on the display, so it is driven from a thread the test
/// can give up on: a watcher that never comes back is the very failure being
/// looked for, and it must not take the test down with it.
fn next_change_within(
    mut watcher: X11Clipboard,
    patience: Duration,
) -> Option<(X11Clipboard, Vec<ClipFormat>)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let formats = watcher.next_change().map(|a| a.formats);
        let _ = tx.send((watcher, formats));
    });
    match rx.recv_timeout(patience) {
        Ok((watcher, Some(formats))) => Some((watcher, formats)),
        _ => None,
    }
}

#[test]
fn the_watcher_keeps_noticing_after_it_has_looked() {
    let Some(server) = start_server() else {
        eprintln!("skipping: could not start Xvfb");
        return;
    };
    let display = server.display.clone();

    let mut watcher = X11Clipboard::open_display(Some(&display)).expect("watcher connects");
    watcher.watch().expect("asks to be told of changes");

    // Each copy supersedes the one before, so there is exactly one change to
    // notice per copy. Looking at a copy asks its owner questions, and the
    // answers leave events on the watcher's connection; the next copy has to
    // get through regardless, and the one after that.
    let mut owners = Vec::new();
    for text in ["first", "second", "third"] {
        owners.push(copy(&display, text));
        let (back, formats) = next_change_within(watcher, Duration::from_secs(5))
            .unwrap_or_else(|| panic!("the watcher stopped noticing before the {text} copy"));
        assert_eq!(formats, vec![ClipFormat::Text], "after the {text} copy");
        watcher = back;
    }

    drop(owners);
    drop(server);
}
