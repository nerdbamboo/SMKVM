//! The wrapper Windows puts around HTML on the clipboard.

use smkvm_clipboard::html;

#[test]
fn a_real_block_gives_up_just_the_selection() {
    // What a browser actually puts on the clipboard: a header of byte offsets,
    // then markup with the selected part marked off by comments.
    let body =
        "<html><body>\r\n<!--StartFragment--><b>hi</b><!--EndFragment-->\r\n</body>\r\n</html>";

    // The offsets count from the start of the whole block, so they depend on
    // the header's own length. Measuring it rather than guessing is the same
    // trick the implementation uses.
    let header_of = |start_html: usize, end_html: usize, start: usize, end: usize| {
        format!(
            "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\n\
             StartFragment:{start:010}\r\nEndFragment:{end:010}\r\n"
        )
    };
    let header_len = header_of(0, 0, 0, 0).len();
    let start_fragment = header_len + "<html><body>\r\n<!--StartFragment-->".len();
    let end_fragment = start_fragment + "<b>hi</b>".len();
    let header = header_of(
        header_len,
        header_len + body.len(),
        start_fragment,
        end_fragment,
    );
    assert_eq!(header.len(), header_len, "the header changed size");

    let block = format!("{header}{body}").into_bytes();
    assert_eq!(html::unwrap(&block), b"<b>hi</b>");
}

#[test]
fn wrapping_and_unwrapping_give_back_what_went_in() {
    for fragment in [
        &b"<b>hello</b>"[..],
        b"",
        b"<p>a longer piece with <i>nesting</i> and &amp; entities</p>",
        // Text outside ASCII, since the header counts bytes and not characters.
        "<p>\u{d55c}\u{ad6d}\u{c5b4}</p>".as_bytes(),
    ] {
        let wrapped = html::wrap(fragment);
        assert_eq!(html::unwrap(&wrapped), fragment, "round trip changed it");
        // And the block really does start with the header, so Windows will
        // accept it.
        assert!(wrapped.starts_with(b"Version:0.9\r\nStartHTML:"));
    }
}

#[test]
fn bare_markup_with_no_header_is_left_alone() {
    // Not everything that puts HTML on the clipboard writes the header.
    // Treating that as a malformed block and returning nothing would lose it.
    let bare = b"<b>no header here</b>";
    assert_eq!(html::unwrap(bare), bare);
}

#[test]
fn a_header_with_no_fragment_marked_still_yields_its_markup() {
    let block = b"Version:0.9\r\nStartHTML:0000000045\r\nEndHTML:0000000061\r\n<html>body</html>";
    let got = html::unwrap(block);
    assert!(
        String::from_utf8_lossy(&got).contains("body"),
        "got {:?}",
        String::from_utf8_lossy(&got)
    );
}

#[test]
fn offsets_that_point_outside_the_block_do_not_panic() {
    // The block comes from another application, so its numbers are not this
    // program's to trust.
    let cases: Vec<Vec<u8>> = vec![
        b"Version:0.9\r\nStartFragment:0000009999\r\nEndFragment:0000099999\r\n<b>x</b>".to_vec(),
        b"Version:0.9\r\nStartFragment:0000000050\r\nEndFragment:0000000010\r\nxx".to_vec(),
        b"StartFragment:\r\nEndFragment:\r\n".to_vec(),
        b"Version:0.9\r\nStartHTML:9999999999\r\n".to_vec(),
        vec![0xFF; 64],
        Vec::new(),
    ];
    for case in cases {
        let _ = html::unwrap(&case);
    }
}
