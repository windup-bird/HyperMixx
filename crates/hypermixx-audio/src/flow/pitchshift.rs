//! Pitchshift: real-time time-stretch engine wrapping timestretch-rs.
//!
//! Three profiles: Tape (varispeed, pitch follows tempo), Keylock (SOLA, pitch locked),
//! WideKeylock (phase vocoder, full-spectrum keylock). All run through the same pull-based engine:
//! we push source audio into its ring and pull processed output in `process_block`.
//!
//! **One engine, N streams.** A deck that exposes stems feeds N sources through *one* engine whose
//! interleaved channel count is `2 × N`, instead of building N engines. That is not an optimisation
//! detail, it is what makes stems coherent:
//!
//! * The keylock chain's SOLA corrector scores a **channel mix** for its splice search and keeps a
//!   single read cursor shared by every channel (`mix_channels`, and upstream's "absolute
//!   fractional read cursor (shared: channels are lockstep)"). N separate engines would each pick
//!   their own splice landing — up to ±`SEARCH_RANGE` (160 frames ≈ 3.6 ms) apart — and the stems
//!   would stop being sample-aligned to each other. That is audible as comb filtering between
//!   stems, and it is why "one engine per stem" is wrong rather than merely slower.
//! * It is also cheaper: the splice search and every per-engine warm-up / priming / allocation is
//!   paid once. Measured on 4 stems (rate 1.06, Keylock): 20.6× realtime in one 8-channel engine
//!   against 7.8× in four stereo engines — 2.6×.
//!
//! The per-channel work is still N× (it is N× the audio); what is shared is everything else.

use std::sync::Arc;

use timestretch::engine::{
    Engine, EngineConfig, EngineController, EngineProcessor, EngineProfile, SourceProducer,
};
use timestretch::error::StretchError;

use crate::deck::LoopRangeCell;
use crate::mixer::Bus;
use crate::{BLOCK_SIZE, CHANNELS, SAMPLE_RATE};
use hypermixx_core::Source;

/// How many source frames to push into the engine's ring per feed batch.
const FEED_CHUNK_FRAMES: usize = 1024;
/// Upper bound on feed batches per `process_block`: an empty ring refills to the engine's demand
/// (`demand_hint` ≈ one block at 4× tempo + the resampler's taps, ~1100 frames) in two.
const MAX_FEED_BATCHES: usize = 8;
/// Tempo clamp mirroring `EngineConfig`'s: the controller clamps writes, so feeding must too.
const MIN_TEMPO_RATE: f64 = 0.25;
const MAX_TEMPO_RATE: f64 = 4.0;

/// Most streams one engine can carry.
///
/// Upstream `EngineConfig` validates `channels ∈ 1..=8`; with stereo streams that is **4 stems
/// exactly**. A 6-stem model (12 interleaved channels) does not fit, and splitting it across two
/// engines would reintroduce the per-engine splice divergence described in the module docs.
pub const MAX_STREAMS: usize = 4;
const MAX_ENGINE_CHANNELS: usize = MAX_STREAMS * CHANNELS;

