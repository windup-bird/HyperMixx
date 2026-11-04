//! [`Filter`]: the bipolar sweep that turns a track into an event, on a state-variable core.
//!
//! One normalised `value` (`-1.0 ..= 1.0`) drives the whole shape:
//!
//! - `value < 0` — low-pass, `-1` is 20Hz (closed), `0` is wide open
//! - `value > 0` — high-pass, `0` is wide open, `+1` is 18kHz (closed)
//! - `value == 0` — fully open, so one knob sweeps low-pass through neutral into high-pass
//!
//! `resonance` is the peak gain: it maps to Q `0.3 ..= 18`. There is no auto-levelling — a resonant
//! low-pass is *supposed* to get loud near cutoff — the master limiter is the safety net.
//!
//! # Why two branches instead of one filter that changes type
//!
//! The sweep used to be a single biquad whose `Section` flipped from `LowPass` to `HighPass` when
//! `value` crossed zero. That flip is a **topology** change on a filter that keeps its delay memory,
//! and the two ends of the flip are not the same cutoff (the low-pass sits at 18 kHz, the high-pass
//! at 20 Hz), so for one block the new topology read the old topology's history. On low-frequency
//! material that is a large transient, and after the master limiter it reads as a brief dropout.
//!
//! Here both branches are always running, each with its own **continuous** cutoff, and their outputs
//! are crossfaded around the centre:
//!
//! ```text
//! low-pass  cutoff = hertz(clamp(value + 1))   (20 Hz .. 18 kHz, flat above zero)
//! high-pass cutoff = hertz(clamp(value))       (20 Hz .. 18 kHz, flat below zero)
//! out = w · low-pass + (1 - w) · high-pass,   w smoothed per sample
//! ```
//!
//! No coefficient set is ever handed a state that belongs to a different topology, and the crossfade
//! weight has its own time constant, so a knob moved across the centre in one block is a *fade*
//! rather than a step. Around the centre both branches are transparent (18 kHz low-pass, 20 Hz
//! high-pass), which is what makes the crossfade inaudible in the first place.
//!
//! [`Mode`] picks the pole count, not a different control surface: both variants answer to the same
//! `value`/`resonance` parameters, so a slot can swap one for the other under a mapped knob.

use super::super::{Fx, FxContext, FxError, Param};
use crate::mixer::Bus;

/// Which character the sweep is built from. Both expose the same parameters and the same travel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Two poles, 12 dB/oct: the clean "linear" sweep.
    Linear,
    /// Four poles, 24 dB/oct, ladder-style: steeper and squelchier.
    Moog,
}

impl Mode {
    /// The effect-kind name this mode is built as; the fixed `filter` slot's `kind`.
    pub const fn name(self) -> &'static str {
        match self {
            Mode::Linear => "filter",
            Mode::Moog => "moog",
        }
    }

    /// Stages in each branch: one 2-pole stage for `Linear`, two cascaded for `Moog`.
    const fn stages(self) -> usize {
        match self {
            Mode::Linear => 1,
            Mode::Moog => 2,
        }
    }
}

/// Cutoff travel. Below 20 Hz a sweep is a DC servo, not a filter; past ~18 kHz there is nothing
/// musical left to remove.
pub const MIN_HZ: f32 = 20.0;
pub const MAX_HZ: f32 = 18_000.0;

/// Long enough to hide a stepped knob move, short enough that a hand-timed sweep is still hand-timed
/// and the resonance never zipper-noises under a moving coefficient set.
const FILTER_TAU: f32 = 0.006;
/// Half-width, in `value` units, of the low-pass/high-pass crossfade around the centre. At this
/// width the outer edges are a ~12 kHz low-pass and a ~21 Hz high-pass: transparent enough that
/// blending the two is inaudible.
const BLEND_HALF: f32 = 0.05;
/// Time constant of that crossfade. Long enough that a knob which jumps across the centre is a fade,
/// short enough that a hand sweep still tracks the hand.
const BLEND_TAU: f32 = 0.004;
/// The second pole pair of a `Moog` cascade is a plain Butterworth pair; the resonance lives in the
/// first pair, exactly as a ladder puts its feedback around the whole chain.
const SECOND_PAIR_Q: f64 = 0.707;

