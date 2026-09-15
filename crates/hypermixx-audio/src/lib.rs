//! Hypermixx audio engine: dual-deck transport, time-stretch, mixing, cpal output.
//!
//! Depends on `core` (types) and `media` (PCM). Knows nothing about files or analysis — the CLI
//! decodes and analyses elsewhere, then hands results in via [`Command`](hypermixx_core::Command).

// `deck/deck.rs` and `flow/flow.rs` keep their names from the project spec.
#![allow(clippy::module_inception)]

pub mod deck;
pub mod flow;
pub mod pipeline;
pub mod ringbuf;

// The engine's public surface: re-export the core types it trades in plus its own runtime types.
pub use deck::{Deck, FlowShift, LoopState, Seek};
pub use flow::{Flow, FlowState, PitchShiftEngine};
pub use pipeline::AudioPipeline;
pub use ringbuf::AudioRingBuffer;

pub use {hypermixx_core as core, hypermixx_media as media};

pub use hypermixx_core::{
    Backend, BeatGrid, Command, CommandResponse, DeckId, DeckState, Key, KeyFormat, KeyMode,
    TrackAnalysis, CHANNELS, SAMPLE_RATE,
};

/// Frames per processing block (one deck tick).
pub const BLOCK_SIZE: usize = 256;
/// Output ring buffer capacity, in frames.
pub const OUTPUT_RING_CAPACITY: usize = 4096;
/// Frames pre-filled into the output ring before the audio stream starts.
pub const PREFILL_FRAMES: usize = 2048;
/// Number of decks the pipeline owns: 0 and 1.
pub const DECK_COUNT: usize = 2;
/// Per-deck gain when summing decks; keeps a two-deck mix at unity.
pub const DECK_MIX_GAIN: f32 = 0.5;

/// Samples (f32) per processing block.
pub const BLOCK_SAMPLES: usize = BLOCK_SIZE * CHANNELS;
/// Ring buffer capacity in samples.
pub const OUTPUT_RING_SAMPLES: usize = OUTPUT_RING_CAPACITY * CHANNELS;
