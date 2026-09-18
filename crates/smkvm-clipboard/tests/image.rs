//! Converting between Windows bitmaps and PNG.
//!
//! Pure arithmetic over bytes, so all of it runs anywhere. The awkward parts
//! are the ones a casual implementation gets wrong: bitmaps are stored bottom
//! row first, rows are padded, and the fourth byte of a 32-bit pixel may or
//! may not mean anything.

use smkvm_clipboard::image::{dib_to_png, png_to_dib};

const HEADER_V3: usize = 40;

/// Build a bitmap by hand, the way an application would put one on the
/// clipboard. A negative height means the rows run top to bottom.
fn dib(width: i32, height: i32, bits: u16, pixels: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_V3];
    out[0..4].copy_from_slice(&(HEADER_V3 as u32).to_le_bytes());
    out[4..8].copy_from_slice(&width.to_le_bytes());
    out[8..12].copy_from_slice(&height.to_le_bytes());
    out[12..14].copy_from_slice(&1u16.to_le_bytes());
    out[14..16].copy_from_slice(&bits.to_le_bytes());
    // BI_RGB
    out[16..20].copy_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(pixels);
    out
}

/// Read a PNG back to plain RGBA so pixels can be compared.
fn png_pixels(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("a PNG came out");
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let frame = reader
        .next_frame(&mut buffer)
        .expect("its pixels read back");
    assert_eq!(frame.color_type, png::ColorType::Rgba);
    (
        frame.width,
        frame.height,
        buffer[..frame.buffer_size()].to_vec(),
    )
}

#[test]
fn rows_stored_bottom_first_come_out_the_right_way_up() {
    // Two rows, one red and one blue. Stored bottom first, so the red row on
    // the wire is the *lower* one in the picture.
    let pixels = [
        0x00, 0x00, 0xFF, // bottom row: red, stored blue-green-red
        0xFF, 0x00, 0x00, // top row: blue
    ];
    // One pixel of 24-bit colour is three bytes, padded out to four.
    let mut padded = Vec::new();
    padded.extend_from_slice(&pixels[0..3]);
    padded.push(0);
    padded.extend_from_slice(&pixels[3..6]);
    padded.push(0);

    let png = dib_to_png(&dib(1, 2, 24, &padded)).expect("converts");
    let (w, h, rgba) = png_pixels(&png);
    assert_eq!((w, h), (1, 2));
    assert_eq!(&rgba[0..4], &[0x00, 0x00, 0xFF, 0xFF], "top should be blue");
    assert_eq!(
        &rgba[4..8],
        &[0xFF, 0x00, 0x00, 0xFF],
        "bottom should be red"
    );
}

#[test]
fn a_negative_height_means_the_rows_are_already_in_order() {
    let mut padded = Vec::new();
    padded.extend_from_slice(&[0x00, 0x00, 0xFF]); // red, and it is the top row
    padded.push(0);
    padded.extend_from_slice(&[0xFF, 0x00, 0x00]); // blue
    padded.push(0);

    let png = dib_to_png(&dib(1, -2, 24, &padded)).expect("converts");
    let (_, _, rgba) = png_pixels(&png);
    assert_eq!(&rgba[0..4], &[0xFF, 0x00, 0x00, 0xFF], "top should be red");
    assert_eq!(
        &rgba[4..8],
        &[0x00, 0x00, 0xFF, 0xFF],
        "bottom should be blue"
    );
}

