//! The wrapper Windows puts around HTML on the clipboard.
//!
//! Windows does not carry HTML as itself. It carries a small plain-text header
//! giving byte offsets into what follows, then the markup, with the part that
//! was actually selected marked off by comments. An application that hands the
//! whole thing across unchanged produces a paste beginning `Version:0.9`, and
//! one that strips the header without reading it loses the selection.
//!
//! Offsets count bytes from the start of the whole block, and are written as
//! fixed-width decimal so the header's own length does not change as they are
//! filled in.

/// Pull the fragment out of a Windows HTML clipboard block.
///
/// Falls back to everything after the header when no fragment is marked, and
/// to the input unchanged when there is no header at all -- some applications
/// put bare markup on the clipboard regardless.
pub fn unwrap(block: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(block);
    let Some((start, end)) = fragment_bounds(&text) else {
        return match header_end(&text) {
            Some(at) => block[at.min(block.len())..].to_vec(),
            None => block.to_vec(),
        };
    };
    let (start, end) = (start.min(block.len()), end.min(block.len()));
    if start >= end {
        return Vec::new();
    }
    block[start..end].to_vec()
}

/// Wrap a fragment of HTML the way Windows expects it.
pub fn wrap(fragment: &[u8]) -> Vec<u8> {
    const BEFORE: &str = "<html><body>\r\n<!--StartFragment-->";
    const AFTER: &str = "<!--EndFragment-->\r\n</body>\r\n</html>";

    // The offsets depend on the header's length, and the header contains the
    // offsets. Writing them at a fixed width keeps that from circling: the
    // length is known before the numbers are.
    let template = "Version:0.9\r\n\
         StartHTML:{:010}\r\n\
         EndHTML:{:010}\r\n\
         StartFragment:{:010}\r\n\
         EndFragment:{:010}\r\n";
    let header_len = template
        .replace("{:010}", "0000000000")
        .replace('\\', "")
        .len();

    let start_html = header_len;
    let start_fragment = start_html + BEFORE.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + AFTER.len();

    let mut out = format!(
        "Version:0.9\r\n\
         StartHTML:{start_html:010}\r\n\
         EndHTML:{end_html:010}\r\n\
         StartFragment:{start_fragment:010}\r\n\
         EndFragment:{end_fragment:010}\r\n"
    )
    .into_bytes();
    debug_assert_eq!(out.len(), header_len, "the header changed size");
    out.extend_from_slice(BEFORE.as_bytes());
    out.extend_from_slice(fragment);
    out.extend_from_slice(AFTER.as_bytes());
    out
}

fn number_after(text: &str, key: &str) -> Option<usize> {
    let at = text.find(key)? + key.len();
    let rest = &text[at..];
    let digits: String = rest
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

fn fragment_bounds(text: &str) -> Option<(usize, usize)> {
    let start = number_after(text, "StartFragment:")?;
    let end = number_after(text, "EndFragment:")?;
    (start <= end).then_some((start, end))
}

fn header_end(text: &str) -> Option<usize> {
    number_after(text, "StartHTML:")
}
