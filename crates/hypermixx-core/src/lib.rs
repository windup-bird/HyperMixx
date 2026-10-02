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
pub mod stem;

pub use analysis::TrackAnalysis;
pub use beatgrid::BeatGrid;
pub use command::{
    Backend, Command, CommandResponse, CueOp, FaderTarget, FxChainId, FxSlotRef, FxSlotStatus,
    KeylockMode, LoopEditOp, LoopOp, LoopQuantum, NudgeOp, PhaseMode, SyncOp, VinylOp,
};
pub use deck::{DeckId, DeckState};
pub use key::{Key, KeyFormat, KeyMode};
pub use source::{Shared, Source};
pub use stem::{Stem, StemOp, StemPreset, StemSet, StemStatus};

/// Engine-wide sample rate. The decoder resamples everything to it at load time.
pub const SAMPLE_RATE: u32 = 44_100;
/// Interleaved stereo.
pub const CHANNELS: usize = 2;
