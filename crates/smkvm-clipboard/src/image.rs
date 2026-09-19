//! Turning Windows bitmaps into PNG, and back.
//!
//! Windows puts images on the clipboard as device-independent bitmaps. Almost
//! everything else asks for `image/png`, which is why an image copied on one
//! machine so often cannot be pasted on another: the two never agree on a
//! format, and nothing converts between them.
//!
//! The conversion is deliberately plain arithmetic over bytes, with no
//! platform calls, so every case can be tested anywhere.

use std::io::Cursor;

use crate::{ClipboardError, Result};

/// The fixed part of a bitmap header, which every version begins with.
const HEADER_V3: usize = 40;
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn i32_at(bytes: &[u8], at: usize) -> i32 {
    u32_at(bytes, at) as i32
}

fn bad(why: &str) -> ClipboardError {
    ClipboardError::Display(format!("the bitmap on the clipboard is unusable: {why}"))
}

/// What a bitmap header says about the image.
struct Info {
    width: u32,
    height: u32,
    /// Bitmaps are stored bottom row first unless the height says otherwise.
    top_down: bool,
    bits_per_pixel: u16,
    /// Where the pixels start, past the header and any masks or palette.
    pixels_at: usize,
}

fn parse_header(dib: &[u8]) -> Result<Info> {
    if dib.len() < HEADER_V3 {
        return Err(bad("it is shorter than a header"));
    }
    let header_size = u32_at(dib, 0) as usize;
    if header_size < HEADER_V3 || header_size > dib.len() {
        return Err(bad("the header size makes no sense"));
    }

    let width = i32_at(dib, 4);
    let raw_height = i32_at(dib, 8);
    let bits_per_pixel = u16_at(dib, 14);
    let compression = u32_at(dib, 16);
    let palette_entries = u32_at(dib, 32);

    if width <= 0 || raw_height == 0 {
        return Err(bad("it has no area"));
    }
    // Only the truecolour forms. A palette image would need its table read as
    // well, and getting that wrong produces an image in the wrong colours
    // rather than an error, which is worse than declining.
    if !matches!(bits_per_pixel, 24 | 32) {
        return Err(bad(&format!(
            "{bits_per_pixel} bits per pixel is not one of the forms this reads"
        )));
    }
    if !matches!(compression, BI_RGB | BI_BITFIELDS) {
        return Err(bad("it is compressed in a form this does not read"));
    }

    // Three colour masks follow a v3 header when the format is BI_BITFIELDS;
    // later headers carry the masks inside themselves.
    let masks = if compression == BI_BITFIELDS && header_size == HEADER_V3 {
        12
    } else {
        0
    };
    let palette = palette_entries as usize * 4;
    let pixels_at = header_size + masks + palette;
    if pixels_at >= dib.len() {
        return Err(bad("it contains no pixels"));
    }

    Ok(Info {
        width: width as u32,
        height: raw_height.unsigned_abs(),
        top_down: raw_height < 0,
        bits_per_pixel,
        pixels_at,
    })
}

/// Convert a device-independent bitmap to PNG.
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    let info = parse_header(dib)?;
    let bytes_per_pixel = usize::from(info.bits_per_pixel / 8);
    // Each row is padded out to a four-byte boundary.
    let stride = (info.width as usize * bytes_per_pixel + 3) & !3;
    let needed = stride * info.height as usize;
    if dib.len() < info.pixels_at + needed {
        return Err(bad("it is shorter than its own dimensions require"));
    }
    let pixels = &dib[info.pixels_at..];

    let mut rgba = Vec::with_capacity(info.width as usize * info.height as usize * 4);
    // A 32-bit bitmap's fourth byte is officially unused, and plenty of
    // applications leave it zero. Taking that as "fully transparent" would
    // turn a perfectly good image invisible, so it only counts as alpha when
    // something in it is not zero.
    let has_alpha = info.bits_per_pixel == 32
        && (0..info.height as usize).any(|row| {
            let start = row * stride;
            pixels[start..start + info.width as usize * 4]
                .chunks_exact(4)
                .any(|px| px[3] != 0)
        });

    for row in 0..info.height as usize {
        let source = if info.top_down {
            row
        } else {
            info.height as usize - 1 - row
        };
        let start = source * stride;
        for x in 0..info.width as usize {
            let px = &pixels[start + x * bytes_per_pixel..];
            // Bitmaps store blue first.
            rgba.push(px[2]);
            rgba.push(px[1]);
            rgba.push(px[0]);
            rgba.push(if has_alpha { px[3] } else { 0xFF });
        }
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(Cursor::new(&mut out), info.width, info.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| ClipboardError::Display(format!("could not write a PNG: {e}")))?;
        writer
            .write_image_data(&rgba)
            .map_err(|e| ClipboardError::Display(format!("could not write a PNG: {e}")))?;
    }
    Ok(out)
}

