//! [`Cut`]: a beat-locked gate — the "Trans" of a DJM, in one insert.
//!
//! A [`div`]-beat cycle opens for the first [`width`] of it and closes for the rest. [`depth`] says how
//! far the closed half drops (1.0 is exact silence), and [`edge`] rounds the two transitions in
//! milliseconds so a fast gate does not click. The phase is derived from the deck's beat position,
//! so a loop or a jump does not shift the groove: the gate always opens on the beat.
//!
//! Without a grid the phase free-runs at the context's 120 BPM fallback, which makes the effect
//! usable on an unanalysed track without pretending to be locked to it.

use super::super::{Fx, FxContext, FxError, Param};
use crate::mixer::Bus;

/// A gate cycle from a 1/16 note through a whole bar.
pub const MIN_DIV: f32 = 1.0 / 16.0;
pub const MAX_DIV: f32 = 4.0;
/// Duty cycle of the open window. Below 5% the gate is a click; above 95% it is a bypass.
pub const MIN_WIDTH: f32 = 0.05;
pub const MAX_WIDTH: f32 = 0.95;
/// Longest transition, in milliseconds. At 20 ms a gate becomes a tremolo.
pub const MAX_EDGE_MS: f32 = 20.0;

const PARAM_TAU: f32 = 0.02;

/// A beat-locked gate.
pub struct Cut {
    depth: Param,
    div: Param,
    width: Param,
    edge: Param,
    /// Free-running phase, in cycles. Grid-locked blocks recompute it from the beat position.
    phase: f64,
    /// The rate the effect was prepared at; a gate has no buffers, but it reports the rate it uses
    /// for [`edge`] so a rate change is visible.
    prepared_rate: u32,
}

impl Cut {
    /// A half-beat gate at 85% depth with a 3 ms edge.
    pub fn new() -> Self {
        Self {
            depth: Param::new(0.85, PARAM_TAU),
            div: Param::new(0.5, PARAM_TAU),
            width: Param::new(0.5, PARAM_TAU),
            edge: Param::new(3.0, PARAM_TAU),
            phase: 0.0,
            prepared_rate: 0,
        }
    }

    /// Parameter names in performance order: how deep the cut is first, then its shape.
    pub const PARAMS: &'static [&'static str] = &["depth", "div", "width", "edge"];

    /// The phase the next block will start from, in cycles.
    pub fn phase(&self) -> f64 {
        self.phase
    }

    fn set_unit(param: &Param, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        param.set(value.clamp(0.0, 1.0));
        Ok(())
    }
}

impl Default for Cut {
    fn default() -> Self {
        Self::new()
    }
}

