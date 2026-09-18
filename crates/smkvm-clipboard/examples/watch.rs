//! Watch the clipboard and print what appears on it.
//!
//!     cargo run -p smkvm-clipboard --example watch
//!
//! Reads only; it never takes the clipboard over.

fn main() {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use smkvm_clipboard::platform::x11::X11Clipboard;
        use smkvm_clipboard::{Read as _, Watch as _};

        let mut clipboard = match X11Clipboard::open() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("cannot reach the display: {e}");
                std::process::exit(1);
            }
        };
        if let Err(e) = clipboard.watch() {
            eprintln!("cannot watch the clipboard: {e}");
            std::process::exit(1);
        }

        match clipboard.available() {
            Ok(now) if !now.is_empty() => println!("on the clipboard now: {:?}", now.formats),
            Ok(_) => println!("the clipboard is empty, or holds nothing this can carry"),
            Err(e) => println!("could not look: {e}"),
        }
        println!("watching. Copy something.\n");

        while let Some(available) = clipboard.next_change() {
            if available.is_empty() {
                println!("changed, but to nothing this can carry");
                continue;
            }
            println!("changed: {:?}", available.formats);
            for format in &available.formats {
                match clipboard.read(format) {
                    Ok(bytes) => {
                        let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(60)]);
                        let preview = preview.replace('\n', "\\n");
                        println!(
                            "  {:<8} {:>9} bytes  {}",
                            format!("{format:?}"),
                            bytes.len(),
                            preview
                        );
                    }
                    Err(e) => println!("  {format:?}: {e}"),
                }
            }
            println!();
        }
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    eprintln!("no clipboard backend on this platform");
}
