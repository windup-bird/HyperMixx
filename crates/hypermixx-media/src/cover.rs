//! Embedded cover art, for the deck header.
//!
//! Two sources, in this order:
//!
//! 1. The container's own metadata reader (symphonia): FLAC picture blocks and anything else a
//!    format reader exposes as a `Visual`.
//! 2. A hand-written ID3v2 `APIC` scan, because symphonia 0.5's MP3 demuxer does not read ID3 at
//!    all — and MP3 is what a DJ library is mostly made of.
//!
//! The result is deliberately tiny (a thumbnail, not the original): the TUI draws it into a
//! handful of character cells, and holding a 1 MB JPEG per deck for that would be absurd.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{Limit, MetadataOptions};
use symphonia::core::probe::Hint;

/// Longest side of the decoded thumbnail, in pixels. A TUI cover is a few character cells wide, and
/// a character cell holds at most two vertical pixels — 48 leaves room for the image backend and
/// costs a few kilobytes.
const THUMBNAIL_PX: u32 = 48;
/// Refuse to pull anything bigger than this into memory. Real covers are well under 2 MB; past
/// this it is a mis-tagged file rather than art worth decoding.
const MAX_EMBEDDED_BYTES: usize = 8 * 1024 * 1024;

/// A decoded cover thumbnail: straight RGBA, row-major, `width * height * 4` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cover {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// The mean colour, so a renderer that cannot draw pixels still shows something tied to the
    /// artwork instead of an arbitrary placeholder.
    pub average: [u8; 3],
}

impl Cover {
    /// The pixel at `(x, y)`, clamped to the image. Small renderers sample with it.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        if self.width == 0 || self.height == 0 || self.rgba.len() < 4 {
            return [0, 0, 0, 255];
        }
        let x = x.min(self.width - 1);
        let y = y.min(self.height - 1);
        let offset = ((y * self.width + x) * 4) as usize;
        match self.rgba.get(offset..offset + 4) {
            Some(px) => [px[0], px[1], px[2], px[3]],
            None => [0, 0, 0, 255],
        }
    }

    /// Decodes `bytes` (JPEG/PNG/... whatever `image` recognises) into a thumbnail.
    pub fn from_encoded(bytes: &[u8]) -> Option<Self> {
        let image = image::load_from_memory(bytes).ok()?;
        // Never upscale: a small embedded image is already cheaper than the budget.
        let thumb = if image.width() > THUMBNAIL_PX || image.height() > THUMBNAIL_PX {
            image.thumbnail(THUMBNAIL_PX, THUMBNAIL_PX)
        } else {
            image
        };
        let rgba_image = thumb.to_rgba8();
        let (width, height) = rgba_image.dimensions();
        if width == 0 || height == 0 {
            return None;
        }
        let rgba = rgba_image.into_raw();
        let mut sum = [0u64; 3];
        let mut count = 0u64;
        for pixel in rgba.as_chunks::<4>().0 {
            if pixel[3] < 16 {
                continue;
            }
            for (slot, channel) in sum.iter_mut().zip(pixel) {
                *slot += u64::from(*channel);
            }
            count += 1;
        }
        let average = std::array::from_fn(|index| {
            sum[index]
                .checked_div(count)
                .map_or(0, |mean| mean.min(255) as u8)
        });
        Some(Self {
            width,
            height,
            rgba,
            average,
        })
    }
}

/// Reads the embedded cover of `path`, if it has one this build can decode.
///
/// Blocking (a probe opens and seeks the file): call it from a worker thread, never from the audio
/// path or a render loop.
pub fn read_cover(path: &str) -> Option<Cover> {
    if let Some(cover) = from_container(path) {
        return Some(cover);
    }
    let bytes = id3_apic(path)?;
    Cover::from_encoded(&bytes)
}

/// The format reader's own picture, if it has one (FLAC today).
fn from_container(path: &str) -> Option<Cover> {
    let file = File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = Path::new(path).extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(extension);
    }
    let options = MetadataOptions {
        limit_metadata_bytes: Limit::Maximum(MAX_EMBEDDED_BYTES),
        limit_visual_bytes: Limit::Maximum(MAX_EMBEDDED_BYTES),
    };
    let mut probed = symphonia::default::get_probe()
        .format(&hint, mss, &Default::default(), &options)
        .ok()?;
    let metadata = probed.format.metadata();
    let revision = metadata.current()?;
    let visual = revision
        .visuals()
        .iter()
        .find(|visual| visual.media_type.starts_with("image/"))?;
    Cover::from_encoded(&visual.data)
}

/// A minimal ID3v2 `APIC`/`PIC` scan. Returns the first picture's bytes.
fn id3_apic(path: &str) -> Option<Vec<u8>> {
    // The tag is at the head of the file; 4 MB of frames covers an ID3v2 tag plus its artwork
    // without slurping a whole MP3.
    let mut head = Vec::with_capacity(64 * 1024);
    File::open(path)
        .ok()?
        .take(4 * 1024 * 1024)
        .read_to_end(&mut head)
        .ok()?;
    let tag = id3v2_tag(&head)?;
    apic_in_tag(tag)
}

/// The frame area of an ID3v2 tag (`None` for other tag versions and for broken headers).
fn id3v2_tag(bytes: &[u8]) -> Option<(&[u8], u8)> {
    if bytes.len() < 10 || &bytes[0..3] != b"ID3" {
        return None;
    }
    let major = bytes[3];
    if !(2..=4).contains(&major) || bytes[4] == 0xFF {
        return None;
    }
    let flags = bytes[5];
    let size = syncsafe(&bytes[6..10])? as usize;
    let mut start = 10;
    // An extended header (v2.3/2.4) sits between the tag header and the frames.
    if flags & 0x40 != 0 {
        let extended = syncsafe(bytes.get(start..start + 4)?)? as usize;
        start += if major == 4 { extended } else { extended + 4 };
    }
    let end = (10 + size).min(bytes.len());
    (start < end).then(|| (&bytes[start..end], major))
}

