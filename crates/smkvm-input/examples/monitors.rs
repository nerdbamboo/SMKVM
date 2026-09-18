//! Print the monitors this machine would report to the server.
//!
//!     cargo run -p smkvm-input --example monitors
//!
//! Read-only: it injects nothing and moves nothing.

fn main() {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use smkvm_input::platform::x11::X11Input;
        use smkvm_input::Monitors;

        let mut x11 = match X11Input::open() {
            Ok(x) => x,
            Err(e) => {
                eprintln!("cannot reach the display: {e}");
                std::process::exit(1);
            }
        };
        match x11.monitors() {
            Ok(monitors) => {
                println!("{} monitor(s):", monitors.len());
                for m in &monitors {
                    println!(
                        "  {:<10} {:>5} x {:<5} at {:>6},{:<6}{}",
                        m.id.as_str(),
                        m.local.w,
                        m.local.h,
                        m.local.x,
                        m.local.y,
                        if m.primary { "  (primary)" } else { "" }
                    );
                }
            }
            Err(e) => {
                eprintln!("cannot read monitors: {e}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    eprintln!("no X11 backend on this platform");
}
