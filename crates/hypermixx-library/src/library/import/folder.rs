//! Folder-source scanner: directories become the multi-level list tree, audio files become tracks.

use std::collections::HashMap;
use std::path::PathBuf;

use walkdir::WalkDir;

use super::super::db::{
    finish_source_scan, insert_playlist, insert_playlist_item, touch_location, unchanged_location,
    unix_now, upsert_location, upsert_track, LibraryDb, Result,
};
use super::super::model::{ScanEvent, ScanStats, Source};
use super::super::tags;
use super::util;

pub(crate) fn scan(
    db: &LibraryDb,
    source: &Source,
    scan_id: i64,
    progress: &mut dyn FnMut(ScanEvent),
) -> Result<ScanStats> {
    let root = source.root.clone();
    let root_name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    let now = unix_now();

    db.with_transaction(|tx| {
        // Rebuild the source's tree each refresh. Track/location rows survive by content/path, so
        // analysis and stable track ids are unaffected.
        tx.execute("DELETE FROM playlist WHERE source_id = ?1", [source.id])?;
        let root_playlist = insert_playlist(tx, source.id, None, "", &root_name, 0)?;

        let mut directories: HashMap<PathBuf, i64> = HashMap::new();
        directories.insert(root.clone(), root_playlist);
        let mut positions: HashMap<i64, i64> = HashMap::new();
        positions.insert(root_playlist, 0);
        let mut child_positions: HashMap<i64, i64> = HashMap::new();

        let mut stats = ScanStats::default();
        let walker = WalkDir::new(&root)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
            });

        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    stats.errors.push(err.to_string());
                    continue;
                }
            };
            let path = entry.path().to_path_buf();
            if entry.file_type().is_dir() {
                if entry.depth() == 0 {
                    continue;
                }
                let relative = util::relative_key(&root, &path);
                let parent = path.parent().unwrap_or(&root);
                let parent_id = *directories.get(parent).unwrap_or(&root_playlist);
                let name = entry.file_name().to_string_lossy().into_owned();
                let position = child_positions.entry(parent_id).or_insert(0);
                let playlist_id =
                    insert_playlist(tx, source.id, Some(parent_id), &relative, &name, *position)?;
                *position += 1;
                directories.insert(path, playlist_id);
                positions.insert(playlist_id, 0);
                continue;
            }
            if !util::is_audio(&path) {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(err) => {
                    stats.errors.push(format!("{}: {err}", path.display()));
                    continue;
                }
            };
            let file_size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
            let mtime_ns = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos() as i64)
                .unwrap_or(0);
            let parent = path.parent().unwrap_or(&root);
            let playlist_id = *directories.get(parent).unwrap_or(&root_playlist);
            if let Some(location_id) =
                unchanged_location(tx, source.id, &path, file_size, mtime_ns)?
            {
                touch_location(tx, location_id, scan_id)?;
                let position = positions.entry(playlist_id).or_insert(0);
                insert_playlist_item(tx, playlist_id, location_id, *position)?;
                *position += 1;
                stats.files_seen += 1;
                stats.files_updated += 1;
                progress(ScanEvent {
                    index: stats.files_seen,
                    path,
                });
                continue;
            }
            let file_hash = match util::hash_file(&path) {
                Ok(hash) => hash,
                Err(err) => {
                    stats.errors.push(err);
                    continue;
                }
            };
            let mut file_tags = match tags::read_tags(&path) {
                Ok(tags) => tags,
                Err(err) => {
                    stats.errors.push(err.to_string());
                    continue;
                }
            };
            if file_tags.title.is_none() {
                file_tags.title = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned());
            }
            let content_key = format!("sha256:{file_hash}:{file_size}");
            let track_id = upsert_track(tx, &content_key, &file_tags, now)?;
            let relative = util::relative_key(&root, &path);
            let (location_id, added) = upsert_location(
                tx, track_id, source.id, &path, &relative, file_size, mtime_ns, &file_hash, scan_id,
            )?;

            let position = positions.entry(playlist_id).or_insert(0);
            insert_playlist_item(tx, playlist_id, location_id, *position)?;
            *position += 1;

            stats.files_seen += 1;
            if added {
                stats.files_added += 1;
            } else {
                stats.files_updated += 1;
            }
            progress(ScanEvent {
                index: stats.files_seen,
                path,
            });
        }

        stats.files_removed = finish_source_scan(tx, source.id, scan_id)?;
        Ok(stats)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_folder_walk_still_sees_supported_files() {
        assert!(util::is_audio(Path::new("a.MP3")));
        assert!(!util::is_audio(Path::new("cover.jpg")));
    }
}
