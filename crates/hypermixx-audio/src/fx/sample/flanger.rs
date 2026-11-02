//! [`Flanger`]: the short modulated delay that turns a sustained note into a jet sweep.
//!
//! A bucket-brigade flanger is a delay of a few milliseconds swept by an LFO, with part of the
//! output fed back. The dry and delayed copies comb-filter each other, and because the delay moves,
//! the notches move with it. This implementation keeps that shape:
//!
//! * **The LFO is in beats, not hertz.** [`rate`] is cycles per beat, so a sweep stays locked to the
//!   track when the tempo changes; without a grid it falls back to a free 120 BPM.
//! * **The feedback is bipolar.** Positive feedback gives the bright, hollow jet; negative feedback
//!   gives the thinner "through-zero" colour. Both are stable because the magnitude is clamped
//!   below one.
//! * **The channels are 90 degrees apart.** A mono flanger collapses; quadrature keeps the sweep
//!   moving across the stereo image.

use super::super::delay::{DelayLine, SineLfo};
use super::super::{Fx, FxContext, FxError, Param};
use crate::mixer::Bus;

/// Slowest and fastest LFO, in cycles per beat: one sweep every 16 beats to four sweeps per beat.
pub const MIN_RATE: f32 = 1.0 / 16.0;
pub const MAX_RATE: f32 = 4.0;
/// Feedback magnitude past this self-oscillates; the clamp is the effect's own stability guarantee.
pub const MAX_FEEDBACK: f32 = 0.95;

/// Centre delay and the swing at full [`depth`], in seconds. 0.2..4.8 ms is the range a BBD flanger
/// covers, and the swing is what makes the comb audible.
const CENTRE_SECONDS: f32 = 0.0025;
const SWING_SECONDS: f32 = 0.0023;
/// Buffer headroom: the longest delay plus the read-ahead the interpolation needs.
const BUFFER_SECONDS: f32 = 0.010;

const PARAM_TAU: f32 = 0.03;
/// The right channel's LFO offset, in cycles (90 degrees).
const STEREO_OFFSET: f64 = 0.25;

/// A stereo flanger.
pub struct Flanger {
    mix: Param,
    depth: Param,
    rate: Param,
    feedback: Param,
    left: DelayLine,
    right: DelayLine,
    lfo: SineLfo,
    /// The rate the buffers were sized at; [`process`] prepares once if the caller did not.
    prepared_rate: u32,
    /// Delay applied to the last left-channel sample, in frames (for meters and tests).
    delay_frames: f32,
}

impl Flanger {
    /// A 2.5 ms comb with a quarter-beat sweep.
    pub fn new() -> Self {
        let mut flanger = Self {
            mix: Param::new(0.5, PARAM_TAU),
            depth: Param::new(0.5, PARAM_TAU),
            rate: Param::new(0.25, PARAM_TAU),
            feedback: Param::new(0.3, PARAM_TAU),
            left: DelayLine::default(),
            right: DelayLine::default(),
            lfo: SineLfo::new(),
            prepared_rate: 0,
            delay_frames: 0.0,
        };
        flanger.prepare(crate::SAMPLE_RATE);
        flanger
    }

    /// Parameter names in performance order.
    pub const PARAMS: &'static [&'static str] = &["mix", "depth", "rate", "feedback"];

    /// The LFO's current phase, in cycles.
    pub fn lfo_phase(&self) -> f64 {
        self.lfo.phase()
    }

    /// Delay applied to the last left sample, in frames.
    pub fn delay_frames(&self) -> f32 {
        self.delay_frames
    }

    fn set_unit(param: &Param, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        param.set(value.clamp(0.0, 1.0));
        Ok(())
    }
}

impl Default for Flanger {
    fn default() -> Self {
        Self::new()
    }
}

impl Fx for Flanger {
    fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        let frames = bus.frames();
        if frames == 0 {
            return;
        }
        let rate = ctx.sample_rate.max(1);
        if self.prepared_rate != rate {
            self.prepare(rate);
        }

        let mix = self.mix.next_block(frames, rate).clamp(0.0, 1.0);
        let depth = self.depth.next_block(frames, rate).clamp(0.0, 1.0);
        let lfo_rate = self.rate.next_block(frames, rate).clamp(MIN_RATE, MAX_RATE);
        let feedback = self
            .feedback
            .next_block(frames, rate)
            .clamp(-MAX_FEEDBACK, MAX_FEEDBACK);

