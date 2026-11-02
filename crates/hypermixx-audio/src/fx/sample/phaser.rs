//! [`Phaser`]: a cascade of swept all-pass filters — the "whoosh" that is not a flanger.
//!
//! A phaser does not delay the signal. It passes it through a chain of first-order all-pass filters
//! whose phase response is swept by an LFO; summing the dry signal with the phase-shifted copy
//! produces moving notches. Six stages is the classic pedal count (MXR Phase 90, Small Stone): enough
//! notches to hear the sweep, few enough that it stays a colour rather than a filter.
//!
//! The all-pass coefficient is read from a table built at [`prepare`] time. The exact formula is
//! `a = (tan(π f / sr) - 1) / (tan(π f / sr) + 1)`, and a `tan` per sample per stage would be
//! wasteful; a 512-point table interpolated per sample is smooth enough that the sweep has no
//! zipper and cheap enough to run on every stem.
//!
//! [`rate`] is in cycles per beat, like the flanger, so the sweep stays musical across tempo changes.

use super::super::delay::SineLfo;
use super::super::{Fx, FxContext, FxError, Param};
use crate::mixer::Bus;

/// All-pass stages in the cascade.
const STAGES: usize = 6;
/// Points in the coefficient table. 512 over a 4.3-octave sweep is finer than a semitone.
const TABLE_SIZE: usize = 512;
/// The swept range of the all-pass break frequency.
const MIN_HZ: f32 = 150.0;
const MAX_HZ: f32 = 3_000.0;

/// Slowest and fastest LFO, in cycles per beat.
pub const MIN_RATE: f32 = 1.0 / 16.0;
pub const MAX_RATE: f32 = 4.0;
/// Feedback past this self-oscillates. The magnitude of the all-pass is one, so this *is* the loop
/// gain and the bound is exact.
pub const MAX_FEEDBACK: f32 = 0.95;

const PARAM_TAU: f32 = 0.03;
/// Right-channel LFO offset, in cycles (90 degrees).
const STEREO_OFFSET: f64 = 0.25;

/// One first-order all-pass section, transposed direct form II.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Allpass1 {
    previous_input: f32,
    previous_output: f32,
}

impl Allpass1 {
    #[inline]
    fn process(&mut self, input: f32, a: f32) -> f32 {
        let output = a * input + self.previous_input - a * self.previous_output;
        self.previous_input = input;
        self.previous_output = output;
        output
    }

    fn clear(&mut self) {
        self.previous_input = 0.0;
        self.previous_output = 0.0;
    }
}

/// A six-stage stereo phaser.
pub struct Phaser {
    mix: Param,
    depth: Param,
    rate: Param,
    feedback: Param,
    /// One cascade per channel.
    stages: [[Allpass1; STAGES]; 2],
    /// Last output of each cascade, the feedback source.
    feedback_state: [f32; 2],
    /// All-pass coefficient versus normalised sweep position, built in [`prepare`].
    table: Vec<f32>,
    lfo: SineLfo,
    prepared_rate: u32,
    /// Coefficient applied to the last left-channel sample (for meters and tests).
    coefficient: f32,
}

impl Phaser {
    /// A six-stage sweep with moderate feedback.
    pub fn new() -> Self {
        let mut phaser = Self {
            mix: Param::new(0.5, PARAM_TAU),
            depth: Param::new(0.7, PARAM_TAU),
            rate: Param::new(0.25, PARAM_TAU),
            feedback: Param::new(0.3, PARAM_TAU),
            stages: [[Allpass1::default(); STAGES]; 2],
            feedback_state: [0.0; 2],
            table: Vec::new(),
            lfo: SineLfo::new(),
            prepared_rate: 0,
            coefficient: 0.0,
        };
        phaser.prepare(crate::SAMPLE_RATE);
        phaser
    }