/// The sweep.
pub struct Filter {
    mode: Mode,
    value: Param,
    /// Peak gain, `0.0 ..= 1.0` → Q `0.3 ..= 18`.
    resonance: Param,
    low: Branch,
    high: Branch,
    /// Weight of the low-pass branch: `1.0` = low-pass only, `0.0` = high-pass only. Smoothed per
    /// sample so crossing the centre cannot step.
    blend: f32,
    /// (value, resonance) as of the last coefficient redraw.
    cached: (f32, f32),
    /// The rate `cached` was designed at; a rate change invalidates the design like a knob move.
    cached_rate: u32,
    /// Divergences caught and cleared. Should stay 0.
    blowups: u32,
}

impl Filter {
    /// A wide-open linear sweep (`value = 0`).
    pub fn new() -> Self {
        Self::with_mode(Mode::Linear)
    }

    /// A wide-open four-pole sweep (`value = 0`).
    pub fn moog() -> Self {
        Self::with_mode(Mode::Moog)
    }

    fn with_mode(mode: Mode) -> Self {
        let mut filter = Self {
            mode,
            value: Param::new(0.0, FILTER_TAU),
            resonance: Param::new(0.2, FILTER_TAU),
            low: Branch::new(mode),
            high: Branch::new(mode),
            blend: target_blend(0.0),
            // NaN forces a redesign on the first block, at the rate the engine is really running —
            // a coefficient set built at 44.1k on a 48k stream is a 9% pitch error on the cutoff.
            cached: (f32::NAN, f32::NAN),
            cached_rate: 0,
            blowups: 0,
        };
        // Sane coefficients before the first block, in case a caller inspects or runs it early; the
        // NaN `cached` still makes the first real block redesign at the engine's own rate.
        filter.design(crate::SAMPLE_RATE, 0.0, 0.2);
        filter
    }

    pub const PARAMS: &'static [&'static str] = &["value", "resonance"];

    /// The pole count / character this instance was built with.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Where the cutoff is set, in Hz (which side of the sweep depends on the sign of `value`).
    pub fn cutoff_hz(&self) -> f32 {
        cutoff_of(self.value.target()) as f32
    }

    /// The cutoff actually applied this block (mid-slew while the knob moves).
    pub fn applied_cutoff_hz(&self) -> f32 {
        cutoff_of(self.value.get()) as f32
    }

    /// Q currently applied.
    pub fn resonance_q(&self) -> f64 {
        resonance_to_q(self.resonance.get())
    }

    /// How many times the filter caught itself diverging and rebuilt. Should stay 0.
    pub fn blowup_count(&self) -> u32 {
        self.blowups
    }

    /// Designs both branches for one (value, resonance) pair.
    fn design(&mut self, sample_rate: u32, value: f32, resonance: f32) {
        let q = resonance_to_q(resonance);
        // Both cutoffs are continuous over the whole travel: each branch stays "open" on the side
        // of the centre it does not own, so a sweep never hands a branch a cutoff it did not have.
        self.low
            .design(hertz_of(value + 1.0), q, sample_rate);
        self.high.design(hertz_of(value), q, sample_rate);
        self.cached = (value, resonance);
        self.cached_rate = sample_rate;
    }
}

impl Default for Filter {
    fn default() -> Self {
        Self::new()
    }
}

/// The cutoff a UI should show for a normalised `value`: the branch that is currently weighted in.
fn cutoff_of(value: f32) -> f64 {
    if value <= 0.0 {
        hertz_of(value + 1.0)
    } else {
        hertz_of(value)
    }
}

/// The 20Hz..18kHz mapping for an exponent in `0.0 ..= 1.0`, clamped.
fn hertz_of(exponent: f32) -> f64 {
    let t = f64::from(exponent.clamp(0.0, 1.0));
    let hz = f64::from(MIN_HZ) * (f64::from(MAX_HZ) / f64::from(MIN_HZ)).powf(t);
    hz.clamp(f64::from(MIN_HZ), f64::from(MAX_HZ))
}

fn resonance_to_q(value: f32) -> f64 {
    let v = value.clamp(0.0, 1.0) as f64;
    0.3f64 * (18.0f64 / 0.3).powf(v)
}

