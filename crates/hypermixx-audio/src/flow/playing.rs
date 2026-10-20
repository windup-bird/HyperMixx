//! Playing: one independent playback voice inside a deck.
//!
//! A `Playing` is the engine plus its clock and loop cell. The deck owns one `playing` voice and,
//! while a slip effect / transition is prepared, at most one `shadow` voice. Both carry **one
//! engine over N streams** (`2 × N` interleaved channels), so stems are never split across
//! engines.
//!
//! The deck may retain `playing` while a `shadow` is audible (slip loop / slip roll / slip vinyl).
//! That is why this type has no lifecycle: it is only the transport, not "active/preparing".

use std::sync::Arc;

use super::EngineProfile;
use super::PitchShiftEngine;
use crate::deck::{LoopRange, LoopRangeCell, LoopSource};
use crate::mixer::Bus;
use crate::CHANNELS;
use hypermixx_core::Source;

/// Which way the source cursor walks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    #[default]
    Forward,
    Reverse,
}

/// A `Source` that reads the loop range backwards (slip roll).
struct ReverseSource {
    inner: Arc<dyn Source>,
    cell: Arc<LoopRangeCell>,
}

impl ReverseSource {
    fn new(inner: Arc<dyn Source>, cell: Arc<LoopRangeCell>) -> Self {
        Self { inner, cell }
    }
}

impl Source for ReverseSource {
    fn read_frames(&self, start: u64, output: &mut [f32]) -> usize {
        let Some(range) = self.cell.load() else {
            return self.inner.read_frames(start, output);
        };
        let span = range.len();
        if span == 0 {
            return 0;
        }
        let frames = output.len() / CHANNELS;
        let mut frame = [0.0f32; CHANNELS];
        for i in 0..frames {
            let virtual_pos = start + i as u64;
            let offset = if virtual_pos < range.in_frame {
                0
            } else {
                (virtual_pos - range.in_frame) % span
            };
            let actual = range.out_frame - 1 - offset;
            let got = self.inner.read_frames(actual, &mut frame);
            if got == 0 {
                output[i * CHANNELS..].fill(0.0);
                return i;
            }
            output[i * CHANNELS] = frame[0];
            output[i * CHANNELS + 1] = frame[1];
        }
        frames
    }

    fn total_frames(&self) -> u64 {
        self.inner.total_frames()
    }

    fn available_frames(&self) -> u64 {
        self.inner.available_frames()
    }
}

/// One playback voice: engine + clock + loop mapping.
pub struct Playing {
    pitchshift: PitchShiftEngine,
    loop_cell: Arc<LoopRangeCell>,
    direction: Direction,
    total: u64,
}

impl Playing {
    /// A voice at unity tempo with the Tape profile.
    pub fn new(sources: Vec<Arc<dyn Source>>, start_frame: u64) -> Self {
        Self::with_profile_and_direction(
            sources,
            start_frame,
            1.0,
            EngineProfile::Tape,
            Direction::Forward,
        )
        .expect("tape engine config is statically valid")
    }

    /// A voice born at `start_frame`, with a tempo, profile and direction.
    pub fn with_profile_and_direction(
        sources: Vec<Arc<dyn Source>>,
        start_frame: u64,
        ratio: f32,
        profile: EngineProfile,
        direction: Direction,
    ) -> Result<Self, super::EngineError> {
        let loop_cell = Arc::new(LoopRangeCell::new());
        let wrapped: Vec<Arc<dyn Source>> = sources
            .into_iter()
            .map(|source| {
                if direction == Direction::Reverse {
                    Arc::new(ReverseSource::new(source, Arc::clone(&loop_cell))) as Arc<dyn Source>
                } else {
                    LoopSource::new(source, Arc::clone(&loop_cell)) as Arc<dyn Source>
                }
            })
            .collect();
        let mut playing = Self {
            pitchshift: PitchShiftEngine::with_profile(wrapped, ratio, profile)?,
            loop_cell,
            direction,
            total: 0,
        };
        playing.total = playing.pitchshift.total_frames();
        playing.prepare_jump(start_frame);
        Ok(playing)
    }

    pub fn with_profile(
        sources: Vec<Arc<dyn Source>>,
        start_frame: u64,
        ratio: f32,
        profile: EngineProfile,
    ) -> Result<Self, super::EngineError> {
        Self::with_profile_and_direction(sources, start_frame, ratio, profile, Direction::Forward)
    }

    pub fn stream_count(&self) -> usize {
        self.pitchshift.stream_count()
    }

    pub fn channels(&self) -> usize {
        self.pitchshift.channels()
    }

