//! Hypermixx audio engine: two decks, beat grid, passthrough pitchshift, no mixer yet.

// `flow/flow.rs` and `deck/deck.rs` are named by the project spec, not by us.
#![allow(clippy::module_inception)]

pub mod beatgrid;
pub mod command;
pub mod deck;
pub mod flow;
pub mod pipeline;
pub mod ringbuf;
pub mod source;

pub use beatgrid::{BeatGrid, KeyMode, KeyReport, TrackAnalysis};
pub use command::{Command, CommandResponse, DeckState};
pub use deck::Deck;
pub use flow::Flow;
pub use pipeline::AudioPipeline;
pub use ringbuf::AudioRingBuffer;
pub use source::{PcmPool, Source};

/// Engine-wide sample rate. Everything downstream of the decoder is 48kHz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Interleaved stereo.
pub const CHANNELS: usize = 2;
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
