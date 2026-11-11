//! [`Param`]: the only way a parameter crosses from the command thread to the audio thread.
//!
//! Two atomics and a smoothing time constant. The command thread stores a target; the audio thread
//! reads it through [`Param::ramp`], which advances one block and returns a [`Ramp`]. No lock, no
//! allocation.
//!
//! [`Ramp`] is the single place that knows how a smoothed value is spread over a block: it starts
//! where the previous block ended and ends at this block's settled value, so a gain read with
//! [`Ramp::at`] is continuous at every block boundary (applying one block-quantised value instead
//! steps by up to `alpha` every 5.8 ms and zips on a fast fader move). Coefficients that only need
//! the settled value read [`Ramp::end`] — the same entry point, so there is no second API to
//! remember and no effect has to implement interpolation itself.

use std::sync::atomic::{AtomicU32, Ordering};

/// How long a jump takes to settle, when `coeff` is given as 0 (or nonsense).
const DEFAULT_TAU_SECONDS: f32 = 0.02;
/// Longest allowed smoothing time. Past half a second a "smoothed" parameter is just lagging.
const MAX_TAU_SECONDS: f32 = 0.5;

/// A smoothed fader, knob or cutoff.
///
/// `coeff` is the **smoothing time constant in seconds** (63% of the way there per τ). Pass `0.0`
/// for an instant, block-quantised parameter.
pub struct Param {
    /// Live value. Only the audio thread writes it, so a command-thread *read* is informational.
    current: AtomicU32,
    /// Desired value. Written by whoever owns the knob.
    target: AtomicU32,
    /// Time constant, seconds.
    tau: f32,
    /// Per-sample approach factor for `tau`, precomputed at the engine rate so the hot path is a
    /// multiply-add. `1.0` means "snap", which is what an instant parameter wants.
    sample_alpha: f32,
}

impl Param {
    /// A parameter resting at `initial`, settling with time constant `coeff` seconds.
    pub fn new(initial: f32, coeff: f32) -> Self {
        let tau = if coeff.is_finite() && coeff > 0.0 {
            coeff.min(MAX_TAU_SECONDS)
        } else {
            DEFAULT_TAU_SECONDS
        };
        let initial = sanitize(initial);
        // `next_block` is exact at the caller's rate; the cached alpha is the same maths at the
        // engine rate, which is the only rate this crate ever runs at.
        let sample_alpha = approach_factor(tau, 1, crate::SAMPLE_RATE);
        Self {
            current: AtomicU32::new(initial.to_bits()),
            target: AtomicU32::new(initial.to_bits()),
            tau: if coeff.is_finite() && coeff > 0.0 { coeff } else { 0.0 },
            sample_alpha,
        }
    }

    /// An instant parameter: every [`Param::next_block`] lands on the target in one step.
    pub fn instant(initial: f32) -> Self {
        let v = sanitize(initial);
        Self {
            current: AtomicU32::new(v.to_bits()),
            target: AtomicU32::new(v.to_bits()),
            tau: 0.0,
            sample_alpha: 1.0,
        }
    }

    /// Requests a new target. Never blocks; the audio thread sees it on its next step.
    #[inline]
    pub fn set(&self, value: f32) {
        self.target.store(sanitize(value).to_bits(), Ordering::Relaxed);
    }

    /// The settled value after one block: exactly [`Ramp::end`] of [`Param::ramp`]. Kept as a
    /// short-hand for callers and tests that only need the endpoint; the audio path should use
    /// `ramp` so it cannot accidentally apply a whole block at once.
    ///
    /// Mathematically identical to `frames` calls to [`Param::next_sample`], in one exp.
    #[inline]
    pub fn next_block(&self, frames: usize, sr: u32) -> f32 {
        let alpha = approach_factor(self.tau, frames, sr);
        self.step(alpha)
    }

    /// Advances one block and returns the [`Ramp`] describing it. This is the *only* way the audio
    /// thread should read a moving parameter: gains use [`Ramp::at`] inside the DSP loop,
    /// coefficients use [`Ramp::end`] once per block.
    #[inline]
    pub fn ramp(&self, frames: usize, sr: u32) -> Ramp {
        let start = self.get();
        let end = self.next_block(frames, sr);
        Ramp::new(start, end, frames)
    }

    /// One per-sample step, for parameters that must move *within* a block (filter sweeps).
    #[inline]
    pub fn next_sample(&self) -> f32 {
        self.step(self.sample_alpha)
    }

    /// Skips smoothing: current and target both become `value`. Used when (re)engaging an effect,
    /// so an enabled filter starts where the knob says rather than sliding in from wherever it was.
    #[inline]
    pub fn snap(&self, value: f32) {
        let bits = sanitize(value).to_bits();
        self.current.store(bits, Ordering::Relaxed);
        self.target.store(bits, Ordering::Relaxed);
    }