/// Which bitmap header to write.
#[derive(Clone, Copy)]
enum Header {
    /// The 40-byte `BITMAPINFOHEADER`, `BI_RGB`, 32 bits a pixel. This is
    /// what `CF_DIB` is defined to hold, and the only form Windows will turn
    /// into a `CF_BITMAP` for the many applications that ask for one. It has
    /// nowhere to declare the fourth byte as alpha; applications treat it as
    /// padding, and an image with transparency comes out opaque.
    V3,
    /// The 124-byte `BITMAPV5HEADER`, `BI_BITFIELDS`, with masks that say
    /// where the alpha lives. This is what `CF_DIBV5` holds, and carries
    /// transparency across intact for the applications that read it.
    V5,
}

impl Header {
    fn len(self) -> usize {
        match self {
            Header::V3 => HEADER_V3,
            Header::V5 => 124,
        }
    }
}

/// Convert a PNG to the bitmap `CF_DIB` holds: a plain 40-byte header.
///
/// Rows are written bottom first, the arrangement every application
/// understands. Transparency is carried in the fourth byte, where our own
/// reading finds it again, but Windows does not know it is there; use
/// [`png_to_dibv5`] for the form that declares it.
pub fn png_to_dib(png_bytes: &[u8]) -> Result<Vec<u8>> {
    png_to_bitmap(png_bytes, Header::V3)
}

/// Convert a PNG to the bitmap `CF_DIBV5` holds, with its alpha declared.
pub fn png_to_dibv5(png_bytes: &[u8]) -> Result<Vec<u8>> {
    png_to_bitmap(png_bytes, Header::V5)
}

fn png_to_bitmap(png_bytes: &[u8], header: Header) -> Result<Vec<u8>> {
    let decoder = png::Decoder::new(Cursor::new(png_bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|e| ClipboardError::Display(format!("that is not a PNG this reads: {e}")))?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|e| ClipboardError::Display(format!("could not read the PNG: {e}")))?;

    let (width, height) = (frame.width, frame.height);
    let source = &buffer[..frame.buffer_size()];
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        other => {
            return Err(ClipboardError::Display(format!(
                "a {other:?} PNG is not one of the forms this converts"
            )))
        }
    };
    if frame.bit_depth != png::BitDepth::Eight {
        return Err(ClipboardError::Display(
            "only eight bits per channel are converted".into(),
        ));
    }

    let header_len = header.len();
    let stride = width as usize * 4;
    let pixels_len = stride * height as usize;
    let mut dib = vec![0u8; header_len + pixels_len];

    let put32 = |dib: &mut [u8], at: usize, value: u32| {
        dib[at..at + 4].copy_from_slice(&value.to_le_bytes());
    };
    put32(&mut dib, 0, header_len as u32);
    put32(&mut dib, 4, width);
    put32(&mut dib, 8, height); // positive: rows run bottom to top
    dib[12..14].copy_from_slice(&1u16.to_le_bytes()); // one plane
    dib[14..16].copy_from_slice(&32u16.to_le_bytes());
    put32(&mut dib, 20, pixels_len as u32);
    match header {
        Header::V3 => put32(&mut dib, 16, BI_RGB),
        Header::V5 => {
            put32(&mut dib, 16, BI_BITFIELDS);
            put32(&mut dib, 40, 0x00FF_0000); // red
            put32(&mut dib, 44, 0x0000_FF00); // green
            put32(&mut dib, 48, 0x0000_00FF); // blue
            put32(&mut dib, 52, 0xFF00_0000); // alpha
            put32(&mut dib, 56, 0x7352_4742); // 'BGRs': sRGB
        }
    }

    for row in 0..height as usize {
        // Bottom first.
        let source_row = height as usize - 1 - row;
        let from = source_row * width as usize * channels;
        let to = header_len + row * stride;
        for x in 0..width as usize {
            let px = &source[from + x * channels..];
            let (r, g, b) = (px[0], px[1], px[2]);
            let a = if channels == 4 { px[3] } else { 0xFF };
            let at = to + x * 4;
            dib[at] = b;
            dib[at + 1] = g;
            dib[at + 2] = r;
            dib[at + 3] = a;
        }
    }
    Ok(dib)
}
