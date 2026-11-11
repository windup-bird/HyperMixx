//! [`Reverb`]: a Freeverb-style room — the hardware-free reverb every DJ effect bank ships.
//!
//! The topology is Jez Arden's **Freeverb** (public domain, and the reference implementation behind
//! countless DAW reverbs): eight parallel damped comb filters per channel, then four series all-pass
//! filters. The left and right banks use the same tunings with the right side shifted by
//! [`STEREO_SPREAD`] samples, which decorrelates the two channels and is what makes the tail feel
//! wide without a stereo widener.
//!
//! Three deliberate details:
//!
//! * **Comb lengths scale with the engine rate.** The published tunings are for 44.1 kHz; a 48 kHz
//!   engine that reused them would detune the whole tail by 9%. They are multiplied by the rate
//!   ratio at [`prepare`] time.
//! * **The input is a mono sum.** A reverb that keeps a hard-panned source hard-panned is a
//!   delay, not a room; the comb banks do the stereo work.
//! * **Freeverb's fixed gains are kept**, so [`size`] and [`damping`] mean what they mean in every
//!   other Freeverb. [`mix`] is then a crossfade onto that wet signal, with [`WET_GAIN`] bringing
//!   the wet path up to a comparable level.

use super::super::{Fx, FxContext, FxError, Param};
use crate::mixer::Bus;

/// Comb delays in samples at 44.1 kHz, from Freeverb's own tuning table.
const COMB_TUNING: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
/// All-pass delays in samples at 44.1 kHz.
const ALLPASS_TUNING: [usize; 4] = [556, 441, 341, 225];
/// Samples the right bank is shifted by, at 44.1 kHz.
const STEREO_SPREAD: usize = 23;

/// Freeverb's fixed input gain. The comb bank's feedback supplies the rest of the level.
const INPUT_GAIN: f32 = 0.015;
/// Freeverb's wet gain (it ships as wet 1/3 and scalewet 3), folded into [`mix`].
const WET_GAIN: f32 = 3.0;
/// Freeverb's room mapping: feedback = offset + scale * size.
const ROOM_OFFSET: f32 = 0.7;
const ROOM_SCALE: f32 = 0.28;
/// Freeverb's damping scale: the comb's one-pole weight is at most this.
const DAMP_SCALE: f32 = 0.4;

/// Smoothing for the four knobs. A reverb whose size or mix steps at a block boundary is a click.
const PARAM_TAU: f32 = 0.05;

/// One damped comb filter: the tail's body.
#[derive(Clone, Debug, PartialEq)]
struct Comb {
    buffer: Vec<f32>,
    index: usize,
    store: f32,
}

impl Comb {
    fn with_frames(frames: usize) -> Self {
        Self {
            buffer: vec![0.0; frames.max(1)],
            index: 0,
            store: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, input: f32, feedback: f32, damp: f32) -> f32 {
        let output = self.buffer[self.index];
        self.store = output * (1.0 - damp) + self.store * damp;
        self.buffer[self.index] = input + self.store * feedback;
        self.index += 1;
        if self.index >= self.buffer.len() {
            self.index = 0;
        }
        output
    }

    fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.index = 0;
        self.store = 0.0;
    }
}

/// One Schroeder all-pass: smears the comb output into a dense tail.
#[derive(Clone, Debug, PartialEq)]
struct Allpass {
    buffer: Vec<f32>,
    index: usize,
}

impl Allpass {
    fn with_frames(frames: usize) -> Self {
        Self {
            buffer: vec![0.0; frames.max(1)],
            index: 0,
        }
    }

    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        let buffered = self.buffer[self.index];
        let output = -input + buffered;
        self.buffer[self.index] = input + buffered * 0.5;
        self.index += 1;
        if self.index >= self.buffer.len() {
            self.index = 0;
        }
        output
    }

    fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.index = 0;
    }
}