/// How much of the low-pass branch a `value` asks for: `1.0` far counter-clockwise, `0.0` far
/// clockwise, a smooth ramp across [`BLEND_HALF`].
fn target_blend(value: f32) -> f32 {
    let t = (0.5 - value / (2.0 * BLEND_HALF)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// One side of the crossfade: one 2-pole stage, or two cascaded for [`Mode::Moog`].
struct Branch {
    stages: [Svf; 2],
    count: usize,
}

impl Branch {
    fn new(mode: Mode) -> Self {
        Self {
            stages: [Svf::default(); 2],
            count: mode.stages(),
        }
    }

    /// Redesigns every stage at one cutoff. The resonance lives in the first stage.
    fn design(&mut self, hz: f64, q: f64, sample_rate: u32) {
        self.stages[0].design(hz, q, sample_rate);
        if self.count > 1 {
            self.stages[1].design(hz, SECOND_PAIR_Q, sample_rate);
        }
    }

    #[inline]
    fn low_pass(&mut self, channel: usize, mut x: f32) -> f32 {
        for stage in &mut self.stages[..self.count] {
            x = stage.process(channel, x).0;
        }
        x
    }

    #[inline]
    fn high_pass(&mut self, channel: usize, mut x: f32) -> f32 {
        for stage in &mut self.stages[..self.count] {
            x = stage.process(channel, x).1;
        }
        x
    }

    fn clear(&mut self) {
        for stage in &mut self.stages {
            stage.clear();
        }
    }

    fn is_blowing(&self) -> bool {
        self.stages.iter().any(Svf::is_blowing)
    }
}

/// One TPT (topology-preserving transform) state-variable section: one shared state, low-pass and
/// high-pass outputs.
///
/// The TPT form is what makes a moving cutoff safe: the integrator state means the same thing at any
/// cutoff, so a coefficient change is a change of *character*, never a topology the old state no
/// longer belongs to.
#[derive(Clone, Copy, Debug, Default)]
struct Svf {
    a1: f32,
    a2: f32,
    a3: f32,
    /// `1 / Q`.
    k: f32,
    ic1: [f32; 2],
    ic2: [f32; 2],
}

impl Svf {
    fn design(&mut self, hz: f64, q: f64, sample_rate: u32) {
        let sr = f64::from(sample_rate.max(1));
        // Keep the cutoff inside the representable band: a control swept past Nyquist or to 0 Hz
        // must still yield stable coefficients, not a `NaN` that rides into the output for the rest
        // of the session.
        let nyquist = sr * 0.499;
        let freq = if hz.is_finite() {
            hz.clamp(5.0, nyquist.max(5.0))
        } else {
            sr / 4.0
        };
        let q = if q.is_finite() { q.max(0.05) } else { 0.707 };
        let g = (std::f64::consts::PI * freq / sr).tan() as f32;
        let k = (1.0 / q) as f32;
        let a1 = 1.0 / (1.0 + g * (g + k));
        self.a1 = a1;
        self.a2 = g * a1;
        self.a3 = g * self.a2;
        self.k = k;
    }

    /// One sample in, `(low-pass, high-pass)` out for this stage.
    #[inline]
    fn process(&mut self, channel: usize, x: f32) -> (f32, f32) {
        let channel = channel & 1;
        let v3 = x - self.ic2[channel];
        let v1 = self.a1 * self.ic1[channel] + self.a2 * v3;
        let v2 = self.ic2[channel] + self.a2 * self.ic1[channel] + self.a3 * v3;
        self.ic1[channel] = 2.0 * v1 - self.ic1[channel];
        self.ic2[channel] = 2.0 * v2 - self.ic2[channel];
        (v2, x - self.k * v1 - v2)
    }

    fn clear(&mut self) {
        self.ic1 = [0.0; 2];
        self.ic2 = [0.0; 2];
    }

    fn is_blowing(&self) -> bool {
        self.ic1
            .iter()
            .chain(self.ic2.iter())
            .chain([&self.a1, &self.a2, &self.a3, &self.k])
            .any(|value| !value.is_finite())
    }
}

impl Fx for Filter {
    fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        let (frames, sr) = (ctx.block_frames.min(bus.frames()), ctx.sample_rate.max(1));
        let value = self.value.next_block(frames, sr);
        let resonance = self.resonance.next_block(frames, sr);

        if (value, resonance) != self.cached || sr != self.cached_rate {
            self.design(sr, value, resonance);
        }
        // A diverged state would mute the deck for the rest of the session; clearing it costs one
        // block of that one branch rather than the whole channel. The guard is a handful of finite
        // checks per block, not per sample.
        if self.low.is_blowing() || self.high.is_blowing() {
            self.low.clear();
            self.high.clear();
            self.blowups += 1;
        }

        let target = target_blend(value);
        // Per-sample weight step: the crossfade is a fade even when the knob jumps across centre.
        let alpha = 1.0 - (-1.0 / (BLEND_TAU * sr as f32)).exp();
        let mut weight = self.blend;

        let (left, right) = (&mut bus.l, &mut bus.r);
        for i in 0..frames {
            weight += alpha * (target - weight);
            let (l, r) = (left[i], right[i]);
            let l = weight * self.low.low_pass(0, l) + (1.0 - weight) * self.high.high_pass(0, l);
            let r = weight * self.low.low_pass(1, r) + (1.0 - weight) * self.high.high_pass(1, r);
            left[i] = l;
            right[i] = r;
        }
        self.blend = weight;
    }

    fn reset(&mut self) {
        self.value.snap(self.value.target());
        self.resonance.snap(self.resonance.target());
        self.blend = target_blend(self.value.target());
        self.low.clear();
        self.high.clear();
        self.cached = (f32::NAN, f32::NAN);
    }

    fn set_param(&self, name: &str, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        match name {
            // `cutoff` is a legacy alias: older configs used it for the same normalised sweep.
            "value" | "cutoff" => {
                self.value.set(value.clamp(-1.0, 1.0));
                Ok(())
            }
            "resonance" => {
                self.resonance.set(value.clamp(0.0, 1.0));
                Ok(())
            }
            _ => Err(FxError::UnknownParam),
        }
    }

    fn get_param(&self, name: &str) -> Option<f32> {
        match name {
            "value" | "cutoff" => Some(self.value.target()),
            "resonance" => Some(self.resonance.target()),
            _ => None,
        }
    }

    fn param_names(&self) -> &'static [&'static str] {
        Self::PARAMS
    }
}

