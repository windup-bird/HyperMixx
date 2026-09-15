//! The compiled, runtime-facing analysis a deck consumes.

use crate::beatgrid::BeatGrid;
use crate::key::Key;

/// Everything a deck knows about a track's musical grid and key, ready for playback.
///
/// This is a *compiled* product: `library` turns an editable [`crate::BeatSpec`](crate) plus an
/// analysis into one of these via the grid compiler, then ships it to a deck. It is intentionally
/// free of editor concerns (segments, provenance) — those live in `library`.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackAnalysis {
    pub beatgrid: BeatGrid,
    pub key: Option<Key>,
    pub bpm: Option<f32>,
}

impl TrackAnalysis {
    /// A grid-only analysis (constant-BPM load path), no key.
    pub fn from_grid(beatgrid: BeatGrid, bpm: f32) -> Self {
        Self {
            beatgrid,
            key: None,
            bpm: Some(bpm),
        }
    }

    /// Reported BPM, falling back to the grid-derived average.
    pub fn bpm(&self) -> f32 {
        self.bpm.unwrap_or_else(|| self.beatgrid.average_bpm())
    }
}