        let rate_hz = f64::from(lfo_rate) * ctx.beats_per_second();
        let increment = rate_hz / f64::from(rate);
        let centre = f64::from(rate) * f64::from(CENTRE_SECONDS);
        let swing = f64::from(rate) * f64::from(SWING_SECONDS) * f64::from(depth);
        let max_delay = self.left.max_delay();
        let dry = 1.0 - mix;
        let mut last_delay = 0.0f32;

        for i in 0..frames {
            let left_lfo = f64::from(self.lfo.value());
            let right_lfo = f64::from(self.lfo.value_at(STEREO_OFFSET));
            self.lfo.advance(increment);
            let delay_left = ((centre + swing * left_lfo) as f32).clamp(1.0, max_delay);
            let delay_right = ((centre + swing * right_lfo) as f32).clamp(1.0, max_delay);
            last_delay = delay_left;

            let wet_left = self.left.read(delay_left);
            let wet_right = self.right.read(delay_right);
            let input_left = bus.l[i];
            let input_right = bus.r[i];
            self.left.push(input_left + wet_left * feedback);
            self.right.push(input_right + wet_right * feedback);
            bus.l[i] = input_left * dry + wet_left * mix;
            bus.r[i] = input_right * dry + wet_right * mix;
        }
        self.delay_frames = last_delay;
    }

    fn prepare(&mut self, sample_rate: u32) {
        let rate = sample_rate.max(1);
        let frames = (BUFFER_SECONDS * rate as f32).ceil() as usize + 4;
        self.left.resize(frames);
        self.right.resize(frames);
        self.prepared_rate = rate;
        self.delay_frames = 0.0;
    }

    fn reset(&mut self) {
        self.mix.snap(self.mix.target());
        self.depth.snap(self.depth.target());
        self.rate.snap(self.rate.target());
        self.feedback.snap(self.feedback.target());
        self.left.clear();
        self.right.clear();
        self.lfo.reset();
        self.delay_frames = 0.0;
    }

    fn set_param(&self, name: &str, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        match name {
            "mix" => Self::set_unit(&self.mix, value),
            "depth" => Self::set_unit(&self.depth, value),
            "rate" => {
                self.rate.set(value.clamp(MIN_RATE, MAX_RATE));
                Ok(())
            }
            "feedback" => {
                self.feedback.set(value.clamp(-MAX_FEEDBACK, MAX_FEEDBACK));
                Ok(())
            }
            _ => Err(FxError::UnknownParam),
        }
    }

    fn get_param(&self, name: &str) -> Option<f32> {
        match name {
            "mix" => Some(self.mix.target()),
            "depth" => Some(self.depth.target()),
            "rate" => Some(self.rate.target()),
            "feedback" => Some(self.feedback.target()),
            "delay_frames" => Some(self.delay_frames),
            _ => None,
        }
    }

    fn param_names(&self) -> &'static [&'static str] {
        Self::PARAMS
    }
}

