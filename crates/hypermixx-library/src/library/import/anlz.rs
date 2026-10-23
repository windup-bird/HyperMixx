//! Rekordbox ANLZ reader: the beat grid (`PQTZ`) from `.DAT` and cues (`PCO2`/`PCOB`) from `.EXT`.
//!
//! Values are kept in the source's own timeline. The importer records which clock they came from
//! and the calibration offset separately; this module never silently shifts a time.

use std::path::Path;

use crate::library::db::{LibraryError, Result};

/// Which writer's fingerprint was found. Rekordbox writes `PSSI`; VirtualDJ's CDJ exporter writes
/// `PQT2` instead. The distinction is useful when a USB contains both exports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnlzWriter {
    #[default]
    Unknown,
    Rekordbox,
    VirtualDj,
}

/// One `PQTZ` beat: its bar position, tempo at that beat, and source time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnlzBeat {
    pub beat: u16,
    pub bpm: f32,
    pub time_ms: f64,
}

/// One cue from `PCO2` (or the older `PCOB` fallback).
#[derive(Clone, Debug, PartialEq)]
pub struct AnlzCue {
    /// 0 for a memory point; 1.. for hot cue A, B, ...
    pub hot_cue: u32,
    pub is_loop: bool,
    pub time_ms: f64,
    /// End of the loop, when `is_loop`.
    pub loop_ms: Option<f64>,
    /// RGB colour, when the extended tag carried one.
    pub color: Option<[u8; 3]>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Anlz {
    pub writer: AnlzWriter,
    pub path: Option<String>,
    pub beats: Vec<AnlzBeat>,
    pub cues: Vec<AnlzCue>,
}

impl Anlz {
    /// Parses one `.DAT`, `.EXT`, or `.2EX` file. Unknown tags are ignored, so a newer writer does
    /// not make the whole analysis unreadable.
    pub fn parse(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::parse_bytes(&bytes)
            .map_err(|err| LibraryError::Invalid(format!("{}: {err}", path.display())))
    }

