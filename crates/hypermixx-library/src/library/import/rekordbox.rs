//! Rekordbox export reader: OneLibrary's `exportLibrary.db` plus the per-track ANLZ analysis files.
//!
//! The database is opened from a private copy. The USB stays read-only, and a WAL that has not been
//! checkpointed by rekordbox is still visible because `-wal`/`-shm` travel with the copy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hypermixx_core::Key;
use rusqlite::Connection;

use super::super::db::{
    clear_imported_timing, finish_source_scan, insert_playlist_item, insert_playlist_kind,
    unix_now, upsert_location, upsert_track, LibraryDb, LibraryError, Result,
};
use super::super::model::{ScanEvent, ScanStats, Source};
use super::super::tags;
use super::analysis;
use super::util;

/// The public `exportLibrary.db` key. It is a constant, not a secret, but it is versioned: a future
/// database can be opened with `HYPERMIXX_REKORDBOX_KEY` without a rebuild.
const EXPORT_KEY: &str = "r8gddnr4k847830ar6cqzbkk0el6qytmb3trbbx805jm74vez64i5o8fnrqryqls";

/// The historical RB↔Serato difference in the validated sample. Production uses the configured
/// [`ImportCalibration`](crate::library::db::ImportCalibration); this constant only keeps the
/// format-offset unit test explicit.
#[cfg(test)]
const MP3_ANLZ_OFFSET_MS: f64 = 16.5;

struct RbContent {
    content_id: i64,
    title: String,
    bpm: Option<f32>,
    length_seconds: Option<u64>,
    sample_rate: Option<u32>,
    path: String,
    file_name: String,
    analysis_path: Option<String>,
    key: Option<Key>,
    artist: Option<String>,
    album: Option<String>,
    genre: Option<String>,
    year: Option<u32>,
}

