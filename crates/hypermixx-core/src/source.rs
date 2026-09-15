//! The PCM provider interface shared by playback, analysis and library code.

use std::sync::Arc;

/// A sample-accurate, thread-safe PCM provider.
///
/// Implementations are read by the producer thread and must never block: `read_frames` is the
/// only sampling entry point the flow/pitchshift layer is allowed to use. Lives here (not in
/// `media`) because the [`Command::Load`](crate::Command::Load) payload references it, and `core`
/// must stay the bottom of the dependency graph.
pub trait Source: Send + Sync {
    /// Reads PCM from `start_frame` into `output` (interleaved stereo f32).
    /// Returns the number of frames actually read.
    fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize;
    /// Total number of frames in this source.
    fn total_frames(&self) -> u64;
}

/// Convenience alias: the handle threaded through the pipeline and analyser.
pub type Shared = Arc<dyn Source>;
