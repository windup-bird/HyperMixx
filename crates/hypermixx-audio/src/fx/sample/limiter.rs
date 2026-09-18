//! [`Limiter`]: the last stage before a sound card, and the only one that may not be skipped.
//!
//! Three behaviours make it a limiter rather than a compressor:
//!
//! 1. **Peak look-ahead.** The sidechain reads the *incoming* sample while the output still emits
//!    the one [`LOOKAHEAD`] frames ago, so gain is pulled down before a transient arrives instead of
//!    chasing it. At 44.1 kHz that is 0.18 ms: inaudible as delay, long enough to catch a kick edge.
//! 2. **Asymmetric time constants.** Attack is instantaneous (the gain snaps down the moment the
//!    sidechain sees an over-level); release is a slow ramp in tens of milliseconds. Symmetric
//!    settings pump.
//! 3. **A releasing threshold.** Under sustained over-level the ceiling backs off by a few dB, then
//!    climbs back at its own rate — the difference between "safe" and "safe *and* musical".
//!
//! Nothing hard-clips: past the knee, samples are curved exponentially toward the ceiling, so the
//! worst case a listener ever hears is soft saturation rather than fuzz.

use super::super::{Fx, FxContext, FxError, Param};
use crate::mixer::Bus;

/// Frames of look-ahead delay. 8 @ 44.1 kHz = 0.18 ms.
pub const LOOKAHEAD: usize = 8;
/// How far the threshold may back off under sustained over-level.
pub const RELEASE_RANGE_DB: f32 = 6.0;
/// Where the soft knee begins, as a fraction of the ceiling.
///
/// Only the top ~1.4 dB of travel is curved, so ordinary material — anything at or below the knee —
/// passes through *bit-exact*. A wider knee (say 0.5) would soft-limit a signal that merely approaches
/// the ceiling, which is audible as the mix losing its top end whenever a deck gets loud.
pub const KNEE_START: f32 = 0.85;
const DEFAULT_RELEASE_SECONDS: f32 = 0.12;
/// Threshold recovery: ~1.5 s to climb back the full [`RELEASE_RANGE_DB`].
const DEFAULT_RECOVERY_SECONDS: f32 = 1.5;
const DEFAULT_BACKOFF_DB: f32 = 3.0;

/// A safety limiter.
pub struct Limiter {
    ceiling: Param,
    /// Release time, seconds.
    release: Param,
    /// Threshold recovery time, seconds.
    recovery: Param,
    /// How far the threshold may back off, dB.
    backoff: Param,
    /// How many dB of threshold the limiter has currently given away, `0.0 ..= backoff`.
    ///
    /// The ceiling in force is *derived* from this and the requested ceiling each block, rather than
    /// stored, so moving the ceiling knob takes effect at once instead of crawling toward it at the
    /// recovery rate. Storing `live_ceiling` directly was the bug: a ceiling raised from −1 to +6 dB
    /// left the limiter squeezing everything at the old −1 while it eased upward.
    backoff_db: f32,
    /// Gain applied to the delayed output, 1.0 = no reduction.
    gain: f32,
    /// Look-ahead delay lines, one per channel. Fixed size, so the hot path never allocates.
    delay: [[f32; LOOKAHEAD]; 2],
    write_head: usize,
    reduction_db: f32,
    input_peak_db: f32,
}

impl Limiter {
    /// A limiter at −1 dBFS with a 120 ms release.
    pub fn new() -> Self {
        Self::with_ceiling(db_to_lin(-1.0))
    }

    pub fn with_ceiling(ceiling: f32) -> Self {
        let ceiling = sanitize_ceiling(ceiling);
        Self {
            ceiling: Param::instant(ceiling),
            release: Param::new(DEFAULT_RELEASE_SECONDS, 0.0),
            recovery: Param::new(DEFAULT_RECOVERY_SECONDS, 0.0),
            backoff: Param::instant(DEFAULT_BACKOFF_DB),
            backoff_db: 0.0,
            gain: 1.0,
            delay: [[0.0; LOOKAHEAD]; 2],
            write_head: 0,
            reduction_db: 0.0,
            input_peak_db: f32::NEG_INFINITY,
        }
    }

