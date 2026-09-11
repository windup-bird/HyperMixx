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

use crate::source::Source;
use crate::{CHANNELS, SAMPLE_RATE};

/// How many source frames to push into the engine's ring per process_block.
/// The ring absorbs excess; the engine consumes at its own tempo rate.
const FEED_CHUNK_FRAMES: usize = 1024;

/// A real-time time-stretch engine for one flow.
pub struct PitchShiftEngine {
    controller: EngineController,
    processor: EngineProcessor,
    source_producer: SourceProducer,
    source: Arc<dyn Source>,
    /// Absolute frame position in the source that we've fed up to.
    track_position: u64,
    /// Output playhead (what the listener hears). At ratio=1.0 this equals the
    /// source position.
    output_frame: u64,
    ratio: f32,
    total: u64,
    feed_buf: Vec<f32>,
    /// Output frames remaining in the current warm-start priming (silence).
    priming_remaining: usize,
    profile: EngineProfile,
}

impl PitchShiftEngine {
    /// Creates an engine with the default Tape profile (zero latency, passthrough at ratio 1.0).
    pub fn new(source: Arc<dyn Source>, ratio: f32) -> Self {
        Self::with_profile(source, ratio, EngineProfile::Tape)
            .expect("timestretch engine config is statically valid")
    }

    /// Creates an engine with a specific profile.
    pub fn with_profile(
        source: Arc<dyn Source>,
        ratio: f32,
        profile: EngineProfile,
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
            ratio,
            total,
            feed_buf: vec![0.0; FEED_CHUNK_FRAMES * CHANNELS],
            priming_remaining: 0,
            profile,
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
            self.output_frame += capacity as u64;
        }
        capacity
    }

    /// Full seek protocol. Tape skips the warm-start (zero-latency passthrough);
    /// Keylock/WideKeylock run the priming to converge stage state.
    pub fn prepare_jump(&mut self, target_frame: u64) {
        let target = target_frame.min(self.total);
        self.processor.reset();
        self.output_frame = target;

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

    /// Current output playhead (what the listener hears), in source frames.
    /// At ratio=1.0 this equals the source position.
    pub fn current_frame(&self) -> u64 {
        self.output_frame
    }

    pub fn ratio(&self) -> f32 {
        self.ratio
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

    /// Pushes source audio into the engine's ring buffer.
    fn feed(&mut self) {
        if self.track_position >= self.total {
            return;
        }
        let read = self
            .source
            .read_frames(self.track_position, &mut self.feed_buf);
        if read > 0 {
            let pushed = self.source_producer.push(&self.feed_buf[..read * CHANNELS]);
            self.track_position += pushed as u64;
        }
    }
}
