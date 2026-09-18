//! Draw a virtual desktop and trace the cursor across its seams.
//!
//!     cargo run -p smkvm-layout --example desktop
//!
//! The arrangement below is data, exactly as it will be in the config file.
//! Edit it, re-run, and watch where the cursor lands.

use smkvm_layout::{DeviceId, EdgeOverflow, Layout, Monitor, Point, Rect};

/// `(machine, monitor, local rect, where it sits on the global desktop)`
const ARRANGEMENT: &[(&str, &str, Rect, Point)] = &[
    // Two single-monitor machines on top, centred over the pair below so the
    // vertical seams line up at global x = 2560.
    (
        "WIN-STUDY",
        "primary",
        Rect::new(0, 0, 1920, 1080),
        Point::new(640, 0),
    ),
    (
        "WIN-LAPTOP",
        "primary",
        Rect::new(0, 0, 1920, 1080),
        Point::new(2560, 0),
    ),
    // One machine contributing two monitors, side by side in its own space.
    (
        "ubuntu-box",
        "DP-2",
        Rect::new(0, 0, 2560, 1440),
        Point::new(0, 1080),
    ),
    (
        "ubuntu-box",
        "DP-0",
        Rect::new(2560, 0, 2560, 1440),
        Point::new(2560, 1080),
    ),
];

fn main() {
    let mut layout = Layout::new(EdgeOverflow::Clamp);
    let mut names: Vec<&str> = Vec::new();
    for (machine, ..) in ARRANGEMENT {
        if !names.contains(machine) {
            names.push(machine);
        }
    }
    for (i, machine) in names.iter().enumerate() {
        let id = DeviceId::from_bytes([i as u8 + 1; 32]);
        let monitors = ARRANGEMENT
            .iter()
            .filter(|(m, ..)| m == machine)
            .map(|(_, mon, local, _)| Monitor::new(*mon, *local))
            .collect();
        layout.report_monitors(id, *machine, monitors);
        for (m, mon, _, origin) in ARRANGEMENT {
            if m == machine {
                layout.place(id, &(*mon).into(), *origin);
            }
        }
    }

    let bounds = layout.bounds();
    println!("virtual desktop: {} x {}\n", bounds.w, bounds.h);
    draw(&layout, bounds);

    println!("\ncursor crossings");
    println!("{:-<64}", "");
    for (label, from, dx, dy) in [
        ("up from the middle of DP-2", Point::new(1500, 1085), 0, -10),
        (
            "up from the last pixel of DP-2",
            Point::new(2559, 1085),
            0,
            -10,
        ),
        (
            "up from the first pixel of DP-0",
            Point::new(2560, 1085),
            0,
            -10,
        ),
        (
            "up from the far left, past any machine",
            Point::new(300, 1085),
            0,
            -10,
        ),
        ("right across the top seam", Point::new(2559, 500), 1, 0),
        ("right across the bottom seam", Point::new(2559, 1200), 1, 0),
        (
            "down from the right-hand machine",
            Point::new(3000, 1075),
            0,
            10,
        ),
    ] {
        let m = layout.resolve(from, dx, dy).unwrap();
        let name = layout
            .device(m.located.device)
            .map(|d| d.name.as_str())
            .unwrap_or("?");
        println!(
            "{label:<40} -> {name} / {} at {},{}{}",
            m.located.monitor,
            m.located.local.x,
            m.located.local.y,
            if m.adjusted { "  (nudged)" } else { "" }
        );
    }
}

/// A rough plan view, one character per 160 x 120 pixels.
fn draw(layout: &Layout, bounds: Rect) {
    const SX: i32 = 160;
    const SY: i32 = 120;
    let cells = layout.cells();
    // `div_ceil` is still unstable for signed integers.
    let rows = (bounds.h + SY - 1) / SY;
    let cols = (bounds.w + SX - 1) / SX;
    for row in 0..rows {
        let mut line = String::new();
        let mut label = String::new();
        for col in 0..cols {
            let p = Point::new(bounds.x + col * SX + SX / 2, bounds.y + row * SY + SY / 2);
            match cells.iter().position(|c| c.global.contains(p)) {
                Some(i) => line.push((b'A' + i as u8) as char),
                None => line.push('.'),
            }
        }
        if row == 0 {
            label = "  A/B = the two machines on top".into();
        } else if row == rows / 2 {
            label = "  C/D = one machine, two monitors".into();
        }
        println!("  {line}{label}");
    }
    println!("\n  '.' is empty space: nothing is placed there.");
}