    pub fn total_frames(&self) -> u64 {
        self.total
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Hard seek: the next output block starts at `target`.
    pub fn prepare_jump(&mut self, target: u64) {
        self.pitchshift.prepare_jump(target);
    }

    pub fn reset_to(&mut self, target: u64) {
        self.pitchshift.reset_to(target);
    }

    pub fn set_ratio(&mut self, ratio: f32) {
        self.pitchshift.set_ratio(ratio);
    }

    pub fn set_profile(&mut self, profile: EngineProfile) {
        let sources = self.pitchshift.sources_ref();
        let fallback = sources.clone();
        let ratio = self.pitchshift.ratio();
        let position = self.pitchshift.current_frame();
        self.pitchshift = PitchShiftEngine::with_profile(sources, ratio, profile)
            .unwrap_or_else(|_| PitchShiftEngine::new(fallback, ratio));
        self.pitchshift.prepare_jump(position);
    }

    pub fn loop_range(&self) -> Option<LoopRange> {
        self.loop_cell.load()
    }

    pub fn set_loop_range(&self, range: Option<LoopRange>) {
        self.loop_cell.store(range);
    }

    pub fn loop_cell(&self) -> Arc<LoopRangeCell> {
        Arc::clone(&self.loop_cell)
    }

    pub fn process_block(&mut self, output: &mut [f32]) -> usize {
        self.pitchshift.process_block(output)
    }

    pub fn process_streams(&mut self, outs: &mut [Bus]) -> usize {
        let frames = outs.iter().map(Bus::frames).min().unwrap_or(0);
        self.pitchshift.process_streams(outs, frames);
        frames
    }

    pub fn render(&mut self, output: &mut [f32]) -> usize {
        self.pitchshift.process_block(output)
    }

    pub fn render_discard(&mut self, frames: usize) -> usize {
        self.pitchshift.render_discard(frames)
    }

    pub fn virtual_frame(&self) -> u64 {
        self.pitchshift.current_frame()
    }

    pub fn actual_frame(&self) -> u64 {
        self.loop_cell.map(self.virtual_frame())
    }

    pub fn reached_end(&self) -> bool {
        self.loop_cell.load().is_none() && self.virtual_frame() >= self.total
    }

    pub fn ratio(&self) -> f32 {
        self.pitchshift.ratio()
    }

    pub fn profile(&self) -> EngineProfile {
        self.pitchshift.profile()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_media::{DecodedAudio, PcmPool};

    fn source(n_frames: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n_frames as usize)
                .flat_map(|i| [i as f32, i as f32])
                .collect(),
            total_frames: n_frames,
            sample_rate: crate::SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    fn tagged_source(n_frames: u64, tag: f32) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n_frames as usize)
                .flat_map(|i| [i as f32 + tag, -(i as f32) - tag])
                .collect(),
            total_frames: n_frames,
            sample_rate: crate::SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    #[test]
    fn tape_unity_starts_at_the_requested_frame() {
        let mut p = Playing::new(vec![source(10_000)], 100);
        let mut out = vec![0.0f32; 16 * CHANNELS];
        p.process_block(&mut out);
        assert_eq!(out[0], 100.0);
        assert_eq!(p.virtual_frame(), 116);
    }

    #[test]
    fn four_streams_render_their_own_source_into_their_own_bus() {
        let tags = [0.0f32, 1000.0, 2000.0, 3000.0];
        let sources: Vec<Arc<dyn Source>> =
            tags.iter().map(|tag| tagged_source(10_000, *tag)).collect();
        let mut p = Playing::new(sources, 0);
        let frames = crate::BLOCK_SIZE;
        let mut buses: Vec<Bus> = (0..4).map(|_| Bus::stereo(frames)).collect();
        assert_eq!(p.process_streams(&mut buses), frames);
        for (stream, bus) in buses.iter().enumerate() {
            let tag = tags[stream];
            for i in 0..frames {
                let want = i as f32 + tag;
                assert!((bus.l[i] - want).abs() < 1e-3, "stream {stream} frame {i}");
            }
        }
        assert_eq!(p.virtual_frame(), frames as u64);
    }

    #[test]
    fn loop_mapping_folds_the_audible_position() {
        let mut p = Playing::new(vec![source(100_000)], 0);
        p.set_loop_range(Some(LoopRange::new(10, 74)));
        p.prepare_jump(10);
        let mut out = vec![0.0f32; 128 * CHANNELS];
        p.process_block(&mut out);
        assert_eq!(p.virtual_frame(), 138);
        assert_eq!(p.actual_frame(), 10 + (138 - 10) % 64);
    }

    #[test]
    fn reverse_reads_the_loop_backwards() {
        let mut p = Playing::with_profile_and_direction(
            vec![source(1000)],
            0,
            1.0,
            EngineProfile::Tape,
            Direction::Reverse,
        )
        .unwrap();
        p.set_loop_range(Some(LoopRange::new(10, 20)));
        p.prepare_jump(10);
        let mut out = vec![0.0f32; 4 * CHANNELS];
        p.process_block(&mut out);
        // virtual 10 -> actual 19, then 18, 17, 16
        assert_eq!(out[0], 19.0);
        assert_eq!(out[CHANNELS], 18.0);
        assert_eq!(out[2 * CHANNELS], 17.0);
        assert_eq!(out[3 * CHANNELS], 16.0);
    }
}
