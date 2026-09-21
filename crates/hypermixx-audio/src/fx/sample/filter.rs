//! [`Filter`]: the resonant bipolar sweep that turns a track into an event.
//!
//! One biquad section driven by a single normalised `value` (`-1.0 ..= 1.0`):
//!
//! - `value < 0` — low-pass, `-1` is 20Hz (closed), `0` is wide open
//! - `value > 0` — high-pass, `0` is wide open, `+1` is 18kHz (closed)
//! - `value == 0` — fully open, so one knob sweeps low-pass through neutral into high-pass
//!
//! `resonance` is the peak gain: it maps to Q `0.3 ..= 18` on the active section. There is no
//! auto-levelling — a resonant low-pass is *supposed* to get loud near cutoff — the master limiter
//! is the safety net.

use super::super::{Fx, FxContext, FxError, Param};
use crate::fx::dsp::{BiquadChain, Section};
use crate::mixer::Bus;

/// The sweep. `value` is normalised (`-1.0 ..= 1.0`); the sign picks low-pass or high-pass and the
/// magnitude opens or closes it.
pub struct Filter {
    value: Param,
    /// Peak gain, `0.0 ..= 1.0` → Q `0.3 ..= 18`.
    resonance: Param,
    chain: BiquadChain,
    /// (value, resonance) as of the last coefficient redraw.
    cached: (f32, f32),
}

/// Cutoff travel. Below 20 Hz a sweep is a DC servo, not a filter; past ~18 kHz there is nothing
/// musical left to remove.
pub const MIN_HZ: f32 = 20.0;
pub const MAX_HZ: f32 = 18_000.0;

/// Long enough to hide a stepped knob move, short enough that a hand-timed sweep is still hand-timed
/// and the resonance never zipper-noises under a moving coefficient set.
const FILTER_TAU: f32 = 0.006;

impl Filter {
    /// A wide-open sweep (`value = 0`).
    pub fn new() -> Self {
        Self {
            value: Param::new(0.0, FILTER_TAU),
            resonance: Param::new(0.2, FILTER_TAU),
            chain: BiquadChain::from_design(&[section(0.0, 0.707)], crate::SAMPLE_RATE),
            cached: (0.0, 0.2),
        }
    }

    pub const PARAMS: &'static [&'static str] = &["value", "resonance"];

    /// Where the cutoff is set, in Hz (which side of the sweep depends on the sign of `value`).
    pub fn cutoff_hz(&self) -> f32 {
        hertz_of(f64::from(self.value.target())) as f32
    }

    /// The cutoff actually applied this block (mid-slew while the knob moves).
    pub fn applied_cutoff_hz(&self) -> f32 {
        hertz_of(f64::from(self.value.get())) as f32
    }

    /// Q currently applied.
    pub fn resonance_q(&self) -> f64 {
        resonance_to_q(self.resonance.get())
    }

    /// How many times the filter caught itself diverging and rebuilt. Should stay 0.
    pub fn blowup_count(&self) -> u32 {
        self.chain.blowup_count()
    }
}

impl Default for Filter {
    fn default() -> Self {
        Self::new()
    }
}

/// The section for a normalised `value`: low-pass below zero, high-pass above, open at zero.
fn section(value: f64, q: f64) -> Section {
    if value <= 0.0 {
        Section::LowPass {
            freq: hertz_of(value),
            q,
        }
    } else {
        Section::HighPass {
            freq: hertz_of(value),
            q,
        }
    }
}

/// The 20Hz..18kHz crossover for a normalised `value`, continuous through zero.
fn hertz_of(value: f64) -> f64 {
    let exponent = if value <= 0.0 { value + 1.0 } else { value };
    bipolar_to_hertz(exponent)
}

/// `0.0 ..= 1.0` → 20Hz..18kHz, exponential.
fn bipolar_to_hertz(exponent: f64) -> f64 {
    let t = exponent.clamp(0.0, 1.0);
    let hz = f64::from(MIN_HZ) * (f64::from(MAX_HZ) / f64::from(MIN_HZ)).powf(t);
    hz.clamp(f64::from(MIN_HZ), f64::from(MAX_HZ))
}

fn resonance_to_q(value: f32) -> f64 {
    let v = value.clamp(0.0, 1.0) as f64;
    0.3f64 * (18.0f64 / 0.3).powf(v)
}

impl Fx for Filter {
    fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        let (frames, sr) = (ctx.block_frames.min(bus.frames()), ctx.sample_rate);
        let value = self.value.next_block(frames, sr);
        let resonance = self.resonance.next_block(frames, sr);
        let q = resonance_to_q(resonance);
        let normalised = f64::from(value);

        if (value, resonance) != self.cached {
            let design = [section(normalised, q)];
            if !self.chain.apply_design(&design, sr) {
                self.chain = BiquadChain::from_design(&design, sr);
            }
            self.cached = (value, resonance);
        }
        self.chain.recover_if_blowing(|| {
            [section(normalised, q)]
                .iter()
                .map(|section| section.build(sr))
                .collect()
        });

        for (channel, plane) in [(0usize, &mut bus.l), (1, &mut bus.r)] {
            for sample in plane.iter_mut() {
                *sample = self.chain.process_sample(channel, *sample);
            }
        }
    }

    fn reset(&mut self) {
        self.value.snap(self.value.target());
        self.resonance.snap(self.resonance.target());
        self.chain.clear();
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

    #[test]
    fn zero_is_wide_open() {
        let mut fx = Filter::new();
        fx.set_param("value", 0.0).unwrap();
        let db = settled(&mut fx, 1_000.0);
        assert!(db.abs() < 2.0, "open sweep measured {db:.1} dB");
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
        for (value, resonance) in [(-1.0, 1.0), (1.0, 1.0), (0.0, 1.0), (-1.0, 0.0), (1.0, 0.0)] {
            let mut fx = Filter::new();
            fx.set_param("value", value).unwrap();
            fx.set_param("resonance", resonance).unwrap();
            let mut bus = Bus::new(BLOCK);
            sine(&mut bus, 440.0, 0);
            for _ in 0..40 {
                fx.process(&mut bus, &ctx);
                assert!(bus.peak().is_finite(), "{value}/{resonance} blew up");
            }
            assert_eq!(fx.blowup_count(), 0, "{value}/{resonance} diverged and rebuilt");
        }
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
