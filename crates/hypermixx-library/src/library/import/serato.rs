//! Serato reader: `database V2` for the track list, `Subcrates/*.crate` for playlists, and the
//! audio file's ID3 `GEOB` frames for beat grid and hot cues.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine;
use hypermixx_core::Key;

use super::super::db::{
    clear_imported_timing, finish_source_scan, insert_beatgrid, insert_cue, insert_playlist_item,
    insert_playlist_kind, unix_now, upsert_location, upsert_track, LibraryDb, LibraryError, Result,
};
use super::super::model::{ScanEvent, ScanStats, Source};
use super::super::tags;
use super::util;

#[derive(Clone, Debug, Default)]
struct SeratoTrack {
    path: String,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    bpm: Option<f32>,
    key: Option<Key>,
    duration_seconds: Option<u64>,
}

#[derive(Clone, Debug, Default)]
struct SeratoTagData {
    beat_markers: Vec<(f64, f32)>,
    cues: Vec<SeratoCue>,
}

#[derive(Clone, Debug, PartialEq)]
struct SeratoCue {
    index: u8,
    time_ms: f64,
    color: Option<[u8; 3]>,
}

pub(crate) fn scan(
    db: &LibraryDb,
    source: &Source,
    scan_id: i64,
    progress: &mut dyn FnMut(ScanEvent),
) -> Result<ScanStats> {
    let root = source.root.clone();
    let database = [
        root.join("_Serato_/database V2"),
        root.join("database V2"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or_else(|| LibraryError::Invalid(format!("{}: no Serato database V2", root.display())))?;
    let tracks = parse_database(&database)?;
    let crates = parse_crates(&root);
    let serato_offset_ms = db.import_calibration().serato_offset_ms;

    let now = unix_now();
    db.with_transaction(|tx| {
        tx.execute("DELETE FROM playlist WHERE source_id = ?1", [source.id])?;
        clear_imported_timing(tx, source.id)?;

        let mut stats = ScanStats::default();
        let mut locations: HashMap<PathBuf, i64> = HashMap::new();
        for track in &tracks {
            let Some(path) = util::resolve_under_root(&root, &track.path) else {
                stats.errors.push(format!("no file for {}", track.path));
                continue;
            };
            let (file_size, mtime_ns, file_hash) = match util::file_identity(&path) {
                Ok(identity) => identity,
                Err(err) => {
                    stats.errors.push(err);
                    continue;
                }
            };
            let mut tags = tags::read_tags(&path).unwrap_or_default();
            tags.title = track.title.clone().or(tags.title);
            tags.artist = track.artist.clone().or(tags.artist);
            tags.album = track.album.clone().or(tags.album);
            tags.bpm = track.bpm.or(tags.bpm);
            tags.key = track.key.or(tags.key);
            if tags.sample_rate == 0 {
                tags.sample_rate = 44_100;
            }
            if let Some(seconds) = track.duration_seconds {
                tags.duration_frames = seconds * u64::from(tags.sample_rate);
            }
            let content_key = format!("sha256:{file_hash}:{file_size}");
            let track_id = upsert_track(tx, &content_key, &tags, now)?;
            let relative = util::relative_key(&root, &path);
            let (location_id, added) = upsert_location(
                tx, track_id, source.id, &path, &relative, file_size, mtime_ns, &file_hash, scan_id,
            )?;
            locations.insert(path.clone(), location_id);
            stats.files_seen += 1;
            if added {
                stats.files_added += 1;
            } else {
                stats.files_updated += 1;
            }
            match read_serato_tags(&path) {
                Ok(tag_data) => {
                    import_tag_data(tx, source.id, location_id, &tag_data, serato_offset_ms)?
                }
                Err(err) => stats.errors.push(err),
            }
            progress(ScanEvent {
                index: stats.files_seen,
                path,
            });
        }

        let by_name: HashMap<String, i64> = locations
            .iter()
            .filter_map(|(path, id)| {
                path.file_name()
                    .map(|name| (name.to_string_lossy().into_owned(), *id))
            })
            .collect();
        if crates.is_empty() {
            let playlist_id = insert_playlist_kind(
                tx,
                source.id,
                None,
                "serato:tracks",
                "serato",
                "playlist",
                0,
            )?;
            let mut position = 0;
            for location_id in locations.values() {
                insert_playlist_item(tx, playlist_id, *location_id, position)?;
                position += 1;
            }
        } else {
            for (index, (name, paths)) in crates.iter().enumerate() {
                let playlist_id = insert_playlist_kind(
                    tx,
                    source.id,
                    None,
                    &format!("serato:crate:{index}"),
                    name,
                    "crate",
                    index as i64,
                )?;
                let mut position = 0;
                for raw in paths {
                    let Some(path) = util::resolve_under_root(&root, raw) else {
                        continue;
                    };
                    let location_id = locations.get(&path).copied().or_else(|| {
                        path.file_name()
                            .and_then(|name| by_name.get(name.to_string_lossy().as_ref()).copied())
                    });
                    if let Some(location_id) = location_id {
                        insert_playlist_item(tx, playlist_id, location_id, position)?;
                        position += 1;
                    }
                }
            }
        }
        stats.files_removed = finish_source_scan(tx, source.id, scan_id)?;
        Ok(stats)
    })
}

fn import_tag_data(
    conn: &rusqlite::Connection,
    source_id: i64,
    location_id: i64,
    tags: &SeratoTagData,
    offset_ms: f64,
) -> Result<()> {
    if let Some((position_s, bpm)) = tags.beat_markers.first() {
        let raw_ms = position_s * 1000.0;
        insert_beatgrid(
            conn,
            location_id,
            source_id,
            "serato_tag",
            Some(*bpm),
            Some(raw_ms),
            "serato_tag",
            offset_ms,
            Some(1.0),
            "serato-geob-v1",
            &[(1, *bpm, raw_ms)],
        )?;
    }
    for cue in &tags.cues {
        let color = cue.color.map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}"));
        insert_cue(
            conn,
            location_id,
            source_id,
            "serato_tag",
            "hot",
            Some(i64::from(cue.index)),
            cue.time_ms,
            "serato_tag",
            offset_ms,
            color.as_deref(),
            None,
        )?;
    }
    Ok(())
}

fn parse_database(path: &Path) -> Result<Vec<SeratoTrack>> {
    let data = std::fs::read(path)?;
    let mut tracks = Vec::new();
    let mut offset = 0usize;
    while offset + 8 <= data.len() {
        let tag = &data[offset..offset + 4];
        let length = be_u32(&data, offset + 4)? as usize;
        if offset + 8 + length > data.len() {
            break;
        }
        let payload = &data[offset + 8..offset + 8 + length];
        if tag == b"otrk" {
            tracks.push(parse_otrk(payload)?);
        }
        offset += 8 + length;
    }
    Ok(tracks)
}

fn parse_otrk(data: &[u8]) -> Result<SeratoTrack> {
    let mut track = SeratoTrack::default();
    let mut offset = 0usize;
    while offset + 8 <= data.len() {
        let tag = &data[offset..offset + 4];
        let length = be_u32(data, offset + 4)? as usize;
        if offset + 8 + length > data.len() {
            break;
        }
        let value = &data[offset + 8..offset + 8 + length];
        match tag {
            b"pfil" => track.path = utf16be(value),
            b"tsng" => track.title = Some(utf16be(value)),
            b"tart" => track.artist = Some(utf16be(value)),
            b"talb" => track.album = Some(utf16be(value)),
            b"tbpm" => track.bpm = utf16be(value).parse().ok(),
            b"tkey" => track.key = Key::parse(&utf16be(value)),
            b"tlen" => track.duration_seconds = parse_length(&utf16be(value)),
            _ => {}
        }
        offset += 8 + length;
    }
    Ok(track)
}

fn parse_crates(root: &Path) -> Vec<(String, Vec<String>)> {
    let dir = [
        root.join("_Serato_/Subcrates"),
        root.join("Subcrates"),
    ]
    .into_iter()
    .find(|path| path.is_dir());
    let Some(dir) = dir else {
        return Vec::new();
    };
    let mut crates = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return crates;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("crate") {
            continue;
        }
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().replace("%%", " / "))
            .unwrap_or_else(|| "crate".to_owned());
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let mut paths = Vec::new();
        let mut offset = 0usize;
        while offset + 8 <= data.len() {
            let tag = &data[offset..offset + 4];
            let length = match be_u32(&data, offset + 4) {
                Ok(length) => length as usize,
                Err(_) => break,
            };
            if offset + 8 + length > data.len() {
                break;
            }
            let payload = &data[offset + 8..offset + 8 + length];
            if tag == b"otrk" {
                let mut field = 0usize;
                while field + 8 <= payload.len() {
                    let field_tag = &payload[field..field + 4];
                    let field_len = match be_u32(payload, field + 4) {
                        Ok(length) => length as usize,
                        Err(_) => break,
                    };
                    if field + 8 + field_len > payload.len() {
                        break;
                    }
                    if field_tag == b"ptrk" {
                        paths.push(utf16be(&payload[field + 8..field + 8 + field_len]));
                    }
                    field += 8 + field_len;
                }
            }
            offset += 8 + length;
        }
        if !paths.is_empty() {
            crates.push((name, paths));
        }
    }
    crates
}