    pub fn parse_bytes(bytes: &[u8]) -> std::result::Result<Self, String> {
        if bytes.len() < 8 || &bytes[..4] != b"PMAI" {
            return Err("not an ANLZ file (missing PMAI)".to_owned());
        }
        let header_len = be_u32(bytes, 4)? as usize;
        if header_len < 8 || header_len > bytes.len() {
            return Err(format!("bad ANLZ header length {header_len}"));
        }
        let mut anlz = Anlz::default();
        let mut offset = header_len;
        while offset + 12 <= bytes.len() {
            let fourcc = &bytes[offset..offset + 4];
            let tag_header = be_u32(bytes, offset + 4)? as usize;
            let tag_len = be_u32(bytes, offset + 8)? as usize;
            if tag_len < 12 || offset + tag_len > bytes.len() {
                break;
            }
            let data = &bytes[offset + 12..offset + tag_len];
            match fourcc {
                b"PPTH" => {
                    let path_at = tag_header.saturating_sub(12).min(data.len());
                    anlz.path = Some(decode_utf16be(&data[path_at..]));
                }
                b"PSSI" => anlz.writer = AnlzWriter::Rekordbox,
                b"PQT2" => anlz.writer = AnlzWriter::VirtualDj,
                b"PQTZ" => parse_pqtz(data, &mut anlz.beats)?,
                b"PCO2" => parse_pco2(data, tag_header, &mut anlz.cues)?,
                b"PCOB" => parse_pcob(data, tag_header, &mut anlz.cues)?,
                _ => {}
            }
            offset += tag_len;
        }
        Ok(anlz)
    }
}

fn parse_pqtz(data: &[u8], beats: &mut Vec<AnlzBeat>) -> std::result::Result<(), String> {
    if data.len() < 12 {
        return Err("PQTZ too short".to_owned());
    }
    let count = be_u32(data, 8)? as usize;
    let entries = &data[12..];
    let available = entries.len() / 8;
    if count > available {
        return Err(format!("PQTZ says {count} beats, only {available} fit"));
    }
    for index in 0..count {
        let entry = &entries[index * 8..index * 8 + 8];
        let beat = u16::from_be_bytes([entry[0], entry[1]]);
        let tempo = u16::from_be_bytes([entry[2], entry[3]]);
        let time_ms = u32::from_be_bytes([entry[4], entry[5], entry[6], entry[7]]) as f64;
        beats.push(AnlzBeat {
            beat,
            bpm: f32::from(tempo) / 100.0,
            time_ms,
        });
    }
    Ok(())
}

fn parse_pco2(
    data: &[u8],
    tag_header: usize,
    cues: &mut Vec<AnlzCue>,
) -> std::result::Result<(), String> {
    // len_header is 20: 12 bytes of generic tag header plus 8 bytes of PCO2 header.
    if data.len() < 8 {
        return Ok(());
    }
    let entries_at = tag_header.saturating_sub(12).min(data.len());
    let mut offset = entries_at;
    while offset + 16 <= data.len() {
        if &data[offset..offset + 4] != b"PCP2" {
            break;
        }
        let entry_len = be_u32(data, offset + 8)? as usize;
        if entry_len < 28 || offset + entry_len > data.len() {
            break;
        }
        let entry = &data[offset..offset + entry_len];
        let hot_cue = be_u32(entry, 12)?;
        let is_loop = entry[16] == 2;
        let time_ms = be_u32(entry, 20)? as f64;
        let loop_ms = if is_loop {
            Some(be_u32(entry, 24)? as f64)
        } else {
            None
        };
        let color = entry
            .get(0x2c..0x2f)
            .map(|rgb| [rgb[0], rgb[1], rgb[2]])
            .filter(|rgb| *rgb != [0, 0, 0]);
        cues.push(AnlzCue {
            hot_cue,
            is_loop,
            time_ms,
            loop_ms,
            color,
        });
        offset += entry_len;
    }
    Ok(())
}

fn parse_pcob(
    data: &[u8],
    tag_header: usize,
    cues: &mut Vec<AnlzCue>,
) -> std::result::Result<(), String> {
    // The legacy tag is a 56-byte PCPT entry with the same time/type layout at the same offsets.
    let entries_at = tag_header.saturating_sub(12).min(data.len());
    let mut offset = entries_at;
    while offset + 56 <= data.len() {
        if &data[offset..offset + 4] != b"PCPT" {
            break;
        }
        let entry_len = be_u32(data, offset + 8)? as usize;
        if entry_len < 56 || offset + entry_len > data.len() {
            break;
        }
        let entry = &data[offset..offset + 56];
        let hot_cue = be_u32(entry, 12)?;
        let is_loop = entry[28] == 2;
        let time_ms = be_u32(entry, 32)? as f64;
        let loop_ms = if is_loop {
            Some(be_u32(entry, 36)? as f64)
        } else {
            None
        };
        cues.push(AnlzCue {
            hot_cue,
            is_loop,
            time_ms,
            loop_ms,
            color: None,
        });
        offset += entry_len;
    }
    Ok(())
}

fn be_u32(bytes: &[u8], offset: usize) -> std::result::Result<u32, String> {
    bytes
        .get(offset..offset + 4)
        .map(|slice| u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
        .ok_or_else(|| format!("truncated u32 at {offset}"))
}

fn decode_utf16be(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(fourcc: &[u8; 4], header: u32, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(fourcc);
        out.extend_from_slice(&header.to_be_bytes());
        out.extend_from_slice(&((12 + body.len()) as u32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn pqtz_parses_the_documented_eight_byte_entries() {
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&0x0080_0000u32.to_be_bytes());
        body.extend_from_slice(&2u32.to_be_bytes());
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&12400u16.to_be_bytes());
        body.extend_from_slice(&46u32.to_be_bytes());
        body.extend_from_slice(&2u16.to_be_bytes());
        body.extend_from_slice(&12400u16.to_be_bytes());
        body.extend_from_slice(&530u32.to_be_bytes());
        let mut file = b"PMAI".to_vec();
        file.extend_from_slice(&0x1cu32.to_be_bytes());
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(&[0; 16]);
        file.extend_from_slice(&tag(b"PQTZ", 24, &body));
        let anlz = Anlz::parse_bytes(&file).unwrap();
        assert_eq!(anlz.beats.len(), 2);
        assert_eq!(anlz.beats[0].bpm, 124.0);
        assert_eq!(anlz.beats[0].time_ms, 46.0);
    }

    #[test]
    fn pco2_parses_a_hot_cue_entry() {
        let mut entry = b"PCP2".to_vec();
        entry.extend_from_slice(&16u32.to_be_bytes());
        entry.extend_from_slice(&88u32.to_be_bytes());
        entry.extend_from_slice(&2u32.to_be_bytes());
        entry.push(1);
        entry.extend_from_slice(&[0, 0x03, 0xe8]);
        entry.extend_from_slice(&31061u32.to_be_bytes());
        entry.extend_from_slice(&u32::MAX.to_be_bytes());
        entry.resize(0x2c, 0);
        entry.extend_from_slice(&[0x24, 0xff, 0x8c, 0]);
        entry.resize(88, 0);
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&[0, 1, 0, 0]);
        body.extend_from_slice(&entry);
        let mut file = b"PMAI".to_vec();
        file.extend_from_slice(&0x1cu32.to_be_bytes());
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(&[0; 16]);
        file.extend_from_slice(&tag(b"PCO2", 20, &body));
        let anlz = Anlz::parse_bytes(&file).unwrap();
        assert_eq!(anlz.cues.len(), 1);
        assert_eq!(anlz.cues[0].hot_cue, 2);
        assert_eq!(anlz.cues[0].time_ms, 31061.0);
        assert_eq!(anlz.cues[0].color, Some([0x24, 0xff, 0x8c]));
    }
}