#[test]
fn rows_are_padded_out_to_four_bytes() {
    // Three pixels of 24-bit colour is nine bytes, which pads to twelve.
    // Reading without accounting for that shears the image.
    let row: Vec<u8> = vec![
        0x11, 0x22, 0x33, // pixel 0
        0x44, 0x55, 0x66, // pixel 1
        0x77, 0x88, 0x99, // pixel 2
        0xAA, 0xBB, 0xCC, // padding, and not pixels
    ];
    let png = dib_to_png(&dib(3, 1, 24, &row)).expect("converts");
    let (w, h, rgba) = png_pixels(&png);
    assert_eq!((w, h), (3, 1));
    assert_eq!(&rgba[0..4], &[0x33, 0x22, 0x11, 0xFF]);
    assert_eq!(&rgba[4..8], &[0x66, 0x55, 0x44, 0xFF]);
    assert_eq!(&rgba[8..12], &[0x99, 0x88, 0x77, 0xFF]);
}

#[test]
fn a_thirty_two_bit_image_with_no_alpha_set_is_opaque_not_invisible() {
    // The fourth byte is officially unused and plenty of applications leave it
    // zero. Believing it would turn a perfectly good image completely
    // transparent, which looks exactly like the paste having failed.
    let pixels = vec![
        0x33, 0x22, 0x11, 0x00, // blue, green, red, and a zero where alpha sits
        0x66, 0x55, 0x44, 0x00,
    ];
    let png = dib_to_png(&dib(2, 1, 32, &pixels)).expect("converts");
    let (_, _, rgba) = png_pixels(&png);
    assert_eq!(rgba[3], 0xFF, "the first pixel vanished");
    assert_eq!(rgba[7], 0xFF, "the second pixel vanished");
    assert_eq!(&rgba[0..3], &[0x11, 0x22, 0x33]);
}

#[test]
fn real_transparency_is_kept() {
    let pixels = vec![
        0x33, 0x22, 0x11, 0x80, // half transparent
        0x66, 0x55, 0x44, 0xFF, // opaque
    ];
    let png = dib_to_png(&dib(2, 1, 32, &pixels)).expect("converts");
    let (_, _, rgba) = png_pixels(&png);
    assert_eq!(rgba[3], 0x80);
    assert_eq!(rgba[7], 0xFF);
}

#[test]
fn a_picture_survives_the_trip_out_and_back() {
    // What actually happens when an image is copied on one machine and pasted
    // on another: PNG on the wire, a bitmap at each end.
    let (w, h) = (7u32, 5u32); // deliberately not a multiple of four
    let mut rgba = Vec::new();
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[
                (x * 30) as u8,
                (y * 50) as u8,
                ((x + y) * 20) as u8,
                if (x + y) % 3 == 0 { 0x80 } else { 0xFF },
            ]);
        }
    }
    let mut original = Vec::new();
    {
        let mut encoder = png::Encoder::new(std::io::Cursor::new(&mut original), w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&rgba).unwrap();
    }

    let bitmap = png_to_dib(&original).expect("a bitmap comes out");
    let back = dib_to_png(&bitmap).expect("and a PNG goes back");
    let (bw, bh, got) = png_pixels(&back);
    assert_eq!((bw, bh), (w, h));
    assert_eq!(got, rgba, "the picture changed on the way round");
}

#[test]
fn a_form_this_cannot_read_is_refused_rather_than_guessed_at() {
    // Eight bits per pixel means a colour table, and reading it wrongly would
    // produce an image in the wrong colours -- which is worse than declining,
    // because nobody would know.
    assert!(dib_to_png(&dib(2, 2, 8, &[0u8; 16])).is_err());
    // A header that promises more than it delivers.
    assert!(dib_to_png(&dib(1000, 1000, 32, &[0u8; 16])).is_err());
    // Not a bitmap at all.
    assert!(dib_to_png(&[0u8; 8]).is_err());
    assert!(dib_to_png(&[]).is_err());
}

#[test]
fn nonsense_never_panics() {
    // Clipboard contents come from another application, so the shape of them
    // is not this program's to trust.
    for len in [0usize, 1, 39, 40, 41, 100, 512] {
        for fill in [0x00u8, 0xFF, 0x41] {
            let _ = dib_to_png(&vec![fill; len]);
            let _ = png_to_dib(&vec![fill; len]);
        }
    }
}
