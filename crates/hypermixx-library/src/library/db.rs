//! The SQLite cache that holds sources, lists, tracks and the analysis result.
//!
//! The connection lives behind a mutex: scanning runs on a worker, the TUI reads on the UI thread,
//! and neither holds the lock while decoding. WAL mode lets the reads proceed while a scan writes.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hypermixx_core::{BeatGrid, HotCue, TrackAnalysis};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use super::import::{folder, rekordbox, serato, vdj};
use super::model::{
    key_from_columns, key_mode_str, FileTags, Playlist, ScanEvent, ScanStats, Source, SourceKind,
    SourceOrigin, TrackLoad, TrackRow,
};

/// Bumped whenever `schema.sql` changes in a way that needs a new migration.
pub const SCHEMA_VERSION: i64 = 2;
const ANALYSIS_ALGORITHM: &str = "hypermixx";
const ANALYSIS_VERSION: &str = env!("CARGO_PKG_VERSION");
const SCHEMA: &str = include_str!("schema.sql");

/// Where the cache lives: `$HYPERMIXX_CACHE`, or `.cache` in the working directory.
pub const DEFAULT_CACHE_DIR: &str = ".cache";

pub fn cache_root() -> PathBuf {
    let override_dir = std::env::var("HYPERMIXX_CACHE").ok();
    cache_root_from(override_dir.as_deref())
}

fn cache_root_from(override_dir: Option<&str>) -> PathBuf {
    match override_dir.map(str::trim).filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(DEFAULT_CACHE_DIR),
    }
}

/// The default database file. The directory is created on open.
pub fn default_database_path() -> PathBuf {
    cache_root().join("library").join("library.sqlite3")
}

#[derive(Debug)]
pub enum LibraryError {
    Io(String),
    Sqlite(rusqlite::Error),
    Invalid(String),
}

impl fmt::Display for LibraryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => f.write_str(message),
            Self::Sqlite(err) => write!(f, "sqlite: {err}"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for LibraryError {}

impl From<std::io::Error> for LibraryError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<rusqlite::Error> for LibraryError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

pub type Result<T> = std::result::Result<T, LibraryError>;

/// Cue set ready to seed a deck after load.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadedCues {
    pub memory: Option<u64>,
    pub hot: Vec<HotCue>,
}

/// Source-side calibration applied while scanning. The decoder-start trim is deliberately not here:
/// it belongs to the file's decode and is supplied when a grid is materialised for playback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImportCalibration {
    /// Rekordbox ANLZ clock offset for MP3. The validated sample aligns with decoded frame 0 at 0.
    pub rkb_mp3_offset_ms: f64,
    /// Serato's tag clock. The validated sample sits 16.5 ms later than the RB/VDJ clock.
    pub serato_offset_ms: f64,
    /// VirtualDJ's ANLZ clock. Defaults to zero because its CDJ export writes the file clock.
    pub vdj_offset_ms: f64,
}

impl Default for ImportCalibration {
    fn default() -> Self {
        Self {
            rkb_mp3_offset_ms: 0.0,
            serato_offset_ms: -16.5,
            vdj_offset_ms: 0.0,
        }
    }
}

/// The library cache handle. Cheap to clone through an `Arc`; all access serialises on one mutex.
pub struct LibraryDb {
    conn: Mutex<Connection>,
    path: PathBuf,
    calibration: Mutex<ImportCalibration>,
}

impl LibraryDb {
    /// Opens (creating if needed) the default cache database.
    pub fn open_default() -> Result<Self> {
        Self::open(default_database_path())
    }

    /// Opens `path`, creating its folder and applying the schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
            calibration: Mutex::new(ImportCalibration::default()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_import_calibration(&self, calibration: ImportCalibration) {
        if let Ok(mut slot) = self.calibration.lock() {
            *slot = calibration;
        }
    }

    pub fn import_calibration(&self) -> ImportCalibration {
        self.calibration
            .lock()
            .map(|slot| *slot)
            .unwrap_or_default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| LibraryError::Invalid("library database lock poisoned".to_owned()))
    }

    /// Registers a folder source. Re-adding the same canonical root is idempotent.
    pub fn add_folder_source(&self, root: impl AsRef<Path>, name: Option<&str>) -> Result<Source> {
        self.add_source_with_origin(SourceKind::Folder, SourceOrigin::Folder, root, name)
    }

    /// Registers one local source of a known kind.
    pub fn add_source(
        &self,
        kind: SourceKind,
        root: impl AsRef<Path>,
        name: Option<&str>,
    ) -> Result<Source> {
        self.add_source_with_origin(kind, SourceOrigin::Local, root, name)
    }

