//! Hypermixx audio engine: decks, time-stretch, a mixer with FX buses, multi-output.
//!
//! Depends on `core` (types) and `media` (PCM). Knows nothing about files or analysis — the CLI
//! decodes and analyses elsewhere, then hands results in via [`Command`](hypermixx_core::Command).
//!
//! Layering inside the crate, top to bottom:
//!
//! ```text
//! pipeline ──► sync ──► mixer ──► deck ──► flow ──► fx
//!                 └────► output ─► cpal (the only device user in the crate)
//! ```

// `deck/deck.rs` and `flow/flow.rs` keep their names from the project spec.
#![allow(clippy::module_inception)]

pub mod deck;
pub mod flow;
pub mod fx;
pub mod mixer;
pub mod pipeline;
pub mod ringbuf;
pub mod sync;

// The engine's public surface: the core types it trades in, plus its own runtime types.
pub use deck::{Deck, LoopRange, Seek};
pub use flow::{Direction, Playing, PitchShiftEngine};
pub use fx::{Fx, FxChain, FxContext, FxError, FxKind, FxSlot, FxTarget, Param};
pub use mixer::{
    reference_toml, silent_test_channel, simple_dj, Bus, Channel, ChannelConfig,
    CrossfaderConfig, CrossfaderCurve, CrossfaderPlace, CueMixConfig, CueTap, MasterBus, Mixer,
    MixerConfig, MixerError, Output, OutputConfig, OutputError, OutputId, OutputRole, OutputState,
    OutputStatus, Outputs,
};
pub use pipeline::{AudioPipeline, AudioTuning, Meters, PipelineError};
pub use ringbuf::AudioRingBuffer;
pub use sync::{SyncGroup, SyncMode};

pub use {hypermixx_core as core, hypermixx_media as media};

pub use hypermixx_core::{
    Backend, BeatGrid, Command, CommandResponse, DeckId, DeckState, FxChainId, Key, KeyFormat,
    KeyMode, NudgeOp, PhaseMode, Source, SyncOp, TrackAnalysis, VinylOp, CHANNELS, SAMPLE_RATE,
};

/// Frames per processing block: one deck tick, and the mixer's block length.
///
/// A **contract**, not a setting: the time-stretch engine is fed to a `BLOCK_SIZE` demand, and the
/// deck's catch-up and loop arithmetic are expressed in it. The live config deliberately does not
/// offer it; the ring and the engine rate are what a machine's performance actually needs to vary.
pub const BLOCK_SIZE: usize = 256;
/// Output ring buffer capacity, in frames.
pub const OUTPUT_RING_CAPACITY: usize = 4096;
/// Frames of silence pre-filled into each output ring before its stream starts, so the device is
/// never waiting on the producer's first block. Capped at half the ring.
pub const PREFILL_FRAMES: usize = 2048;
/// Number of decks the default topology owns: 0 and 1.
pub const DECK_COUNT: usize = 2;

/// Samples (f32) per processing block.
pub const BLOCK_SAMPLES: usize = BLOCK_SIZE * CHANNELS;
/// Ring buffer capacity in samples.
pub const OUTPUT_RING_SAMPLES: usize = OUTPUT_RING_CAPACITY * CHANNELS;