    /// Parameter names in performance order.
    pub const PARAMS: &'static [&'static str] = &["mix", "depth", "rate", "feedback"];

    /// The all-pass coefficient applied on the last left-channel sample.
    pub fn coefficient(&self) -> f32 {
        self.coefficient
    }

    /// The LFO's current phase, in cycles.
    pub fn lfo_phase(&self) -> f64 {
        self.lfo.phase()
    }

    /// The coefficient table built at [`prepare`] time (test hook).
    pub fn table(&self) -> &[f32] {
        &self.table
    }

    /// Interpolated coefficient for a normalised sweep position.
    #[inline]
    fn coefficient_at(&self, position: f32) -> f32 {
        if self.table.is_empty() {
            return 0.0;
        }
        let position = if position.is_finite() {
            position.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let last = self.table.len() - 1;
        let x = position * last as f32;
        let index = (x as usize).min(last);
        let fraction = x - index as f32;
        let next = (index + 1).min(last);
        let a = self.table[index];
        let b = self.table[next];
        a + (b - a) * fraction
    }

    fn set_unit(param: &Param, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        param.set(value.clamp(0.0, 1.0));
        Ok(())
    }
}

impl Default for Phaser {
    fn default() -> Self {
        Self::new()
    }
}

impl Fx for Phaser {
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
            .clamp(0.0, MAX_FEEDBACK);
        let increment = f64::from(lfo_rate) * ctx.beats_per_second() / f64::from(rate);
        let dry = 1.0 - mix;

        for i in 0..frames {
            let left_lfo = f64::from(self.lfo.value());
            let right_lfo = f64::from(self.lfo.value_at(STEREO_OFFSET));
            self.lfo.advance(increment);
            let left_position = 0.5 + 0.5 * f64::from(depth) * left_lfo;
            let right_position = 0.5 + 0.5 * f64::from(depth) * right_lfo;
            let a_left = self.coefficient_at(left_position as f32);
            let a_right = self.coefficient_at(right_position as f32);
            self.coefficient = a_left;

            let dry_left = bus.l[i];
            let dry_right = bus.r[i];
            let mut wet_left = dry_left + self.feedback_state[0] * feedback;
            let mut wet_right = dry_right + self.feedback_state[1] * feedback;
            for stage in 0..STAGES {
                wet_left = self.stages[0][stage].process(wet_left, a_left);
                wet_right = self.stages[1][stage].process(wet_right, a_right);
            }
            // The cascade is unity-magnitude, so the loop gain *is* the feedback knob; the clamp only
            // guards against a non-finite input riding around the loop.
            self.feedback_state[0] = wet_left.clamp(-8.0, 8.0);
            self.feedback_state[1] = wet_right.clamp(-8.0, 8.0);
            bus.l[i] = dry_left * dry + wet_left * mix;
            bus.r[i] = dry_right * dry + wet_right * mix;
        }
    }

    fn prepare(&mut self, sample_rate: u32) {
        let rate = sample_rate.max(1) as f32;
        let table: Vec<f32> = (0..TABLE_SIZE)
            .map(|i| {
                let position = i as f32 / (TABLE_SIZE - 1) as f32;
                let hz = MIN_HZ * (MAX_HZ / MIN_HZ).powf(position);
                let tangent = (std::f32::consts::PI * hz / rate).tan();
                ((tangent - 1.0) / (tangent + 1.0)).clamp(-0.9999, 0.9999)
            })
            .collect();
        self.table = table;
        self.prepared_rate = sample_rate.max(1);
        self.coefficient = 0.0;
    }

    fn reset(&mut self) {
        self.mix.snap(self.mix.target());
        self.depth.snap(self.depth.target());
        self.rate.snap(self.rate.target());
        self.feedback.snap(self.feedback.target());
        for channel in &mut self.stages {
            for stage in channel {
                stage.clear();
            }
        }
        self.feedback_state = [0.0; 2];
        self.lfo.reset();
        self.coefficient = 0.0;
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
                self.feedback.set(value.clamp(0.0, MAX_FEEDBACK));
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
            "coefficient" => Some(self.coefficient),
            _ => None,
        }
    }

    fn param_names(&self) -> &'static [&'static str] {
        Self::PARAMS
    }
}

