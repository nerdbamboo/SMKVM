//! Show what an existing Barrier installation migrates to.
//!
//!     cargo run -p smkvm-config --example import -- [qt-settings] [barrier.conf]
//!
//! Reads without writing anything, so it is safe to run against a live setup.

use std::path::PathBuf;

use smkvm_config::barrier::{Import, ServerConfig};
use smkvm_config::Config;

fn main() {
    let mut args = std::env::args().skip(1);
    let settings = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(default_qt_settings);
    let server_config = args.next().map(PathBuf::from);

    let text = match std::fs::read_to_string(&settings) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read {}: {e}", settings.display());
            std::process::exit(1);
        }
    };

    let mut import = Import::from_qt_settings(&text);
    println!("# read {}", settings.display());
    if let Some(path) = &server_config {
        match std::fs::read_to_string(path) {
            Ok(t) => {
                import.merge_server_config(&ServerConfig::parse(&t));
                println!("# read {}", path.display());
            }
            Err(e) => eprintln!("# skipped {}: {e}", path.display()),
        }
    }

    println!("#");
    println!("# Not carried over:");
    for note in &import.notes {
        for (i, line) in textwrap(note, 74).into_iter().enumerate() {
            println!("#   {}{line}", if i == 0 { "- " } else { "  " });
        }
    }
    println!();

    match Config::from_barrier(&import).to_toml() {
        Ok(toml) => print!("{toml}"),
        Err(e) => {
            eprintln!("could not render configuration: {e}");
            std::process::exit(1);
        }
    }
}

fn default_qt_settings() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/Debauchee/Barrier.conf")
}

fn textwrap(s: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}