fn read_serato_tags(path: &Path) -> std::result::Result<SeratoTagData, String> {
    let mut file = File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut header = [0u8; 10];
    file.read_exact(&mut header)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    if &header[..3] != b"ID3" {
        return Ok(SeratoTagData::default());
    }
    let major = header[3];
    if major != 3 && major != 4 {
        return Ok(SeratoTagData::default());
    }
    let size = syncsafe(&header[6..10]);
    let mut body = vec![0u8; size];
    file.read_exact(&mut body)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    let mut tags = SeratoTagData::default();
    let mut offset = 0usize;
    while offset + 10 <= body.len() {
        if body[offset] == 0 {
            break;
        }
        let id = &body[offset..offset + 4];
        let frame_size = if major == 4 {
            syncsafe(&body[offset + 4..offset + 8])
        } else {
            be_u32(&body, offset + 4).map_err(|err| err.to_string())? as usize
        };
        if frame_size == 0 || offset + 10 + frame_size > body.len() {
            break;
        }
        let frame = &body[offset + 10..offset + 10 + frame_size];
        if id == b"GEOB" {
            if let Some((description, payload)) = parse_geob(frame) {
                match description.as_str() {
                    "Serato BeatGrid" => tags.beat_markers = parse_beatgrid(payload),
                    "Serato Markers2" => tags.cues = parse_markers2(payload),
                    _ => {}
                }
            }
        }
        offset += 10 + frame_size;
    }
    Ok(tags)
}