    /// Parameter names, in knob order. `reduction` is a meter and read-only.
    pub const PARAMS: &'static [&'static str] =
        &["ceiling", "release", "recovery", "backoff", "reduction"];

    /// Gain reduction in effect, dB (negative = limiting).
    pub fn reduction_db(&self) -> f32 {
        self.reduction_db
    }

    /// The ceiling actually applied (below the requested one while backed off).
    pub fn effective_ceiling(&self) -> f32 {
        db_to_lin(lin_to_db(sanitize_ceiling(self.ceiling.target())) - self.backoff_db)
    }

    /// dB of threshold currently given away to headroom.
    pub fn backoff_db(&self) -> f32 {
        self.backoff_db
    }

    /// Peak of the last block, dBFS.
    pub fn input_peak_db(&self) -> f32 {
        self.input_peak_db
    }

    fn set_seconds(param: &Param, value: f32, max: f32) -> Result<(), FxError> {
        if !value.is_finite() || value < 0.0 {
            return Err(FxError::InvalidValue(value));
        }
        param.set(value.min(max));
        Ok(())
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[inline]
fn lin_to_db(lin: f32) -> f32 {
    20.0 * lin.max(1e-9).log10()
}

/// A ceiling above full scale is not a limiter; below −40 dBFS it is a mute button. Both are
/// clamped into the useful range rather than rejected, because this value arrives from a fader.
fn sanitize_ceiling(value: f32) -> f32 {
    if !value.is_finite() {
        return db_to_lin(-1.0);
    }
    value.clamp(db_to_lin(-40.0), 4.0)
}

impl Fx for Limiter {
    fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        // The bus, not the context, sets the frame count: a limiter must shape *every* sample it is
        // handed. Leaving a tail behind would let un-shaped audio reach the DAC.
        let frames = bus.frames();
        let sr = ctx.sample_rate;
        let target_ceiling = sanitize_ceiling(self.ceiling.next_block(frames, sr));
        let release = self.release.next_block(frames, sr).max(0.001);
        let recovery = self.recovery.next_block(frames, sr).max(0.01);
        let backoff = self.backoff.next_block(frames, sr).clamp(0.0, RELEASE_RANGE_DB);

        let block_peak = bus
            .l
            .iter()
            .chain(bus.r.iter())
            .fold(0.0f32, |m, s| m.max(s.abs()));
        self.input_peak_db = lin_to_db(block_peak);

        let mut gain = self.gain;
        // The threshold in force for this block, derived (see `backoff_db`).
        let live = self.effective_ceiling();

        // The sidechain reads the *whole* incoming block before any of it is emitted. That is the
        // look-ahead, and it is why attack is free to be instantaneous: the peak is known one block
        // early, so nothing slips past a per-block step.
        let wanted = if block_peak > live && block_peak > 0.0 {
            (live / block_peak).max(0.0)
        } else {
            1.0
        };
        if wanted < gain {
            gain = wanted;
        } else {
            let block_seconds = frames as f32 / sr.max(1) as f32;
            let release_alpha = 1.0 - (-block_seconds / release).exp();
            gain += (1.0 - gain) * release_alpha;
            if 1.0 - gain < 1e-5 {
                gain = 1.0;
            }
        }

        // Emit what entered `LOOKAHEAD` frames ago, now that its gain is decided. The head advances
        // once per frame and both channels share it, so the delay is exactly `LOOKAHEAD` and L/R
        // stay sample-aligned (a half-frame offset would smear the stereo image).
        let gain_target = gain;
        let mut head = self.write_head;
        for i in 0..frames {
            for (channel, plane) in [(0usize, &mut bus.l), (1, &mut bus.r)] {
                let history = &mut self.delay[channel];
                let input = plane[i];
                plane[i] = shape(history[head], gain_target, live);
                history[head] = input;
            }
            head = (head + 1) % LOOKAHEAD;
        }
        self.write_head = head;

        // Threshold management, once per block. The comparison is against the *requested* ceiling:
        // material that fits what the user asked for is not over-level, no matter how far above the
        // last block's backed-off threshold it happens to sit.
        let block_seconds = frames as f32 / sr.max(1) as f32;
        let backoff_amount = if block_peak > target_ceiling {
            let over = (lin_to_db(block_peak) - lin_to_db(target_ceiling)).min(backoff);
            (self.backoff_db + over * 0.25).min(backoff)
        } else {
            // Exponential release: the threshold climbs back at `recovery`'s own rate.
            self.backoff_db * (-block_seconds / recovery).exp()
        };
        self.backoff_db = backoff_amount.max(0.0);
        self.gain = gain;
        self.reduction_db = lin_to_db(gain.min(1.0));
    }

    fn reset(&mut self) {
        self.gain = 1.0;
        self.reduction_db = 0.0;
        self.backoff_db = 0.0;
        self.delay = [[0.0; LOOKAHEAD]; 2];
        self.write_head = 0;
    }

    fn set_param(&self, name: &str, value: f32) -> Result<(), FxError> {
        match name {
            "ceiling" => {
                self.ceiling.set(sanitize_ceiling(value));
                Ok(())
            }
            "release" => Self::set_seconds(&self.release, value, 5.0),
            "recovery" => Self::set_seconds(&self.recovery, value, 30.0),
            "backoff" => {
                if !value.is_finite() {
                    return Err(FxError::InvalidValue(value));
                }
                self.backoff.set(value.clamp(0.0, RELEASE_RANGE_DB));
                Ok(())
            }
            // A meter, not a knob: accepted so a UI can write it blindly, then ignored.
            "reduction" => Ok(()),
            _ => Err(FxError::UnknownParam),
        }
    }

    fn get_param(&self, name: &str) -> Option<f32> {
        match name {
            "ceiling" => Some(self.ceiling.target()),
            "release" => Some(self.release.target()),
            "recovery" => Some(self.recovery.target()),
            "backoff" => Some(self.backoff.target()),
            "reduction" => Some(self.reduction_db),
            "input_peak" => Some(self.input_peak_db),
            _ => None,
        }
    }

    fn param_names(&self) -> &'static [&'static str] {
        Self::PARAMS
    }
}

