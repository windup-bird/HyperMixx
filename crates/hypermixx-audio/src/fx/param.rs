//! [`Param`]: the only way a parameter crosses from the command thread to the audio thread.
//!
//! Two atomics and a smoothing time constant. The command thread stores a target; the audio thread
//! walks the current value toward it, one step per block ([`Param::next_block`]) or per sample
//! ([`Param::next_sample`]). No lock, no allocation, no discontinuity — which is the entire reason
//! a cutoff sweep through this type does not click and a fader move does not zip.

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

    /// One block-quantised step: the value that holds for this block of `frames` frames.
    ///
    /// Mathematically identical to `frames` calls to [`Param::next_sample`], in one exp.
    #[inline]
    pub fn next_block(&self, frames: usize, sr: u32) -> f32 {
        let alpha = approach_factor(self.tau, frames, sr);
        self.step(alpha)
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
    let elapsed = frames as f64 / f64::from(sr) as f64;
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