fn parse_geob(data: &[u8]) -> Option<(String, &[u8])> {
    if data.len() < 4 {
        return None;
    }
    let encoding = data[0];
    let mut offset = 1usize;
    let mime_end = data[offset..].iter().position(|byte| *byte == 0)? + offset;
    offset = mime_end + 1;
    let name_end = data[offset..].iter().position(|byte| *byte == 0)? + offset;
    offset = name_end + 1;
    let (description, payload_at) = if matches!(encoding, 1 | 2) {
        let mut at = offset;
        while at + 2 <= data.len() {
            if data[at] == 0 && data[at + 1] == 0 {
                break;
            }
            at += 2;
        }
        (utf16be(&data[offset..at]), (at + 2).min(data.len()))
    } else {
        let desc_end = data[offset..].iter().position(|byte| *byte == 0)? + offset;
        (
            String::from_utf8_lossy(&data[offset..desc_end]).into_owned(),
            desc_end + 1,
        )
    };
    Some((description, &data[payload_at..]))
}

fn parse_beatgrid(data: &[u8]) -> Vec<(f64, f32)> {
    if data.len() < 6 || data[0] != 1 {
        return Vec::new();
    }
    let count = u32::from_be_bytes([data[2], data[3], data[4], data[5]]) as usize;
    let mut markers = Vec::new();
    let mut offset = 6usize;
    for _ in 0..count {
        if offset + 8 > data.len() {
            break;
        }
        let position = f32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as f64;
        let bpm = f32::from_be_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]);
        if bpm.is_finite() && bpm > 0.0 {
            markers.push((position, bpm));
        }
        offset += 8;
    }
    markers
}