/// A Freeverb-style stereo reverb.
pub struct Reverb {
    mix: Param,
    size: Param,
    damping: Param,
    width: Param,
    combs_left: Vec<Comb>,
    combs_right: Vec<Comb>,
    allpass_left: Vec<Allpass>,
    allpass_right: Vec<Allpass>,
    /// The rate the buffers were sized at; [`process`] prepares once if the caller did not.
    prepared_rate: u32,
}

impl Reverb {
    /// A medium room at 30% wet.
    pub fn new() -> Self {
        let mut reverb = Self {
            mix: Param::new(0.30, PARAM_TAU),
            size: Param::new(0.50, PARAM_TAU),
            damping: Param::new(0.50, PARAM_TAU),
            width: Param::new(1.0, PARAM_TAU),
            combs_left: Vec::new(),
            combs_right: Vec::new(),
            allpass_left: Vec::new(),
            allpass_right: Vec::new(),
            prepared_rate: 0,
        };
        reverb.prepare(crate::SAMPLE_RATE);
        reverb
    }

    /// Parameter names in performance order.
    pub const PARAMS: &'static [&'static str] = &["mix", "size", "damping", "width"];

    /// Samples a 44.1 kHz tuning becomes at [`rate`].
    fn scaled(length: usize, rate: u32) -> usize {
        ((length as f64 * f64::from(rate.max(1)) / 44_100.0).round() as usize).max(1)
    }

    fn set_unit(param: &Param, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        param.set(value.clamp(0.0, 1.0));
        Ok(())
    }
}

impl Default for Reverb {
    fn default() -> Self {
        Self::new()
    }
}

impl Fx for Reverb {
    fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        let frames = bus.frames();
        if frames == 0 {
            return;
        }
        let rate = ctx.sample_rate.max(1);
        if self.prepared_rate != rate {
            self.prepare(rate);
        }

        // Every one of these four ends up as a gain on the samples (dry/wet, comb feedback, comb
        // damping, stereo width): read `at(i)` so a fast move does not step at the boundary. The
        // comb/all-pass state is continuous, so changing its gains per sample is safe. Derived laws
        // are linear, so they can be mapped once with `Ramp::map` instead of recomputed per sample.
        let mix = self.mix.ramp(frames, rate).clamped(0.0, 1.0);
        let feedback = self
            .size
            .ramp(frames, rate)
            .clamped(0.0, 1.0)
            .map(|size| ROOM_OFFSET + ROOM_SCALE * size);
        let damp = self
            .damping
            .ramp(frames, rate)
            .clamped(0.0, 1.0)
            .map(|damping| DAMP_SCALE * damping);
        let width = self.width.ramp(frames, rate).clamped(0.0, 1.0);

