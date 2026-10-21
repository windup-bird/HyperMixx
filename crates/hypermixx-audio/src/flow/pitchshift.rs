//! Pitchshift: the playback stretch engine.
//!
//! Two backends behind one API:
//!
//! * [`Backend::Tape`] — varispeed. Unity is a sample-exact copy; other rates use a small cubic
//!   interpolator. No FFT, no added latency. This is also the backend vinyl/scratch uses.
//! * [`Backend::Stretch`] — Signalsmith Stretch. One instance carries all `2 × N` interleaved
//!   channels, so every stem shares the same time factor and analysis/synthesis hops.
//!
//! **One engine, N streams.** A deck that exposes stems feeds N sources through *one* engine whose
//! interleaved channel count is `2 × N`, instead of building N engines. That is what keeps the
//! stems sample-aligned: a separate engine per stem would get its own history and its own
//! analysis timing, and the stems would stop lining up.
//!
//! The engine is pull-based: `render_own` computes how many source frames this output block needs,
//! reads them through the caller's `LoopSource` mapping, and asks the backend to produce `frames`
//! output frames.

use std::fmt;
use std::sync::Arc;

use signalsmith_stretch::Stretch;

use crate::mixer::Bus;
use crate::{BLOCK_SIZE, CHANNELS};
use hypermixx_core::{KeylockSettings, Source};

/// Keylock profile / engine configuration.
///
/// `Keylock` carries the configured STFT window (from `live.toml`); Tape is a separate backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineProfile {
    /// Pure varispeed: pitch follows tempo.
    Tape,
    /// Keylock through the DJ range, with its configured block/interval.
    Keylock(KeylockSettings),
}

