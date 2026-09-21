//! Hypermixx audio engine: decks, time-stretch, a mixer with FX buses, multi-output.
//!
//! Depends on `core` (types) and `media` (PCM). Knows nothing about files or analysis — the CLI
//! decodes and analyses elsewhere, then hands results in via [`Command`](hypermixx_core::Command).
//!
//! Layering inside the crate, top to bottom:
//!
//! ```text
//! pipeline ──► mixer ──► deck ──► flow ──► fx
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

// The engine's public surface: the core types it trades in, plus its own runtime types.
pub use deck::{Deck, FlowShift, LoopState, Seek};
pub use flow::{Flow, FlowState, PitchShiftEngine};
pub use fx::{Fx, FxChain, FxContext, FxError, FxKind, FxSlot, FxTarget, Param};
pub use mixer::{
    reference_toml, simple_dj, Bus, Channel, ChannelConfig, CueTap, MasterBus, Mixer, MixerConfig,
    MixerError, Output, OutputConfig, OutputError, OutputId, Outputs,
};
pub use pipeline::{AudioPipeline, Meters, PipelineError};
pub use ringbuf::AudioRingBuffer;

pub use {hypermixx_core as core, hypermixx_media as media};

pub use hypermixx_core::{
    Backend, BeatGrid, Command, CommandResponse, DeckId, DeckState, FxChainId, Key, KeyFormat,
    KeyMode, Source, TrackAnalysis, CHANNELS, SAMPLE_RATE,
};

/// Frames per processing block: one deck tick, and the mixer's block length.
pub const BLOCK_SIZE: usize = 256;
/// Output ring buffer capacity, in frames.
pub const OUTPUT_RING_CAPACITY: usize = 4096;
/// Frames of silence pre-filled into each output ring before its stream starts, so the device is
/// never waiting on the producer's first block. Capped at half the ring.
pub const PREFILL_FRAMES: usize = 2048;
/// Number of decks the default topology owns: 0 and 1.
pub const DECK_COUNT: usize = 2;
/// Per-deck gain when summing two decks by hand. The mixer owns level now (a master fader and a
/// limiter replace a fixed constant), so this survives only for the pre-mixer integration tests.
pub const DECK_MIX_GAIN: f32 = 0.5;

/// Samples (f32) per processing block.
pub const BLOCK_SAMPLES: usize = BLOCK_SIZE * CHANNELS;
/// Ring buffer capacity in samples.
pub const OUTPUT_RING_SAMPLES: usize = OUTPUT_RING_CAPACITY * CHANNELS;