/// Raised-cosine ramp: zero slope at both ends, so the gate has no corner to click on.
#[inline]
fn smoothstep(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The open/closed envelope at [`phase`], with [`fade`] of the cycle spent on each transition.
fn gate_gain(phase: f64, width: f64, fade: f64) -> f64 {
    if !phase.is_finite() {
        return 1.0;
    }
    let phase = phase.rem_euclid(1.0);
    if fade <= 1e-9 {
        return if phase < width { 1.0 } else { 0.0 };
    }
    if phase < fade {
        smoothstep(phase / fade)
    } else if phase < width - fade {
        1.0
    } else if phase < width {
        1.0 - smoothstep((phase - (width - fade)) / fade)
    } else {
        0.0
    }
}

impl Fx for Cut {
    fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        let frames = bus.frames();
        if frames == 0 {
            return;
        }
        let rate = ctx.sample_rate.max(1);
        if self.prepared_rate != rate {
            self.prepare(rate);
        }

        // Every control goes through `Param::ramp` — the one entry point. Depth is a gain, so the
        // loop reads `at(i)`; div/width/edge set the gate's shape and only need `end()`. The gate
        // phase already advances per sample, so those coefficients produce no amplitude step.
        let depth = self.depth.ramp(frames, rate).clamped(0.0, 1.0);
        let div = self.div.ramp(frames, rate).end().clamp(MIN_DIV, MAX_DIV);
        let width = self
            .width
            .ramp(frames, rate)
            .end()
            .clamp(MIN_WIDTH, MAX_WIDTH);
        let edge_ms = self.edge.ramp(frames, rate).end().clamp(0.0, MAX_EDGE_MS);

        let period = (f64::from(div) * ctx.beat_frames()).max(2.0);
        // The fade is a fraction of the cycle, bounded so the two ramps can never meet.
        let fade = (f64::from(edge_ms) * 0.001 * f64::from(rate) / period)
            .min(f64::from(width) * 0.5)
            .min((1.0 - f64::from(width)) * 0.5);
        let increment = 1.0 / period;
        let width = f64::from(width);

        // Grid-locked when the context knows where the beat is; a loop or a jump therefore re-locks
        // the gate instead of sliding it.
        let mut phase = match ctx.beat() {
            Some(beat) => (beat / f64::from(div)).rem_euclid(1.0),
            None => self.phase,
        };
        for i in 0..frames {
            let depth = depth.at(i);
            let floor = 1.0 - depth;
            let gain = floor + depth * gate_gain(phase, width, fade) as f32;
            bus.l[i] *= gain;
            bus.r[i] *= gain;
            phase += increment;
            if phase >= 1.0 {
                phase -= 1.0;
            }
        }
        self.phase = phase;
    }

    fn prepare(&mut self, sample_rate: u32) {
        self.prepared_rate = sample_rate.max(1);
    }

    fn on_trigger(&mut self) {
        // A pad press starts the free-running cycle at its open edge. A grid-locked block ignores
        // this because it derives the phase from the beat.
        self.phase = 0.0;
    }

    fn reset(&mut self) {
        self.depth.snap(self.depth.target());
        self.div.snap(self.div.target());
        self.width.snap(self.width.target());
        self.edge.snap(self.edge.target());
        self.phase = 0.0;
    }

    fn set_param(&self, name: &str, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        match name {
            "depth" => Self::set_unit(&self.depth, value),
            "div" => {
                self.div.set(value.clamp(MIN_DIV, MAX_DIV));
                Ok(())
            }
            "width" => {
                self.width.set(value.clamp(MIN_WIDTH, MAX_WIDTH));
                Ok(())
            }
            "edge" => {
                self.edge.set(value.clamp(0.0, MAX_EDGE_MS));
                Ok(())
            }
            _ => Err(FxError::UnknownParam),
        }
    }

    fn get_param(&self, name: &str) -> Option<f32> {
        match name {
            "depth" => Some(self.depth.target()),
            "div" => Some(self.div.target()),
            "width" => Some(self.width.target()),
            "edge" => Some(self.edge.target()),
            "phase" => Some(self.phase as f32),
            _ => None,
        }
    }

    fn param_names(&self) -> &'static [&'static str] {
        Self::PARAMS
    }
}

