//! VirtualDJ `database.xml` reader, with its CDJ-export analysis files when present.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hypermixx_core::Key;
use quick_xml::events::Event;
use quick_xml::Reader;
use walkdir::WalkDir;

use super::super::db::{
    clear_imported_timing, finish_source_scan, insert_playlist_item, insert_playlist_kind,
    unix_now, upsert_location, upsert_track, LibraryDb, LibraryError, Result,
};
use super::super::model::{ScanEvent, ScanStats, Source};
use super::super::tags;
use super::analysis;
use super::anlz::Anlz;
use super::util;

#[derive(Clone, Debug, Default)]
struct VdjSong {
    file_path: String,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    bpm: Option<f32>,
    key: Option<Key>,
    length_seconds: Option<u64>,
}

pub(crate) fn scan(
    db: &LibraryDb,
    source: &Source,
    scan_id: i64,
    progress: &mut dyn FnMut(ScanEvent),
) -> Result<ScanStats> {
    let root = source.root.clone();
    let xml_path = [
        root.join("VirtualDJ/database.xml"),
        root.join("database.xml"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or_else(|| LibraryError::Invalid(format!("{}: no database.xml", root.display())))?;
    let songs = parse_database(&xml_path)?;
    let analysis_index = build_analysis_index(&root);
    let playlist_path = root.join("vdj-export/vdj-list.m3u");
    let playlist = parse_m3u(&playlist_path);
    let vdj_offset_ms = db.import_calibration().vdj_offset_ms;

    let now = unix_now();
    db.with_transaction(|tx| {
        tx.execute("DELETE FROM playlist WHERE source_id = ?1", [source.id])?;
        clear_imported_timing(tx, source.id)?;

        let mut stats = ScanStats::default();
        let mut locations: HashMap<PathBuf, i64> = HashMap::new();
        for song in &songs {
            let Some(path) = util::resolve_under_root(&root, &song.file_path) else {
                stats.errors.push(format!("no file for {}", song.file_path));
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
            tags.title = song.title.clone().or(tags.title);
            tags.artist = song.artist.clone().or(tags.artist);
            tags.album = song.album.clone().or(tags.album);
            tags.bpm = song.bpm.or(tags.bpm);
            tags.key = song.key.or(tags.key);
            if tags.sample_rate == 0 {
                tags.sample_rate = 44_100;
            }
            if let Some(seconds) = song.length_seconds {
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
            let relative = relative_key(&root, &path);
            let analysis = analysis_index.get(&relative).or_else(|| {
                path.file_name()
                    .and_then(|name| analysis_index.get(name.to_string_lossy().as_ref()))
            });
            if let Some(dat) = analysis {
                let ext = dat.with_extension("EXT");
                analysis::import_sidecars(
                    tx,
                    source.id,
                    location_id,
                    dat,
                    &ext,
                    vdj_offset_ms,
                    "vdj_anlz",
                    "vdj_anlz",
                    &mut stats,
                )?;
            }
            progress(ScanEvent {
                index: stats.files_seen,
                path,
            });
        }

        let playlist_name = if playlist.is_empty() {
            "virtualdj".to_owned()
        } else {
            playlist_path
                .file_stem()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "vdj-list".to_owned())
        };
        let playlist_id = insert_playlist_kind(
            tx,
            source.id,
            None,
            "vdj:tracks",
            &playlist_name,
            "playlist",
            0,
        )?;
        let by_name: HashMap<String, i64> = locations
            .iter()
            .filter_map(|(path, id)| {
                path.file_name()
                    .map(|name| (name.to_string_lossy().into_owned(), *id))
            })
            .collect();
        let mut position = 0i64;
        if playlist.is_empty() {
            for location_id in locations.values() {
                insert_playlist_item(tx, playlist_id, *location_id, position)?;
                position += 1;
            }
        } else {
            for raw in playlist {
                let Some(path) = util::resolve_under_root(&root, &raw) else {
                    stats.errors.push(format!("m3u: no file for {raw}"));
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
        stats.files_removed = finish_source_scan(tx, source.id, scan_id)?;
        Ok(stats)
    })
}

fn parse_database(path: &Path) -> Result<Vec<VdjSong>> {
    let text = std::fs::read_to_string(path)?;
    let mut reader = Reader::from_str(&text);
    reader.config_mut().trim_text(true);
    let mut songs = Vec::new();
    let mut current: Option<VdjSong> = None;
    loop {
        let event = reader
            .read_event()
            .map_err(|err| LibraryError::Invalid(format!("{}: {err}", path.display())))?;
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                let name = element.name();
                match name.as_ref() {
                    b"Song" => {
                        let mut song = VdjSong::default();
                        for attr in element.attributes().with_checks(false).flatten() {
                            if attr.key.as_ref() == b"FilePath" {
                                if let Ok(value) = attr.decoded_and_normalized_value(
                                    quick_xml::XmlVersion::Implicit1_0,
                                    reader.decoder(),
                                ) {
                                    song.file_path = value.into_owned();
                                }
                            }
                        }
                        if empty {
                            songs.push(song);
                        } else {
                            current = Some(song);
                        }
                    }
                    b"Tags" => {
                        let Some(song) = current.as_mut() else {
                            continue;
                        };
                        for attr in element.attributes().with_checks(false).flatten() {
                            let Ok(value) = attr.decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                reader.decoder(),
                            ) else {
                                continue;
                            };
                            match attr.key.as_ref() {
                                b"Title" => song.title = Some(value.into_owned()),
                                b"Author" => song.artist = Some(value.into_owned()),
                                b"Album" => song.album = Some(value.into_owned()),
                                b"Bpm" => song.bpm = parse_bpm(&value),
                                b"Key" => song.key = Key::parse(&value),
                                _ => {}
                            }
                        }
                    }
                    b"Infos" => {
                        let Some(song) = current.as_mut() else {
                            continue;
                        };
                        for attr in element.attributes().with_checks(false).flatten() {
                            if attr.key.as_ref() != b"SongLength" {
                                continue;
                            }
                            if let Ok(value) = attr.decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                reader.decoder(),
                            ) {
                                song.length_seconds = value
                                    .parse::<f32>()
                                    .ok()
                                    .map(|seconds| seconds.max(0.0) as u64);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::End(element) if element.name().as_ref() == b"Song" => {
                if let Some(song) = current.take() {
                    songs.push(song);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(songs)
}

fn parse_bpm(raw: &str) -> Option<f32> {
    let value = raw.parse::<f32>().ok()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    // VirtualDJ writes seconds per beat in its database.xml (`0.483871` = 124 BPM).
    Some(if value < 10.0 { 60.0 / value } else { value })
}

fn parse_m3u(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| line.trim_start_matches('\u{feff}').trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned)
        .collect()
}

fn build_analysis_index(root: &Path) -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    let mut candidates = vec![root.join("PIONEER/USBANLZ")];
    if let Some(parent) = root.parent() {
        candidates.push(parent.join("PIONEER/USBANLZ"));
    }
    let Some(base) = candidates.into_iter().find(|path| path.is_dir()) else {
        return index;
    };
    if !base.is_dir() {
        return index;
    }
    for entry in WalkDir::new(base)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("DAT") {
            continue;
        }
        let Ok(anlz) = Anlz::parse(path) else {
            continue;
        };
        let Some(track_path) = anlz.path else {
            continue;
        };
        let track_path = track_path.trim_start_matches('/').replace('\\', "/");
        index.insert(track_path.clone(), path.to_path_buf());
        if let Some(name) = Path::new(&track_path).file_name() {
            index.insert(name.to_string_lossy().into_owned(), path.to_path_buf());
        }
    }
    index
}

fn relative_key(root: &Path, path: &Path) -> String {
    util::relative_key(root, path).replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtualdj_bpm_is_seconds_per_beat() {
        assert!((parse_bpm("0.483871").unwrap() - 124.0).abs() < 0.01);
        assert!((parse_bpm("124").unwrap() - 124.0).abs() < 0.01);
    }
}