fn parse_markers2(data: &[u8]) -> Vec<SeratoCue> {
    if data.len() < 2 {
        return Vec::new();
    }
    let encoded = String::from_utf8_lossy(&data[2..]);
    let encoded = encoded.split('\0').next().unwrap_or_default();
    let compact: String = encoded.chars().filter(|ch| !ch.is_whitespace()).collect();
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(compact) else {
        return Vec::new();
    };
    let mut cues = Vec::new();
    let mut offset = 0usize;
    while offset < decoded.len() {
        let Some(name_end) = decoded[offset..].iter().position(|byte| *byte == 0) else {
            break;
        };
        let name = String::from_utf8_lossy(&decoded[offset..offset + name_end]).into_owned();
        offset += name_end + 1;
        if offset + 4 > decoded.len() {
            break;
        }
        let length = be_u32(&decoded, offset).unwrap_or(0) as usize;
        offset += 4;
        if offset + length > decoded.len() {
            break;
        }
        let payload = &decoded[offset..offset + length];
        offset += length;
        if name == "CUE" && payload.len() >= 13 {
            let time_ms =
                u32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]) as f64;
            let color = [payload[7], payload[8], payload[9]];
            cues.push(SeratoCue {
                index: payload[1],
                time_ms,
                color: (color != [0, 0, 0]).then_some(color),
            });
        }
    }
    cues
}

fn parse_length(text: &str) -> Option<u64> {
    let (minutes, rest) = text.split_once(':')?;
    let seconds = rest.split('.').next()?.parse::<f64>().ok()?;
    let minutes = minutes.parse::<f64>().ok()?;
    Some((minutes * 60.0 + seconds) as u64)
}

fn utf16be(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn be_u32(data: &[u8], offset: usize) -> Result<u32> {
    data.get(offset..offset + 4)
        .map(|slice| u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
        .ok_or_else(|| LibraryError::Invalid(format!("truncated u32 at {offset}")))
}

fn syncsafe(data: &[u8]) -> usize {
    ((data[0] as usize & 0x7f) << 21)
        | ((data[1] as usize & 0x7f) << 14)
        | ((data[2] as usize & 0x7f) << 7)
        | (data[3] as usize & 0x7f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_and_bpm_fields_parse() {
        assert_eq!(parse_length("03:58.16"), Some(238));
        assert_eq!(parse_length("bad"), None);
    }

    #[test]
    fn a_beatgrid_marker_round_trips_the_serato_layout() {
        let mut data = vec![1, 0];
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&0.06267f32.to_be_bytes());
        data.extend_from_slice(&126.007f32.to_be_bytes());
        let markers = parse_beatgrid(&data);
        assert_eq!(markers.len(), 1);
        assert!((markers[0].0 - 0.06267).abs() < 0.001);
    }

    #[test]
    fn markers2_cue_payload_parses() {
        let mut raw = vec![1, 1];
        raw.extend_from_slice(b"AQFDT0xPUgAAAAAEAP///0NVRQAAAAANAAAAAPYcAMwAAAAAAENVRQAAAAANAAEAAXEPAMyIAAAAAEJQTUxPQ0sAAAAAAQA=");
        let cues = parse_markers2(&raw);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].index, 0);
        assert_eq!(cues[0].time_ms, 63004.0);
        assert_eq!(cues[0].color, Some([204, 0, 0]));
    }
}
