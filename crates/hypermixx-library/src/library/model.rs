//! The library domain: sources, their multi-level lists, tracks, and the rows a front-end reads.

use std::fmt;
use std::path::PathBuf;

use hypermixx_core::{Key, KeyMode};

/// What kind of source a library row came from. Only `Folder` is read in phase 1; the other
/// variants are reserved for the adapters that follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Folder,
    Rekordbox,
    VirtualDj,
    Serato,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Rekordbox => "rekordbox",
            Self::VirtualDj => "vdj",
            Self::Serato => "serato",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "folder" => Some(Self::Folder),
            "rekordbox" => Some(Self::Rekordbox),
            "vdj" => Some(Self::VirtualDj),
            "serato" => Some(Self::Serato),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Rekordbox => "rekordbox",
            Self::VirtualDj => "virtualdj",
            Self::Serato => "serato",
        }
    }

    /// The format name shown next to a source in the browser.
    pub fn source_label(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Rekordbox => "onelibrary",
            Self::VirtualDj => "vdj",
            Self::Serato => "serato",
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Where a source was discovered. This is independent of its format: the same USB can carry an
/// OneLibrary, Serato, or VirtualDJ source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceOrigin {
    Folder,
    Local,
    Usb,
}

impl SourceOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Local => "local",
            Self::Usb => "usb",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "folder" => Some(Self::Folder),
            "local" => Some(Self::Local),
            "usb" => Some(Self::Usb),
            _ => None,
        }
    }
}

/// One registered library. A USB with rekordbox and VirtualDJ exports becomes two sources with the
/// same root; the UI names both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub id: i64,
    pub kind: SourceKind,
    pub origin: SourceOrigin,
    pub name: String,
    pub root: PathBuf,
    pub last_scan_at: Option<i64>,
}

impl Source {
    pub fn display_name(&self) -> String {
        if self.kind == SourceKind::Folder {
            self.name.clone()
        } else {
            format!("{} ({})", self.name, self.kind.source_label())
        }
    }
}

/// One node of a source's list tree. A folder source maps its directories onto these rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playlist {
    pub id: i64,
    pub source_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    /// `folder`, `playlist`, or `crate`, used by the browser's icon.
    pub kind: String,
    pub position: i64,
}

/// One track as the browser and the load path see it: the content metadata plus the location that
/// actually holds the file.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackRow {
    pub track_id: i64,
    pub location_id: i64,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub bpm: Option<f32>,
    pub key: Option<Key>,
    pub duration_frames: u64,
    pub sample_rate: u32,
    pub path: PathBuf,
}

/// The snapshot the loader needs, queried before the decode worker starts.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackLoad {
    pub track_id: i64,
    pub location_id: i64,
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub bpm: Option<f32>,
    pub key: Option<Key>,
    pub duration_frames: u64,
    pub sample_rate: u32,
}

/// Per-file metadata read from the audio tags. Missing values stay `None`; the file still enters the
/// library, because an untagged track is playable and should not disappear.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub year: Option<u32>,
    pub bpm: Option<f32>,
    pub key: Option<Key>,
    pub duration_frames: u64,
    pub sample_rate: u32,
    pub channels: u32,
}

/// One progress line while a source is being scanned. `index` is 1-based over files visited so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanEvent {
    pub index: usize,
    pub path: PathBuf,
}

/// What one refresh changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub files_seen: usize,
    pub files_added: usize,
    pub files_updated: usize,
    pub files_removed: usize,
    pub errors: Vec<String>,
}

impl ScanStats {
    pub fn summary(&self) -> String {
        let errors = if self.errors.is_empty() {
            String::new()
        } else {
            format!(", {} error(s)", self.errors.len())
        };
        format!(
            "{} file(s): {} added, {} updated, {} removed{errors}",
            self.files_seen, self.files_added, self.files_updated, self.files_removed
        )
    }
}

/// Reconstructs a `Key` from the two columns the schema stores.
pub(crate) fn key_from_columns(
    pc: Option<i64>,
    mode: Option<&str>,
    confidence: Option<f64>,
) -> Option<Key> {
    let pc = u8::try_from(pc?).ok()?;
    let mode = match mode? {
        "minor" => KeyMode::Minor,
        _ => KeyMode::Major,
    };
    Some(Key {
        pc: pc % 12,
        mode,
        confidence: confidence.unwrap_or(1.0) as f32,
    })
}

pub(crate) fn key_mode_str(mode: KeyMode) -> &'static str {
    match mode {
        KeyMode::Major => "major",
        KeyMode::Minor => "minor",
    }
}
