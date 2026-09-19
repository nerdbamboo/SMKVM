//! The list of files Windows puts on the clipboard, and the list everything
//! else uses.
//!
//! Windows carries copied files as a small header followed by a run of paths
//! ending in two nulls. The rest of the world uses `text/uri-list`: one
//! `file://` URI per line. Copying files between the two means translating,
//! since neither will read the other's form.
//!
//! Pure byte handling, so it is tested wherever this is built rather than only
//! on Windows.

use crate::{ClipboardError, Result};

/// `DROPFILES`: an offset to the paths, a point, and two flags.
const HEADER: usize = 20;
/// Where the offset to the path list sits.
const OFFSET_AT: usize = 0;
/// Where the flag saying the paths are wide characters sits.
const WIDE_AT: usize = 16;

fn bad(why: &str) -> ClipboardError {
    ClipboardError::Display(format!("the file list on the clipboard is unusable: {why}"))
}

/// Turn the Windows form into `text/uri-list`.
pub fn hdrop_to_uri_list(hdrop: &[u8]) -> Result<Vec<u8>> {
    if hdrop.len() < HEADER {
        return Err(bad("it is shorter than a header"));
    }
    let offset = u32::from_le_bytes([hdrop[0], hdrop[1], hdrop[2], hdrop[3]]) as usize;
    let wide = u32::from_le_bytes([
        hdrop[WIDE_AT],
        hdrop[WIDE_AT + 1],
        hdrop[WIDE_AT + 2],
        hdrop[WIDE_AT + 3],
    ]) != 0;
    if offset < HEADER || offset > hdrop.len() {
        return Err(bad("the paths begin outside it"));
    }
    let _ = OFFSET_AT;

    let paths: Vec<String> = if wide {
        let units: Vec<u16> = hdrop[offset..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        units
            .split(|c| *c == 0)
            .take_while(|part| !part.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    } else {
        hdrop[offset..]
            .split(|b| *b == 0)
            .take_while(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect()
    };

    let mut out = String::new();
    for path in paths {
        out.push_str(&to_uri(&path));
        out.push_str("\r\n");
    }
    Ok(out.into_bytes())
}

/// Turn `text/uri-list` into the Windows form.
pub fn uri_list_to_hdrop(uri_list: &[u8]) -> Result<Vec<u8>> {
    let text = String::from_utf8_lossy(uri_list);
    let paths: Vec<String> = text
        .lines()
        .map(str::trim)
        // A uri-list may carry comments, which are not files.
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(from_uri)
        .collect();
    if paths.is_empty() {
        return Err(bad("it names no files"));
    }

    let mut out = vec![0u8; HEADER];
    out[0..4].copy_from_slice(&(HEADER as u32).to_le_bytes());
    // Wide characters, so paths outside the system's code page survive.
    out[WIDE_AT..WIDE_AT + 4].copy_from_slice(&1u32.to_le_bytes());
    for path in &paths {
        for unit in path.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    // A second null ends the run.
    out.extend_from_slice(&0u16.to_le_bytes());
    Ok(out)
}

/// `C:\dir\file name.txt` becomes `file:///C:/dir/file%20name.txt`.
fn to_uri(path: &str) -> String {
    let mut out = String::from("file:///");
    for byte in path.replace('\\', "/").trim_start_matches('/').bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The reverse, tolerating a plain path where a URI was expected.
fn from_uri(line: &str) -> Option<String> {
    let rest = match line.strip_prefix("file://") {
        // `file://host/path` is not something to hand to Windows as a local
        // path, and guessing would produce a path to nowhere.
        Some(rest) if !rest.starts_with('/') => return None,
        Some(rest) => rest.trim_start_matches('/'),
        // Not a URI at all: some sources put bare paths on the clipboard.
        None if line.contains(":\\") || line.starts_with('/') => return Some(line.to_string()),
        None => return None,
    };

    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                decoded.push(byte);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    let path = String::from_utf8_lossy(&decoded).replace('/', "\\");
    (!path.is_empty()).then_some(path)
}

/// The local paths named by a `text/uri-list`, on this platform.
///
/// Only `file:` URIs for this machine are paths here: one naming another host
/// is dropped, as is anything that is not a URI. On Windows the leading slash
/// before a drive letter goes and the separators turn round; elsewhere the
/// path is taken as it decodes.
pub fn local_paths(uri_list: &[u8]) -> Vec<std::path::PathBuf> {
    String::from_utf8_lossy(uri_list)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(local_path)
        .collect()
}

fn local_path(line: &str) -> Option<std::path::PathBuf> {
    let rest = line.strip_prefix("file://")?;
    // `file://host/path` belongs to `host`; only an empty host, or
    // `localhost`, is this machine.
    let rest = match rest.strip_prefix("localhost/") {
        Some(after) => after,
        None => rest.strip_prefix('/')?,
    };
    let decoded = percent_decode(rest);
    let text = String::from_utf8_lossy(&decoded);
    if text.is_empty() {
        return None;
    }
    if cfg!(windows) {
        Some(std::path::PathBuf::from(text.replace('/', "\\")))
    } else {
        Some(std::path::PathBuf::from(format!("/{text}")))
    }
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// A `text/uri-list` naming these local paths, one `file:` URI a line.
pub fn uri_list(paths: &[std::path::PathBuf]) -> Vec<u8> {
    let mut out = String::new();
    for path in paths {
        out.push_str(&to_uri(&path.to_string_lossy()));
        out.push_str("\r\n");
    }
    out.into_bytes()
}

#[cfg(test)]
mod local_path_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_uri_for_this_machine_becomes_a_path_and_back() {
        let list = b"file:///tmp/some%20dir/a%23b.txt\r\n# a comment\r\nfile://elsewhere/x\r\n";
        let paths = local_paths(list);
        if cfg!(windows) {
            assert_eq!(paths, vec![PathBuf::from("tmp\\some dir\\a#b.txt")]);
        } else {
            assert_eq!(paths, vec![PathBuf::from("/tmp/some dir/a#b.txt")]);
            let back = uri_list(&paths);
            assert_eq!(back, b"file:///tmp/some%20dir/a%23b.txt\r\n");
        }
    }

    #[test]
    fn localhost_counts_as_here_and_nonsense_does_not() {
        let paths = local_paths(b"file://localhost/var/y\nnot a uri\nhttp://x/y\n");
        if cfg!(windows) {
            assert_eq!(paths, vec![PathBuf::from("var\\y")]);
        } else {
            assert_eq!(paths, vec![PathBuf::from("/var/y")]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_path_survives_the_round_trip() {
        let paths = vec![PathBuf::from("C:\\Users\\me\\a b.txt")];
        let list = uri_list(&paths);
        assert_eq!(list, b"file:///C:/Users/me/a%20b.txt\r\n");
        assert_eq!(local_paths(&list), paths);
    }
}