/// A real-time time-stretch engine for one flow, carrying one or more parallel streams.
///
/// With one source this is the historical single-stream deck. With N sources (`N ≤ MAX_STREAMS`)
/// every stream is read at the same track position through the same loop mapping and rendered in
/// lockstep by the engine, so `virtual_frame` / `current_frame` are a single clock for all of them.
pub struct PitchShiftEngine {
    controller: EngineController,
    processor: EngineProcessor,
    source_producer: SourceProducer,
    /// The deck's streams, each already wrapped in a [`LoopSource`](crate::deck::LoopSource) so the
    /// loop mapping is applied per stream but driven by one shared cell.
    sources: Vec<Arc<dyn Source>>,
    /// `2 × sources.len()`: the engine's interleaved channel count, and the feed stride.
    channels: usize,
    /// Absolute frame position in the sources that we've fed up to (all streams share it).
    track_position: u64,
    /// Output playhead (what the listener hears), as an **absolute track position**.
    ///
    /// It advances by `ratio × block`, not by the block: `ratio` is source frames per output
    /// frame, so this is the only accounting that lands where the music actually is. Counting
    /// output frames instead would make the position independent of tempo — every read (waveform,
    /// beat phase, jump compensation, loop mapping) would silently disagree with the audio as
    /// soon as the tempo left unity. `exact` carries the fraction so per-block rounding never
    /// accumulates into drift.
    output_frame: u64,
    output_frame_exact: f64,
    ratio: f32,
    total: u64,
    /// Interleaved staging for one feed batch, laid out `(frame, stream, side)`.
    feed_buf: Vec<f32>,
    /// One stream's contiguous stereo output, before it is scattered into [`Self::feed_buf`].
    stem_buf: Vec<f32>,
    /// Interleaved staging for one rendered block, laid out `(frame, stream, side)`.
    out_buf: Vec<f32>,
    /// Output frames remaining in the current warm-start priming (silence).
    priming_remaining: usize,
    profile: EngineProfile,
    /// The loop mapping the feed reads through, shared with the flow that owns this engine (and
    /// with every stream's `LoopSource`): the engine sees nothing but ever-increasing virtual
    /// positions, and the cell folds each read into the range.
    loop_cell: Arc<LoopRangeCell>,
}

impl PitchShiftEngine {
    /// Creates an engine with the default Tape profile (zero latency, passthrough at ratio 1.0).
    pub fn new(
        sources: Vec<Arc<dyn Source>>,
        ratio: f32,
        loop_cell: Arc<LoopRangeCell>,
    ) -> Self {
        Self::with_profile(sources, ratio, EngineProfile::Tape, loop_cell)
            .expect("timestretch engine config is statically valid")
    }

