//! Translating between Windows' file list and `text/uri-list`.

use smkvm_clipboard::files::{hdrop_to_uri_list, uri_list_to_hdrop};

/// Build the Windows form by hand, the way an application would.
fn hdrop(paths: &[&str], wide: bool) -> Vec<u8> {
    let mut out = vec![0u8; 20];
    out[0..4].copy_from_slice(&20u32.to_le_bytes());
    out[16..20].copy_from_slice(&u32::from(wide).to_le_bytes());
    for path in paths {
        if wide {
            for unit in path.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
            out.extend_from_slice(&0u16.to_le_bytes());
        } else {
            out.extend_from_slice(path.as_bytes());
            out.push(0);
        }
    }
    if wide {
        out.extend_from_slice(&0u16.to_le_bytes());
    } else {
        out.push(0);
    }
    out
}

fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn copied_files_become_uris() {
    let got = hdrop_to_uri_list(&hdrop(
        &[r"C:\Users\USER\notes.txt", r"D:\pictures\a b.png"],
        true,
    ))
    .expect("translates");
    assert_eq!(
        lines(&got),
        vec![
            "file:///C:/Users/USER/notes.txt",
            // A space cannot appear in a URI as itself.
            "file:///D:/pictures/a%20b.png",
        ]
    );
}

#[test]
fn the_narrow_form_is_read_too() {
    // Older applications write bytes rather than wide characters.
    let got = hdrop_to_uri_list(&hdrop(&[r"C:\temp\x.txt"], false)).expect("translates");
    assert_eq!(lines(&got), vec!["file:///C:/temp/x.txt"]);
}

#[test]
fn uris_become_a_list_windows_will_take() {
    let list = b"file:///C:/Users/USER/notes.txt\r\nfile:///D:/pictures/a%20b.png\r\n";
    let built = uri_list_to_hdrop(list).expect("translates");
    // And it reads back as what went in.
    assert_eq!(
        lines(&hdrop_to_uri_list(&built).expect("reads back")),
        vec![
            "file:///C:/Users/USER/notes.txt",
            "file:///D:/pictures/a%20b.png"
        ]
    );
}

#[test]
fn a_path_outside_ascii_survives_the_trip() {
    let path = r"C:\Users\USER\한글 문서.txt";
    let uris = hdrop_to_uri_list(&hdrop(&[path], true)).expect("translates");
    let back = uri_list_to_hdrop(&uris).expect("and back");
    let again = hdrop_to_uri_list(&back).expect("and once more");
    assert_eq!(lines(&uris), lines(&again));
    // Wide characters are used precisely so this is possible.
    assert_eq!(
        u32::from_le_bytes([back[16], back[17], back[18], back[19]]),
        1
    );
}

#[test]
fn comments_and_blank_lines_are_not_files() {
    let list = b"# a comment\r\n\r\nfile:///C:/a.txt\r\n";
    let built = uri_list_to_hdrop(list).expect("translates");
    assert_eq!(lines(&hdrop_to_uri_list(&built).unwrap()).len(), 1);
}

#[test]
fn a_uri_naming_another_machine_is_not_treated_as_a_local_path() {
    // `file://server/share` is somewhere else. Guessing a local path from it
    // would produce a path to nothing.
    assert!(uri_list_to_hdrop(b"file://server/share/x.txt\r\n").is_err());
}

#[test]
fn a_list_naming_nothing_is_an_error_rather_than_an_empty_drop() {
    assert!(uri_list_to_hdrop(b"").is_err());
    assert!(uri_list_to_hdrop(b"# only a comment\r\n").is_err());
}

#[test]
fn nonsense_never_panics() {
    // Both forms arrive from another application, so neither is this
    // program's to trust.
    for len in [0usize, 1, 19, 20, 21, 64] {
        for fill in [0x00u8, 0xFF, 0x41] {
            let _ = hdrop_to_uri_list(&vec![fill; len]);
            let _ = uri_list_to_hdrop(&vec![fill; len]);
        }
    }
    // A header whose paths begin past the end of it.
    let mut broken = vec![0u8; 20];
    broken[0..4].copy_from_slice(&9999u32.to_le_bytes());
    assert!(hdrop_to_uri_list(&broken).is_err());
}