    /// The live value the audio thread last produced.
    #[inline]
    pub fn get(&self) -> f32 {
        f32::from_bits(self.current.load(Ordering::Relaxed))
    }

    /// The requested value, i.e. where the parameter is heading.
    #[inline]
    pub fn target(&self) -> f32 {
        f32::from_bits(self.target.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn is_settled(&self) -> bool {
        self.get() == self.target()
    }

    /// Advances one step of size `alpha` and publishes the result.
    #[inline]
    fn step(&self, alpha: f32) -> f32 {
        let current = f32::from_bits(self.current.load(Ordering::Relaxed));
        let target = self.target();
        // alpha >= 1 must snap: `current + 1.0 * (target - current)` is only target to within a
        // rounding step, and that residual would never be flushed on a block-quantised parameter.
        let next = if alpha >= 1.0 {
            target
        } else {
            let settled = current + alpha * (target - current);
            // Close the last denormal-sized sliver so `is_settled` can actually become true and a
            // gate or envelope can detect the end of a move.
            if (target - settled).abs() <= f32::EPSILON * target.abs().max(1.0) {
                target
            } else {
                settled
            }
        };
        self.current.store(next.to_bits(), Ordering::Relaxed);
        next
    }
}

/// One parameter's value across a single block.
///
/// `start` is where the previous block ended, `end` is where this block ends. [`Ramp::at`]
/// interpolates linearly between them, which is what keeps a gain continuous at block boundaries;
/// [`Ramp::end`] is the settled value for coefficients computed once per block. A `Ramp` is a plain
/// `Copy` value, so reading it in a DSP loop costs one multiply-add per sample and no atomics.
#[derive(Clone, Copy, Debug)]
pub struct Ramp {
    start: f32,
    end: f32,
    span: f32,
    frames: usize,
    last: usize,
    inv_last: f32,
}

impl Ramp {
    /// Builds a ramp over `frames` samples. `frames == 0` is harmless: `at` then just returns
    /// `start` and `end` is still the settled value.
    pub fn new(start: f32, end: f32, frames: usize) -> Self {
        let last = frames.saturating_sub(1).max(1);
        Self {
            start,
            end,
            span: end - start,
            frames,
            last,
            inv_last: 1.0 / last as f32,
        }
    }

    /// The value at sample `sample`, clamped to the block so an out-of-range index cannot run the
    /// interpolation past `end`.
    #[inline]
    pub fn at(&self, sample: usize) -> f32 {
        self.start + self.span * (sample.min(self.last) as f32 * self.inv_last)
    }

    /// The value the previous block ended on (and this block's first sample).
    #[inline]
    pub fn start(&self) -> f32 {
        self.start
    }

    /// This block's settled value — the last sample, and the value a coefficient should use.
    #[inline]
    pub fn end(&self) -> f32 {
        self.end
    }

    /// True when the parameter did not move this block, so a caller can fall back to a constant
    /// gain (and keep the unity no-op).
    #[inline]
    pub fn is_constant(&self) -> bool {
        self.start == self.end
    }

    /// Clamps both endpoints to a range. Do this once at the boundary instead of clamping every
    /// sample inside the loop.
    #[inline]
    pub fn clamped(self, lo: f32, hi: f32) -> Self {
        Self::new(
            self.start.clamp(lo, hi),
            self.end.clamp(lo, hi),
            self.frames,
        )
    }

    /// Maps both endpoints through a derived law (`fader_amp`, `db_to_amp`, a crossfader curve,
    /// `size → feedback`). For a linear law this is exact; for a nonlinear one the ramp runs
    /// between the mapped endpoints rather than `f(at(i))`, which differs only within one 5.8 ms
    /// block and stays continuous either way.
    #[inline]
    pub fn map(self, f: impl Fn(f32) -> f32) -> Self {
        Self::new(f(self.start), f(self.end), self.frames)
    }
}

impl Clone for Param {
    /// Copies the live and target values (not the atomics' identity), so a chain can be built from
    /// a template.
    fn clone(&self) -> Self {
        Self {
            current: AtomicU32::new(self.get().to_bits()),
            target: AtomicU32::new(self.target().to_bits()),
            tau: self.tau,
            sample_alpha: self.sample_alpha,
        }
    }
}

impl Default for Param {
    fn default() -> Self {
        Self::instant(0.0)
    }
}

impl std::fmt::Debug for Param {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Param")
            .field("current", &self.get())
            .field("target", &self.target())
            .field("tau", &self.tau)
            .finish()
    }
}

/// Fraction of the remaining distance closed over `frames` samples at rate `sr`.
fn approach_factor(tau_seconds: f32, frames: usize, sr: u32) -> f32 {
    if tau_seconds <= 0.0 || frames == 0 || sr == 0 {
        return 1.0;
    }
    let elapsed = frames as f64 / f64::from(sr);
    let alpha = 1.0 - (-elapsed / f64::from(tau_seconds)).exp();
    (alpha as f32).clamp(0.0, 1.0)
}

/// A `NaN` in a fader position would poison a bus for the rest of the session.
#[inline]
fn sanitize(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BLOCK_SIZE;

    #[test]
    fn instant_param_lands_in_one_block() {
        let p = Param::instant(0.0);
        p.set(0.7);
        assert_eq!(p.next_block(BLOCK_SIZE, 44_100), 0.7);
        assert!(p.is_settled());
    }

    #[test]
    fn smoothed_param_approaches_then_settles() {
        let p = Param::new(0.0, 0.02);
        p.set(1.0);
        let mut v = 0.0f32;
        for _ in 0..400 {
            v = p.next_block(BLOCK_SIZE, 44_100);
        }
        assert!((v - 1.0).abs() < 1e-3, "v={v}");
        assert!(p.is_settled(), "the sliver clamp must let a move finish exactly");
    }

    #[test]
    fn block_step_matches_the_per_sample_step() {
        // Same tau, same total distance: the one-exp block path must agree with 256 per-sample
        // steps to within a hair, or automation would sound different per code path.
        let per_sample = Param::new(0.0, 0.02);
        let per_block = Param::new(0.0, 0.02);
        per_sample.set(1.0);
        per_block.set(1.0);
        let mut v = 0.0f32;
        for _ in 0..BLOCK_SIZE {
            v = per_sample.next_sample();
        }
        let block = per_block.next_block(BLOCK_SIZE, 44_100);
        assert!((v - block).abs() < 1e-4, "sample={v} block={block}");
    }

    #[test]
    fn a_nan_target_cannot_poison_the_value() {
        let p = Param::new(0.5, 0.0);
        p.set(f32::NAN);
        assert_eq!(p.next_block(BLOCK_SIZE, 44_100), 0.0);
        let p = Param::new(f32::NAN, 0.0);
        assert_eq!(p.get(), 0.0);
    }

    #[test]
    fn block_ramps_join_without_a_step() {
        // A fast fader move used to jump by ~50% of the remaining distance at every block
        // boundary. Consecutive ramps must meet exactly, and each block's gain must move.
        let p = Param::new(0.0, 0.008);
        p.set(1.0);
        let mut previous_end = None;
        for _ in 0..4 {
            let ramp = p.ramp(BLOCK_SIZE, 44_100);
            if let Some(previous) = previous_end {
                assert_eq!(
                    ramp.start(),
                    previous,
                    "a block must start where the last one ended"
                );
            }
            assert!(
                ramp.end() > ramp.start(),
                "the gain must still be moving in this block"
            );
            assert_eq!(ramp.at(0), ramp.start());
            assert_eq!(ramp.at(BLOCK_SIZE - 1), ramp.end());
            previous_end = Some(ramp.end());
        }
    }

    #[test]
    fn ramp_helpers_clamp_and_map_endpoints() {
        let ramp = Ramp::new(-0.5, 1.5, 8);
        let clamped = ramp.clamped(0.0, 1.0);
        assert_eq!(clamped.start(), 0.0);
        assert_eq!(clamped.end(), 1.0);
        // A linear law is exact under `map`.
        let doubled = clamped.map(|v| v * 2.0);
        assert_eq!(doubled.start(), 0.0);
        assert_eq!(doubled.end(), 2.0);
        assert_eq!(doubled.at(7), 2.0);
        assert!(!ramp.is_constant());
        assert!(Ramp::new(0.3, 0.3, 8).is_constant());
        // An out-of-range index cannot run past the end, and a zero-frame ramp does not divide by 0.
        assert_eq!(ramp.at(999), ramp.end());
        assert_eq!(Ramp::new(0.0, 1.0, 0).at(0), 0.0);
    }

    #[test]
    fn snap_bypasses_the_ramp() {
        let p = Param::new(0.0, 0.1);
        p.snap(0.25);
        assert_eq!(p.get(), 0.25);
        assert_eq!(p.target(), 0.25);
    }

    /// The command thread and the audio thread sharing one parameter, no lock.
    #[test]
    fn cross_thread_handoff_is_lossless() {
        let p = std::sync::Arc::new(Param::new(0.0, 0.0));
        let writer = {
            let p = p.clone();
            std::thread::spawn(move || {
                for i in 1..=1000 {
                    p.set(i as f32);
                }
            })
        };
        let mut last = 0.0f32;
        while !writer.is_finished() {
            last = p.next_block(BLOCK_SIZE, 44_100);
        }
        writer.join().unwrap();
        for _ in 0..8 {
            last = p.next_block(BLOCK_SIZE, 44_100);
        }
        assert_eq!(last, 1000.0);
    }
}
