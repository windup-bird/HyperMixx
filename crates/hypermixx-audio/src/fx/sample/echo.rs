//! [`Echo`]: a tempo-synced feedback delay — the "throw it into the next bar" effect.
//!
//! The repeat time is expressed in **beats**, not milliseconds, because a DJ echo is a musical
//! event: a half-beat repeat at 128 BPM must land on the grid, and the grid is exactly what
//! [`FxContext::beat_frames`] carries. A deck without an analysis falls back to a free 120 BPM
//! (see [`FxContext::beat_frames`]) rather than going silent.
//!
//! Two details make it sound like an echo pedal rather than a bare delay line:
//!
//! * **The feedback path is damped.** Every pass through a one-pole low-pass makes the repeat
//!   darker than the one before it, which is the difference between "a copy" and "a tape echo".
//!   [`tone`] opens or closes that pole.
//! * **Ping-pong is a cross-feed, not a pan.** At [`pingpong`] 1 the left line feeds the right and
//!   vice versa, so a single-sided transient alternates channels while the dry image stays put.
//!
//! The lines are sized for [`MAX_DELAY_SECONDS`] at the engine's rate, so the effect adds no
//! allocation to [`process`] after [`prepare`] and no latency to the dry path.

use super::super::delay::{DelayLine, OnePole};
use super::super::{Fx, FxContext, FxError, Param};
use crate::mixer::Bus;

/// Longest repeat the buffers hold. Two seconds covers a quarter note at 60 BPM and every musically
/// useful division at club tempos; longer requests clamp rather than allocate megabytes per slot.
pub const MAX_DELAY_SECONDS: f32 = 2.0;

/// Slowest and fastest repeat, in beats: a 1/32 note through a whole bar.
pub const MIN_BEATS: f32 = 1.0 / 32.0;
pub const MAX_BEATS: f32 = 4.0;

/// Feedback above this runs away on any input that persists. The master limiter would catch it, but
/// an effect must not rely on the safety net for its own stability.
pub const MAX_FEEDBACK: f32 = 0.95;

/// Long enough to hide a stepped knob, short enough that a hand-timed delay is still hand-timed.
const PARAM_TAU: f32 = 0.03;

/// The damping pole at [`tone`] 0 and 1. Below 300 Hz a repeat is a rumble; above 18 kHz there is
/// nothing left to remove.
const TONE_MIN_HZ: f32 = 300.0;
const TONE_MAX_HZ: f32 = 18_000.0;

/// A tempo-synced stereo echo.
pub struct Echo {
    mix: Param,
    feedback: Param,
    time: Param,
    tone: Param,
    pingpong: Param,
    left: DelayLine,
    right: DelayLine,
    damp_left: OnePole,
    damp_right: OnePole,
    /// The rate the buffers were sized at; [`process`] prepares once if the caller did not.
    prepared_rate: u32,
    /// Repeat length actually applied this block, in frames (for meters and tests).
    delay_frames: f32,
}

impl Echo {
    /// A half-beat echo at 45% feedback, darkening every pass.
    pub fn new() -> Self {
        let mut echo = Self {
            mix: Param::new(0.35, PARAM_TAU),
            feedback: Param::new(0.45, PARAM_TAU),
            time: Param::new(0.5, PARAM_TAU),
            tone: Param::new(0.6, PARAM_TAU),
            pingpong: Param::new(0.0, PARAM_TAU),
            left: DelayLine::default(),
            right: DelayLine::default(),
            damp_left: OnePole::new(),
            damp_right: OnePole::new(),
            prepared_rate: 0,
            delay_frames: 0.0,
        };
        echo.prepare(crate::SAMPLE_RATE);
        echo
    }

    /// Parameter names in performance order: the wet amount first, then the echo's own shape.
    pub const PARAMS: &'static [&'static str] = &["mix", "feedback", "time", "tone", "pingpong"];

    /// Repeat length applied on the last block, in frames.
    pub fn delay_frames(&self) -> f32 {
        self.delay_frames
    }

    /// Repeat length requested by the current [`time`] and the grid, in frames.
    pub fn target_delay_frames(&self, ctx: &FxContext) -> f32 {
        let beats = f64::from(self.time.target().clamp(MIN_BEATS, MAX_BEATS));
        (beats * ctx.beat_frames()) as f32
    }

    fn set_unit(param: &Param, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        param.set(value.clamp(0.0, 1.0));
        Ok(())
    }
}

