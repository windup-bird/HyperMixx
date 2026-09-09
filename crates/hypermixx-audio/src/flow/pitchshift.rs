//! Pitchshift: time-stretches a `Source` for a `Flow`. v1 is a passthrough.

use std::sync::Arc;

use crate::source::Source;
use crate::CHANNELS;

/// Placeholder time-stretch engine: it copies input to output 1:1 (`ratio` is stored but has no
/// effect). The interface is what matters — swapping in a real OLA/SOLA engine keeps
/// `prepare_jump` / `process_block` / `reset_to` as the only integration points.
#[derive(Clone)]
pub struct PitchShiftEngine {
    source: Arc<dyn Source>,
    current_input_frame: u64,
    ratio: f32,
}

impl PitchShiftEngine {
    pub fn new(source: Arc<dyn Source>, ratio: f32) -> Self {
        Self {
            source,
            current_input_frame: 0,
            ratio,
        }
    }

    /// Processes one block (up to `output.len() / CHANNELS` frames), copying raw source data into
    /// `output`. Returns the number of frames written; the tail is zero-filled.
    pub fn process_block(&mut self, output: &mut [f32]) -> usize {
        let capacity = output.len() / CHANNELS;
        let frames = self
            .source
            .read_frames(self.current_input_frame, &mut output[..capacity * CHANNELS]);
        self.current_input_frame += frames as u64;
        for sample in &mut output[frames * CHANNELS..capacity * CHANNELS] {
            *sample = 0.0;
        }
        frames
    }

    /// Warms up to `target_frame`. The passthrough version only needs to reposition.
    pub fn prepare_jump(&mut self, target_frame: u64) {
        self.current_input_frame = target_frame;
    }

    /// Light-weight reset to `target_frame` (used for loops). Same as `prepare_jump` here, but a
    /// real engine skips the overlap warm-up on this path.
    pub fn reset_to(&mut self, target_frame: u64) {
        self.prepare_jump(target_frame);
    }

    /// Current input-side playhead, in frames.
    pub fn current_frame(&self) -> u64 {
        self.current_input_frame
    }

    /// Current playback rate (1.0 = untouched pitch/tempo).
    pub fn ratio(&self) -> f32 {
        self.ratio
    }

    /// Sets the playback rate. Stored only; v1 does not alter timing or pitch.
    pub fn set_ratio(&mut self, ratio: f32) {
        self.ratio = ratio;
    }

    /// Total frames available from the backing source.
    pub fn total_frames(&self) -> u64 {
        self.source.total_frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::PcmPool;

    fn ramp(n_frames: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(crate::source::DecodedAudio {
            pcm: (0..n_frames as usize)
                .flat_map(|i| [i as f32, i as f32])
                .collect(),
            total_frames: n_frames,
            sample_rate: 48_000,
            channels: CHANNELS,
        }))
    }

    #[test]
    fn passthrough_copies_1_to_1() {
        let mut engine = PitchShiftEngine::new(ramp(1000), 1.0);
        let mut out = vec![0.0f32; 256 * CHANNELS];
        assert_eq!(engine.process_block(&mut out), 256);
        assert_eq!(&out[..2], &[0.0, 0.0]);
        assert_eq!(engine.current_frame(), 256);
    }

    #[test]
    fn prepare_jump_repositions_without_processing() {
        let mut engine = PitchShiftEngine::new(ramp(1000), 1.0);
        engine.prepare_jump(500);
        let mut out = vec![0.0f32; 4 * CHANNELS];
        assert_eq!(engine.process_block(&mut out), 4);
        assert_eq!(out[0], 500.0);
    }

    #[test]
    fn tail_is_silenced_at_end_of_source() {
        let mut engine = PitchShiftEngine::new(ramp(10), 1.0);
        engine.prepare_jump(8);
        let mut out = vec![1.0f32; 4 * CHANNELS];
        assert_eq!(engine.process_block(&mut out), 2);
        assert!(out[2 * CHANNELS..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn ratio_is_stored_but_inert() {
        let mut engine = PitchShiftEngine::new(ramp(1000), 1.0);
        engine.set_ratio(0.5);
        assert_eq!(engine.ratio(), 0.5);
        let mut out = vec![0.0f32; 256 * CHANNELS];
        assert_eq!(engine.process_block(&mut out), 256);
    }
}