/// Walks the frames of a tag looking for the picture frame of that version.
fn apic_in_tag(tag: (&[u8], u8)) -> Option<Vec<u8>> {
    let (frames, major) = tag;
    let header = if major == 2 { 6 } else { 10 };
    let mut rest = frames;
    loop {
        if rest.len() < header {
            return None;
        }
        let size = if major == 2 {
            (usize::from(rest[3]) << 16) | (usize::from(rest[4]) << 8) | usize::from(rest[5])
        } else if major == 4 {
            syncsafe(&rest[4..8])? as usize
        } else {
            u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize
        };
        if size == 0 || rest.len() < header + size {
            return None;
        }
        let is_picture = if major == 2 {
            rest.starts_with(b"PIC")
        } else {
            rest.starts_with(b"APIC")
        };
        if is_picture {
            return apic_body(&rest[header..header + size], major == 2);
        }
        rest = &rest[header + size..];
    }
}

/// Splits an `APIC`/`PIC` body into (mime, picture type, description) + data, returning the data.
fn apic_body(body: &[u8], v22: bool) -> Option<Vec<u8>> {
    let encoding = *body.first()?;
    let mut index = 1usize;
    if v22 {
        // v2.2 stores a 3-byte image format ("JPG", "PNG") instead of a MIME type.
        index += 3;
    } else {
        let end = body[index..].iter().position(|byte| *byte == 0)? + index;
        index = end + 1;
    }
    // Picture type.
    index += 1;
    // Description, terminated per text encoding: UTF-16 uses two bytes.
    let wide = encoding == 1 || encoding == 2;
    loop {
        let byte = *body.get(index)?;
        let next = *body.get(index + 1).unwrap_or(&0);
        if byte == 0 && (!wide || next == 0) {
            index += if wide { 2 } else { 1 };
            break;
        }
        index += if wide { 2 } else { 1 };
    }
    (index < body.len()).then(|| body[index..].to_vec())
}

/// An ID3 syncsafe integer (7 bits per byte).
fn syncsafe(bytes: &[u8]) -> Option<u32> {
    let mut value = 0u32;
    for byte in bytes.iter().take(4) {
        if *byte & 0x80 != 0 {
            return None;
        }
        value = (value << 7) | u32::from(*byte);
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 PNG: enough for `image` to decode, small enough to paste as a test fixture.
    const PNG_1X1: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, b'I', b'D', b'A', b'T', 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, b'I',
        b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82,
    ];

    fn id3v23_with_apic(picture: &[u8]) -> Vec<u8> {
        let mut body = vec![0u8]; // text encoding: latin1
        body.extend_from_slice(b"image/png\0");
        body.push(3); // front cover
        body.push(0); // empty description
        body.extend_from_slice(picture);

        let mut frame = Vec::new();
        frame.extend_from_slice(b"APIC");
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]); // flags
        frame.extend_from_slice(&body);

        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3");
        tag.extend_from_slice(&[3, 0, 0]); // v2.3, no flags
        tag.extend_from_slice(&syncsafe_bytes(frame.len() as u32));
        tag.extend_from_slice(&frame);
        tag
    }

    fn syncsafe_bytes(value: u32) -> [u8; 4] {
        let mut out = [0u8; 4];
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = ((value >> (7 * (3 - index))) & 0x7F) as u8;
        }
        out
    }

    #[test]
    fn an_id3v23_apic_frame_is_found_and_decoded() {
        let tag = id3v23_with_apic(PNG_1X1);
        let picture = id3v2_tag(&tag).and_then(apic_in_tag).expect("APIC");
        assert_eq!(picture, PNG_1X1);
        let cover = Cover::from_encoded(&picture).expect("decodable");
        assert_eq!((cover.width, cover.height), (1, 1));
        assert_eq!(cover.rgba.len(), 4);
    }

    #[test]
    fn a_tag_without_a_picture_returns_none() {
        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3");
        tag.extend_from_slice(&[3, 0, 0]);
        tag.extend_from_slice(&syncsafe_bytes(11));
        tag.extend_from_slice(b"TIT2");
        tag.extend_from_slice(&1u32.to_be_bytes());
        tag.extend_from_slice(&[0, 0]);
        tag.push(0);
        assert!(id3v2_tag(&tag).and_then(apic_in_tag).is_none());
    }

    #[test]
    fn a_non_id3_file_is_not_mistaken_for_a_tag() {
        assert!(id3v2_tag(b"RIFF....WAVE").is_none());
        assert!(id3v2_tag(b"").is_none());
    }

    #[test]
    fn a_broken_syncsafe_size_is_rejected_instead_of_wrapping() {
        assert!(syncsafe(&[0x80, 0, 0, 0]).is_none());
    }

    #[test]
    fn the_thumbnail_is_capped_and_keeps_its_mean_colour() {
        let source = image::RgbImage::from_pixel(256, 128, image::Rgb([200, 100, 50]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(source)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let cover = Cover::from_encoded(&png).expect("decodable");
        assert!(cover.width <= THUMBNAIL_PX && cover.height <= THUMBNAIL_PX);
        assert_eq!(cover.average, [200, 100, 50]);
    }
}
