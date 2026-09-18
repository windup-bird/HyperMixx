//! [`Gain`]: the simplest possible insert, and the reference implementation for what an [`Fx`] looks
//! like. One parameter, one multiply per sample, no state.
//!
//! It is also the type the mixer's own faders are built on, so it doubles as the readable spec for
//! "a fader is just a gain with a smoothing time constant".

use std::sync::atomic::Ordering;

use super::super::{Fx, FxContext, FxError, Param};
use crate::mixer::Bus;

/// Linear gain with per-block smoothing.
pub struct Gain {
    gain: Param,
}

impl Gain {
    /// Starts at `gain` (1.0 = unity). Moves are smoothed over [`GAIN_TAU_SECONDS`].
    pub fn new(gain: f32) -> Self {
        Self {
            gain: Param::new(gain, GAIN_TAU_SECONDS),
        }
    }

    /// A gain stage that follows a fader position of `-1.0 ..= 1.0` (centred = unity), the law the
    /// channel faders use.
    pub fn from_bipolar(position: f32) -> Self {
        Self::new(crate::fx::dsp::bipolar_amp(position))
    }

    /// The value the audio thread is currently applying.
    pub fn current(&self) -> f32 {
        self.gain.get()
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain.set(gain);
    }

    /// Instantly re-anchors the stage, e.g. when a bus is unmuted and must not fade in.
    pub fn snap_gain(&self, gain: f32) {
        self.gain.snap(gain);
    }

    /// Parameter names, as an associated const so the registry can list them without building one.
    pub const PARAMS: &'static [&'static str] = &["gain"];
}

/// Half a block at 44.1 kHz: long enough to kill a click from a stepped fader move, short enough
/// that a fast fade still feels direct.
const GAIN_TAU_SECONDS: f32 = 0.005;

impl Fx for Gain {
    fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        let gain = self.gain.next_block(ctx.block_frames, ctx.sample_rate);
        if gain == 1.0 {
            return;
        }
        bus.scale(gain);
    }

    fn reset(&mut self) {
        self.gain.snap(self.gain.target());
    }

    fn set_param(&self, name: &str, value: f32) -> Result<(), FxError> {
        match name {
            "gain" => {
                if !value.is_finite() {
                    return Err(FxError::InvalidValue(value));
                }
                self.gain.set(value.clamp(0.0, 8.0));
                Ok(())
            }
            _ => Err(FxError::UnknownParam),
        }
    }

    fn get_param(&self, name: &str) -> Option<f32> {
        match name {
            "gain" => Some(self.gain.target()),
            _ => None,
        }
    }

    fn param_names(&self) -> &'static [&'static str] {
        Self::PARAMS
    }
}

impl std::fmt::Debug for Gain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gain")
            .field("gain", &self.gain.target())
            .finish()
    }
}

/// A fader that also reports whether it is currently moving, for meters and for the mixer's
/// "skip silent work" decisions.
pub struct Fader {
    position: Param,
    moving: std::sync::atomic::AtomicBool,
}