impl std::fmt::Debug for Flanger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Flanger")
            .field("rate", &self.rate.target())
            .field("depth", &self.depth.target())
            .field("feedback", &self.feedback.target())
            .field("mix", &self.mix.target())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const BEAT_FRAMES: f64 = 24_000.0;
    const BLOCK: usize = 256;

    fn ctx(frames: usize) -> FxContext {
        FxContext {
            sample_rate: RATE,
            block_frames: frames,
            frames_per_beat: BEAT_FRAMES,
            beat_position: 0.0,
        }
    }

    fn tuned(overrides: &[(&str, f32)]) -> Flanger {
        let mut fx = Flanger::new();
        fx.prepare(RATE);
        for (name, value) in overrides {
            fx.set_param(name, *value).unwrap();
        }
        fx.reset();
        fx
    }

    fn run(fx: &mut Flanger, frames: usize, sine_hz: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames);
        let blocks = frames.div_ceil(BLOCK);
        for block in 0..blocks {
            let mut bus = Bus::stereo(BLOCK);
            for i in 0..BLOCK {
                let n = block * BLOCK + i;
                let s = if sine_hz > 0.0 {
                    (2.0 * std::f32::consts::PI * sine_hz * n as f32 / RATE as f32).sin() * 0.5
                } else if n == 0 {
                    1.0
                } else {
                    0.0
                };
                bus.l[i] = s;
                bus.r[i] = s;
            }
            fx.process(&mut bus, &ctx(BLOCK));
            out.extend_from_slice(&bus.l);
        }
        out.truncate(frames);
        out
    }

    #[test]
    fn a_dry_mix_is_transparent() {
        let mut fx = tuned(&[("mix", 0.0)]);
        let mut bus = Bus::stereo(BLOCK);
        for i in 0..BLOCK {
            bus.l[i] = (i as f32 * 0.07).sin();
            bus.r[i] = -(i as f32 * 0.07).sin();
        }
        let original = bus.clone();
        fx.process(&mut bus, &ctx(BLOCK));
        assert_eq!(bus, original, "mix=0 must let the signal through untouched");
    }

    #[test]
    fn depth_zero_is_a_fixed_comb() {
        let mut fx = tuned(&[("mix", 1.0), ("depth", 0.0), ("feedback", 0.0)]);
        let out = run(&mut fx, 512, 0.0);
        let expected = (RATE as f32 * CENTRE_SECONDS) as usize;
        assert!(
            (out[expected] - 1.0).abs() < 1e-3,
            "the fixed comb landed at {} instead of {expected}",
            out.iter().position(|s| s.abs() > 0.5).unwrap()
        );
        assert!((fx.delay_frames() - expected as f32).abs() < 1e-3);
    }

    #[test]
    fn depth_moves_the_comb() {
        let mut still = tuned(&[("mix", 1.0), ("depth", 0.0), ("feedback", 0.0)]);
        let mut swept = tuned(&[("mix", 1.0), ("depth", 1.0), ("feedback", 0.0)]);
        let a = run(&mut still, 8_000, 440.0);
        let b = run(&mut swept, 8_000, 440.0);
        let difference: f32 = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum();
        assert!(
            difference > 1.0,
            "the LFO swept nothing: difference {difference}"
        );
    }

    #[test]
    fn the_lfo_stays_locked_to_the_beat() {
        let mut fx = tuned(&[("mix", 0.0), ("rate", 0.25)]);
        // 0.25 cycles per beat at 2 beats/s is 0.5 cycles in one second. One whole-second block,
        // because the helper's 256-frame blocks would process 48 128 frames.
        let mut bus = Bus::stereo(RATE as usize);
        fx.process(&mut bus, &ctx(RATE as usize));
        assert!(
            (fx.lfo_phase() - 0.5).abs() < 1e-6,
            "phase after one second was {}",
            fx.lfo_phase()
        );
    }

    #[test]
    fn feedback_is_clamped_and_stays_finite() {
        let mut fx = tuned(&[("feedback", 99.0), ("depth", 1.0), ("mix", 1.0)]);
        assert_eq!(fx.get_param("feedback"), Some(MAX_FEEDBACK));
        let out = run(&mut fx, RATE as usize, 220.0);
        assert!(out.iter().all(|s| s.is_finite()));
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak < 100.0, "the flanger ran away to {peak}");
    }

    #[test]
    fn reset_clears_the_delay_and_the_lfo() {
        let mut fx = tuned(&[("mix", 1.0), ("depth", 1.0)]);
        run(&mut fx, 2_000, 220.0);
        assert!(fx.delay_frames() > 0.0);
        fx.reset();
        assert_eq!(fx.lfo_phase(), 0.0);
        assert_eq!(fx.delay_frames(), 0.0);
        let mut silent = Bus::stereo(BLOCK);
        fx.process(&mut silent, &ctx(BLOCK));
        assert!(silent.is_silent(), "a reset left the delay line ringing");
    }

    #[test]
    fn a_change_of_rate_re_prepares_the_buffer() {
        let mut fx = tuned(&[("mix", 1.0), ("depth", 0.0)]);
        let mut ctx = ctx(BLOCK);
        ctx.sample_rate = 96_000;
        let mut bus = Bus::stereo(BLOCK);
        bus.l[0] = 1.0;
        fx.process(&mut bus, &ctx);
        assert_eq!(fx.prepared_rate, 96_000);
        assert!(bus.l.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn param_surface_is_strict() {
        let fx = Flanger::new();
        assert_eq!(fx.set_param("speed", 1.0), Err(FxError::UnknownParam));
        assert!(matches!(
            fx.set_param("rate", f32::NAN),
            Err(FxError::InvalidValue(_))
        ));
        fx.set_param("rate", 99.0).unwrap();
        assert_eq!(fx.get_param("rate"), Some(MAX_RATE));
        fx.set_param("feedback", -99.0).unwrap();
        assert_eq!(fx.get_param("feedback"), Some(-MAX_FEEDBACK));
        assert_eq!(fx.param_names(), Flanger::PARAMS);
    }
}
