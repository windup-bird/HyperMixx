//! Hypermixx shared protocol crate: types every layer speaks, zero engine/analysis logic.
//!
//! Dependency rule: this crate depends only on `serde`. Nothing here knows about cpal,
//! symphonia, stratum-dsp or timestretch.

pub mod analysis;
pub mod beatgrid;
pub mod command;
pub mod deck;
pub mod key;
pub mod source;

pub use analysis::TrackAnalysis;
pub use beatgrid::BeatGrid;
pub use command::{Backend, Command, CommandResponse, FxChainId, FxSlotRef, FxSlotStatus,
    LoopEditOp, LoopOp, LoopQuantum};
pub use deck::{DeckId, DeckState};
pub use key::{Key, KeyFormat, KeyMode};
pub use source::{Shared, Source};

/// Engine-wide sample rate. Everything downstream of the decoder is 48kHz.
pub const SAMPLE_RATE: u32 = 44_100;
/// Interleaved stereo.
pub const CHANNELS: usize = 2;
