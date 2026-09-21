//! The built-in effects.
//!
//! "Sample" here means *sample-level* processing — the DSP that runs per sample inside a chain — as
//! distinct from the mixer's own stages (faders, sums, cue sends) and from any future protocol-level
//! effect. Each file is self-contained: it depends on [`crate::fx`] for the trait and on
//! [`crate::mixer::Bus`] for the signal, and on nothing else in the engine. That is what makes an
//! effect portable between a channel chain, the master chain and the limiter's safety stage.

mod eq;
mod filter;
mod gain;
mod limiter;

pub use eq::{Eq, DEFAULT_HIGH_HZ, DEFAULT_LOW_HZ, DEFAULT_MID_HZ};
pub use filter::{Filter, MAX_HZ, MIN_HZ};
pub use gain::{Fader, Gain};
pub use limiter::{Limiter, LOOKAHEAD, RELEASE_RANGE_DB};