impl std::fmt::Debug for Cut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cut")
            .field("div", &self.div.target())
            .field("width", &self.width.target())
            .field("depth", &self.depth.target())
            .field("edge", &self.edge.target())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const BEAT_FRAMES: f64 = 1_000.0;
    const FRAMES: usize = 2_000;

    fn ctx(frames: usize, beat: f64) -> FxContext {
        FxContext {
            sample_rate: RATE,
            block_frames: frames,
            frames_per_beat: BEAT_FRAMES,
            beat_position: beat,
        }
    }

    fn tuned(overrides: &[(&str, f32)]) -> Cut {
        let mut fx = Cut::new();
        fx.prepare(RATE);
        for (name, value) in overrides {
            fx.set_param(name, *value).unwrap();
        }
        fx.reset();
        fx
    }

    fn constant(frames: usize) -> Bus {
        let mut bus = Bus::stereo(frames);
        bus.fill_from(1.0, 1.0);
        bus
    }

    #[test]
    fn depth_zero_is_transparent() {
        let mut fx = tuned(&[("depth", 0.0)]);
        let mut bus = constant(FRAMES);
        let original = bus.clone();
        fx.process(&mut bus, &ctx(FRAMES, 0.0));
        assert_eq!(bus, original, "depth=0 must not touch the signal");
    }

    #[test]
    fn full_depth_gates_half_the_cycle() {
        let mut fx = tuned(&[("depth", 1.0), ("div", 1.0), ("width", 0.5), ("edge", 0.0)]);
        let mut bus = constant(FRAMES);
        fx.process(&mut bus, &ctx(FRAMES, 0.0));
        assert!(
            bus.l[..500].iter().all(|s| *s == 1.0),
            "the first beat must be open"
        );
        assert!(
            bus.l[500..1_000].iter().all(|s| *s == 0.0),
            "the off-beat must be silent"
        );
        assert!(
            bus.l[1_000..1_500].iter().all(|s| *s == 1.0),
            "the next beat must open again"
        );
        assert!(bus.l[1_500..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn the_gate_is_locked_to_the_beat_position() {
        let mut fx = tuned(&[("depth", 1.0), ("div", 1.0), ("width", 0.5), ("edge", 0.0)]);
        let mut bus = constant(FRAMES);
        // Starting 0.6 of the way through the beat: the gate opens only at the next beat.
        fx.process(&mut bus, &ctx(FRAMES, 0.6));
        assert_eq!(bus.l[0], 0.0, "the gate opened off the beat");
        assert_eq!(bus.l[399], 0.0, "the close must last until the beat");
        assert_eq!(bus.l[400], 1.0, "the gate should open on the beat");
    }

    #[test]
    fn depth_sets_the_floor_of_the_cut() {
        let mut fx = tuned(&[("depth", 0.5), ("div", 1.0), ("width", 0.5), ("edge", 0.0)]);
        let mut bus = constant(FRAMES);
        fx.process(&mut bus, &ctx(FRAMES, 0.0));
        assert_eq!(bus.l[100], 1.0);
        assert_eq!(bus.l[600], 0.5, "a half-depth cut must stop at -6 dB");
    }

    #[test]
    fn the_edge_smooths_the_transitions() {
        let mut fx = tuned(&[("depth", 1.0), ("div", 1.0), ("width", 0.5), ("edge", 10.0)]);
        let mut bus = constant(FRAMES);
        fx.process(&mut bus, &ctx(FRAMES, 0.0));
        let mut max_jump = 0.0f32;
        for pair in bus.l.windows(2) {
            max_jump = max_jump.max((pair[1] - pair[0]).abs());
        }
        assert!(max_jump < 0.05, "the transition stepped by {max_jump}");
        assert!(bus.l.iter().any(|s| *s > 0.9), "the gate never opened");
        assert!(bus.l.iter().any(|s| *s < 0.1), "the gate never closed");
    }

    #[test]
    fn without_a_grid_the_phase_free_runs() {
        let mut fx = tuned(&[("depth", 1.0), ("div", 0.5), ("width", 0.5), ("edge", 0.0)]);
        let mut bus = constant(1_000);
        let ctx = FxContext::gridless(RATE, 1_000);
        fx.process(&mut bus, &ctx);
        // 120 BPM fallback: 0.5 beat = 12 000 frames at 48k, so 1 000 frames is 1/12 of a cycle.
        assert!(
            (fx.phase() - 1_000.0 / 12_000.0).abs() < 1e-6,
            "phase {}",
            fx.phase()
        );
    }

    #[test]
    fn a_trigger_restarts_the_free_running_cycle() {
        let mut fx = tuned(&[("div", 0.25)]);
        let mut bus = constant(1_000);
        fx.process(&mut bus, &FxContext::gridless(RATE, 1_000));
        assert!(fx.phase() > 0.0);
        fx.on_trigger();
        assert_eq!(fx.phase(), 0.0);
    }

    #[test]
    fn param_surface_is_strict() {
        let fx = Cut::new();
        assert_eq!(fx.set_param("gate", 1.0), Err(FxError::UnknownParam));
        assert!(matches!(
            fx.set_param("div", f32::NAN),
            Err(FxError::InvalidValue(_))
        ));
        fx.set_param("div", 99.0).unwrap();
        assert_eq!(fx.get_param("div"), Some(MAX_DIV));
        fx.set_param("width", 0.0).unwrap();
        assert_eq!(fx.get_param("width"), Some(MIN_WIDTH));
        fx.set_param("edge", -1.0).unwrap();
        assert_eq!(fx.get_param("edge"), Some(0.0));
        assert_eq!(fx.param_names(), Cut::PARAMS);
        assert!(fx.get_param("phase").is_some());
    }
}