/// Why an engine could not be built.
#[derive(Debug, Clone)]
pub enum EngineError {
    /// More streams than one engine carries.
    TooManyStreams { streams: usize, channels: usize },
    /// The backend rejected its configuration.
    Config(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::TooManyStreams { streams, channels } => write!(
                f,
                "{streams} streams need {channels} interleaved channels, but the engine supports at most \
                 {} ({MAX_STREAMS} stereo streams)",
                MAX_STREAMS * CHANNELS
            ),
            EngineError::Config(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for EngineError {}

/// Most streams one engine can carry.
///
/// Signalsmith itself has no 8-channel limit; this is the current stem model's limit. Keeping it
/// here means the mixer, the deck and the loop state machine agree on the maximum.
pub const MAX_STREAMS: usize = 4;
const MAX_ENGINE_CHANNELS: usize = MAX_STREAMS * CHANNELS;

/// Tempo clamp. The deck clamps its playhead to the same range.
const MIN_TEMPO_RATE: f64 = 0.25;
const MAX_TEMPO_RATE: f64 = 4.0;

/// Maximum source frames `output_seek` may need at the highest supported rate.
/// `presetDefault` at 4× needs ~13k; leave margin for the larger windows.
const MAX_SEEK_FRAMES: usize = 16_384;
/// Maximum source frames one output block needs at 4×, plus interpolation guard.
const MAX_FEED_FRAMES: usize = MAX_TEMPO_RATE as usize * BLOCK_SIZE + 8;

enum Backend {
    Tape {
        /// Source frame at the start of the next output block.
        feed_position: u64,
        /// Fractional offset into that frame.
        phase: f64,
    },
    Stretch {
        stretch: Stretch,
        /// Source frame at the start of the next output block.
        feed_position: u64,
        /// Fractional input-frame carry, so long-term ratio is exact.
        carry: f64,
    },
}

/// A real-time stretch engine for one flow, carrying one or more parallel streams.
pub struct PitchShiftEngine {
    backend: Backend,
    sources: Vec<Arc<dyn Source>>,
    channels: usize,
    /// Absolute output playhead (the listener's position), in source frames.
    output_frame: u64,
    output_frame_exact: f64,
    ratio: f32,
    total: u64,
    profile: EngineProfile,
    /// Interleaved staging for one feed/seek batch.
    feed_buf: Vec<f32>,
    /// One stream's contiguous stereo staging.
    stem_buf: Vec<f32>,
    /// Interleaved staging for one rendered block.
    out_buf: Vec<f32>,
    /// Pre-roll buffer for Signalsmith `outputSeek`.
    seek_buf: Vec<f32>,
    /// Kept for API compatibility with the old engine; always 0 now.
    priming_remaining: usize,
}

impl PitchShiftEngine {
    /// Creates an engine with the default Tape profile (zero latency, passthrough at ratio 1.0).
    pub fn new(sources: Vec<Arc<dyn Source>>, ratio: f32) -> Self {
        Self::with_profile(sources, ratio, EngineProfile::Tape)
            .expect("tape engine config is statically valid")
    }

    /// Creates an engine with a specific profile, over `sources.len()` streams.
    pub fn with_profile(
        sources: Vec<Arc<dyn Source>>,
        ratio: f32,
        profile: EngineProfile,
    ) -> Result<Self, EngineError> {
        assert!(!sources.is_empty(), "a flow needs at least one source");
        let channels = sources.len() * CHANNELS;
        if channels > MAX_ENGINE_CHANNELS {
            return Err(EngineError::TooManyStreams {
                streams: sources.len(),
                channels,
            });
        }
        let total = sources[0].total_frames();
        let backend = match profile {
            EngineProfile::Tape => Backend::Tape {
                feed_position: 0,
                phase: 0.0,
            },
            EngineProfile::Keylock(window) => Backend::Stretch {
                stretch: Stretch::new_split(
                    channels as u32,
                    window.block,
                    window.interval,
                    false,
                ),
                feed_position: 0,
                carry: 0.0,
            },
        };
        Ok(Self {
            backend,
            sources,
            channels,
            output_frame: 0,
            output_frame_exact: 0.0,
            ratio: ratio.clamp(MIN_TEMPO_RATE as f32, MAX_TEMPO_RATE as f32),
            total,
            profile,
            feed_buf: vec![0.0; MAX_FEED_FRAMES * channels],
            stem_buf: vec![0.0; MAX_SEEK_FRAMES * CHANNELS],
            out_buf: vec![0.0; BLOCK_SIZE * channels],
            seek_buf: vec![0.0; MAX_SEEK_FRAMES * channels],
            priming_remaining: 0,
        })
    }

    /// Streams this engine renders. `1` for a plain track, `4` for stems.
    pub fn stream_count(&self) -> usize {
        self.sources.len()
    }

    /// Interleaved channels this engine renders (`2 × stream_count`).
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The streams, for rebuilding the engine with a different profile.
    pub fn sources_ref(&self) -> Vec<Arc<dyn Source>> {
        self.sources.clone()
    }

    /// Processes one block and writes the **mixed** interleaved stereo result into `output`.
    pub fn process_block(&mut self, output: &mut [f32]) -> usize {
        let frames = output.len() / CHANNELS;
        self.render_own(frames);
        if self.channels == CHANNELS {
            output[..frames * CHANNELS].copy_from_slice(&self.out_buf[..frames * CHANNELS]);
        } else {
            let width = self.channels;
            for i in 0..frames {
                let (mut left, mut right) = (0.0f32, 0.0f32);
                for s in 0..self.sources.len() {
                    left += self.out_buf[i * width + s * CHANNELS];
                    right += self.out_buf[i * width + s * CHANNELS + 1];
                }
                output[i * CHANNELS] = left;
                output[i * CHANNELS + 1] = right;
            }
        }
        frames
    }

    /// Processes one block and de-interleaves it into one [`Bus`] per stream.
    pub fn process_streams(&mut self, outs: &mut [Bus], frames: usize) -> usize {
        let streams = outs.len().min(self.sources.len());
        let frames = frames.min(outs.iter().map(Bus::frames).min().unwrap_or(0));
        self.render_own(frames);
        let width = self.channels;
        for (s, bus) in outs.iter_mut().take(streams).enumerate() {
            for i in 0..frames {
                bus.l[i] = self.out_buf[i * width + s * CHANNELS];
                bus.r[i] = self.out_buf[i * width + s * CHANNELS + 1];
            }
        }
        frames
    }

    /// Renders `frames` and throws the audio away, advancing the clock.
    pub fn render_discard(&mut self, frames: usize) -> usize {
        self.render_own(frames);
        frames
    }

    /// Full seek: the next output block starts at `target_frame`.
    pub fn prepare_jump(&mut self, target_frame: u64) {
        let target = target_frame.min(self.total);
        self.output_frame = target;
        self.output_frame_exact = target as f64;
        self.priming_remaining = 0;

        let channels = self.channels;
        match &mut self.backend {
            Backend::Tape {
                feed_position,
                phase,
            } => {
                *feed_position = target;
                *phase = 0.0;
            }
            Backend::Stretch {
                stretch,
                feed_position,
                carry,
            } => {
                let n = stretch.output_seek_length(f64::from(self.ratio)).max(1);
                if self.seek_buf.len() < n * channels {
                    self.seek_buf.resize(n * channels, 0.0);
                }
                if self.stem_buf.len() < n * CHANNELS {
                    self.stem_buf.resize(n * CHANNELS, 0.0);
                }
                read_interleaved(
                    &self.sources,
                    &mut self.stem_buf,
                    target,
                    n,
                    &mut self.seek_buf[..n * channels],
                );
                stretch.output_seek(&self.seek_buf[..n * channels]);
                *feed_position = target + n as u64;
                *carry = 0.0;
            }
        }
    }

    /// Repositions without the full warm-start. The new engine has no priming, so this is a seek.
    pub fn reset_to(&mut self, target_frame: u64) {
        self.prepare_jump(target_frame);
    }

    /// Kept for API compatibility: the new engine prepares inside `prepare_jump`.
    pub fn drain_priming(&mut self) {}

    /// Current playhead: the absolute track position the listener is hearing.
    pub fn current_frame(&self) -> u64 {
        self.output_frame
    }

    pub fn ratio(&self) -> f32 {
        self.ratio
    }

    /// The profile this engine was built with.
    pub fn profile(&self) -> EngineProfile {
        self.profile
    }

    /// Sets the tempo rate. ratio=1.0 is unity, >1 speeds up, <1 slows down.
    pub fn set_ratio(&mut self, ratio: f32) {
        self.ratio = ratio.clamp(MIN_TEMPO_RATE as f32, MAX_TEMPO_RATE as f32);
    }

    /// Total frames available from the backing source.
    pub fn total_frames(&self) -> u64 {
        self.total
    }

    /// Feeds, renders one block, and advances the clock.
    fn render_own(&mut self, frames: usize) {
        if frames == 0 {
            return;
        }
        let samples = frames * self.channels;
        if self.out_buf.len() < samples {
            self.out_buf.resize(samples, 0.0);
        }
        match &mut self.backend {
            Backend::Tape {
                feed_position,
                phase,
            } => {
                let step = f64::from(self.ratio).clamp(MIN_TEMPO_RATE, MAX_TEMPO_RATE);
                let need =
                    (((frames.saturating_sub(1)) as f64) * step + *phase).ceil() as usize + 4;
                let need = need.max(4);
                if self.feed_buf.len() < need * self.channels {
                    self.feed_buf.resize(need * self.channels, 0.0);
                }
                if self.stem_buf.len() < need * CHANNELS {
                    self.stem_buf.resize(need * CHANNELS, 0.0);
                }
                read_interleaved(
                    &self.sources,
                    &mut self.stem_buf,
                    *feed_position,
                    need,
                    &mut self.feed_buf[..need * self.channels],
                );
                if step == 1.0 && *phase == 0.0 {
                    self.out_buf[..samples].copy_from_slice(&self.feed_buf[..samples]);
                    *feed_position += frames as u64;
                } else {
                    let channels = self.channels;
                    for i in 0..frames {
                        let pos = i as f64 * step + *phase;
                        let idx = pos as usize;
                        let frac = pos - idx as f64;
                        let i0 = idx.saturating_sub(1);
                        let i1 = idx;
                        let i2 = idx + 1;
                        let i3 = idx + 2;
                        for c in 0..channels {
                            let s0 = self.feed_buf[i0 * channels + c];
                            let s1 = self.feed_buf[i1 * channels + c];
                            let s2 = self.feed_buf[i2 * channels + c];
                            let s3 = self.feed_buf[i3 * channels + c];
                            self.out_buf[i * channels + c] = cubic(s0, s1, s2, s3, frac as f32);
                        }
                    }
                    let advance = (*phase + frames as f64 * step).floor();
                    *phase += frames as f64 * step - advance;
                    *feed_position += advance as u64;
                }
            }
            Backend::Stretch {
                stretch,
                feed_position,
                carry,
            } => {
                let step = f64::from(self.ratio).clamp(MIN_TEMPO_RATE, MAX_TEMPO_RATE);
                let want = (frames as f64 * step + *carry).round() as usize;
                *carry += frames as f64 * step - want as f64;
                if self.feed_buf.len() < want * self.channels {
                    self.feed_buf.resize(want * self.channels, 0.0);
                }
                if self.stem_buf.len() < want * CHANNELS {
                    self.stem_buf.resize(want * CHANNELS, 0.0);
                }
                read_interleaved(
                    &self.sources,
                    &mut self.stem_buf,
                    *feed_position,
                    want,
                    &mut self.feed_buf[..want * self.channels],
                );
                stretch.process(
                    &self.feed_buf[..want * self.channels],
                    &mut self.out_buf[..samples],
                );
                *feed_position += want as u64;
            }
        }
        self.output_frame_exact += frames as f64 * f64::from(self.ratio);
        self.output_frame = self.output_frame_exact as u64;
    }
}

/// Cubic Hermite interpolation over four samples.
#[inline]
fn cubic(s0: f32, s1: f32, s2: f32, s3: f32, frac: f32) -> f32 {
    let c0 = s1;
    let c1 = 0.5 * (s2 - s0);
    let c2 = s0 - 2.5 * s1 + 2.0 * s2 - 0.5 * s3;
    let c3 = 0.5 * (s3 - s0) + 1.5 * (s1 - s2);
    ((c3 * frac + c2) * frac + c1) * frac + c0
}

/// Reads `frames` frames starting at `start` and interleaves them `(frame, stream, side)`.
///
/// Each source is an already-wrapped `LoopSource`, so a loop wrap is handled by the source itself.
/// A short read is zero-padded; the caller still gets a full input buffer.
fn read_interleaved(
    sources: &[Arc<dyn Source>],
    stem_buf: &mut [f32],
    start: u64,
    frames: usize,
    out: &mut [f32],
) -> usize {
    let channels = sources.len() * CHANNELS;
    debug_assert!(out.len() >= frames * channels);
    let mut read = frames;
    for (stream, source) in sources.iter().enumerate() {
        let got = source.read_frames(start, &mut stem_buf[..frames * CHANNELS]);
        if got < frames {
            stem_buf[got * CHANNELS..frames * CHANNELS].fill(0.0);
        }
        read = read.min(got);
        for i in 0..frames {
            let dst = i * channels + stream * CHANNELS;
            out[dst] = stem_buf[i * CHANNELS];
            out[dst + 1] = stem_buf[i * CHANNELS + 1];
        }
    }
    read
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deck::LoopRangeCell;
    use hypermixx_media::{DecodedAudio, PcmPool};

    fn ramp(n: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n as usize)
                .flat_map(|i| [i as f32, -(i as f32)])
                .collect(),
            total_frames: n,
            sample_rate: crate::SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    fn cell() -> Arc<LoopRangeCell> {
        Arc::new(LoopRangeCell::new())
    }

    #[test]
    fn tape_unity_is_passthrough() {
        let source = ramp(10_000);
        let mut engine = PitchShiftEngine::with_profile(
            vec![crate::deck::LoopSource::new(source, cell()) as Arc<dyn Source>],
            1.0,
            EngineProfile::Tape,
        )
        .unwrap();
        engine.prepare_jump(100);
        let mut out = vec![0.0f32; 16 * CHANNELS];
        engine.process_block(&mut out);
        for i in 0..16 {
            assert_eq!(out[i * CHANNELS], 100.0 + i as f32);
            assert_eq!(out[i * CHANNELS + 1], -(100.0 + i as f32));
        }
        assert_eq!(engine.current_frame(), 116);
    }

    #[test]
    fn keylock_at_unity_stays_on_pitch_and_moves_the_clock() {
        let source = ramp(200_000);
        let cell = cell();
        let mut engine = PitchShiftEngine::with_profile(
            vec![crate::deck::LoopSource::new(source, cell) as Arc<dyn Source>],
            1.0,
            EngineProfile::Keylock(Default::default()),
        )
        .unwrap();
        engine.prepare_jump(1000);
        let mut out = vec![0.0f32; BLOCK_SIZE * CHANNELS];
        engine.process_block(&mut out);
        assert_eq!(engine.current_frame(), 1000 + BLOCK_SIZE as u64);
        // The ramp is not a useful frequency test; this only proves the engine produced audio and
        // advanced the clock. The deck-level tone test covers fidelity.
        assert!(out.iter().any(|s| *s != 0.0));
    }
}