pub(crate) fn scan(
    db: &LibraryDb,
    source: &Source,
    scan_id: i64,
    progress: &mut dyn FnMut(ScanEvent),
) -> Result<ScanStats> {
    let root = source.root.clone();
    let database = [
        root.join("PIONEER/rekordbox/exportLibrary.db"),
        root.join("exportLibrary.db"),
    ]
    .into_iter()
    .find(|path| path.is_file());
    let Some(database) = database else {
        return scan_pdb(db, source, scan_id, progress);
    };
    let copy = copy_database(&database)?;
    let conn = open_encrypted(&copy.path)?;
    let contents = query_contents(&conn)?;
    let playlists = query_playlists(&conn)?;
    let items = query_playlist_items(&conn)?;

    let rb_mp3_offset_ms = db.import_calibration().rkb_mp3_offset_ms;
    let now = unix_now();
    let stats = db.with_transaction(|tx| {
        tx.execute("DELETE FROM playlist WHERE source_id = ?1", [source.id])?;
        clear_imported_timing(tx, source.id)?;

        let mut stats = ScanStats::default();
        let mut locations: HashMap<i64, i64> = HashMap::new();
        for content in &contents {
            let path = util::resolve_under_root(&root, &content.path)
                .or_else(|| util::resolve_under_root(&root, &content.file_name));
            let Some(path) = path else {
                stats.errors.push(format!(
                    "content {}: no file for {}",
                    content.content_id, content.path
                ));
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
            tags.title = Some(content.title.clone());
            tags.artist = content.artist.clone().or(tags.artist);
            tags.album = content.album.clone().or(tags.album);
            tags.genre = content.genre.clone().or(tags.genre);
            tags.year = content.year.or(tags.year);
            tags.bpm = content.bpm.or(tags.bpm);
            tags.key = content.key.or(tags.key);
            if let Some(rate) = content.sample_rate {
                tags.sample_rate = rate;
            }
            if tags.sample_rate == 0 {
                tags.sample_rate = 44_100;
            }
            if let Some(seconds) = content.length_seconds {
                tags.duration_frames = seconds * u64::from(tags.sample_rate);
            }
            let content_key = format!("sha256:{file_hash}:{file_size}");
            let track_id = upsert_track(tx, &content_key, &tags, now)?;
            let relative = util::relative_key(&root, &path);
            let (location_id, added) = upsert_location(
                tx, track_id, source.id, &path, &relative, file_size, mtime_ns, &file_hash, scan_id,
            )?;
            locations.insert(content.content_id, location_id);
            stats.files_seen += 1;
            if added {
                stats.files_added += 1;
            } else {
                stats.files_updated += 1;
            }
            import_analysis(
                tx,
                source.id,
                location_id,
                &root,
                content.analysis_path.as_deref(),
                &path,
                rb_mp3_offset_ms,
                &mut stats,
            )?;
            progress(ScanEvent {
                index: stats.files_seen,
                path,
            });
        }

        import_playlists(tx, source.id, &playlists, &items, &locations, &mut stats)?;
        stats.files_removed = finish_source_scan(tx, source.id, scan_id)?;
        Ok(stats)
    })?;
    Ok(stats)
}

/// Legacy DeviceSQL fallback: `export.pdb` has the same content model as OneLibrary. The helper
/// crate is a read-only MIT parser; we only use its in-memory rows.
fn scan_pdb(
    db: &LibraryDb,
    source: &Source,
    scan_id: i64,
    progress: &mut dyn FnMut(ScanEvent),
) -> Result<ScanStats> {
    let root = source.root.clone();
    let pdb_path = [
        root.join("PIONEER/rekordbox/export.pdb"),
        root.join("export.pdb"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or_else(|| LibraryError::Invalid(format!("{}: no export.pdb", root.display())))?;
    let rb_mp3_offset_ms = db.import_calibration().rkb_mp3_offset_ms;
    let pdb = rekordbox_pdb::Database::from_file(&pdb_path)
        .map_err(|err| LibraryError::Invalid(format!("{}: {err}", pdb_path.display())))?;
    let now = unix_now();
    db.with_transaction(|tx| {
        tx.execute("DELETE FROM playlist WHERE source_id = ?1", [source.id])?;
        clear_imported_timing(tx, source.id)?;

        let artists: HashMap<u32, &str> = pdb
            .artists
            .iter()
            .map(|artist| (artist.id, artist.name.as_str()))
            .collect();
        let albums: HashMap<u32, &str> = pdb
            .albums
            .iter()
            .map(|album| (album.id, album.name.as_str()))
            .collect();
        let genres: HashMap<u32, &str> = pdb
            .genres
            .iter()
            .map(|genre| (genre.id, genre.name.as_str()))
            .collect();
        let keys: HashMap<u32, &str> = pdb
            .keys
            .iter()
            .map(|key| (key.id, key.name.as_str()))
            .collect();

        let mut stats = ScanStats::default();
        let mut locations: HashMap<u32, i64> = HashMap::new();
        for track in &pdb.tracks {
            let Some(path) = util::resolve_under_root(&root, track.file_path()) else {
                stats.errors.push(format!(
                    "track {}: no file for {}",
                    track.id,
                    track.file_path()
                ));
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
            tags.title = Some(track.title().to_owned()).filter(|title| !title.is_empty());
            tags.artist = artists.get(&track.artist_id).map(|name| (*name).to_owned());
            tags.album = albums.get(&track.album_id).map(|name| (*name).to_owned());
            tags.genre = genres.get(&track.genre_id).map(|name| (*name).to_owned());
            tags.year = (track.year > 0).then_some(u32::from(track.year));
            tags.bpm = (track.tempo > 0).then_some(track.tempo as f32 / 100.0);
            tags.key = keys.get(&track.key_id).and_then(|name| Key::parse(name));
            if track.sample_rate > 0 {
                tags.sample_rate = track.sample_rate;
            }
            if tags.sample_rate == 0 {
                tags.sample_rate = 44_100;
            }
            if track.duration > 0 {
                tags.duration_frames = u64::from(track.duration) * u64::from(tags.sample_rate);
            }
            let content_key = format!("sha256:{file_hash}:{file_size}");
            let track_id = upsert_track(tx, &content_key, &tags, now)?;
            let relative = util::relative_key(&root, &path);
            let (location_id, added) = upsert_location(
                tx, track_id, source.id, &path, &relative, file_size, mtime_ns, &file_hash, scan_id,
            )?;
            locations.insert(track.id, location_id);
            stats.files_seen += 1;
            if added {
                stats.files_added += 1;
            } else {
                stats.files_updated += 1;
            }
            import_analysis(
                tx,
                source.id,
                location_id,
                &root,
                Some(track.analyze_path()),
                &path,
                rb_mp3_offset_ms,
                &mut stats,
            )?;
            progress(ScanEvent {
                index: stats.files_seen,
                path,
            });
        }

        let mut playlist_ids: HashMap<u32, i64> = HashMap::new();
        let mut pending: Vec<&rekordbox_pdb::PlaylistTreeNode> = pdb.playlist_tree.iter().collect();
        loop {
            let before = pending.len();
            pending.retain(|node| {
                let parent = if node.parent_id == 0 {
                    None
                } else {
                    playlist_ids.get(&node.parent_id).copied()
                };
                if node.parent_id != 0 && parent.is_none() {
                    return true;
                }
                match insert_playlist_kind(
                    tx,
                    source.id,
                    parent,
                    &format!("pdb:{}", node.id),
                    &node.name,
                    if node.is_folder { "folder" } else { "playlist" },
                    i64::from(node.sort_order),
                ) {
                    Ok(id) => {
                        playlist_ids.insert(node.id, id);
                        false
                    }
                    Err(err) => {
                        stats.errors.push(err.to_string());
                        false
                    }
                }
            });
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }

        let mut positions: HashMap<i64, i64> = HashMap::new();
        for entry in &pdb.playlist_entries {
            let (Some(playlist_id), Some(location_id)) = (
                playlist_ids.get(&entry.playlist_id),
                locations.get(&entry.track_id),
            ) else {
                continue;
            };
            let position = positions.entry(*playlist_id).or_insert(0);
            insert_playlist_item(tx, *playlist_id, *location_id, *position)?;
            *position += 1;
        }
        stats.files_removed = finish_source_scan(tx, source.id, scan_id)?;
        Ok(stats)
    })
}

fn import_analysis(
    conn: &Connection,
    source_id: i64,
    location_id: i64,
    root: &Path,
    analysis_path: Option<&str>,
    audio_path: &Path,
    rb_mp3_offset_ms: f64,
    stats: &mut ScanStats,
) -> Result<()> {
    let Some(analysis_path) = analysis_path.filter(|path| !path.is_empty()) else {
        return Ok(());
    };
    let dat = root.join(analysis_path.trim_start_matches('/'));
    let ext = dat.with_extension("EXT");
    analysis::import_sidecars(
        conn,
        source_id,
        location_id,
        &dat,
        &ext,
        anlz_offset_ms(audio_path, rb_mp3_offset_ms),
        "rekordbox_anlz",
        "rekordbox_anlz",
        stats,
    )
}

fn import_playlists(
    conn: &Connection,
    source_id: i64,
    playlists: &[(i64, i64, String, i64)],
    items: &[(i64, i64, i64)],
    locations: &HashMap<i64, i64>,
    stats: &mut ScanStats,
) -> Result<()> {
    let mut ids: HashMap<i64, i64> = HashMap::new();
    let mut pending: Vec<&(i64, i64, String, i64)> = playlists.iter().collect();
    loop {
        let before = pending.len();
        pending.retain(|(id, sequence, name, parent)| {
            let parent_id = if *parent <= 0 {
                None
            } else {
                ids.get(parent).copied()
            };
            if *parent > 0 && parent_id.is_none() {
                return true;
            }
            match insert_playlist_kind(
                conn,
                source_id,
                parent_id,
                &format!("rb:{id}"),
                name,
                "playlist",
                *sequence,
            ) {
                Ok(new_id) => {
                    ids.insert(*id, new_id);
                    false
                }
                Err(err) => {
                    stats.errors.push(err.to_string());
                    false
                }
            }
        });
        if pending.is_empty() {
            break;
        }
        if pending.len() == before {
            // A broken parent chain: keep the rows visible as roots rather than dropping them.
            for (id, sequence, name, _) in pending.drain(..) {
                if let Ok(new_id) = insert_playlist_kind(
                    conn,
                    source_id,
                    None,
                    &format!("rb:{id}"),
                    name,
                    "playlist",
                    *sequence,
                ) {
                    ids.insert(*id, new_id);
                }
            }
            break;
        }
    }

    let mut positions: HashMap<i64, i64> = HashMap::new();
    for (playlist_id, content_id, _sequence) in items {
        let (Some(playlist_id), Some(location_id)) =
            (ids.get(playlist_id), locations.get(content_id))
        else {
            continue;
        };
        let position = positions.entry(*playlist_id).or_insert(0);
        if let Err(err) = insert_playlist_item(conn, *playlist_id, *location_id, *position) {
            stats.errors.push(err.to_string());
        }
        *position += 1;
    }
    Ok(())
}

fn query_contents(conn: &Connection) -> Result<Vec<RbContent>> {
    let mut statement = conn.prepare(
        "SELECT c.content_id, c.title, c.bpmx100, c.length, c.samplingRate, c.path, c.fileName,
                c.releaseYear, c.analysisDataFilePath,
                k.name, ar.name, al.name, g.name
         FROM content c
         LEFT JOIN key k ON k.key_id = c.key_id
         LEFT JOIN artist ar ON ar.artist_id = c.artist_id_artist
         LEFT JOIN album al ON al.album_id = c.album_id
         LEFT JOIN genre g ON g.genre_id = c.genre_id
         ORDER BY c.content_id",
    )?;
    let rows = statement.query_map([], |row| {
        let bpmx100: Option<i64> = row.get(2)?;
        let length: Option<i64> = row.get(3)?;
        let sample_rate: Option<i64> = row.get(4)?;
        let year: Option<i64> = row.get(7)?;
        Ok(RbContent {
            content_id: row.get(0)?,
            title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            bpm: bpmx100.map(|bpm| bpm as f32 / 100.0),
            length_seconds: length.and_then(|seconds| u64::try_from(seconds).ok()),
            sample_rate: sample_rate.and_then(|rate| u32::try_from(rate).ok()),
            path: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            file_name: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            analysis_path: row.get(8)?,
            key: row
                .get::<_, Option<String>>(9)?
                .as_deref()
                .and_then(Key::parse),
            artist: row.get(10)?,
            album: row.get(11)?,
            genre: row.get(12)?,
            year: year.and_then(|year| u32::try_from(year).ok()),
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(LibraryError::from)
}

fn query_playlists(conn: &Connection) -> Result<Vec<(i64, i64, String, i64)>> {
    let mut statement = conn.prepare(
        "SELECT playlist_id, COALESCE(sequenceNo, 0), name, COALESCE(playlist_id_parent, 0)
         FROM playlist ORDER BY sequenceNo, playlist_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            row.get(3)?,
        ))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(LibraryError::from)
}

fn query_playlist_items(conn: &Connection) -> Result<Vec<(i64, i64, i64)>> {
    let mut statement = conn.prepare(
        "SELECT playlist_id, content_id, COALESCE(sequenceNo, 0)
         FROM playlist_content ORDER BY playlist_id, sequenceNo",
    )?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(LibraryError::from)
}

fn anlz_offset_ms(path: &Path, rb_mp3_offset_ms: f64) -> f64 {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "mp3" {
        rb_mp3_offset_ms
    } else {
        0.0
    }
}

fn open_encrypted(path: &Path) -> Result<Connection> {
    let key = std::env::var("HYPERMIXX_REKORDBOX_KEY").unwrap_or_else(|_| EXPORT_KEY.to_owned());
    let conn = Connection::open(path)?;
    conn.execute_batch(&format!("PRAGMA key='{key}';"))?;
    // A copied WAL is replayed before the read-only guard goes up.
    let _: rusqlite::Result<()> = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    conn.execute_batch("PRAGMA query_only=ON;")?;
    let check: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if check != "ok" {
        return Err(LibraryError::Invalid(format!(
            "rekordbox database failed integrity_check: {check}"
        )));
    }
    Ok(conn)
}

struct TempCopy {
    dir: PathBuf,
    path: PathBuf,
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn copy_database(source: &Path) -> Result<TempCopy> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "hypermixx-rekordbox-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir)?;
    let target = dir.join("exportLibrary.db");
    std::fs::copy(source, &target)?;
    for suffix in ["-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if from.is_file() {
            let to = PathBuf::from(format!("{}{suffix}", target.display()));
            std::fs::copy(from, to)?;
        }
    }
    Ok(TempCopy { dir, path: target })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::db::detect_source_kinds;
    use crate::library::SourceKind;

    #[test]
    fn a_rekordbox_root_is_detected_by_its_library_file() {
        let dir = std::env::temp_dir().join(format!(
            "hypermixx-rb-detect-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let rb = dir.join("PIONEER/rekordbox");
        std::fs::create_dir_all(&rb).unwrap();
        std::fs::write(rb.join("exportLibrary.db"), b"x").unwrap();
        assert_eq!(detect_source_kinds(&dir), vec![SourceKind::Rekordbox]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mp3_is_the_only_calibrated_format() {
        assert_eq!(anlz_offset_ms(Path::new("a.MP3"), MP3_ANLZ_OFFSET_MS), MP3_ANLZ_OFFSET_MS);
        assert_eq!(anlz_offset_ms(Path::new("a.flac"), MP3_ANLZ_OFFSET_MS), 0.0);
    }
}
