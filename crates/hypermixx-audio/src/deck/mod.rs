//! Deck subsystem: transport state machine, seek arithmetic, loops, and flow warm-up.
//!
//! The payload types (`TrackAnalysis`, `Key`, `BeatGrid`, …) live in `core` so the `Command`
//! protocol can reference them without a dependency cycle; they are re-exported here for the
//! engine's internal `use crate::deck::…` imports.

mod deck;
mod flowshift;
mod jump;
mod loop_;
mod sync;

pub use deck::Deck;
pub use flowshift::FlowShift;
pub use jump::{phase_preserving, Seek};
pub use loop_::{
    beats_after, beat_shift, provisional_range, quantize_offset, quantize_to_beat, LoopRange,
    LoopRangeCell, LoopSource,
};
pub use sync::{
    wrap_phase, LeaderSample, Nudge, PhaseAlign, Playhead, Pll, SyncCtx, BLOCK_SECONDS,
    MAX_RATE, MIN_RATE, NUDGE_LIMIT, NUDGERATE_LIMIT, PLL_LIMIT,
};

pub use hypermixx_core::{BeatGrid, Key, KeyFormat, KeyMode, TrackAnalysis};
