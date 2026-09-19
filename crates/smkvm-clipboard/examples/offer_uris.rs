//! Hold the X11 clipboard with a `text/uri-list` naming these paths, the way
//! a file manager does after a copy, for a number of seconds.
//!
//!     cargo run -p smkvm-clipboard --example offer_uris -- 30 /path/a /path/b
//!
//! A hand tool for trying the file side of the clipboard against a running
//! daemon without a file manager in the loop.

#[cfg(all(unix, not(target_os = "macos")))]
fn main() {
    use smkvm_clipboard::files::uri_list;
    use smkvm_clipboard::platform::x11::{X11Clipboard, X11Owner};
    use smkvm_clipboard::{Fetch, Result};
    use smkvm_proto::ClipFormat;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    struct Uris(Vec<u8>);
    impl Fetch for Uris {
        fn fetch(&self, _format: &ClipFormat) -> Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    let mut args = std::env::args().skip(1);
    let seconds: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .expect("first argument: how many seconds to hold the clipboard");
    let paths: Vec<PathBuf> = args
        .map(|p| std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(p)))
        .collect();
    assert!(!paths.is_empty(), "then the paths to offer");
    let list = uri_list(&paths);
    print!("{}", String::from_utf8_lossy(&list));

    let conn = X11Clipboard::open().expect("the display");
    let mut owner =
        X11Owner::take(conn, &[ClipFormat::Uris], Box::new(Uris(list))).expect("the clipboard");
    let until = Instant::now() + Duration::from_secs(seconds);
    while owner.owns_clipboard() && Instant::now() < until {
        match owner.serve_pending() {
            Ok(true) => std::thread::sleep(Duration::from_millis(5)),
            Ok(false) => {
                eprintln!("something else copied");
                return;
            }
            Err(e) => {
                eprintln!("could not serve: {e}");
                return;
            }
        }
    }
    let _ = owner.release();
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn main() {
    eprintln!("this tool is for X11");
}
