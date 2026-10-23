//! Hypermixx library: track records, editable beat specs, the grid compiler, waveform, and the
//! beat/key analyser.
//!
//! Knows `core` (types) and `media` (PCM + decode) but never `audio` — analysis and the real-time
//! engine are kept independent so a library scan can run without an audio device.

pub mod analyser;
pub mod beat_spec;
pub mod grid_compiler;
pub mod library;
pub mod track;
pub mod waveform;

pub use analyser::{analyze, AnalyserError, Backend, RawAnalysis, RefineConfig};
pub use beat_spec::{BeatSpec, Segment};
pub use grid_compiler::GridCompiler;
pub use library::Result;
pub use library::{
    cache_root, default_database_path, detect_source_kinds, FileTags, LibraryDb, LibraryError,
    ImportCalibration, LoadedCues, Playlist, ScanEvent, ScanStats, Source, SourceKind, SourceOrigin, TrackLoad, TrackRow,
};
pub use track::{Metadata, TrackId, TrackInfo};
pub use waveform::{BandPeaks, StereoPeaks, Waveform, BASE_BUCKET};

// Re-exported so callers get an end-to-end path without naming core directly.
pub use hypermixx_core::{Key, KeyMode, TrackAnalysis};