    /// Registers one source with its discovery origin.
    pub fn add_source_with_origin(
        &self,
        kind: SourceKind,
        origin: SourceOrigin,
        root: impl AsRef<Path>,
        name: Option<&str>,
    ) -> Result<Source> {
        let root = std::fs::canonicalize(root.as_ref())?;
        if !root.is_dir() {
            return Err(LibraryError::Invalid(format!(
                "{} is not a directory",
                root.display()
            )));
        }
        let name = name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                root.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| root.display().to_string());
        let root_text = root.to_string_lossy().into_owned();
        let now = unix_now();
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO source (kind, name, root, origin, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(kind, root) DO UPDATE SET
                name = excluded.name, origin = excluded.origin",
            params![kind.as_str(), name, root_text, origin.as_str(), now],
        )?;
        source_by_root(&conn, kind, &root_text)
    }

    /// Detects and registers every source a path carries. A USB with rekordbox, VirtualDJ and
    /// Serato exports becomes three sources; an ordinary music folder becomes one folder source.
    pub fn add_path_sources(
        &self,
        root: impl AsRef<Path>,
        name: Option<&str>,
    ) -> Result<Vec<Source>> {
        let root = std::fs::canonicalize(root.as_ref())?;
        let mut kinds = detect_source_kinds(&root);
        if kinds.is_empty() {
            kinds.push(SourceKind::Folder);
        }
        kinds
            .into_iter()
            .map(|kind| self.add_source(kind, &root, name))
            .collect()
    }

    pub fn sources(&self) -> Result<Vec<Source>> {
        let conn = self.lock()?;
        let mut statement = conn.prepare(
            "SELECT id, kind, name, root, origin, last_scan_at
             FROM source ORDER BY kind, name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], source_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(LibraryError::from)
    }

    pub fn source(&self, id: i64) -> Result<Option<Source>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, kind, name, root, origin, last_scan_at FROM source WHERE id = ?1",
            [id],
            source_from_row,
        )
        .optional()
        .map_err(LibraryError::from)
    }

    pub fn remove_source(&self, id: i64) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM source WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn root_playlists(&self, source_id: i64) -> Result<Vec<Playlist>> {
        self.playlists(source_id, None)
    }

    pub fn child_playlists(&self, source_id: i64, parent_id: i64) -> Result<Vec<Playlist>> {
        self.playlists(source_id, Some(parent_id))
    }

    fn playlists(&self, source_id: i64, parent_id: Option<i64>) -> Result<Vec<Playlist>> {
        let conn = self.lock()?;
        let mut statement = conn.prepare(
            "SELECT id, source_id, parent_id, name, kind, position
             FROM playlist
             WHERE source_id = ?1 AND parent_id IS ?2
             ORDER BY position, name COLLATE NOCASE",
        )?;
        let rows = statement.query_map(params![source_id, parent_id], |row| {
            Ok(Playlist {
                id: row.get(0)?,
                source_id: row.get(1)?,
                parent_id: row.get(2)?,
                name: row.get(3)?,
                kind: row.get(4)?,
                position: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(LibraryError::from)
    }

    pub fn playlist(&self, id: i64) -> Result<Option<Playlist>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, source_id, parent_id, name, kind, position FROM playlist WHERE id = ?1",
            [id],
            |row| {
                Ok(Playlist {
                    id: row.get(0)?,
                    source_id: row.get(1)?,
                    parent_id: row.get(2)?,
                    name: row.get(3)?,
                    kind: row.get(4)?,
                    position: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(LibraryError::from)
    }

    /// The tracks directly in one list, in the order the source gave them.
    pub fn tracks_in_playlist(&self, playlist_id: i64) -> Result<Vec<TrackRow>> {
        let conn = self.lock()?;
        let mut statement = conn.prepare(
            "SELECT t.id, l.id, t.title, t.artist, t.album, t.bpm,
                    t.key_pc, t.key_mode, t.key_confidence,
                    t.duration_frames, t.sample_rate, l.path
             FROM playlist_item pi
             JOIN track_location l ON l.id = pi.location_id
             JOIN track t ON t.id = l.track_id
             WHERE pi.playlist_id = ?1
             ORDER BY pi.position",
        )?;
        let rows = statement.query_map([playlist_id], |row| {
            let title: Option<String> = row.get(2)?;
            let path: String = row.get(11)?;
            Ok(TrackRow {
                track_id: row.get(0)?,
                location_id: row.get(1)?,
                title: title
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| file_stem(&path)),
                artist: row.get(3)?,
                album: row.get(4)?,
                bpm: row.get::<_, Option<f64>>(5)?.map(|bpm| bpm as f32),
                key: key_from_columns(
                    row.get(6)?,
                    row.get::<_, Option<String>>(7)?.as_deref(),
                    row.get(8)?,
                ),
                duration_frames: row.get::<_, i64>(9)?.max(0) as u64,
                sample_rate: u32::try_from(row.get::<_, i64>(10)?).unwrap_or(44_100),
                path: PathBuf::from(path),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(LibraryError::from)
    }

    /// The load snapshot for one location. The caller queries this before spawning the decode
    /// worker, so the database lock is never held across a decode.
    pub fn track_load(&self, location_id: i64) -> Result<Option<TrackLoad>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT t.id, l.id, l.path, t.title, t.artist, t.album, t.bpm,
                    t.key_pc, t.key_mode, t.key_confidence,
                    t.duration_frames, t.sample_rate
             FROM track_location l JOIN track t ON t.id = l.track_id
             WHERE l.id = ?1",
            [location_id],
            |row| {
                let title: Option<String> = row.get(3)?;
                let path: String = row.get(2)?;
                Ok(TrackLoad {
                    track_id: row.get(0)?,
                    location_id: row.get(1)?,
                    path: PathBuf::from(&path),
                    title: title
                        .filter(|title| !title.trim().is_empty())
                        .unwrap_or_else(|| file_stem(&path)),
                    artist: row.get(4)?,
                    album: row.get(5)?,
                    bpm: row.get::<_, Option<f64>>(6)?.map(|bpm| bpm as f32),
                    key: key_from_columns(
                        row.get(7)?,
                        row.get::<_, Option<String>>(8)?.as_deref(),
                        row.get(9)?,
                    ),
                    duration_frames: row.get::<_, i64>(10)?.max(0) as u64,
                    sample_rate: u32::try_from(row.get::<_, i64>(11)?).unwrap_or(44_100),
                })
            },
        )
        .optional()
        .map_err(LibraryError::from)
    }

    /// Scans one registered folder source. `progress` is called per file on the scanning thread.
    pub fn scan_source(
        &self,
        source_id: i64,
        progress: &mut dyn FnMut(ScanEvent),
    ) -> Result<ScanStats> {
        let source = self
            .source(source_id)?
            .ok_or_else(|| LibraryError::Invalid(format!("no source {source_id}")))?;
        let scan_id = self.begin_scan(source_id)?;
        let outcome = match source.kind {
            SourceKind::Folder => folder::scan(self, &source, scan_id, progress),
            SourceKind::Rekordbox => rekordbox::scan(self, &source, scan_id, progress),
            SourceKind::VirtualDj => vdj::scan(self, &source, scan_id, progress),
            SourceKind::Serato => serato::scan(self, &source, scan_id, progress),
        };
        let (stats, status) = match &outcome {
            Ok(stats) => (stats.clone(), "ok"),
            Err(err) => (
                ScanStats {
                    errors: vec![err.to_string()],
                    ..Default::default()
                },
                "error",
            ),
        };
        self.finish_scan(scan_id, source_id, &stats, status)?;
        outcome
    }

    pub(crate) fn with_transaction<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.lock()?;
        let transaction = conn.transaction()?;
        let value = f(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }

    fn begin_scan(&self, source_id: i64) -> Result<i64> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO scan_run (source_id, started_at, status) VALUES (?1, ?2, 'running')",
            params![source_id, unix_now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn finish_scan(
        &self,
        scan_id: i64,
        source_id: i64,
        stats: &ScanStats,
        status: &str,
    ) -> Result<()> {
        let now = unix_now();
        let conn = self.lock()?;
        conn.execute(
            "UPDATE scan_run
             SET finished_at = ?1, status = ?2, files_seen = ?3, files_added = ?4,
                 files_updated = ?5, files_removed = ?6, errors = ?7
             WHERE id = ?8",
            params![
                now,
                status,
                stats.files_seen as i64,
                stats.files_added as i64,
                stats.files_updated as i64,
                stats.files_removed as i64,
                stats.errors.len() as i64,
                scan_id
            ],
        )?;
        conn.execute(
            "UPDATE source SET last_scan_at = ?1 WHERE id = ?2",
            params![now, source_id],
        )?;
        Ok(())
    }

    /// Builds the imported grid for a location at the requested engine rate. Imported grids win
    /// over the local analysis cache; `origin` priority breaks ties if a source ever carries two.
    pub fn imported_analysis(
        &self,
        location_id: i64,
        sample_rate: u32,
        decoder_start_ms: f64,
        extra_offset_ms: f64,
    ) -> Result<Option<TrackAnalysis>> {
        let conn = self.lock()?;
        let grid: Option<(i64, Option<f64>)> = conn
            .query_row(
                "SELECT id, bpm FROM beatgrid
                 WHERE location_id = ?1
                 ORDER BY CASE origin
                     WHEN 'manual' THEN 0
                     WHEN 'rekordbox_anlz' THEN 1
                     WHEN 'serato_tag' THEN 2
                     WHEN 'vdj_anlz' THEN 3
                     ELSE 9 END, id DESC
                 LIMIT 1",
                [location_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((grid_id, grid_bpm)) = grid else {
            return Ok(None);
        };
        let mut statement = conn.prepare(
            "SELECT a.raw_time_ms, b.offset_ms, a.bpm
             FROM beat_anchor a JOIN beatgrid b ON b.id = a.grid_id
             WHERE a.grid_id = ?1 ORDER BY a.seq",
        )?;
        let rows = statement.query_map([grid_id], |row| {
            Ok((
                row.get::<_, f64>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, Option<f64>>(2)?,
            ))
        })?;
        let anchors: Vec<(f64, f64, Option<f64>)> = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(LibraryError::from)?;
        let mut frames = Vec::with_capacity(anchors.len());
        for (raw_ms, offset_ms, _) in &anchors {
            let frame =
                ((raw_ms + offset_ms - decoder_start_ms + extra_offset_ms).max(0.0)
                    * f64::from(sample_rate)
                    / 1000.0)
                    .round() as u64;
            if frames.last().is_none_or(|last| frame > *last) {
                frames.push(frame);
            }
        }
        if frames.is_empty() {
            return Ok(None);
        }
        let (key_pc, key_mode, key_confidence, track_bpm, duration_frames, track_rate) = conn
            .query_row(
                "SELECT t.key_pc, t.key_mode, t.key_confidence, t.bpm,
                        t.duration_frames, t.sample_rate
                 FROM track_location l JOIN track t ON t.id = l.track_id
                 WHERE l.id = ?1",
                [location_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<f64>>(2)?,
                        row.get::<_, Option<f64>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?
            .unwrap_or((None, None, None, None, 0, 44_100));
        let bpm = track_bpm.or(grid_bpm).map(|bpm| bpm as f32);
        let beatgrid = if frames.len() == 1 && bpm.is_some_and(|bpm| bpm > 0.0) {
            // Serato often stores one marker at an arbitrary beat in the track. Build a constant
            // grid whose phase passes through that marker, starting at the last beat before zero.
            let bpm = bpm.unwrap();
            let frames_per_beat = f64::from(sample_rate) * 60.0 / f64::from(bpm);
            let anchor = frames[0] as f64;
            let start = anchor - (anchor / frames_per_beat).floor() * frames_per_beat;
            let total = if duration_frames > 0 && track_rate > 0 {
                duration_frames as f64 * f64::from(sample_rate) / track_rate as f64
            } else {
                anchor
            };
            BeatGrid::from_constant_bpm(bpm, start.round() as u64, total as u64, sample_rate)
        } else {
            BeatGrid::from_frames(frames, sample_rate)
        };
        Ok(Some(TrackAnalysis {
            beatgrid,
            key: key_from_columns(key_pc, key_mode.as_deref(), key_confidence),
            bpm,
        }))
    }

    /// Reads imported memory/hot cues in engine frames. Hot-cue indexes are stored 0-based.
    pub fn loaded_cues(
        &self,
        location_id: i64,
        sample_rate: u32,
        decoder_start_ms: f64,
        extra_offset_ms: f64,
    ) -> Result<LoadedCues> {
        let conn = self.lock()?;
        let mut statement = conn.prepare(
            "SELECT kind, idx, raw_time_ms, offset_ms
             FROM cue
             WHERE location_id = ?1
             ORDER BY CASE origin
                 WHEN 'manual' THEN 0
                 WHEN 'rekordbox_anlz' THEN 1
                 WHEN 'serato_tag' THEN 2
                 WHEN 'vdj_anlz' THEN 3
                 ELSE 9 END, id",
        )?;
        let rows = statement.query_map([location_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })?;
        let mut cues = LoadedCues::default();
        for row in rows {
            let (kind, idx, raw_ms, offset_ms) = row?;
            let frame =
                ((raw_ms + offset_ms - decoder_start_ms + extra_offset_ms).max(0.0)
                    * f64::from(sample_rate)
                    / 1000.0)
                    .round() as u64;
            match kind.as_str() {
                "memory" | "loop" if cues.memory.is_none() => cues.memory = Some(frame),
                "hot" | "hot_loop" => {
                    let Some(index) = idx.and_then(|idx| u8::try_from(idx).ok()) else {
                        continue;
                    };
                    if (index as usize) < hypermixx_core::HOT_CUE_SLOTS
                        && !cues.hot.iter().any(|cue| cue.slot == index)
                    {
                        cues.hot.push(HotCue { slot: index, frame });
                    }
                }
                _ => {}
            }
        }
        Ok(cues)
    }

    /// Stores the analyser result for a track, replacing an older result.
    pub fn save_analysis(&self, track_id: i64, analysis: &TrackAnalysis) -> Result<()> {
        let beat_frames_json = serde_json::to_string(&analysis.beatgrid.beat_frames)
            .map_err(|err| LibraryError::Invalid(format!("analysis JSON: {err}")))?;
        let (key_pc, key_mode, key_confidence) = match analysis.key {
            Some(key) => (
                Some(i64::from(key.pc)),
                Some(key_mode_str(key.mode)),
                Some(f64::from(key.confidence)),
            ),
            None => (None, None, None),
        };
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO analysis
                (track_id, bpm, key_pc, key_mode, key_confidence,
                 beat_frames_json, sample_rate, algorithm, version, analysed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(track_id) DO UPDATE SET
                bpm = excluded.bpm, key_pc = excluded.key_pc, key_mode = excluded.key_mode,
                key_confidence = excluded.key_confidence,
                beat_frames_json = excluded.beat_frames_json,
                sample_rate = excluded.sample_rate, algorithm = excluded.algorithm,
                version = excluded.version, analysed_at = excluded.analysed_at",
            params![
                track_id,
                analysis.bpm.map(f64::from),
                key_pc,
                key_mode,
                key_confidence,
                beat_frames_json,
                i64::from(analysis.beatgrid.sample_rate),
                ANALYSIS_ALGORITHM,
                ANALYSIS_VERSION,
                unix_now()
            ],
        )?;
        Ok(())
    }

    /// Reads the cached analyser result, if it was produced by the current analyser version.
    pub fn load_analysis(&self, track_id: i64) -> Result<Option<TrackAnalysis>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT bpm, key_pc, key_mode, key_confidence, beat_frames_json, sample_rate
                 FROM analysis
                 WHERE track_id = ?1 AND algorithm = ?2 AND version = ?3",
                params![track_id, ANALYSIS_ALGORITHM, ANALYSIS_VERSION],
                |row| {
                    Ok((
                        row.get::<_, Option<f64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<f64>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((bpm, key_pc, key_mode, key_confidence, beat_frames_json, sample_rate)) = row
        else {
            return Ok(None);
        };
        let beat_frames: Vec<u64> = serde_json::from_str(&beat_frames_json)
            .map_err(|err| LibraryError::Invalid(format!("analysis JSON: {err}")))?;
        let sample_rate = u32::try_from(sample_rate).unwrap_or(44_100);
        Ok(Some(TrackAnalysis {
            beatgrid: BeatGrid::from_frames(beat_frames, sample_rate),
            key: key_from_columns(key_pc, key_mode.as_deref(), key_confidence),
            bpm: bpm.map(|bpm| bpm as f32),
        }))
    }
}

/// Which formats a root carries, by their marker files. Read-only detection; no scan yet.
pub fn detect_source_kinds(root: &Path) -> Vec<SourceKind> {
    let mut kinds = Vec::new();
    if root.join("PIONEER/rekordbox/exportLibrary.db").is_file()
        || root.join("PIONEER/rekordbox/export.pdb").is_file()
        || root.join("exportLibrary.db").is_file()
        || root.join("export.pdb").is_file()
    {
        kinds.push(SourceKind::Rekordbox);
    }
    if root.join("VirtualDJ/database.xml").is_file() || root.join("database.xml").is_file() {
        kinds.push(SourceKind::VirtualDj);
    }
    if root.join("_Serato_/database V2").is_file() || root.join("database V2").is_file() {
        kinds.push(SourceKind::Serato);
    }
    kinds
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == 0 {
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    } else if version == 1 {
        conn.execute_batch(
            "ALTER TABLE source ADD COLUMN origin TEXT NOT NULL DEFAULT 'local';
             UPDATE source SET origin = 'folder' WHERE kind = 'folder';
             PRAGMA user_version = 2;",
        )?;
    } else if version != SCHEMA_VERSION {
        return Err(LibraryError::Invalid(format!(
            "library schema {} is newer than this build ({SCHEMA_VERSION})",
            version
        )));
    }
    Ok(())
}

fn source_by_root(conn: &Connection, kind: SourceKind, root: &str) -> Result<Source> {
    Ok(conn.query_row(
        "SELECT id, kind, name, root, origin, last_scan_at
         FROM source WHERE kind = ?1 AND root = ?2",
        params![kind.as_str(), root],
        source_from_row,
    )?)
}

fn source_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Source> {
    let kind: String = row.get(1)?;
    let origin: String = row.get(4)?;
    Ok(Source {
        id: row.get(0)?,
        kind: SourceKind::parse(&kind).unwrap_or(SourceKind::Folder),
        origin: SourceOrigin::parse(&origin).unwrap_or(SourceOrigin::Local),
        name: row.get(2)?,
        root: PathBuf::from(row.get::<_, String>(3)?),
        last_scan_at: row.get(5)?,
    })
}

pub(crate) fn insert_playlist(
    conn: &Connection,
    source_id: i64,
    parent_id: Option<i64>,
    native_key: &str,
    name: &str,
    position: i64,
) -> Result<i64> {
    insert_playlist_kind(
        conn, source_id, parent_id, native_key, name, "folder", position,
    )
}

pub(crate) fn insert_playlist_kind(
    conn: &Connection,
    source_id: i64,
    parent_id: Option<i64>,
    native_key: &str,
    name: &str,
    kind: &str,
    position: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO playlist (source_id, parent_id, native_key, name, kind, position)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![source_id, parent_id, native_key, name, kind, position],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn upsert_track(
    conn: &Connection,
    content_key: &str,
    tags: &FileTags,
    now: i64,
) -> Result<i64> {
    let (key_pc, key_mode, key_confidence) = match tags.key {
        Some(key) => (
            Some(i64::from(key.pc)),
            Some(key_mode_str(key.mode)),
            Some(f64::from(key.confidence)),
        ),
        None => (None, None, None),
    };
    conn.execute(
        "INSERT INTO track
            (content_key, title, artist, album, genre, year, duration_frames, sample_rate,
             channels, bpm, key_pc, key_mode, key_confidence, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)
         ON CONFLICT(content_key) DO UPDATE SET
            title = excluded.title, artist = excluded.artist, album = excluded.album,
            genre = excluded.genre, year = excluded.year,
            duration_frames = excluded.duration_frames, sample_rate = excluded.sample_rate,
            channels = excluded.channels, bpm = excluded.bpm,
            key_pc = excluded.key_pc, key_mode = excluded.key_mode,
            key_confidence = excluded.key_confidence, updated_at = excluded.updated_at",
        params![
            content_key,
            tags.title,
            tags.artist,
            tags.album,
            tags.genre,
            tags.year.map(i64::from),
            tags.duration_frames as i64,
            i64::from(tags.sample_rate),
            i64::from(tags.channels),
            tags.bpm.map(f64::from),
            key_pc,
            key_mode,
            key_confidence,
            now
        ],
    )?;
    Ok(conn.query_row(
        "SELECT id FROM track WHERE content_key = ?1",
        [content_key],
        |row| row.get(0),
    )?)
}

pub(crate) fn upsert_location(
    conn: &Connection,
    track_id: i64,
    source_id: i64,
    path: &Path,
    relative_path: &str,
    file_size: i64,
    mtime_ns: i64,
    file_hash: &str,
    scan_id: i64,
) -> Result<(i64, bool)> {
    let path = path.to_string_lossy().into_owned();
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM track_location WHERE source_id = ?1 AND path = ?2",
            params![source_id, path],
            |row| row.get(0),
        )
        .optional()?;
    conn.execute(
        "INSERT INTO track_location
            (track_id, source_id, path, relative_path, file_size, mtime_ns, file_hash,
             last_seen_scan)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(source_id, path) DO UPDATE SET
            track_id = excluded.track_id, relative_path = excluded.relative_path,
            file_size = excluded.file_size, mtime_ns = excluded.mtime_ns,
            file_hash = excluded.file_hash, last_seen_scan = excluded.last_seen_scan",
        params![
            track_id,
            source_id,
            path,
            relative_path,
            file_size,
            mtime_ns,
            file_hash,
            scan_id
        ],
    )?;
    let id = conn.query_row(
        "SELECT id FROM track_location WHERE source_id = ?1 AND path = ?2",
        params![source_id, path],
        |row| row.get(0),
    )?;
    Ok((id, existing.is_none()))
}

pub(crate) fn unchanged_location(
    conn: &Connection,
    source_id: i64,
    path: &Path,
    file_size: i64,
    mtime_ns: i64,
) -> Result<Option<i64>> {
    let path = path.to_string_lossy();
    Ok(conn
        .query_row(
            "SELECT id FROM track_location
             WHERE source_id = ?1 AND path = ?2 AND file_size = ?3 AND mtime_ns = ?4",
            params![source_id, path.as_ref(), file_size, mtime_ns],
            |row| row.get(0),
        )
        .optional()?)
}

pub(crate) fn touch_location(conn: &Connection, location_id: i64, scan_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE track_location SET last_seen_scan = ?1 WHERE id = ?2",
        params![scan_id, location_id],
    )?;
    Ok(())
}

pub(crate) fn insert_playlist_item(
    conn: &Connection,
    playlist_id: i64,
    location_id: i64,
    position: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO playlist_item (playlist_id, location_id, position) VALUES (?1, ?2, ?3)",
        params![playlist_id, location_id, position],
    )?;
    Ok(())
}

pub(crate) fn clear_imported_timing(conn: &Connection, source_id: i64) -> Result<()> {
    conn.execute("DELETE FROM cue WHERE source_id = ?1", [source_id])?;
    conn.execute("DELETE FROM beatgrid WHERE source_id = ?1", [source_id])?;
    Ok(())
}

pub(crate) fn insert_cue(
    conn: &Connection,
    location_id: i64,
    source_id: i64,
    origin: &str,
    kind: &str,
    idx: Option<i64>,
    raw_time_ms: f64,
    clock: &str,
    offset_ms: f64,
    color: Option<&str>,
    comment: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO cue
            (location_id, source_id, origin, kind, idx, raw_time_ms, clock, offset_ms, color, comment)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            location_id,
            source_id,
            origin,
            kind,
            idx,
            raw_time_ms,
            clock,
            offset_ms,
            color,
            comment
        ],
    )?;
    Ok(())
}

pub(crate) fn insert_beatgrid(
    conn: &Connection,
    location_id: i64,
    source_id: i64,
    origin: &str,
    bpm: Option<f32>,
    anchor_raw_ms: Option<f64>,
    clock: &str,
    offset_ms: f64,
    confidence: Option<f64>,
    algorithm_version: &str,
    beats: &[(u16, f32, f64)],
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO beatgrid
            (location_id, source_id, origin, bpm, anchor_raw_ms, clock, offset_ms, confidence,
             algorithm_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            location_id,
            source_id,
            origin,
            bpm.map(f64::from),
            anchor_raw_ms,
            clock,
            offset_ms,
            confidence,
            algorithm_version
        ],
    )?;
    let grid_id: i64 = conn.query_row(
        "SELECT id FROM beatgrid WHERE location_id = ?1 AND origin = ?2",
        params![location_id, origin],
        |row| row.get(0),
    )?;
    for (seq, (beat, beat_bpm, raw_ms)) in beats.iter().enumerate() {
        conn.execute(
            "INSERT OR REPLACE INTO beat_anchor
                (grid_id, seq, beat, bpm, raw_time_ms, time_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                grid_id,
                seq as i64,
                i64::from(*beat),
                f64::from(*beat_bpm),
                *raw_ms,
                *raw_ms + offset_ms
            ],
        )?;
    }
    Ok(())
}

pub(crate) fn finish_source_scan(conn: &Connection, source_id: i64, scan_id: i64) -> Result<usize> {
    let removed = conn.execute(
        "DELETE FROM track_location WHERE source_id = ?1 AND last_seen_scan <> ?2",
        params![source_id, scan_id],
    )?;
    conn.execute(
        "DELETE FROM track
         WHERE id NOT IN (SELECT track_id FROM track_location)",
        [],
    )?;
    Ok(removed)
}

fn file_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> (LibraryDb, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "hypermixx-library-{name}-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = LibraryDb::open(dir.join("library.sqlite3")).unwrap();
        (db, dir)
    }

    #[test]
    fn a_folder_source_is_idempotent() {
        let (db, dir) = temp_db("source");
        let music = dir.join("music");
        std::fs::create_dir_all(&music).unwrap();
        let first = db.add_folder_source(&music, None).unwrap();
        let second = db.add_folder_source(&music, Some("Mix")).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.name, "Mix");
        assert_eq!(db.sources().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn wav_bytes(sample: i16, frames: usize) -> Vec<u8> {
        let data_len = frames * 2;
        let mut out = Vec::with_capacity(44 + data_len);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&8_000u32.to_le_bytes());
        out.extend_from_slice(&16_000u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data_len as u32).to_le_bytes());
        for _ in 0..frames {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    #[test]
    fn a_folder_scan_builds_the_tree_and_is_incremental() {
        let (db, dir) = temp_db("scan");
        let root = dir.join("music");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("a.wav"), wav_bytes(100, 400)).unwrap();
        std::fs::write(sub.join("b.wav"), wav_bytes(200, 400)).unwrap();

        let source = db.add_folder_source(&root, None).unwrap();
        let stats = db.scan_source(source.id, &mut |_| {}).unwrap();
        assert_eq!(stats.files_seen, 2, "{stats:?}");
        assert_eq!(stats.files_added, 2, "{stats:?}");
        assert!(stats.errors.is_empty(), "{stats:?}");

        let roots = db.root_playlists(source.id).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(db.tracks_in_playlist(roots[0].id).unwrap().len(), 1);
        let children = db.child_playlists(source.id, roots[0].id).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(db.tracks_in_playlist(children[0].id).unwrap().len(), 1);

        let again = db.scan_source(source.id, &mut |_| {}).unwrap();
        assert_eq!(again.files_added, 0);
        assert_eq!(again.files_updated, 2);

        std::fs::remove_file(sub.join("b.wav")).unwrap();
        let removed = db.scan_source(source.id, &mut |_| {}).unwrap();
        assert_eq!(removed.files_removed, 1);
        let children = db.child_playlists(source.id, roots[0].id).unwrap();
        assert!(db.tracks_in_playlist(children[0].id).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_analysis_round_trips_through_the_cache() {
        let (db, dir) = temp_db("analysis");
        let root = dir.join("music");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.wav"), wav_bytes(100, 400)).unwrap();
        let source = db.add_folder_source(&root, None).unwrap();
        db.scan_source(source.id, &mut |_| {}).unwrap();
        let playlist = db.root_playlists(source.id).unwrap().remove(0);
        let track = db.tracks_in_playlist(playlist.id).unwrap().remove(0);

        let analysis = TrackAnalysis {
            beatgrid: BeatGrid::from_constant_bpm(124.0, 0, track.duration_frames, 44_100),
            key: Some(hypermixx_core::Key {
                pc: 9,
                mode: hypermixx_core::KeyMode::Minor,
                confidence: 0.9,
            }),
            bpm: Some(124.0),
        };
        db.save_analysis(track.track_id, &analysis).unwrap();
        let loaded = db.load_analysis(track.track_id).unwrap().expect("cached");
        assert_eq!(loaded.beatgrid.beat_frames, analysis.beatgrid.beat_frames);
        assert_eq!(loaded.key.map(|key| key.camelot()), Some("8A".to_owned()));
        assert!((loaded.bpm.unwrap() - 124.0).abs() < 0.01);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn imported_grid_and_cues_become_a_load_snapshot() {
        let (db, dir) = temp_db("imported");
        let root = dir.join("music");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.wav"), wav_bytes(100, 400)).unwrap();
        let source = db.add_folder_source(&root, None).unwrap();
        db.scan_source(source.id, &mut |_| {}).unwrap();
        let playlist = db.root_playlists(source.id).unwrap().remove(0);
        let track = db.tracks_in_playlist(playlist.id).unwrap().remove(0);

        db.with_transaction(|tx| {
            insert_beatgrid(
                tx,
                track.location_id,
                source.id,
                "rekordbox_anlz",
                Some(124.0),
                Some(100.0),
                "rekordbox_anlz",
                16.5,
                Some(1.0),
                "test",
                &[(1, 124.0, 100.0), (2, 124.0, 584.0)],
            )?;
            insert_cue(
                tx,
                track.location_id,
                source.id,
                "rekordbox_anlz",
                "hot",
                Some(0),
                100.0,
                "rekordbox_anlz",
                16.5,
                Some("#24ff8c"),
                None,
            )?;
            insert_cue(
                tx,
                track.location_id,
                source.id,
                "rekordbox_anlz",
                "memory",
                Some(0),
                200.0,
                "rekordbox_anlz",
                16.5,
                None,
                None,
            )?;
            Ok(())
        })
        .unwrap();

        let analysis = db
            .imported_analysis(track.location_id, 48_000, 0.0, 0.0)
            .unwrap()
            .expect("imported grid");
        assert_eq!(analysis.beatgrid.beat_frames.len(), 2);
        assert_eq!(analysis.beatgrid.beat_frames[0], 5_592);
        let cues = db.loaded_cues(track.location_id, 48_000, 0.0, 0.0).unwrap();
        assert_eq!(
            cues.hot,
            vec![HotCue {
                slot: 0,
                frame: 5_592
            }]
        );
        assert_eq!(cues.memory, Some(10_392));

        // Gapless decoding removes 25.0567 ms; without this subtraction every imported grid is
        // late by exactly that amount. A positive extra offset shifts it later again.
        let compensated = db
            .imported_analysis(track.location_id, 48_000, 25.056689, 0.0)
            .unwrap()
            .expect("compensated grid");
        assert_eq!(compensated.beatgrid.beat_frames[0], 4_389);
        let cues = db
            .loaded_cues(track.location_id, 48_000, 25.056689, 0.0)
            .unwrap();
        assert_eq!(
            cues.hot,
            vec![HotCue {
                slot: 0,
                frame: 4_389
            }]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_one_cache_gains_the_source_origin_column() {
        let dir = std::env::temp_dir().join(format!(
            "hypermixx-library-migrate-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("library.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE source (
                    id INTEGER PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                    root TEXT NOT NULL, fingerprint TEXT, created_at INTEGER NOT NULL,
                    last_scan_at INTEGER, UNIQUE (kind, root));
                 INSERT INTO source (kind, name, root, created_at)
                    VALUES ('folder', 'old', '/music', 1);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let db = LibraryDb::open(&path).unwrap();
        let source = db.sources().unwrap().remove(0);
        assert_eq!(source.origin, SourceOrigin::Folder);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_empty_cache_has_the_expected_schema() {
        let (db, dir) = temp_db("schema");
        let conn = db.lock().unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN
                 ('source','playlist','track','track_location','playlist_item','cue','beatgrid')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 7);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