        for i in 0..frames {
            let mix = mix.at(i);
            let feedback = feedback.at(i);
            let damp = damp.at(i);
            let width = width.at(i);
            let wet = mix * WET_GAIN;
            let dry = 1.0 - mix;
            let wet1 = wet * (width * 0.5 + 0.5);
            let wet2 = wet * ((1.0 - width) * 0.5);
            let input = (bus.l[i] + bus.r[i]) * INPUT_GAIN;
            let mut left = 0.0;
            for comb in &mut self.combs_left {
                left += comb.process(input, feedback, damp);
            }
            let mut right = 0.0;
            for comb in &mut self.combs_right {
                right += comb.process(input, feedback, damp);
            }
            for allpass in &mut self.allpass_left {
                left = allpass.process(left);
            }
            for allpass in &mut self.allpass_right {
                right = allpass.process(right);
            }
            let wet_left = left * wet1 + right * wet2;
            let wet_right = right * wet1 + left * wet2;
            bus.l[i] = bus.l[i] * dry + wet_left;
            bus.r[i] = bus.r[i] * dry + wet_right;
        }
    }

    fn prepare(&mut self, sample_rate: u32) {
        let rate = sample_rate.max(1);
        self.combs_left = COMB_TUNING
            .iter()
            .map(|&length| Comb::with_frames(Self::scaled(length, rate)))
            .collect();
        self.combs_right = COMB_TUNING
            .iter()
            .map(|&length| Comb::with_frames(Self::scaled(length + STEREO_SPREAD, rate)))
            .collect();
        self.allpass_left = ALLPASS_TUNING
            .iter()
            .map(|&length| Allpass::with_frames(Self::scaled(length, rate)))
            .collect();
        self.allpass_right = ALLPASS_TUNING
            .iter()
            .map(|&length| Allpass::with_frames(Self::scaled(length + STEREO_SPREAD, rate)))
            .collect();
        self.prepared_rate = rate;
    }

    fn reset(&mut self) {
        self.mix.snap(self.mix.target());
        self.size.snap(self.size.target());
        self.damping.snap(self.damping.target());
        self.width.snap(self.width.target());
        for comb in self
            .combs_left
            .iter_mut()
            .chain(self.combs_right.iter_mut())
        {
            comb.clear();
        }
        for allpass in self
            .allpass_left
            .iter_mut()
            .chain(self.allpass_right.iter_mut())
        {
            allpass.clear();
        }
    }

    fn set_param(&self, name: &str, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        match name {
            "mix" => Self::set_unit(&self.mix, value),
            "size" => Self::set_unit(&self.size, value),
            "damping" => Self::set_unit(&self.damping, value),
            "width" => Self::set_unit(&self.width, value),
            _ => Err(FxError::UnknownParam),
        }
    }

    fn get_param(&self, name: &str) -> Option<f32> {
        match name {
            "mix" => Some(self.mix.target()),
            "size" => Some(self.size.target()),
            "damping" => Some(self.damping.target()),
            "width" => Some(self.width.target()),
            _ => None,
        }
    }

    fn param_names(&self) -> &'static [&'static str] {
        Self::PARAMS
    }
}

