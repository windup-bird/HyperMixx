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

    /// How many of those frames **exist yet**, for a source that is still being produced (a
    /// separation arriving block by block, a track being fetched, anything streamed in).
    /// The default says "all of it", which is what a decoded track is.
    ///
    /// Deliberately neutral: the engine's job is to play a source that may still be growing, and it
    /// has no business knowing *why*. A front-end reads it to show "42% separated" or "42% buffered",
    /// and a source that is not growing never has to answer differently.
    fn available_frames(&self) -> u64 {
        self.total_frames()
    }
}

/// Convenience alias: the handle threaded through the pipeline and analyser.
pub type Shared = Arc<dyn Source>;
