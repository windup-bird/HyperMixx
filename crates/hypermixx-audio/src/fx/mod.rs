//! The FX subsystem: the [`Fx`] interface, parameter smoothing, chains and the sample effects.
//!
//! Design rules, all of them load-bearing:
//!
//! * **An effect only sees a [`Bus`] and a [`FxContext`].** It never opens a device, reads a file,
//!   or looks at a deck. That is what lets the same `Limiter` sit in a channel chain, the master
//!   chain and the master's safety stage without knowing the difference.
//! * **The trait has six methods and every one has a default body.** A new effect implements
//!   [`Fx::process`] plus [`Fx::set_param`]/[`Fx::param_names`] if it takes parameters at all;
//!   `reset`, `on_trigger` and `get_param` stay optional.
//! * **Parameters cross the thread boundary through [`Param`], not through a lock.** A command
//!   thread calls [`Param::set`] (an atomic store); the audio thread pulls the moving target down
//!   per block or per sample, so a cutoff sweep can never click and never blocks.
//! * **No hot-swap of a chain.** Slots are added and removed on the producer thread, at a block
//!   boundary, by moving a `Box<dyn Fx>` — which is why a chain is `Vec<FxSlot>` and not an
//!   `ArcSwap`.

pub(crate) mod dsp;
mod chain;
mod param;
mod registry;
pub mod sample;

pub use chain::{FxChain, FxSlot, FxTarget};
pub use param::Param;
pub use registry::FxKind;

use crate::mixer::Bus;

/// What a block of audio looks like to an effect.
///
/// `Copy` on purpose: the mixer builds one per block and hands it to every stage, so a stage can
/// keep a field-by-field copy instead of borrowing the mixer.
#[derive(Clone, Copy, Debug)]
pub struct FxContext {
    pub sample_rate: u32,
    /// Frames in this block, not samples: stereo effects multiply by [`CHANNELS`](crate::CHANNELS).
    pub block_frames: usize,
    /// Beat length in frames. `0.0` when no deck in view has a grid — a tempo-synced effect must
    /// then fall back to a free-running rate rather than divide by zero.
    pub frames_per_beat: f64,
    /// Musical position of the block, in beats (fractional). `NaN` without a grid; the helper
    /// below filters that out so an effect can use it unconditionally.
    pub beat_position: f64,
}

impl FxContext {
    /// A gridless context at the engine's own rate: the right starting point for a mixer that has
    /// not been handed an analysis yet.
    pub fn gridless(sample_rate: u32, block_frames: usize) -> Self {
        Self {
            sample_rate,
            block_frames,
            frames_per_beat: 0.0,
            beat_position: 0.0,
        }
    }

    /// Seconds elapsed in one block of `block_frames`.
    #[inline]
    pub fn block_seconds(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.block_frames as f64 / f64::from(self.sample_rate)
        }
    }

    /// Seconds per sample, guarding against a zero rate.
    #[inline]
    pub fn sample_seconds(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            1.0 / f64::from(self.sample_rate)
        }
    }

    #[inline]
    pub fn has_grid(&self) -> bool {
        self.frames_per_beat.is_finite() && self.frames_per_beat > 0.0
    }

    /// Beat position, or `None` when the context carries no usable grid.
    #[inline]
    pub fn beat(&self) -> Option<f64> {
        (self.has_grid() && self.beat_position.is_finite()).then_some(self.beat_position)
    }
}

/// Why an effect refused a parameter.
#[derive(Clone, Debug, PartialEq)]
pub enum FxError {
    /// The effect has no parameter with that name.
    UnknownParam,
    /// The name exists but the value is unusable (non-finite, or outside the effect's range).
    InvalidValue(f32),
    /// A registry lookup failed: no effect is called by that name.
    UnknownKind(String),
}

impl FxError {
    pub fn message(&self) -> String {
        match self {
            FxError::UnknownParam => "unknown parameter".into(),
            FxError::InvalidValue(v) => format!("invalid value {v}"),
            FxError::UnknownKind(k) => format!("no effect kind `{k}`"),
        }
    }
}

impl std::fmt::Display for FxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for FxError {}

/// A real-time audio effect.
///
/// Implementations must be `Send` (a chain is built on the producer thread and run there too, but
/// a front-end may hand one over) and must not allocate, block or lock in [`Fx::process`].
pub trait Fx: Send {
    /// Processes `bus` in place. This is the only method a trivial effect has to write.
    fn process(&mut self, bus: &mut Bus, ctx: &FxContext);

    /// Clears filter memory / restarts envelopes. Called when a slot is bypassed then re-enabled,
    /// so an effect never resumes with state from audio the listener has since moved past.
    fn reset(&mut self) {}

    /// The moment a pad fired: re-trigger an envelope, restart a sweep, latch a step.
    fn on_trigger(&mut self) {}

    fn set_param(&self, _name: &str, _value: f32) -> Result<(), FxError> {
        Err(FxError::UnknownParam)
    }

    fn get_param(&self, _name: &str) -> Option<f32> {
        None
    }

    /// Parameter names in the order a front-end should show them.
    fn param_names(&self) -> &'static [&'static str] {
        &[]
    }
}

/// Every parameter of an effect, as `(name, value)` pairs — the shape a UI binds against.
pub fn param_snapshot(fx: &dyn Fx) -> Vec<(String, f32)> {
    fx.param_names()
        .iter()
        .map(|name| ((*name).to_owned(), fx.get_param(name).unwrap_or(0.0)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BLOCK_SIZE;

    #[test]
    fn context_reports_grid_state() {
        let mut ctx = FxContext::gridless(44_100, BLOCK_SIZE);
        assert!(!ctx.has_grid());
        assert_eq!(ctx.beat(), None);
        ctx.frames_per_beat = 22050.0;
        ctx.beat_position = 3.5;
        assert!(ctx.has_grid());
        assert_eq!(ctx.beat(), Some(3.5));
        // A grid of the right shape but a NaN position is still unusable.
        ctx.beat_position = f64::NAN;
        assert_eq!(ctx.beat(), None);
        assert_eq!(ctx.block_seconds(), BLOCK_SIZE as f64 / 44_100.0);
    }

    /// An effect that only implements `process` must be writable in two lines.
    struct PassThrough;
    impl Fx for PassThrough {
        fn process(&mut self, _bus: &mut Bus, _ctx: &FxContext) {}
    }

    #[test]
    fn five_of_six_methods_are_optional() {
        // The whole point of default bodies: this compiles, runs, and reports no parameters.
        let mut chain = FxChain::new();
        chain.push(Box::new(PassThrough));
        let mut bus = Bus::stereo(4);
        bus.fill_from(1.0, 1.0);
        chain.process(&mut bus, &FxContext::gridless(44_100, 4));
        assert_eq!(chain.len(), 1);
        assert!(chain.slot(0).unwrap().fx().param_names().is_empty());
        assert_eq!(
            chain.slot(0).unwrap().fx().set_param("loud", 1.0),
            Err(FxError::UnknownParam)
        );
    }
}
