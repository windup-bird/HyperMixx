//! The editable beat description: a track's grid as a list of tempo segments.
//!
//! This is the *source of truth* a human edits and a database persists. It is compact (a rigid
//! grid is one segment), keeps BPM and provenance, and covers every case with one type:
//! rigid = one segment, dynamic/tempo-varying = many `beats: Some(1)` segments, mixed = a run of
//! segments of differing lengths.

use serde::{Deserialize, Serialize};

/// One constant-tempo stretch of the track.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// Tempo of this segment.
    pub bpm: f64,
    /// Frame where the segment's first beat lands.
    pub start_frame: u64,
    /// Number of beats this segment spans; `None` runs to the next segment or the track end.
    pub beats: Option<u64>,
}

/// A whole-track beat description: an ordered, non-empty list of segments.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BeatSpec {
    pub segments: Vec<Segment>,
}

impl BeatSpec {
    /// A single rigid segment at `bpm` starting at frame `first`.
    pub fn rigid(bpm: f64, first: u64) -> Self {
        Self {
            segments: vec![Segment {
                bpm,
                start_frame: first,
                beats: None,
            }],
        }
    }

    /// True when there is nothing to compile.
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}