impl std::fmt::Debug for Phaser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Phaser")
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

    fn tuned(overrides: &[(&str, f32)]) -> Phaser {
        let mut fx = Phaser::new();
        fx.prepare(RATE);
        for (name, value) in overrides {
            fx.set_param(name, *value).unwrap();
        }
        fx.reset();
        fx
    }

    fn run(fx: &mut Phaser, frames: usize, sine_hz: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames);
        let blocks = frames.div_ceil(BLOCK);
        for block in 0..blocks {
            let mut bus = Bus::stereo(BLOCK);
            for i in 0..BLOCK {
                let n = block * BLOCK + i;
                let s = (2.0 * std::f32::consts::PI * sine_hz * n as f32 / RATE as f32).sin() * 0.5;
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
    fn the_cascade_changes_the_signal() {
        let mut fx = tuned(&[("mix", 0.5), ("depth", 0.0), ("feedback", 0.0)]);
        let out = run(&mut fx, 4_000, 700.0);
        let input: Vec<f32> = (0..4_000)
            .map(|n| (2.0 * std::f32::consts::PI * 700.0 * n as f32 / RATE as f32).sin() * 0.5)
            .collect();
        let late = 3_000;
        let difference: f32 = out[late..]
            .iter()
            .zip(&input[late..])
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(
            difference > 1.0,
            "the all-pass cascade did nothing: {difference}"
        );
    }

    #[test]
    fn depth_moves_the_notches() {
        let mut still = tuned(&[("mix", 1.0), ("depth", 0.0), ("feedback", 0.0)]);
        let mut swept = tuned(&[("mix", 1.0), ("depth", 1.0), ("feedback", 0.0)]);
        let a = run(&mut still, 8_000, 700.0);
        let b = run(&mut swept, 8_000, 700.0);
        // A fixed coefficient and a moving one produce different phase responses at every sample;
        // comparing energies can miss a narrow notch, comparing the waveforms cannot.
        let difference: f32 = a[4_000..]
            .iter()
            .zip(&b[4_000..])
            .map(|(x, y)| (x - y).abs())
            .sum();
        assert!(
            difference > 1.0,
            "the sweep changed nothing: difference {difference}"
        );
    }

    #[test]
    fn the_coefficient_table_stays_inside_the_unit_circle() {
        for rate in [8_000, 44_100, 48_000, 96_000, 192_000] {
            let mut fx = Phaser::new();
            fx.prepare(rate);
            for a in fx.table() {
                assert!(a.abs() < 1.0, "a={a} at {rate}Hz");
                assert!(a.is_finite());
            }
        }
    }

    #[test]
    fn the_lfo_stays_locked_to_the_beat() {
        let mut fx = tuned(&[("mix", 0.0), ("rate", 0.25)]);
        let mut bus = Bus::stereo(RATE as usize);
        fx.process(&mut bus, &ctx(RATE as usize));
        assert!(
            (fx.lfo_phase() - 0.5).abs() < 1e-6,
            "phase {}",
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
        assert!(peak < 100.0, "the phaser ran away to {peak}");
    }

    #[test]
    fn reset_clears_the_state() {
        let mut fx = tuned(&[("mix", 1.0), ("depth", 1.0)]);
        run(&mut fx, 2_000, 220.0);
        fx.reset();
        assert_eq!(fx.lfo_phase(), 0.0);
        assert_eq!(fx.coefficient(), 0.0);
        let mut silent = Bus::stereo(BLOCK);
        fx.process(&mut silent, &ctx(BLOCK));
        assert!(silent.is_silent(), "a reset left the cascade ringing");
    }

    #[test]
    fn param_surface_is_strict() {
        let fx = Phaser::new();
        assert_eq!(fx.set_param("stages", 2.0), Err(FxError::UnknownParam));
        assert!(matches!(
            fx.set_param("depth", f32::NAN),
            Err(FxError::InvalidValue(_))
        ));
        fx.set_param("rate", -1.0).unwrap();
        assert_eq!(fx.get_param("rate"), Some(MIN_RATE));
        fx.set_param("feedback", 99.0).unwrap();
        assert_eq!(fx.get_param("feedback"), Some(MAX_FEEDBACK));
        assert_eq!(fx.param_names(), Phaser::PARAMS);
    }
}