impl Fader {
    pub fn new(position: f32, tau_seconds: f32) -> Self {
        Self {
            position: Param::new(position, tau_seconds),
            moving: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn set(&self, position: f32) {
        self.position.set(position);
        self.moving.store(true, Ordering::Relaxed);
    }

    pub fn position(&self) -> f32 {
        self.position.get()
    }

    pub fn target(&self) -> f32 {
        self.position.target()
    }

    pub fn snap(&self, position: f32) {
        self.position.snap(position);
        self.moving.store(false, Ordering::Relaxed);
    }

    /// Steps the fader and reports the raw bipolar position, for a stage that applies its own law
    /// (a crossfader's pan curve, a cue send's mono fold).
    #[inline]
    pub fn next_position(&self, ctx: &FxContext) -> f32 {
        let position = self.position.next_block(ctx.block_frames, ctx.sample_rate);
        self.moving.store(!self.position.is_settled(), Ordering::Relaxed);
        position
    }

    /// Steps the fader and reports the resulting linear gain.
    #[inline]
    pub fn next_amp(&self, ctx: &FxContext) -> f32 {
        let position = self.position.next_block(ctx.block_frames, ctx.sample_rate);
        self.moving
            .store(!self.position.is_settled(), Ordering::Relaxed);
        crate::fx::dsp::bipolar_amp(position)
    }

    pub fn is_moving(&self) -> bool {
        self.moving.load(Ordering::Relaxed)
    }

    /// `true` when the fader is at (or near) the bottom of its travel, so a stage can be skipped.
    pub fn is_closed(&self) -> bool {
        self.position.target() <= -0.999
    }
}

impl std::fmt::Debug for Fader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fader")
            .field("position", &self.position())
            .field("target", &self.target())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BLOCK_SIZE;

    fn ctx(frames: usize) -> FxContext {
        FxContext::gridless(44_100, frames)
    }

    fn bus_with(value: f32, frames: usize) -> Bus {
        let mut b = Bus::new(frames);
        b.fill_from(value, value);
        b
    }

    #[test]
    fn unity_is_a_no_op_and_two_doubles() {
        let mut unity = Gain::new(1.0);
        let mut b = bus_with(0.25, 4);
        unity.process(&mut b, &ctx(4));
        assert_eq!(b.l, vec![0.25; 4]);

        let mut doubled = Gain::new(2.0);
        let mut b = bus_with(0.25, 4);
        doubled.process(&mut b, &ctx(4));
        assert_eq!(b.l, vec![0.5; 4]);
    }

    #[test]
    fn a_target_move_ramps_instead_of_jumping() {
        let mut gain = Gain::new(1.0);
        gain.set_param("gain", 4.0).unwrap();
        // A *fresh* bus every block. Re-using one would feed the previous block's output back in as
        // the next block's input, so the ramp would compound instead of converge.
        let mut first = 0.0f32;
        let mut settled = 0.0f32;
        for block in 0..200 {
            let mut b = bus_with(1.0, BLOCK_SIZE);
            gain.process(&mut b, &ctx(BLOCK_SIZE));
            if block == 0 {
                first = b.l[0];
            }
            settled = b.l[0];
        }
        assert!(first > 1.0 && first < 4.0, "first block jumped to {first}");
        assert!(
            (settled - 4.0).abs() < 0.01,
            "never settled at the target: {settled}"
        );
    }

    #[test]
    fn reset_snaps_to_the_target() {
        let mut gain = Gain::new(1.0);
        gain.set_param("gain", 2.0).unwrap();
        gain.reset();
        let mut b = bus_with(1.0, 4);
        gain.process(&mut b, &ctx(4));
        assert_eq!(b.l[0], 2.0);
    }

    #[test]
    fn refuses_unknown_names_and_bad_values() {
        let gain = Gain::new(1.0);
        assert_eq!(gain.set_param("loudness", 1.0), Err(FxError::UnknownParam));
        assert!(matches!(
            gain.set_param("gain", f32::NAN),
            Err(FxError::InvalidValue(_))
        ));
        assert_eq!(gain.get_param("gain"), Some(1.0));
        assert_eq!(gain.get_param("nope"), None);
        assert_eq!(gain.param_names(), Gain::PARAMS);
    }

    #[test]
    fn bipolar_law_drives_a_fader_from_position() {
        let mut kill = Gain::from_bipolar(-1.0);
        let mut b = bus_with(1.0, 4);
        kill.process(&mut b, &ctx(4));
        assert!(b.peak() < 1e-3, "full CC must be silence, got {}", b.peak());

        let mut full = Gain::from_bipolar(1.0);
        let mut b = bus_with(1.0, 4);
        full.process(&mut b, &ctx(4));
        assert!(b.peak() > 4.0, "full CW must boost, got {}", b.peak());
    }

    #[test]
    fn fader_reports_its_own_movement() {
        let fader = Fader::new(0.0, 0.02);
        assert!(!fader.is_moving());
        fader.set(1.0);
        let ctx = ctx(BLOCK_SIZE);
        assert!(fader.next_amp(&ctx) > 0.0);
        assert!(fader.is_moving());
        for _ in 0..300 {
            fader.next_amp(&ctx);
        }
        assert!(!fader.is_moving());
        assert!(fader.next_amp(&ctx) > 5.0, "top of travel boosts");
        fader.snap(-1.0);
        assert!(fader.is_closed());
        assert!(fader.next_amp(&ctx) < 1e-3);
    }
}