/// Gain, then the knee. Free function so a test can check the curve without a delay line.
///
/// Below the knee the sample passes bit-exact — a safety net must not reshape ordinary material.
/// Above it, the path is `ceiling - (ceiling - knee) * exp(-(x - knee) / (ceiling - knee))`, which
/// matches the identity in both value *and* slope at the knee (so it is inaudible at the crossover)
/// and flattens asymptotically to `ceiling`. Any input, however absurd, stays inside the ceiling —
/// which is what makes this a limiter rather than a clipper.
fn shape(sample: f32, gain: f32, ceiling: f32) -> f32 {
    if ceiling <= 0.0 {
        return 0.0;
    }
    let scaled = sample * gain;
    let magnitude = scaled.abs();
    if !magnitude.is_finite() {
        // A non-finite input must not ride through: this is the last place that can still catch it.
        return ceiling;
    }
    let knee = ceiling * KNEE_START;
    if magnitude <= knee {
        return scaled;
    }
    let span = ceiling - knee;
    let soft = ceiling - span * (-(magnitude - knee) / span).exp();
    if scaled.is_sign_negative() {
        -soft
    } else {
        soft
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SAMPLE_RATE;

    const BLOCK: usize = 256;

    fn ctx() -> FxContext {
        FxContext::gridless(SAMPLE_RATE, BLOCK)
    }

    fn bus_of(value: f32) -> Bus {
        let mut b = Bus::new(BLOCK);
        b.fill_from(value, value);
        b
    }

    #[test]
    fn quiet_audio_is_untouched() {
        let mut lim = Limiter::new();
        let mut b = bus_of(0.2);
        lim.process(&mut b, &ctx());
        // A constant signal is unaffected by the look-ahead delay.
        assert!((b.peak() - 0.2).abs() < 1e-6, "peak moved to {}", b.peak());
        assert_eq!(lim.reduction_db(), 0.0, "no gain reduction on quiet input");
    }

    #[test]
    fn a_hot_signal_is_brought_under_the_ceiling() {
        let mut lim = Limiter::new();
        // Fresh bus per block: feeding the output back in would compound the shaping, and a real
        // mixer never does that.
        for _ in 0..8 {
            let mut b = bus_of(1.8);
            lim.process(&mut b, &ctx());
            assert!(b.peak() <= 1.0, "peak escaped the ceiling: {}", b.peak());
        }
        assert!(lim.reduction_db() < -3.0, "GR was {}", lim.reduction_db());
    }

    #[test]
    fn ceiling_is_respected_when_lowered_and_when_raised() {
        let mut lim = Limiter::new();
        lim.set_param("ceiling", db_to_lin(-12.0)).unwrap();
        let mut b = bus_of(1.0);
        for _ in 0..60 {
            lim.process(&mut b, &ctx());
        }
        assert!(
            b.peak() <= db_to_lin(-11.0),
            "peak {} above a −12 dB ceiling",
            b.peak()
        );

        // A +6 dB ceiling must leave a 1.5 peak alone.
        // A ceiling set above the material must be transparent: no GR, and no reshaping below the
        // knee. This is the regression the derived-threshold model fixes — a stored `live_ceiling`
        // crawled up at the recovery rate and kept squeezing long after the knob said otherwise.
        let mut lim = Limiter::new();
        lim.set_param("ceiling", 2.0).unwrap();
        let mut b = bus_of(1.5);
        lim.process(&mut b, &ctx());
        assert_eq!(lim.reduction_db(), 0.0, "GR engaged under a generous ceiling");
        assert!((b.peak() - 1.5).abs() < 1e-6, "signal was shaped: {}", b.peak());
    }

    #[test]
    fn gain_recovers_after_the_over_level_is_gone() {
        let mut lim = Limiter::new();
        lim.process(&mut bus_of(2.0), &ctx());
        assert!(lim.reduction_db() < 0.0);
        for _ in 0..500 {
            lim.process(&mut bus_of(0.2), &ctx());
        }
        assert!(
            lim.reduction_db().abs() < 0.2,
            "GR stuck at {}",
            lim.reduction_db()
        );
    }

    #[test]
    fn sustained_over_level_backs_the_threshold_off_then_recovers() {
        let mut lim = Limiter::new();
        for _ in 0..64 {
            lim.process(&mut bus_of(3.0), &ctx());
        }
        assert!(
            lim.backoff_db() > 2.5,
            "threshold only gave away {:.1} dB",
            lim.backoff_db()
        );
        assert!(
            lim.backoff_db() <= RELEASE_RANGE_DB + 1e-3,
            "threshold ran away to {:.1} dB",
            lim.backoff_db()
        );
        // Recovery is exponential with a ~1.5 s time constant, so measure the decay rather than
        // asserting an arbitrary absolute: 3 s ≈ two time constants must remove ~86% of it.
        let given_away = lim.backoff_db();
        assert!(given_away > 2.0, "nothing was given away to measure");
        for _ in 0..500 {
            lim.process(&mut bus_of(0.01), &ctx());
        }
        let remaining = lim.backoff_db();
        assert!(
            remaining < given_away * 0.25,
            "threshold only climbed from {given_away:.1} dB to {remaining:.1} dB"
        );
        assert!(
            (lin_to_db(lim.effective_ceiling()) + 1.0).abs() < 0.5,
            "ceiling did not return: {:.1} dBFS",
            lin_to_db(lim.effective_ceiling())
        );
    }

    #[test]
    fn backoff_of_zero_pins_the_ceiling() {
        let mut lim = Limiter::new();
        lim.set_param("backoff", 0.0).unwrap();
        for _ in 0..200 {
            lim.process(&mut bus_of(4.0), &ctx());
        }
        assert_eq!(lim.backoff_db(), 0.0, "a zero backoff still gave threshold away");
        assert!(
            (lin_to_db(lim.effective_ceiling()) + 1.0).abs() < 0.02,
            "pinned ceiling drifted to {:.2} dBFS",
            lin_to_db(lim.effective_ceiling())
        );
    }

    #[test]
    fn output_never_exceeds_full_scale_even_on_a_square_wave() {
        let mut lim = Limiter::new();
        let mut b = Bus::new(BLOCK * 2);
        for (i, s) in b.l.iter_mut().enumerate() {
            *s = if i % 2 == 0 { 4.0 } else { -4.0 };
            b.r[i] = -*s;
        }
        for _ in 0..8 {
            lim.process(&mut b, &ctx());
        }
        assert!(b.peak() <= 1.0 + 1e-3, "peak {}", b.peak());
        // The square wave above is ±4, far outside anything a decoder emits; the point is that even
        // it cannot get past the ceiling.
        assert!(b.l.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn a_nan_input_cannot_poison_the_output() {
        let mut lim = Limiter::new();
        let mut b = bus_of(0.5);
        b.l[7] = f32::NAN;
        lim.process(&mut b, &ctx());
        assert!(b.l.iter().all(|s| s.is_finite()), "NaN rode through");
    }

    #[test]
    fn look_ahead_delays_by_the_configured_amount() {
        let mut lim = Limiter::new();
        let mut b = Bus::new(BLOCK);
        b.l[0] = 0.9; // an impulse, well under the ceiling
        lim.process(&mut b, &ctx());
        let hit = b
            .l
            .iter()
            .position(|s| s.abs() > 0.5)
            .expect("the impulse vanished");
        assert_eq!(hit, LOOKAHEAD, "delay-line length");
    }

    #[test]
    fn reset_releases_everything_and_clears_history() {
        let mut lim = Limiter::new();
        lim.process(&mut bus_of(3.0), &ctx());
        lim.reset();
        assert_eq!(lim.reduction_db(), 0.0);
        assert!((lin_to_db(lim.effective_ceiling()) + 1.0).abs() < 0.01);
        let mut b = bus_of(0.5);
        lim.process(&mut b, &ctx());
        assert!((b.peak() - 0.5).abs() < 1e-3, "reset should not colour audio: {}", b.peak());
    }

    #[test]
    fn param_surface_is_strict() {
        let lim = Limiter::new();
        assert_eq!(lim.set_param("attack", 0.1), Err(FxError::UnknownParam));
        assert!(matches!(
            lim.set_param("release", -1.0),
            Err(FxError::InvalidValue(_))
        ));
        assert!(matches!(
            lim.set_param("recovery", f32::NAN),
            Err(FxError::InvalidValue(_))
        ));
        // A fader can hand nonsense; the limiter clamps instead of refusing.
        lim.set_param("ceiling", f32::NAN).unwrap();
        assert!(lim.ceiling.target() > 0.8 && lim.ceiling.target() < 1.01);
        lim.set_param("ceiling", -999.0).unwrap();
        assert!(lim.ceiling.target() > 0.0);
        lim.set_param("backoff", 99.0).unwrap();
        assert_eq!(lim.get_param("backoff"), Some(RELEASE_RANGE_DB));
        assert_eq!(lim.param_names(), Limiter::PARAMS);
        assert!(lim.get_param("input_peak").is_some());
        assert!(lim.get_param("nope").is_none());
    }

    #[test]
    fn below_the_knee_audio_passes_bit_exact() {
        let ceiling = db_to_lin(-1.0);
        let knee = ceiling * KNEE_START;
        for i in 0..=50 {
            let x = knee * i as f32 / 50.0;
            assert_eq!(shape(x, 1.0, ceiling), x, "{x} was reshaped below the knee");
            assert_eq!(shape(-x, 1.0, ceiling), -x);
        }
    }

    #[test]
    fn the_knee_is_monotonic_continuous_and_bounded() {
        let ceiling = db_to_lin(-1.0);
        let (mut previous, mut monotonic, mut max_jump) = (shape(0.0, 1.0, ceiling), true, 0.0f32);
        let step = ceiling / 500.0;
        let mut x = 0.0f32;
        while x <= ceiling * 6.0 {
            let y = shape(x, 1.0, ceiling);
            monotonic &= y >= previous - 1e-6;
            max_jump = max_jump.max(y - previous);
            assert!(y <= ceiling + 1e-6, "{x} -> {y} escaped the ceiling");
            (previous, x) = (y, x + step);
        }
        assert!(monotonic, "the knee folded back on itself");
        // A hard clip's signature is an immediate flat top *and* a slope discontinuity; the largest
        // single-step change must stay at the identity slope (1.0) or the curve jumped somewhere.
        assert!(max_jump <= step * 1.001, "knee was discontinuous: {max_jump} vs {step}");
        // Saturating but never reaching the ceiling exactly, and a huge input is still caught.
        assert!(shape(ceiling * 3.0, 1.0, ceiling) > ceiling * 0.99);
        assert!(shape(1e6, 1.0, ceiling) <= ceiling + 1e-5);
        assert!(shape(f32::NAN, 1.0, ceiling).is_finite());
    }
}