impl Default for Echo {
    fn default() -> Self {
        Self::new()
    }
}

/// [`tone`] 0..=1 to the damping pole's cutoff, exponentially.
#[inline]
fn tone_to_hertz(tone: f32) -> f32 {
    let t = if tone.is_finite() {
        tone.clamp(0.0, 1.0)
    } else {
        0.5
    };
    TONE_MIN_HZ * (TONE_MAX_HZ / TONE_MIN_HZ).powf(t)
}

impl Fx for Echo {
    fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        let frames = bus.frames();
        if frames == 0 {
            return;
        }
        let rate = ctx.sample_rate.max(1);
        if self.prepared_rate != rate {
            // A hand-built chain that skipped [`prepare`]: one allocation on the first block, never
            // again. The mixer always prepares at construction.
            self.prepare(rate);
        }

        let mix = self.mix.next_block(frames, rate).clamp(0.0, 1.0);
        let feedback = self
            .feedback
            .next_block(frames, rate)
            .clamp(0.0, MAX_FEEDBACK);
        let tone = self.tone.next_block(frames, rate);
        let pingpong = self.pingpong.next_block(frames, rate).clamp(0.0, 1.0);
        let beats = f64::from(
            self.time
                .next_block(frames, rate)
                .clamp(MIN_BEATS, MAX_BEATS),
        );
        let delay = ((beats * ctx.beat_frames()) as f32).clamp(1.0, self.left.max_delay());
        self.delay_frames = delay;

        let cutoff = tone_to_hertz(tone);
        self.damp_left.set_cutoff(cutoff, rate);
        self.damp_right.set_cutoff(cutoff, rate);

        let dry = 1.0 - mix;
        for i in 0..frames {
            let wet_left = self.left.read(delay);
            let wet_right = self.right.read(delay);
            // Damp the copy that goes back around, not the copy that is heard now: the first repeat
            // is bright and each later one is darker, which is what a tape/analog echo does.
            let damped_left = self.damp_left.process(wet_left);
            let damped_right = self.damp_right.process(wet_right);
            let cross_left = damped_left * (1.0 - pingpong) + damped_right * pingpong;
            let cross_right = damped_right * (1.0 - pingpong) + damped_left * pingpong;
            let input_left = bus.l[i];
            let input_right = bus.r[i];
            self.left.push(input_left + cross_left * feedback);
            self.right.push(input_right + cross_right * feedback);
            bus.l[i] = input_left * dry + wet_left * mix;
            bus.r[i] = input_right * dry + wet_right * mix;
        }
    }

    fn prepare(&mut self, sample_rate: u32) {
        let rate = sample_rate.max(1);
        let frames = (MAX_DELAY_SECONDS * rate as f32).ceil() as usize + 4;
        self.left.resize(frames);
        self.right.resize(frames);
        let cutoff = tone_to_hertz(self.tone.target());
        self.damp_left.set_cutoff(cutoff, rate);
        self.damp_right.set_cutoff(cutoff, rate);
        self.prepared_rate = rate;
        self.delay_frames = 0.0;
    }

    fn reset(&mut self) {
        self.mix.snap(self.mix.target());
        self.feedback.snap(self.feedback.target());
        self.time.snap(self.time.target());
        self.tone.snap(self.tone.target());
        self.pingpong.snap(self.pingpong.target());
        self.left.clear();
        self.right.clear();
        self.damp_left.clear();
        self.damp_right.clear();
        self.delay_frames = 0.0;
    }

    fn set_param(&self, name: &str, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        match name {
            "mix" => Self::set_unit(&self.mix, value),
            "feedback" => {
                self.feedback.set(value.clamp(0.0, MAX_FEEDBACK));
                Ok(())
            }
            "time" => {
                self.time.set(value.clamp(MIN_BEATS, MAX_BEATS));
                Ok(())
            }
            "tone" => Self::set_unit(&self.tone, value),
            "pingpong" => Self::set_unit(&self.pingpong, value),
            _ => Err(FxError::UnknownParam),
        }
    }

    fn get_param(&self, name: &str) -> Option<f32> {
        match name {
            "mix" => Some(self.mix.target()),
            "feedback" => Some(self.feedback.target()),
            "time" => Some(self.time.target()),
            "tone" => Some(self.tone.target()),
            "pingpong" => Some(self.pingpong.target()),
            "delay_frames" => Some(self.delay_frames),
            _ => None,
        }
    }

    fn param_names(&self) -> &'static [&'static str] {
        Self::PARAMS
    }
}

