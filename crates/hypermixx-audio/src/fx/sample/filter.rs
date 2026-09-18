//! [`Filter`]: the resonant sweep that turns a track into an event.
//!
//! One biquad section, in one of three shapes. Low-pass and high-pass use the cookbook's Q form,
//! which *does* add gain near cutoff when resonating — so the output is level-managed. Band-pass uses
//! the constant-peak-gain form ([`Section::BandPass`]), which never adds gain at all, so a sweep
//! there is safe by construction rather than safe by correction.
//!
//! The mode is a parameter, so the same slot can be swept from low-pass to band-pass without being
//! rebuilt: every mode is one section, so switching is a coefficient redraw.

use std::sync::atomic::{AtomicU8, Ordering};

use super::super::{Fx, FxContext, FxError, Param};
use crate::fx::dsp::{BiquadChain, Section};
use crate::mixer::Bus;

/// Which filter shape [`Filter`] currently is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FilterMode {
    LowPass = 0,
    HighPass = 1,
    BandPass = 2,
}

impl FilterMode {
    pub const ALL: [FilterMode; 3] = [FilterMode::LowPass, FilterMode::HighPass, FilterMode::BandPass];

    pub fn label(self) -> &'static str {
        match self {
            FilterMode::LowPass => "lowpass",
            FilterMode::HighPass => "highpass",
            FilterMode::BandPass => "bandpass",
        }
    }

    pub fn aliases(self) -> &'static [&'static str] {
        match self {
            FilterMode::LowPass => &["lp", "lowpass", "low"],
            FilterMode::HighPass => &["hp", "highpass", "high"],
            FilterMode::BandPass => &["bp", "bandpass", "band"],
        }
    }

    pub fn parse(label: &str) -> Option<Self> {
        let needle = label.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|mode| mode.aliases().contains(&needle.as_str()))
    }

    /// The section this mode wants at `hertz`, with `resonance` already mapped to the right axis:
    /// Q for the slope filters, an octave-relative bandwidth for the band-pass.
    fn section(self, hertz: f64, resonance: f64) -> Section {
        match self {
            FilterMode::LowPass => Section::LowPass { freq: hertz, q: resonance },
            FilterMode::HighPass => Section::HighPass { freq: hertz, q: resonance },
            // Bandwidth scales with centre so the peak keeps the same musical width as it sweeps.
            FilterMode::BandPass => Section::BandPass {
                freq: hertz,
                bandwidth_hz: hertz / resonance.max(0.1),
            },
        }
    }
}

/// The sweep. `cutoff` is normalised (`-1.0 ..= 1.0`) and exponentially mapped over
/// [`MIN_HZ`]..[`MAX_HZ`], so equal fader travel is equal musical travel across four decades.
pub struct Filter {
    cutoff: Param,
    /// Resonance, `0.0 ..= 1.0`. Mapped to Q `0.3 ..= 18` for LP/HP, or to bandwidth for BP.
    resonance: Param,
    /// Output makeup, `0.0 ..= 2.0`. The only gain control: a sweep's own peak is left alone.
    mix: Param,
    /// Stored as a `u8` so [`Fx::set_param`] (&self) can change the mode without a lock.
    mode: AtomicU8,
    chain: BiquadChain,
    /// (cutoff, resonance, mode-label-index) as of the last coefficient redraw.
    cached: (f32, f32, u8),
}

/// Cutoff travel. Below 20 Hz a sweep is a DC servo, not a filter; past ~18 kHz there is nothing
/// musical left to remove.
pub const MIN_HZ: f32 = 20.0;
pub const MAX_HZ: f32 = 18_000.0;

impl Filter {
    /// A wide-open low-pass.
    pub fn new() -> Self {
        Self::with_mode(FilterMode::LowPass)
    }

    pub fn with_mode(mode: FilterMode) -> Self {
        // Start at the mode's own "nothing removed" end: top of the travel for a low-pass, bottom
        // for a high-pass, centre for a band-pass (which always removes something).
        let open = match mode {
            FilterMode::HighPass => -1.0,
            FilterMode::BandPass => 0.0,
            FilterMode::LowPass => 1.0,
        };
        let hertz = normalised_to_hertz(open);
        Self {
            cutoff: Param::new(open, FILTER_TAU),
            resonance: Param::new(0.2, FILTER_TAU),
            mix: Param::instant(1.0),
            mode: AtomicU8::new(mode as u8),
            chain: BiquadChain::from_design(&[mode.section(f64::from(hertz), 0.707)], crate::SAMPLE_RATE),
            cached: (open, 0.2, mode as u8),
        }
    }

