//! PCM memory pool: the in-memory `Source` the deck and analyser read from.

use std::sync::Arc;

use crate::decoder::DecodedAudio;
use crate::Source;
use hypermixx_core::CHANNELS;

/// Immutable, shareable decoded track. Cloning a pool is an `Arc` bump, so every `Flow` of a deck
/// can hold one without duplicating audio data.
pub struct PcmPool {
    pcm: Arc<Vec<f32>>,
    total_frames: u64,
}

impl PcmPool {
    /// Builds a pool from decoded audio (the decoder already normalised to 48kHz stereo).
    pub fn from_decoded(decoded: DecodedAudio) -> Self {
        let total_frames = (decoded.pcm.len() / decoded.channels.max(1)) as u64;
        Self {
            pcm: Arc::new(decoded.pcm),
            total_frames,
        }
    }

    /// An empty pool, handy as a placeholder before a track is loaded.
    pub fn empty() -> Self {
        Self {
            pcm: Arc::new(Vec::new()),
            total_frames: 0,
        }
    }

    /// Sample data (interleaved f32).
    pub fn pcm(&self) -> &[f32] {
        &self.pcm
    }
}

impl Source for PcmPool {
    fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize {
        let start = start_frame as usize;
        if start >= self.total_frames as usize {
            return 0;
        }
        let want = output.len() / CHANNELS;
        let frames = want
            .min(self.total_frames as usize - start)
            .min(self.pcm.len() / CHANNELS - start);
        let src = &self.pcm[start * CHANNELS..(start + frames) * CHANNELS];
        output[..src.len()].copy_from_slice(src);
        frames
    }

    fn total_frames(&self) -> u64 {
        self.total_frames
    }
}

impl Clone for PcmPool {
    fn clone(&self) -> Self {
        Self {
            pcm: Arc::clone(&self.pcm),
            total_frames: self.total_frames,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(n_frames: u64) -> PcmPool {
        PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n_frames as usize)
                .flat_map(|i| [i as f32, -(i as f32)])
                .collect(),
            total_frames: n_frames,
            sample_rate: 48_000,
            channels: CHANNELS,
        })
    }

    #[test]
    fn reads_requested_frames() {
        let p = pool(10);
        let mut out = vec![0.0f32; 4 * CHANNELS];
        assert_eq!(p.read_frames(2, &mut out), 4);
        assert_eq!(&out[..2], &[2.0, -2.0]);
    }

    #[test]
    fn clamps_at_end_of_track() {
        let p = pool(10);
        let mut out = vec![0.0f32; 8 * CHANNELS];
        assert_eq!(p.read_frames(6, &mut out), 4);
    }

    #[test]
    fn past_end_reads_nothing() {
        let p = pool(4);
        let mut out = vec![0.0f32; CHANNELS];
        assert_eq!(p.read_frames(4, &mut out), 0);
    }

    #[test]
    fn empty_pool_has_no_frames() {
        let p = PcmPool::empty();
        let mut out = vec![1.0f32; CHANNELS];
        assert_eq!(p.read_frames(0, &mut out), 0);
        assert_eq!(p.total_frames(), 0);
    }
}
