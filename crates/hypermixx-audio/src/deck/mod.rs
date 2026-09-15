//! Deck subsystem: transport state machine, seek arithmetic, loops, and flow warm-up.
//!
//! The payload types (`TrackAnalysis`, `Key`, `BeatGrid`, …) live in `core` so the `Command`
//! protocol can reference them without a dependency cycle; they are re-exported here for the
//! engine's internal `use crate::deck::…` imports.

mod deck;
mod flowshift;
mod jump;
mod loop_;

pub use deck::Deck;
pub use flowshift::FlowShift;
pub use jump::{phase_preserving, Seek};
pub use loop_::LoopState;

pub use hypermixx_core::{BeatGrid, Key, KeyFormat, KeyMode, TrackAnalysis};