impl std::fmt::Debug for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Filter")
            .field("mode", &self.mode)
            .field("value", &self.value.target())
            .field("cutoff_hz", &self.cutoff_hz())
            .field("q", &(self.resonance_q() as f32))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SAMPLE_RATE;

    const BLOCK: usize = 256;

    fn sine(bus: &mut Bus, freq: f64, phase_start: usize) {
        for (i, slot) in bus.l.iter_mut().enumerate() {
            let n = phase_start + i;
            let s =
                (2.0 * std::f64::consts::PI * freq * n as f64 / f64::from(SAMPLE_RATE)).sin() as f32;
            *slot = s;
            bus.r[i] = s;
        }
    }

    /// Runs `blocks` blocks of a sine and reports the settled output peak in dB.
    fn level_at(fx: &mut Filter, freq: f64, blocks: usize) -> f64 {
        let ctx = FxContext::gridless(SAMPLE_RATE, BLOCK);
        let mut last = 0.0f32;
        for b in 0..blocks {
            let mut bus = Bus::new(BLOCK);
            sine(&mut bus, freq, b * BLOCK);
            fx.process(&mut bus, &ctx);
            assert!(bus.peak().is_finite(), "filter went non-finite");
            if b > blocks / 2 {
                last = bus.peak();
            }
        }
        20.0 * f64::from(last.max(1e-12)).log10()
    }

    fn settled(fx: &mut Filter, freq: f64) -> f64 {
        level_at(fx, freq, 100)
    }

    /// `(peak, rms)` of every block while a knob move is in flight, for a sine.
    ///
    /// Both are reported because a block is not a whole number of periods for every frequency: at
    /// 60 Hz a 256-frame block is 0.35 of a period, so its *RMS* legitimately varies with the phase
    /// it happens to cover. The peak does not.
    fn transition(
        fx: &mut Filter,
        freq: f64,
        from: f32,
        to: f32,
        blocks: usize,
    ) -> Vec<(f64, f64)> {
        let ctx = FxContext::gridless(SAMPLE_RATE, BLOCK);
        fx.set_param("value", from).unwrap();
        // Settle at the starting position first, so the sweep starts from steady state.
        for b in 0..40 {
            let mut bus = Bus::new(BLOCK);
            sine(&mut bus, freq, b * BLOCK);
            fx.process(&mut bus, &ctx);
        }
        fx.set_param("value", to).unwrap();
        let mut out = Vec::with_capacity(blocks);
        for b in 0..blocks {
            let mut bus = Bus::new(BLOCK);
            sine(&mut bus, freq, (40 + b) * BLOCK);
            fx.process(&mut bus, &ctx);
            assert!(bus.peak().is_finite());
            let sum: f64 = bus
                .l
                .iter()
                .map(|s| f64::from(*s) * f64::from(*s))
                .sum();
            out.push((f64::from(bus.peak()), (sum / BLOCK as f64).sqrt()));
        }
        out
    }

    #[test]
    fn zero_is_wide_open() {
        for mut fx in [Filter::new(), Filter::moog()] {
            fx.set_param("value", 0.0).unwrap();
            let db = settled(&mut fx, 1_000.0);
            assert!(db.abs() < 2.0, "open sweep measured {db:.1} dB ({:?})", fx.mode());
        }
    }

    /// The regression this module exists for: the old single biquad flipped from low-pass to
    /// high-pass at `value = 0` and dropped out. Both variants must now stay transparent while the
    /// knob crosses the centre — including when the smoothed parameter jumps across it in one block.
    #[test]
    fn sweeping_through_zero_never_drops_the_signal() {
        for mode in [Mode::Linear, Mode::Moog] {
            let mut fx = match mode {
                Mode::Moog => Filter::moog(),
                Mode::Linear => Filter::new(),
            };
            // A one-block jump right across the crossfade region, mid-band: every block must stay
            // within a hair of the unit sine it was fed.
            for (block, (peak, rms)) in transition(&mut fx, 1_000.0, -0.2, 0.2, 12).iter().enumerate()
            {
                assert!(
                    *rms > 0.9 * 0.707,
                    "{mode:?} block {block}: the crossing dropped the signal to {rms:.3}"
                );
                assert!(*peak > 0.0);
            }
            assert_eq!(fx.blowup_count(), 0);
        }

        // Low-frequency content is where the old topology flip was worst: a 60 Hz tone must survive
        // the crossing too (both branches are transparent there). Peak, not RMS, for the reason in
        // `transition`.
        for mode in [Mode::Linear, Mode::Moog] {
            let mut fx = match mode {
                Mode::Moog => Filter::moog(),
                Mode::Linear => Filter::new(),
            };
            for (block, (peak, _)) in transition(&mut fx, 60.0, -0.05, 0.05, 12).iter().enumerate()
            {
                assert!(
                    *peak > 0.8,
                    "{mode:?} block {block}: 60 Hz fell to {peak:.3} crossing zero"
                );
                assert!(*peak < 1.5, "{mode:?} block {block}: overshoot {peak:.3}");
            }
        }
    }

    #[test]
    fn negative_sweeps_low_pass_and_removes_highs() {
        let mut fx = Filter::new();
        fx.set_param("value", -0.85).unwrap();
        let cutoff = f64::from(fx.cutoff_hz());
        assert!(cutoff < 200.0, "expected a deep sweep, got {cutoff} Hz");
        assert!(
            settled(&mut fx, cutoff * 4.0) < -6.0,
            "above cutoff should be down"
        );

        let mut fx = Filter::new();
        fx.set_param("value", -0.85).unwrap();
        let cutoff = f64::from(fx.cutoff_hz());
        let low = level_at(&mut fx, cutoff * 0.2, 100);
        assert!(low > -6.0, "below cutoff should survive, got {low:.1} dB");
    }

    #[test]
    fn positive_sweeps_high_pass_and_removes_lows() {
        let mut fx = Filter::new();
        fx.set_param("value", 0.6).unwrap();
        let cutoff = f64::from(fx.cutoff_hz());
        assert!(settled(&mut fx, cutoff * 4.0) > -3.0, "above cutoff passes");

        let mut fx = Filter::new();
        fx.set_param("value", 0.6).unwrap();
        let cutoff = f64::from(fx.cutoff_hz());
        assert!(
            settled(&mut fx, cutoff * 0.2) < -6.0,
            "below cutoff is gone"
        );
    }

    /// A resonant peak is the effect, so it must be measurable — and must stay finite.
    #[test]
    fn resonance_sharpens_the_sweep_without_diverging() {
        let mut soft = Filter::new();
        soft.set_param("value", -0.3).unwrap();
        soft.set_param("resonance", 0.0).unwrap();
        let freq = f64::from(soft.cutoff_hz());
        let quiet = settled(&mut soft, freq);

        let mut loud = Filter::new();
        loud.set_param("value", -0.3).unwrap();
        loud.set_param("resonance", 1.0).unwrap();
        let freq = f64::from(loud.cutoff_hz());
        let peak = settled(&mut loud, freq);
        assert!(
            peak - quiet > 3.0,
            "resonance was decorative: {quiet:.1} dB vs {peak:.1} dB"
        );
        assert!(peak > quiet, "a resonant peak must be *louder* at cutoff");
        assert_eq!(loud.blowup_count(), 0);
    }

    #[test]
    fn extreme_settings_never_produce_a_nan() {
        let ctx = FxContext::gridless(SAMPLE_RATE, BLOCK);
        for fx_mode in [Mode::Linear, Mode::Moog] {
            for (value, resonance) in
                [(-1.0, 1.0), (1.0, 1.0), (0.0, 1.0), (-1.0, 0.0), (1.0, 0.0)]
            {
                let mut fx = if fx_mode == Mode::Moog {
                    Filter::moog()
                } else {
                    Filter::new()
                };
                fx.set_param("value", value).unwrap();
                fx.set_param("resonance", resonance).unwrap();
                let mut bus = Bus::new(BLOCK);
                sine(&mut bus, 440.0, 0);
                for _ in 0..40 {
                    fx.process(&mut bus, &ctx);
                    assert!(
                        bus.peak().is_finite(),
                        "{fx_mode:?} {value}/{resonance} blew up"
                    );
                }
                assert_eq!(
                    fx.blowup_count(),
                    0,
                    "{fx_mode:?} {value}/{resonance} diverged and cleared"
                );
            }
        }
    }

    /// The four-pole variant must actually be four poles: two octaves above cutoff it rolls off
    /// twice as fast as the linear sweep.
    #[test]
    fn moog_rolls_off_four_poles() {
        let mut linear = Filter::new();
        linear.set_param("value", -0.5).unwrap();
        let cutoff = f64::from(linear.cutoff_hz());
        let lin = level_at(&mut linear, cutoff * 4.0, 100);

        let mut moog = Filter::moog();
        moog.set_param("value", -0.5).unwrap();
        let four = level_at(&mut moog, cutoff * 4.0, 100);
        assert!(
            four < lin - 10.0,
            "the ladder is not steeper: linear {lin:.1} dB vs moog {four:.1} dB"
        );
    }

    /// A slot can be swapped between the two variants under a mapped knob, so the parameter surface
    /// must be identical — same names, same order, same meaning.
    #[test]
    fn both_modes_share_the_param_surface() {
        let linear = Filter::new();
        let moog = Filter::moog();
        assert_eq!(linear.param_names(), moog.param_names());
        for name in Filter::PARAMS {
            assert_eq!(
                linear.get_param(name),
                moog.get_param(name),
                "{name} has a different default per mode"
            );
        }
        // Same normalised value means the same cutoff in both modes.
        let moog = Filter::moog();
        moog.set_param("value", -0.4).unwrap();
        let linear = Filter::new();
        linear.set_param("value", -0.4).unwrap();
        assert_eq!(moog.cutoff_hz(), linear.cutoff_hz());
    }

    #[test]
    fn reset_clears_history_and_forces_a_redraw() {
        let mut fx = Filter::new();
        fx.set_param("value", -0.5).unwrap();
        fx.reset();
        assert_eq!(fx.value.get(), -0.5, "reset must snap to the target");
        assert_eq!(fx.blowup_count(), 0);
        assert_ne!(fx.cached, (-0.5, fx.resonance.target()));
    }

    #[test]
    fn param_surface_is_strict() {
        let fx = Filter::new();
        assert_eq!(fx.set_param("freq", 1.0), Err(FxError::UnknownParam));
        assert_eq!(fx.set_param("mode", 0.0), Err(FxError::UnknownParam));
        assert_eq!(fx.set_param("mix", 1.0), Err(FxError::UnknownParam));
        assert!(matches!(
            fx.set_param("value", f32::NAN),
            Err(FxError::InvalidValue(_))
        ));
        // The legacy name keeps working for older configs, and clamps like `value`.
        assert!(fx.set_param("cutoff", 0.5).is_ok());
        assert_eq!(fx.get_param("cutoff"), fx.get_param("value"));
        fx.set_param("resonance", 99.0).unwrap();
        assert_eq!(fx.get_param("resonance"), Some(1.0));
        assert_eq!(fx.param_names(), Filter::PARAMS);
    }
}
