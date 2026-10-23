//! The library cache: sources, multi-level lists, tracks, tags and the scan that fills the database.
//!
//! Phase 1 reads folders and keeps imported cue/grid tables ready for the Rekordbox adapter. The
//! database is a rebuildable cache; the sources remain the authority.

pub mod db;
pub mod import;
pub mod model;
pub mod tags;

pub use db::{
    cache_root, default_database_path, detect_source_kinds, LibraryDb, LibraryError, ImportCalibration, LoadedCues,
    Result, DEFAULT_CACHE_DIR, SCHEMA_VERSION,
};
pub use model::{
    FileTags, Playlist, ScanEvent, ScanStats, Source, SourceKind, SourceOrigin, TrackLoad, TrackRow,
};
