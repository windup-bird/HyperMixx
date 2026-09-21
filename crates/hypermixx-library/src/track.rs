//! A track record: identity, metadata, an editable beat spec + key, and a lazily-compiled
//! runtime analysis.
//!
//! The authoritative fields are [`BeatSpec`] (compact, editable, keeps BPM) and [`Key`]. The
//! [`TrackAnalysis`] the deck consumes is *derived* and cached; editing the spec invalidates it.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use hypermixx_core::{Key, TrackAnalysis};

use crate::beat_spec::BeatSpec;
use crate::grid_compiler::GridCompiler;
use crate::waveform::Waveform;

/// Stable identifier for a track in the library / future database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(pub u64);

/// Descriptive tags. Everything optional so a partially-known file still loads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub year: Option<u32>,
    pub duration_frames: u64,
    pub sample_rate: u32,
}

/// One analysed track. `spec` is the editable truth; `analysis` is the compiled cache.
#[derive(Debug)]
pub struct TrackInfo {
    pub id: TrackId,
    pub path: PathBuf,
    pub metadata: Metadata,
    /// The editable grid description.
    pub spec: Mutex<BeatSpec>,
    pub key: Option<Key>,
    /// Compiled runtime grid, rebuilt after any spec edit.
    compiled: Mutex<Option<TrackAnalysis>>,
    /// Peak overview, computed on demand.
    waveform: OnceLock<Waveform>,
}

impl TrackInfo {
    /// A new record with a given spec/key and an empty compiled cache.
    pub fn new(
        id: TrackId,
        path: PathBuf,
        metadata: Metadata,
        spec: BeatSpec,
        key: Option<Key>,
    ) -> Self {
        Self {
            id,
            path,
            metadata,
            spec: Mutex::new(spec),
            key,
            compiled: Mutex::new(None),
            waveform: OnceLock::new(),
        }
    }

    /// Returns the compiled analysis, building it from the current spec if needed.
    pub fn analysis(&self) -> TrackAnalysis {
        if let Some(cached) = self.compiled.lock().unwrap().as_ref() {
            return cached.clone();
        }
        let spec = self.spec.lock().unwrap().clone();
        let compiler = GridCompiler::new(self.metadata.sample_rate, self.metadata.duration_frames);
        let analysis = TrackAnalysis {
            beatgrid: compiler.compile(&spec),
            key: self.key,
            bpm: None,
        };
        *self.compiled.lock().unwrap() = Some(analysis.clone());
        analysis
    }

    /// Replaces the grid spec and drops the compiled cache (call after any beat edit).
    pub fn set_spec(&self, spec: BeatSpec) {
        *self.spec.lock().unwrap() = spec;
        self.invalidate();
    }

    /// Drops the cached compiled grid so the next [`analysis`](Self::analysis) rebuilds.
    pub fn invalidate(&self) {
        *self.compiled.lock().unwrap() = None;
    }

    /// Peak overview for waveform drawing, built once and memoized.
    pub fn waveform(&self, source: &dyn hypermixx_media::Source) -> &Waveform {
        self.waveform
            .get_or_init(|| crate::waveform::Waveform::build(source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_core::{KeyMode, SAMPLE_RATE};

    fn meta() -> Metadata {
        Metadata {
            duration_frames: 48_000 * 5,
            sample_rate: SAMPLE_RATE,
            ..Default::default()
        }
    }

    #[test]
    fn compiles_and_caches_then_invalidates() {
        let track = TrackInfo::new(
            TrackId(1),
            PathBuf::from("x.mp3"),
            meta(),
            BeatSpec::rigid(122.0, 0),
            Some(Key {
                pc: 9,
                mode: KeyMode::Minor,
                confidence: 0.8,
            }),
        );
        let a1 = track.analysis();
        assert!(!a1.beatgrid.is_empty());
        assert_eq!(a1.key, track.key);

        // Editing the spec to a different tempo must change the compiled grid.
        track.set_spec(BeatSpec::rigid(150.0, 0));
        let a2 = track.analysis();
        assert!(
            (a2.beatgrid.average_bpm() - 150.0).abs() < 1.0,
            "stale cache after edit"
        );
    }
}
