//! Kitty graphics transport for the waveform image.
//!
//! A wide strip of the track is rasterised and transmitted once; every frame then places a source
//! rectangle of that strip into the deck rectangle. `a=p` reuses the transmitted image, so a 60Hz
//! scroll costs a short escape sequence rather than a new bitmap. The strip is rebuilt only when
//! the viewport nears its edge, or when zoom/track/size changes.
//!
//! The strip is rendered at the terminal's real cell-pixel size. That is what keeps waveform edges
//! crisp: Kitty has no nearest-neighbour switch, so the source pixels must land 1:1 on the cells.

use std::fmt::Write;

use ratatui::layout::Rect;

use super::raster::{Palette, Raster};

/// The part of the transmitted strip currently on screen, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Transmit the whole strip and show `source` at the cursor. Used when a strip is (re)built.
pub fn transmit(
    raster: &Raster,
    palette: &Palette,
    id: u32,
    placement: u32,
    area: Rect,
    source: SourceRect,
) -> String {
    let rgba = raster.rgba(palette);
    let encoded = base64(&rgba);
    let mut out = String::with_capacity(encoded.len() + 256);
    clear_area(&mut out, area);

    const CHUNK: usize = 4096;
    let chunks = encoded.as_bytes().chunks(CHUNK);
    let count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        out.push_str("\x1b_G");
        if index == 0 {
            write!(
                out,
                "i={id},a=T,f=32,s={},v={},c={},r={},p={placement},z=-1,C=1,q=2,\
                 x={},y={},w={},h={},",
                raster.width,
                raster.height,
                area.width,
                area.height,
                source.x,
                source.y,
                source.width,
                source.height
            )
            .expect("writing to a String cannot fail");
        } else {
            out.push_str("q=2,");
        }
        out.push_str(if index + 1 < count { "m=1;" } else { "m=0;" });
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push_str("\x1b\\");
    }
    // `C=1` leaves the cursor on the image's top-left cell; ratatui thinks the forced-width symbol
    // occupies one cell, so move it one cell right to keep both in sync.
    out.push_str("\x1b[1C");
    out
}

/// Re-place the already transmitted image, showing a different source rectangle.
pub fn place(id: u32, placement: u32, area: Rect, source: SourceRect) -> String {
    let mut out = String::with_capacity(192);
    clear_area(&mut out, area);
    write!(
        out,
        "\x1b_Ga=p,i={id},p={placement},q=2,z=-1,C=1,c={},r={},\
         x={},y={},w={},h={}\x1b\\",
        area.width,
        area.height,
        source.x,
        source.y,
        source.width,
        source.height
    )
    .expect("writing to a String cannot fail");
    out.push_str("\x1b[1C");
    out
}

/// Erase the character cells of `area` without moving the final cursor away from its origin.
fn clear_area(out: &mut String, area: Rect) {
    out.push_str("\x1b[0m\x1b7");
    for row in 0..area.height {
        write!(
            out,
            "\x1b[{};{}H\x1b[{}X",
            area.y + row + 1,
            area.x + 1,
            area.width
        )
        .expect("writing to a String cannot fail");
    }
    out.push_str("\x1b8");
}

const ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let triple = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> SourceRect {
        SourceRect {
            x: 20,
            y: 0,
            width: 40,
            height: 8,
        }
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn transmit_carries_the_strip_and_the_visible_crop() {
        let raster = Raster {
            width: 4,
            height: 4,
            indices: vec![0; 16],
        };
        let palette = Palette::from_colors(crate::preference::WaveformColors::default());
        let seq = transmit(&raster, &palette, 7, 1, Rect::new(2, 3, 10, 2), source());
        assert!(seq.contains("a=T"));
        assert!(seq.contains("i=7") && seq.contains("p=1"));
        assert!(seq.contains("s=4,v=4") && seq.contains("c=10,r=2"));
        assert!(seq.contains("x=20,y=0,w=40,h=8"), "{seq}");
        assert!(seq.contains("z=-1") && seq.contains("C=1"));
        assert!(seq.contains("\x1b7") && seq.contains("\x1b8"));
    }

    #[test]
    fn place_reuses_the_image_without_a_transmission() {
        let seq = place(7, 1, Rect::new(2, 3, 10, 2), source());
        assert!(seq.contains("a=p"));
        assert!(seq.contains("i=7") && seq.contains("p=1"));
        assert!(seq.contains("x=20,y=0,w=40,h=8"));
        assert!(!seq.contains("f=32"), "placement must not carry pixels: {seq}");
        assert!(!seq.contains("a=T"));
    }
}