    pub const PARAMS: &'static [&'static str] =
        &["cutoff", "resonance", "mix", "mode", "blowups"];

    pub fn mode(&self) -> FilterMode {
        mode_from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Where the cutoff is set, in Hz.
    pub fn cutoff_hz(&self) -> f32 {
        normalised_to_hertz(self.cutoff.target())
    }

    /// The cutoff actually applied this block (mid-slew while the knob moves).
    pub fn applied_cutoff_hz(&self) -> f32 {
        normalised_to_hertz(self.cutoff.get())
    }

    /// Q currently applied (LP/HP) — for band-pass this is the bandwidth divisor.
    pub fn resonance_q(&self) -> f64 {
        resonance_to_q(self.resonance.get())
    }

    /// How many times the filter caught itself diverging and rebuilt. Should stay 0.
    pub fn blowup_count(&self) -> u32 {
        self.chain.blowup_count()
    }

    /// Rejects a bad mode string instead of silently keeping the old one.
    fn set_mode(&self, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        let index = value.round() as i32;
        if !(0..3).contains(&index) {
            return Err(FxError::InvalidValue(value));
        }
        self.mode.store(index as u8, Ordering::Relaxed);
        Ok(())
    }
}

impl Default for Filter {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn mode_from_u8(raw: u8) -> FilterMode {
    match raw {
        1 => FilterMode::HighPass,
        2 => FilterMode::BandPass,
        _ => FilterMode::LowPass,
    }
}

impl FilterMode {
    /// The public path from a `u8` index, for callers that store the mode themselves.
    pub fn from_index(index: u8) -> Self {
        mode_from_u8(index)
    }
}

/// Long enough to hide a stepped knob move, short enough that a hand-timed sweep is still hand-timed
/// and the resonance never zipper-noises under a moving coefficient set.
const FILTER_TAU: f32 = 0.006;

/// Normalised `-1.0 ..= 1.0` → Hz, exponential.
fn normalised_to_hertz(value: f32) -> f32 {
    let t = value.clamp(-1.0, 1.0) * 0.5 + 0.5;
    let hz = f64::from(MIN_HZ) * (f64::from(MAX_HZ) / f64::from(MIN_HZ)).powf(f64::from(t));
    hz.clamp(f64::from(MIN_HZ), f64::from(MAX_HZ)) as f32
}

fn resonance_to_q(value: f32) -> f64 {
    let v = value.clamp(0.0, 1.0) as f64;
    0.3f64 * (18.0f64 / 0.3).powf(v)
}

impl Fx for Filter {
    fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        let (frames, sr) = (ctx.block_frames.min(bus.frames()), ctx.sample_rate);
        let cutoff = self.cutoff.next_block(frames, sr);
        let resonance = self.resonance.next_block(frames, sr);
        let mix = self.mix.next_block(frames, sr);
        let mode = self.mode();
        let hertz = f64::from(normalised_to_hertz(cutoff));
        let q = resonance_to_q(resonance);

        if (cutoff, resonance, mode as u8) != self.cached {
            let design = [mode.section(hertz, q)];
            if !self.chain.apply_design(&design, sr) {
                self.chain = BiquadChain::from_design(&design, sr);
            }
            self.cached = (cutoff, resonance, mode as u8);
        }
        self.chain.recover_if_blowing(|| {
            [mode.section(hertz, q)]
                .iter()
                .map(|section| section.build(sr))
                .collect()
        });

        for (channel, plane) in [(0usize, &mut bus.l), (1, &mut bus.r)] {
            for sample in plane.iter_mut() {
                *sample = self.chain.process_sample(channel, *sample);
            }
        }

        // Deliberately *not* auto-levelled. A resonant low-pass is supposed to get loud near cutoff —
        // that peak is the effect — and a normaliser that divides it out would also erase the sweep's
        // spectral character, making the resonance unmeasurable as well as inaudible. Gain staging is
        // what `mix` is for, and the master limiter is the safety net.
        if mix != 1.0 {
            bus.scale(mix);
        }
    }

    fn reset(&mut self) {
        self.cutoff.snap(self.cutoff.target());
        self.resonance.snap(self.resonance.target());
        self.chain.clear();
        self.cached = (f32::NAN, f32::NAN, self.mode.load(Ordering::Relaxed));
    }