impl std::fmt::Debug for Echo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Echo")
            .field("time", &self.time.target())
            .field("feedback", &self.feedback.target())
            .field("mix", &self.mix.target())
            .field("delay_frames", &self.delay_frames)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const BEAT_FRAMES: f64 = 1_000.0;
    const FRAMES: usize = 4_000;

    fn ctx(frames: usize, beat_frames: f64) -> FxContext {
        FxContext {
            sample_rate: RATE,
            block_frames: frames,
            frames_per_beat: beat_frames,
            beat_position: 0.0,
        }
    }

    fn tuned(overrides: &[(&str, f32)]) -> Echo {
        let mut fx = Echo::new();
        fx.prepare(RATE);
        for (name, value) in overrides {
            fx.set_param(name, *value).unwrap();
        }
        fx.reset();
        fx
    }

    fn impulse(frames: usize) -> Bus {
        let mut bus = Bus::stereo(frames);
        bus.l[0] = 1.0;
        bus.r[0] = 1.0;
        bus
    }

    #[test]
    fn an_impulse_repeats_on_the_beat_grid() {
        let mut fx = tuned(&[("time", 1.0), ("feedback", 0.0), ("mix", 1.0)]);
        let mut bus = impulse(FRAMES);
        fx.process(&mut bus, &ctx(FRAMES, BEAT_FRAMES));
        assert!(bus.l[0].abs() < 1e-6, "the dry impulse leaked at zero");
        assert!(
            (bus.l[1_000] - 1.0).abs() < 1e-4,
            "first repeat landed at {}",
            bus.l[1_000]
        );
        assert!(bus.l[1_500].abs() < 1e-5, "a repeat appeared off the grid");
        assert!(
            bus.l[2_000].abs() < 1e-6,
            "feedback=0 must not repeat twice"
        );
        assert_eq!(fx.delay_frames(), 1_000.0);
    }

    #[test]
    fn feedback_decays_every_repeat() {
        // A maximally open tone keeps the damping pole out of the arithmetic; the feedback law is
        // what this test is about.
        let mut fx = tuned(&[
            ("time", 1.0),
            ("feedback", 0.5),
            ("tone", 1.0),
            ("mix", 1.0),
        ]);
        let mut bus = impulse(FRAMES);
        fx.process(&mut bus, &ctx(FRAMES, BEAT_FRAMES));
        let (one, two, three) = (bus.l[1_000], bus.l[2_000], bus.l[3_000]);
        assert!((one - 1.0).abs() < 1e-4);
        assert!(two > 0.35 && two < 0.55, "second repeat was {two}");
        assert!(three > 0.15 && three < 0.3, "third repeat was {three}");
        assert!(two > three, "the tail must decay");
    }

    #[test]
    fn pingpong_alternates_the_channels() {
        let mut fx = tuned(&[
            ("time", 1.0),
            ("feedback", 0.5),
            ("tone", 1.0),
            ("mix", 1.0),
            ("pingpong", 1.0),
        ]);
        let mut bus = Bus::stereo(FRAMES);
        bus.l[0] = 1.0;
        fx.process(&mut bus, &ctx(FRAMES, BEAT_FRAMES));
        // The source is left-only: the first repeat is left, the cross-feed sends the next one right.
        assert!((bus.l[1_000] - 1.0).abs() < 1e-4);
        assert!(
            bus.r[1_000].abs() < 1e-6,
            "the first repeat must stay on its side"
        );
        assert!(
            bus.l[2_000].abs() < 1e-6,
            "straight feedback leaked into the left line"
        );
        assert!(
            bus.r[2_000] > 0.3,
            "the ping-pong did not cross: {}",
            bus.r[2_000]
        );
    }

    #[test]
    fn a_dry_mix_is_transparent() {
        let mut fx = tuned(&[("mix", 0.0), ("time", 0.25)]);
        let mut bus = Bus::stereo(FRAMES);
        for i in 0..FRAMES {
            let s = ((i as f32) * 0.01).sin();
            bus.l[i] = s;
            bus.r[i] = -s;
        }
        let original = bus.clone();
        fx.process(&mut bus, &ctx(FRAMES, BEAT_FRAMES));
        assert_eq!(bus, original, "mix=0 must let the signal through untouched");
    }

    #[test]
    fn the_feedback_is_clamped_and_never_diverges() {
        let mut fx = tuned(&[("time", 0.25), ("feedback", 99.0), ("mix", 1.0)]);
        assert_eq!(fx.get_param("feedback"), Some(MAX_FEEDBACK));
        let mut peak = 0.0f32;
        for block in 0..200 {
            let mut bus = Bus::stereo(256);
            for (i, s) in bus.l.iter_mut().enumerate() {
                let n = block * 256 + i;
                *s = (n as f32 * 0.05).sin() * 0.5;
                bus.r[i] = *s;
            }
            fx.process(&mut bus, &ctx(256, BEAT_FRAMES));
            assert!(bus.l.iter().all(|s| s.is_finite()));
            peak = peak.max(bus.peak());
        }
        assert!(peak < 100.0, "the echo ran away to a peak of {peak}");
    }

    #[test]
    fn reset_clears_the_tail() {
        let mut fx = tuned(&[("time", 0.5), ("feedback", 0.5), ("mix", 1.0)]);
        fx.process(&mut impulse(FRAMES), &ctx(FRAMES, BEAT_FRAMES));
        fx.reset();
        let mut silent = Bus::stereo(2_000);
        fx.process(&mut silent, &ctx(2_000, BEAT_FRAMES));
        assert!(silent.is_silent(), "a reset left the tail ringing");
    }

    #[test]
    fn without_a_grid_it_runs_at_120_bpm() {
        let mut fx = tuned(&[("time", 1.0), ("feedback", 0.0), ("mix", 1.0)]);
        let mut ctx = FxContext::gridless(RATE, FRAMES);
        ctx.frames_per_beat = 0.0;
        let mut bus = impulse(FRAMES);
        fx.process(&mut bus, &ctx);
        // 120 BPM at 48k is 24 000 frames per beat; the buffer is shorter, so the repeat clamps.
        let expected = 24_000.0f32.min(fx.left.max_delay());
        assert_eq!(fx.delay_frames(), expected);
    }

    #[test]
    fn preparing_at_another_rate_keeps_the_frames_correct() {
        let mut fx = Echo::new();
        fx.prepare(96_000);
        fx.set_param("time", 1.0).unwrap();
        fx.set_param("feedback", 0.0).unwrap();
        fx.set_param("mix", 1.0).unwrap();
        fx.reset();
        let mut ctx = ctx(FRAMES, BEAT_FRAMES);
        ctx.sample_rate = 96_000;
        let mut bus = impulse(FRAMES);
        fx.process(&mut bus, &ctx);
        assert!(
            (bus.l[1_000] - 1.0).abs() < 1e-4,
            "a rate change moved the repeat"
        );
    }

    #[test]
    fn param_surface_is_strict() {
        let fx = Echo::new();
        assert_eq!(fx.set_param("delay", 1.0), Err(FxError::UnknownParam));
        assert!(matches!(
            fx.set_param("time", f32::NAN),
            Err(FxError::InvalidValue(_))
        ));
        fx.set_param("time", 99.0).unwrap();
        assert_eq!(fx.get_param("time"), Some(MAX_BEATS));
        fx.set_param("feedback", -5.0).unwrap();
        assert_eq!(fx.get_param("feedback"), Some(0.0));
        fx.set_param("mix", 2.0).unwrap();
        assert_eq!(fx.get_param("mix"), Some(1.0));
        assert_eq!(fx.param_names(), Echo::PARAMS);
        assert!(fx.get_param("delay_frames").is_some());
    }
}