impl std::fmt::Debug for Reverb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reverb")
            .field("size", &self.size.target())
            .field("damping", &self.damping.target())
            .field("mix", &self.mix.target())
            .field("prepared_rate", &self.prepared_rate)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const BLOCK: usize = 256;

    fn ctx() -> FxContext {
        FxContext::gridless(RATE, BLOCK)
    }

    fn tuned(overrides: &[(&str, f32)]) -> Reverb {
        let mut fx = Reverb::new();
        fx.prepare(RATE);
        for (name, value) in overrides {
            fx.set_param(name, *value).unwrap();
        }
        fx.reset();
        fx
    }

    /// Feeds an impulse (both channels) for [`seconds`], returning the output.
    fn impulse_response(fx: &mut Reverb, seconds: f32) -> Vec<[f32; 2]> {
        let total = (RATE as f32 * seconds) as usize;
        let mut output = Vec::with_capacity(total);
        let blocks = total.div_ceil(BLOCK);
        for block in 0..blocks {
            let mut bus = Bus::stereo(BLOCK);
            if block == 0 {
                bus.l[0] = 1.0;
                bus.r[0] = 1.0;
            }
            fx.process(&mut bus, &ctx());
            for i in 0..BLOCK {
                if output.len() < total {
                    output.push([bus.l[i], bus.r[i]]);
                }
            }
        }
        output
    }

    fn energy(samples: &[[f32; 2]], from: f32, to: f32) -> f32 {
        let a = ((RATE as f32 * from) as usize).min(samples.len());
        let b = ((RATE as f32 * to) as usize).min(samples.len());
        if a >= b {
            return 0.0;
        }
        samples[a..b].iter().map(|[l, r]| l * l + r * r).sum()
    }

    #[test]
    fn a_dry_mix_is_transparent() {
        let mut fx = tuned(&[("mix", 0.0)]);
        let mut bus = Bus::stereo(BLOCK);
        for i in 0..BLOCK {
            let s = ((i as f32) * 0.03).sin();
            bus.l[i] = s;
            bus.r[i] = -s;
        }
        let original = bus.clone();
        fx.process(&mut bus, &ctx());
        assert_eq!(bus, original, "mix=0 must let the signal through untouched");
    }

    #[test]
    fn an_impulse_rings_then_decays() {
        let mut fx = tuned(&[("mix", 1.0), ("size", 0.5), ("damping", 0.5)]);
        let response = impulse_response(&mut fx, 3.0);
        assert!(response.iter().all(|[l, r]| l.is_finite() && r.is_finite()));
        let early = energy(&response, 0.0, 0.1);
        let late = energy(&response, 2.8, 3.0);
        assert!(early > 1e-6, "the impulse produced no tail at all");
        assert!(
            late < early * 0.01,
            "the reverb never decayed: early {early}, late {late}"
        );
    }

    #[test]
    fn a_bigger_room_rings_longer() {
        let mut small = tuned(&[("mix", 1.0), ("size", 0.2)]);
        let mut large = tuned(&[("mix", 1.0), ("size", 0.9)]);
        let small_tail = energy(&impulse_response(&mut small, 2.0), 0.8, 1.2);
        let large_tail = energy(&impulse_response(&mut large, 2.0), 0.8, 1.2);
        assert!(
            large_tail > small_tail * 2.0,
            "size 0.9 ({large_tail}) did not outlast size 0.2 ({small_tail})"
        );
    }

    #[test]
    fn width_zero_is_mono() {
        let mut fx = tuned(&[("mix", 1.0), ("width", 0.0)]);
        let response = impulse_response(&mut fx, 1.0);
        for [l, r] in response {
            assert!((l - r).abs() < 1e-6, "width=0 must fold the banks together");
        }
    }

    #[test]
    fn reset_clears_the_tail() {
        let mut fx = tuned(&[("mix", 1.0)]);
        let mut impulse = Bus::stereo(BLOCK);
        impulse.l[0] = 1.0;
        impulse.r[0] = 1.0;
        fx.process(&mut impulse, &ctx());
        fx.reset();
        let mut silent = Bus::stereo(BLOCK);
        fx.process(&mut silent, &ctx());
        assert!(silent.is_silent(), "a reset left the tail ringing");
    }

    #[test]
    fn extreme_settings_stay_finite() {
        for (size, damping, width, mix) in [
            (1.0, 0.0, 1.0, 1.0),
            (1.0, 1.0, 0.0, 1.0),
            (0.0, 0.0, 0.5, 1.0),
            (0.7, 0.3, 0.2, 0.8),
        ] {
            let mut fx = tuned(&[
                ("size", size),
                ("damping", damping),
                ("width", width),
                ("mix", mix),
            ]);
            let response = impulse_response(&mut fx, 1.0);
            assert!(
                response.iter().all(|[l, r]| l.is_finite() && r.is_finite()),
                "{size}/{damping}/{width}/{mix} produced a non-finite sample"
            );
        }
    }

    #[test]
    fn a_buffer_follows_the_engine_rate() {
        let mut fx = Reverb::new();
        fx.prepare(96_000);
        assert_eq!(fx.prepared_rate, 96_000);
        assert!(
            fx.combs_left[0].buffer.len() > Reverb::scaled(COMB_TUNING[0], 48_000),
            "96k buffers must be longer than 48k ones"
        );
        let mut bus = Bus::stereo(BLOCK);
        bus.l[0] = 1.0;
        let mut ctx = ctx();
        ctx.sample_rate = 96_000;
        fx.process(&mut bus, &ctx);
        assert!(bus.l.iter().all(|s| s.is_finite()));
        assert_eq!(
            fx.prepared_rate, 96_000,
            "a matching rate must not re-prepare"
        );
    }

    #[test]
    fn param_surface_is_strict() {
        let fx = Reverb::new();
        assert_eq!(fx.set_param("room", 0.5), Err(FxError::UnknownParam));
        assert!(matches!(
            fx.set_param("size", f32::NAN),
            Err(FxError::InvalidValue(_))
        ));
        fx.set_param("size", 9.0).unwrap();
        assert_eq!(fx.get_param("size"), Some(1.0));
        fx.set_param("damping", -1.0).unwrap();
        assert_eq!(fx.get_param("damping"), Some(0.0));
        assert_eq!(fx.param_names(), Reverb::PARAMS);
    }
}