    fn set_param(&self, name: &str, value: f32) -> Result<(), FxError> {
        if name != "mode" && !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        match name {
            "cutoff" => {
                self.cutoff.set(value.clamp(-1.0, 1.0));
                Ok(())
            }
            "resonance" => {
                self.resonance.set(value.clamp(0.0, 1.0));
                Ok(())
            }
            "mix" => {
                self.mix.set(value.clamp(0.0, 2.0));
                Ok(())
            }
            "mode" => self.set_mode(value),
            // A counter, not a knob: accepted so a UI can write it blindly, then ignored.
            "blowups" => Ok(()),
            _ => Err(FxError::UnknownParam),
        }
    }

    fn get_param(&self, name: &str) -> Option<f32> {
        match name {
            "cutoff" => Some(self.cutoff.target()),
            "resonance" => Some(self.resonance.target()),
            "mix" => Some(self.mix.target()),
            "mode" => Some(self.mode() as u8 as f32),
            "blowups" => Some(self.blowup_count() as f32),
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
            .field("mode", &self.mode().label())
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
    fn wide_open_is_roughly_transparent() {
        let mut lp = Filter::new();
        lp.set_param("cutoff", 1.0).unwrap();
        let db = settled(&mut lp, 1_000.0);
        assert!(db.abs() < 2.0, "open low-pass measured {db:.1} dB");
    }

    #[test]
    fn sweeping_down_removes_highs_and_keeps_bass() {
        let mut fx = Filter::new();
        fx.set_param("cutoff", -0.85).unwrap();
        let cutoff = f64::from(fx.cutoff_hz());
        assert!(cutoff < 200.0, "expected a deep sweep, got {cutoff} Hz");
        assert!(
            settled(&mut fx, cutoff * 4.0) < -6.0,
            "above cutoff should be down"
        );

        let mut fx = Filter::new();
        fx.set_param("cutoff", -0.85).unwrap();
        let cutoff = f64::from(fx.cutoff_hz());
        let low = level_at(&mut fx, cutoff * 0.2, 100);
        assert!(low > -6.0, "below cutoff should survive, got {low:.1} dB");
    }

    #[test]
    fn high_pass_is_the_mirror() {
        let mut fx = Filter::with_mode(FilterMode::HighPass);
        fx.set_param("cutoff", 0.6).unwrap();
        let cutoff = f64::from(fx.cutoff_hz());
        assert!(settled(&mut fx, cutoff * 4.0) > -3.0, "above cutoff passes");

        let mut fx = Filter::with_mode(FilterMode::HighPass);
        fx.set_param("cutoff", 0.6).unwrap();
        let cutoff = f64::from(fx.cutoff_hz());
        assert!(
            settled(&mut fx, cutoff * 0.2) < -6.0,
            "below cutoff is gone"
        );
    }

    /// A resonant peak is the effect, so it must be measurable — and must stay finite.
    #[test]
    fn resonance_sharpens_the_sweep_without_diverging() {
        // For a low-pass the audible signature of resonance is a peak at cutoff; for a band-pass it
        // is a narrower passband. Both must stay finite and bounded.
        let mut soft = Filter::new();
        soft.set_param("cutoff", -0.3).unwrap();
        soft.set_param("resonance", 0.0).unwrap();
        let freq = f64::from(soft.cutoff_hz());
        let quiet = settled(&mut soft, freq);

        let mut loud = Filter::new();
        loud.set_param("cutoff", -0.3).unwrap();
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
    fn mode_is_hot_swappable_on_one_section() {
        let mut fx = Filter::new();
        for (index, expect) in [(0u8, FilterMode::LowPass), (1, FilterMode::HighPass), (2, FilterMode::BandPass)] {
            fx.set_param("mode", index as f32).unwrap();
            assert_eq!(fx.mode(), expect);
            assert_eq!(fx.chain.len(), 1, "every mode is one section");
            let mut bus = Bus::new(BLOCK);
            sine(&mut bus, 1_000.0, 0);
            fx.process(&mut bus, &FxContext::gridless(SAMPLE_RATE, BLOCK));
            assert!(bus.peak().is_finite(), "{expect:?} blew up");
        }
        assert!(fx.set_param("mode", 7.0).is_err(), "an invented mode must be refused");
    }

    #[test]
    fn bandpass_peaks_at_cutoff_and_rejects_both_sides() {
        let mut fx = Filter::with_mode(FilterMode::BandPass);
        fx.set_param("cutoff", 0.0).unwrap();
        let centre = f64::from(fx.cutoff_hz());
        let at = settled(&mut fx, centre);
        let below = settled(&mut fx, centre / 8.0);
        let above = settled(&mut fx, (centre * 8.0).min(20_000.0));
        assert!(at > below + 6.0, "bp passed below: {at:.1} vs {below:.1}");
        assert!(at > above + 6.0, "bp passed above: {at:.1} vs {above:.1}");
        assert_eq!(fx.blowup_count(), 0);
    }

    #[test]
    fn extreme_settings_never_produce_a_nan() {
        let ctx = FxContext::gridless(SAMPLE_RATE, BLOCK);
        for (cutoff, resonance) in [(-1.0, 1.0), (1.0, 1.0), (0.0, 1.0), (-1.0, 0.0), (1.0, 0.0)] {
            for mode in FilterMode::ALL {
                let mut fx = Filter::with_mode(mode);
                fx.set_param("cutoff", cutoff).unwrap();
                fx.set_param("resonance", resonance).unwrap();
                let mut bus = Bus::new(BLOCK);
                sine(&mut bus, 440.0, 0);
                for _ in 0..40 {
                    fx.process(&mut bus, &ctx);
                    assert!(bus.peak().is_finite(), "{mode:?} {cutoff}/{resonance} blew up");
                }
                assert_eq!(fx.blowup_count(), 0, "{mode:?} diverged and rebuilt");
            }
        }
    }

    #[test]
    fn mix_of_zero_is_silence() {
        let mut muted = Filter::new();
        muted.set_param("mix", 0.0).unwrap();
        let ctx = FxContext::gridless(SAMPLE_RATE, BLOCK);
        let mut bus = Bus::new(BLOCK);
        sine(&mut bus, 1_000.0, 0);
        for _ in 0..50 {
            muted.process(&mut bus, &ctx);
        }
        assert!(bus.peak() < 1e-4, "mix 0 must be silence, got {}", bus.peak());
    }

    #[test]
    fn reset_clears_history_and_forces_a_redraw() {
        let mut fx = Filter::new();
        fx.set_param("cutoff", -0.5).unwrap();
        fx.reset();
        assert_eq!(fx.cutoff.get(), -0.5, "reset must snap to the target");
        assert_eq!(fx.blowup_count(), 0);
        assert_ne!(fx.cached, (-0.5, fx.resonance.target(), fx.mode() as u8));
    }

    #[test]
    fn param_surface_is_strict() {
        let fx = Filter::new();
        assert_eq!(fx.set_param("freq", 1.0), Err(FxError::UnknownParam));
        assert!(matches!(
            fx.set_param("cutoff", f32::NAN),
            Err(FxError::InvalidValue(_))
        ));
        fx.set_param("resonance", 99.0).unwrap();
        assert_eq!(fx.get_param("resonance"), Some(1.0));
        assert_eq!(fx.param_names(), Filter::PARAMS);
        assert_eq!(fx.mode(), FilterMode::LowPass);
        assert_eq!(FilterMode::parse("BP"), Some(FilterMode::BandPass));
        assert_eq!(FilterMode::parse("nope"), None);
        assert_eq!(FilterMode::from_index(9), FilterMode::LowPass, "unknown index degrades safely");
    }

    #[test]
    fn mix_scales_the_output_and_is_the_only_gain_stage() {
        // Both filters run the same number of blocks on fresh buses; re-feeding one bus would
        // compound the gain once per block instead of applying it once.
        let ctx = FxContext::gridless(SAMPLE_RATE, BLOCK);
        let render = |mix: f32| {
            let mut fx = Filter::new();
            fx.set_param("mix", mix).unwrap();
            let mut peak = 0.0f32;
            for b in 0..120 {
                let mut bus = Bus::new(BLOCK);
                sine(&mut bus, 1_000.0, b * BLOCK);
                fx.process(&mut bus, &ctx);
                if b > 60 {
                    peak = peak.max(bus.peak());
                }
            }
            peak
        };
        let (unity, half) = (render(1.0), render(0.5));
        assert!(unity > 0.5, "an open low-pass should barely touch 1kHz, got {unity}");
        assert!(
            (half - unity * 0.5).abs() < 1e-3,
            "mix 0.5 gave {half}, expected {}",
            unity * 0.5
        );
        assert!(render(0.0) < 1e-4, "mix 0 must be silence");
    }
}
