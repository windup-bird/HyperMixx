//! Deck subsystem: transport state machine, seek arithmetic, loops, and flow warm-up.
//!
//! The payload types (`TrackAnalysis`, `Key`, `BeatGrid`, …) live in `core` so the `Command`
//! protocol can reference them without a dependency cycle; they are re-exported here for the
//! engine's internal `use crate::deck::…` imports.

mod deck;
mod jump;
mod loop_;
mod scratch;
mod sync;

pub use deck::Deck;
use scratch::Scratch;
pub use jump::{phase_preserving, Seek};
pub use loop_::{
    beat_shift, beats_after, provisional_range, quantize_offset, quantize_to_beat, LoopRange,
    LoopRangeCell, LoopSource,
};
pub use sync::{
    wrap_phase, LeaderSample, Nudge, PhaseAlign, Playhead, Pll, SyncCtx, BLOCK_SECONDS,
    DEFAULT_TEMPO_RANGE, MAX_RATE, MAX_TEMPO_RANGE, MIN_RATE, NUDGERATE_LIMIT, NUDGE_LIMIT,
    PLL_LIMIT,
};

pub use hypermixx_core::{BeatGrid, Key, KeyFormat, KeyMode, TrackAnalysis};
