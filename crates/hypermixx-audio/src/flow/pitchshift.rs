//! Pitchshift: real-time time-stretch engine wrapping timestretch-rs.
//!
//! Three profiles: Tape (varispeed, pitch follows tempo), Keylock (SOLA, pitch
//! locked), WideKeylock (phase vocoder, full-spectrum keylock). All run through
//! the same pull-based engine: we push source audio into its ring and pull
//! processed output in `process_block`.

use std::sync::Arc;

use timestretch::engine::{
    Engine, EngineConfig, EngineController, EngineProcessor, EngineProfile, SourceProducer,
};
use timestretch::error::StretchError;

use crate::deck::LoopRangeCell;
use crate::{CHANNELS, SAMPLE_RATE};
use hypermixx_core::Source;

/// How many source frames to push into the engine's ring per feed batch.
const FEED_CHUNK_FRAMES: usize = 1024;
/// Upper bound on feed batches per `process_block`: an empty ring refills to the engine's demand
/// (`demand_hint` ≈ one block at 4× tempo + the resampler's taps, ~1100 frames) in two.
const MAX_FEED_BATCHES: usize = 8;
/// Tempo clamp mirroring `EngineConfig`'s: the controller clamps writes, so feeding must too.
const MIN_TEMPO_RATE: f64 = 0.25;
const MAX_TEMPO_RATE: f64 = 4.0;

/// A real-time time-stretch engine for one flow.
pub struct PitchShiftEngine {
    controller: EngineController,
    processor: EngineProcessor,
    source_producer: SourceProducer,
    source: Arc<dyn Source>,
    /// Absolute frame position in the source that we've fed up to.
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
    feed_buf: Vec<f32>,
    /// Output frames remaining in the current warm-start priming (silence).
    priming_remaining: usize,
    profile: EngineProfile,
    /// The loop mapping the feed reads through. Shared with the flow that owns this engine: the
    /// engine sees nothing but ever-increasing virtual positions, and this folds each read into
    /// the range (identity below `out`, modulo above it).
    loop_cell: Arc<LoopRangeCell>,
}

impl PitchShiftEngine {
    /// Creates an engine with the default Tape profile (zero latency, passthrough at ratio 1.0).
    pub fn new(
        source: Arc<dyn Source>,
        ratio: f32,
        loop_cell: Arc<LoopRangeCell>,
    ) -> Self {
        Self::with_profile(source, ratio, EngineProfile::Tape, loop_cell)
            .expect("timestretch engine config is statically valid")
    }

    /// Creates an engine with a specific profile.
    pub fn with_profile(
        source: Arc<dyn Source>,
        ratio: f32,
        profile: EngineProfile,
        loop_cell: Arc<LoopRangeCell>,
    ) -> Result<Self, StretchError> {
        let total = source.total_frames();
        let config = EngineConfig {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            profile,
            initial_tempo_rate: (ratio as f64).clamp(0.25, 4.0),
            max_block_frames: 256,
            source_capacity_frames: 32768,
            pre_analysis: None,
        };
        let handles = Engine::build(config)?;
        Ok(Self {
            controller: handles.controller,
            processor: handles.processor,
            source_producer: handles.source,
            source,
            track_position: 0,
            output_frame: 0,
            output_frame_exact: 0.0,
            ratio,
            total,
            feed_buf: vec![0.0; FEED_CHUNK_FRAMES * CHANNELS],
            priming_remaining: 0,
            profile,
            loop_cell,
        })
    }

    /// Processes one block. Feeds source audio into the engine, then pulls
    /// processed output. Returns the number of frames written.
    pub fn process_block(&mut self, output: &mut [f32]) -> usize {
        let capacity = output.len() / CHANNELS;
        self.feed();
        self.processor.process(output);
        if self.priming_remaining > 0 {
            let silent = capacity.min(self.priming_remaining);
            self.priming_remaining -= silent;
            if self.priming_remaining == 0 {
                // Priming just ended; start counting from the jump target.
                self.output_frame = self.output_frame.max(self.output_frame);
            }
            // output_frame stays frozen during priming (the listener hears silence).
        } else {
            let rate = f64::from(self.ratio).clamp(MIN_TEMPO_RATE, MAX_TEMPO_RATE);
            self.output_frame_exact += capacity as f64 * rate;
            self.output_frame = self.output_frame_exact as u64;
        }
        capacity
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
        let mut scratch = [0.0f32; crate::BLOCK_SAMPLES];
        // Bounded well past any real preroll (the wide keylock's is two FFT windows): a guard
        // against an engine that cannot converge (an empty source), not a normal exit.
        for _ in 0..4096 {
            if self.priming_remaining == 0 {
                break;
            }
            self.process_block(&mut scratch);
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

    /// The backing source, for rebuilding the engine with a different profile.
    pub fn source_ref(&self) -> Arc<dyn Source> {
        Arc::clone(&self.source)
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
    fn feed(&mut self) {
        if self.track_position >= self.total {
            return;
        }
        let rate = f64::from(self.ratio).clamp(MIN_TEMPO_RATE, MAX_TEMPO_RATE);
        let demand = self.source_producer.demand_hint(crate::BLOCK_SIZE, rate);
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
            let read = self
                .source
                .read_frames(self.track_position, &mut self.feed_buf[..frames * CHANNELS]);
            if read == 0 {
                break;
            }
            let pushed = self.source_producer.push(&self.feed_buf[..read * CHANNELS]);
            self.track_position += pushed as u64;
            if pushed < read {
                // The ring refused the rest (can't happen: `want ≤ deficit`) — retry next block.
                break;
            }
        }
    }
}