    /// Creates an engine with a specific profile, over `sources.len()` streams.
    ///
    /// Fails when the stream count exceeds [`MAX_STREAMS`] — the engine's channel limit, not a
    /// policy choice.
    pub fn with_profile(
        sources: Vec<Arc<dyn Source>>,
        ratio: f32,
        profile: EngineProfile,
        loop_cell: Arc<LoopRangeCell>,
    ) -> Result<Self, StretchError> {
        assert!(!sources.is_empty(), "a flow needs at least one source");
        let channels = sources.len() * CHANNELS;
        if channels > MAX_ENGINE_CHANNELS {
            return Err(StretchError::InvalidFormat(format!(
                "{} streams need {channels} interleaved channels, but the engine supports at most \
                 {MAX_ENGINE_CHANNELS} ({MAX_STREAMS} stereo streams)",
                sources.len()
            )));
        }
        let total = sources[0].total_frames();
        let config = EngineConfig {
            sample_rate: SAMPLE_RATE,
            channels,
            profile,
            initial_tempo_rate: (ratio as f64).clamp(MIN_TEMPO_RATE, MAX_TEMPO_RATE),
            max_block_frames: BLOCK_SIZE,
            source_capacity_frames: 32768,
            pre_analysis: None,
        };
        let handles = Engine::build(config)?;
        Ok(Self {
            controller: handles.controller,
            processor: handles.processor,
            source_producer: handles.source,
            sources,
            channels,
            track_position: 0,
            output_frame: 0,
            output_frame_exact: 0.0,
            ratio,
            total,
            feed_buf: vec![0.0; FEED_CHUNK_FRAMES * channels],
            stem_buf: vec![0.0; FEED_CHUNK_FRAMES * CHANNELS],
            out_buf: vec![0.0; BLOCK_SIZE * channels],
            priming_remaining: 0,
            profile,
            loop_cell,
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
    ///
    /// The single-stream case is a direct render. With N streams the block is rendered once and
    /// summed, which is the right answer for the compatibility entry points (`Deck::process_block`
    /// / `pull_into`) but *not* for a mixer, which wants the streams apart: use
    /// [`process_streams`](Self::process_streams).
    ///
    /// Always writes the whole buffer; returns the number of frames.
    pub fn process_block(&mut self, output: &mut [f32]) -> usize {
        let frames = output.len() / CHANNELS;
        if self.channels == CHANNELS {
            self.feed();
            self.processor.process(output);
            self.advance(frames);
            return frames;
        }
        self.render_own(frames);
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
        frames
    }

    /// Processes one block and de-interleaves it into one [`Bus`] per stream, rendering `frames`
    /// frames (the caller's block length, capped by the shortest bus).
    ///
    /// `outs.len()` must equal [`stream_count`](Self::stream_count).
    pub fn process_streams(&mut self, outs: &mut [Bus], frames: usize) -> usize {
        // A caller with fewer buses than streams still renders every channel (one engine) and
        // de-interleaves what it asked for.
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
    ///
    /// The armed LoopFlow's lockstep driver uses this: the engine has to keep running (so its
    /// stages stay converged and its clock stays on the deck's) but the listener must not hear it.
    /// Every stream is driven, because they are one engine — there is nothing to keep in sync.
    pub fn render_discard(&mut self, frames: usize) -> usize {
        self.render_own(frames);
        frames
    }

    /// Full seek protocol. Tape skips the warm-start (zero-latency passthrough);
    /// Keylock/WideKeylock run the priming to converge stage state.
    pub fn prepare_jump(&mut self, target_frame: u64) {
        let target = target_frame.min(self.total);
        self.processor.reset();
        self.output_frame = target;
        self.output_frame_exact = target as f64;

        if self.profile == EngineProfile::Tape {
            // Tape has no stages to converge: re-anchor and go.
            self.source_producer.set_track_position(target);
            self.track_position = target;
            self.priming_remaining = 0;
        } else {
            let preroll = self.processor.warm_start_preroll_frames();
            let start = target.saturating_sub(preroll as u64);
            self.source_producer.set_track_position(start);
            self.controller.warm_start(preroll as u32);
            self.track_position = start;
            self.priming_remaining = preroll + 64; // preroll + declick fade-in
        }
        self.feed();
    }

    /// Repositions without the full warm-start (used by loop wraps). Delegates to prepare_jump
    /// since the timestretch engine always needs its seek protocol for correctness.
    pub fn reset_to(&mut self, target_frame: u64) {
        self.prepare_jump(target_frame);
    }

    /// Runs the warm-start priming (engine-side preroll plus the declick fade) right now,
    /// discarding the output, and returns with the clock still on the prepared target.
    ///
    /// [`Flow::prepare`](crate::flow::Flow::prepare) calls this on the warm-up thread, so
    /// "warm-up finished" means *converged*: a flow that is announced ready plays steady audio
    /// on its first audible block — which is what lets a loop-in's LoopFlow be promoted without
    /// playing priming silence.
    pub fn drain_priming(&mut self) {
        // Bounded well past any real preroll (the wide keylock's is two FFT windows): a guard
        // against an engine that cannot converge (an empty source), not a normal exit.
        for _ in 0..4096 {
            if self.priming_remaining == 0 {
                break;
            }
            self.render_own(BLOCK_SIZE);
        }
    }

    /// Current playhead: the absolute track position the listener is hearing.
    /// At ratio=1.0 this is also the source position fed to the engine; at any other ratio the two
    /// differ by exactly the ring's read-ahead, which is why it is accounted here rather than
    /// inferred from what has been fed.
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
        self.ratio = ratio;
        self.controller.set_tempo_rate(ratio as f64);
    }

    /// Total frames available from the backing source.
    pub fn total_frames(&self) -> u64 {
        self.total
    }

    /// Feeds, renders one block of `frames` into the interleaved staging buffer, and advances the
    /// clock. The one place a block is rendered, so the direct, de-interleaving and discard paths
    /// can never disagree about the transport.
    fn render_own(&mut self, frames: usize) {
        let samples = frames * self.channels;
        if self.out_buf.len() < samples {
            // Only reachable for a caller asking for more than `BLOCK_SIZE` frames; the mixer
            // never does, so this is a correctness guard, not a hot path.
            self.out_buf.resize(samples, 0.0);
        }
        self.feed();
        self.processor.process(&mut self.out_buf[..samples]);
        self.advance(frames);
    }

    /// Output-frame accounting for a block that was just rendered, including the priming rule: the
    /// clock stays frozen (and the audio is silence) until the warm-start priming has drained.
    fn advance(&mut self, frames: usize) {
        if self.priming_remaining > 0 {
            self.priming_remaining = self.priming_remaining.saturating_sub(frames);
        } else {
            let rate = f64::from(self.ratio).clamp(MIN_TEMPO_RATE, MAX_TEMPO_RATE);
            self.output_frame_exact += frames as f64 * rate;
            self.output_frame = self.output_frame_exact as u64;
        }
    }

    /// Pushes source audio into the engine's ring buffer — only up to the engine's own demand.
    ///
    /// The old behaviour kept the ring *full* (32 768 frames), which puts 0.74 s of already-fed
    /// audio between a `loop_range` change and the listener: a loop could not engage or edit
    /// without replaying the stale mapping for the better part of a second. `demand_hint` is the
    /// engine's contract for "the next callback renders without underrun", so topping up to it
    /// keeps the read-ahead window at ~1 100 frames (~25 ms) — and feeds exactly enough after a
    /// tempo change, since the demand is recomputed from the current ratio every block.
    ///
    /// Each batch is also *segmented at the loop boundary* and re-anchored with
    /// `set_track_position(actual)`: the frame pushed next carries its true track position, so a
    /// wrap re-anchors the engine's timeline without any engine reset.
    ///
    /// Every stream is read at the same position through the same shared loop cell, so one segment
    /// decision serves all of them and the batch is interleaved as `(frame, stream, side)`.
    fn feed(&mut self) {
        if self.track_position >= self.total {
            return;
        }
        let rate = f64::from(self.ratio).clamp(MIN_TEMPO_RATE, MAX_TEMPO_RATE);
        let demand = self.source_producer.demand_hint(BLOCK_SIZE, rate);
        let channels = self.channels;
        for _ in 0..MAX_FEED_BATCHES {
            let deficit = demand.saturating_sub(self.source_producer.occupied_frames());
            if deficit == 0 {
                break;
            }
            let range = self.loop_cell.load();
            let want = deficit.min(FEED_CHUNK_FRAMES);
            let (actual, frames) = match &range {
                Some(range) => range.segment(self.track_position, want),
                None => (self.track_position, want),
            };
            // The next pushed frame carries `actual` — true even across a wrap, because the
            // segment stops exactly on the boundary (and the next batch re-anchors at `in`).
            self.source_producer.set_track_position(actual);
            let position = self.track_position;
            // Field-level split: the streams are read while the two staging buffers are written.
            let read = {
                let Self {
                    sources,
                    stem_buf,
                    feed_buf,
                    ..
                } = self;
                let mut read = frames;
                for (stream, source) in sources.iter().enumerate() {
                    let got = source.read_frames(position, &mut stem_buf[..frames * CHANNELS]);
                    if got < frames {
                        // A short read (a stream that ran out before the others) must not leave the
                        // previous batch's samples in the interleave: `read_frames` leaves its tail
                        // untouched.
                        stem_buf[got * CHANNELS..frames * CHANNELS].fill(0.0);
                    }
                    read = read.min(got);
                    for i in 0..frames {
                        let dst = i * channels + stream * CHANNELS;
                        feed_buf[dst] = stem_buf[i * CHANNELS];
                        feed_buf[dst + 1] = stem_buf[i * CHANNELS + 1];
                    }
                }
                read
            };
            if read == 0 {
                break;
            }
            let pushed = self.source_producer.push(&self.feed_buf[..read * channels]);
            self.track_position += pushed as u64;
            if pushed < read {
                // The ring refused the rest (can't happen: `want ≤ deficit`) — retry next block.
                break;
            }
        }
    }
}
