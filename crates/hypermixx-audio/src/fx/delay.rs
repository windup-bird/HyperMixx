//! Time-domain primitives shared by the time-based effects.
//!
//! [`DelayLine`] is a mono delay with linearly interpolated fractional reads, [`OnePole`] is the
//! damping filter a feedback path uses, and [`SineLfo`] is the modulation source behind the flanger
//! and the phaser. They live here rather than in `dsp.rs` because that module is about *frequency*
//! shaping (biquads and fader laws) while this one is about *time*.
//!
//! Buffers are sized by [`Fx::prepare`](crate::fx::Fx::prepare) and never resized afterwards, so a
//! time-based effect's `process` is allocation-free. A caller that forgets `prepare` gets a single
//! allocation on the first block, at the rate that block announces; the mixer always prepares at
//! construction, and again after a topology reload.

use std::f64::consts::TAU;

/// Smallest buffer the line accepts. Two samples would leave no room for a fractional read.
const MIN_CAPACITY: usize = 4;

/// A mono delay line with linear-interpolated fractional reads.
///
/// The write head points at the *oldest* sample, so a read happens *before* the matching push:
/// `read(1.0)` is the sample pushed on the previous call, and a feedback path is
/// `push(input + read(delay) * feedback)`.
#[derive(Clone, Debug, PartialEq)]
pub struct DelayLine {
    buffer: Vec<f32>,
    write: usize,
}

impl Default for DelayLine {
    fn default() -> Self {
        Self::with_frames(MIN_CAPACITY)
    }
}

impl DelayLine {
    /// A silent line of `frames` samples, clamped to the smallest legal size.
    pub fn with_frames(frames: usize) -> Self {
        Self {
            buffer: vec![0.0; frames.max(MIN_CAPACITY)],
            write: 0,
        }
    }

    /// Samples the line can hold.
    pub fn frames(&self) -> usize {
        self.buffer.len()
    }

    /// Longest delay [`DelayLine::read`] will use, in frames.
    ///
    /// Two samples short of the capacity: one for the write head, one so the interpolating read
    /// never has to reach past the newest sample.
    pub fn max_delay(&self) -> f32 {
        (self.frames().saturating_sub(2)) as f32
    }

    /// Clears and resizes. Called from `prepare`; never from the audio hot path.
    pub fn resize(&mut self, frames: usize) {
        let frames = frames.max(MIN_CAPACITY);
        if self.buffer.len() != frames {
            self.buffer = vec![0.0; frames];
        } else {
            self.buffer.fill(0.0);
        }
        self.write = 0;
    }

    /// Forgets the history but keeps the allocation.
    pub fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
    }

    /// Stores one sample and advances the head.
    #[inline]
    pub fn push(&mut self, sample: f32) {
        self.buffer[self.write] = sample;
        self.write += 1;
        if self.write == self.buffer.len() {
            self.write = 0;
        }
    }

    /// The sample `delay_frames` behind the write head, interpolated.
    ///
    /// The delay is clamped into `1.0 ..= max_delay`: a zero delay would read the slot the next
    /// push is about to overwrite, and a longer one would wrap onto itself.
    #[inline]
    pub fn read(&self, delay_frames: f32) -> f32 {
        let capacity = self.buffer.len();
        if !delay_frames.is_finite() {
            return 0.0;
        }
        let delay = delay_frames.clamp(1.0, (capacity - 2) as f32) as f64;
        let mut position = self.write as f64 - delay;
        if position < 0.0 {
            position += capacity as f64;
        }
        let floor = position.floor();
        let fraction = (position - floor) as f32;
        let first = floor as usize % capacity;
        let second = if first + 1 == capacity { 0 } else { first + 1 };
        let a = self.buffer[first];
        let b = self.buffer[second];
        a + (b - a) * fraction
    }
}

/// A one-pole low-pass, used to darken a delay tail (and to damp a reverb's combs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OnePole {
    state: f32,
    coeff: f32,
}

impl OnePole {
    pub fn new() -> Self {
        Self::default()
    }

    /// Designs the pole for `hz` at the given rate. Non-finite or out-of-band cutoffs are clamped
    /// rather than rejected: this value arrives from a knob at audio rate.
    pub fn set_cutoff(&mut self, hz: f32, sample_rate: u32) {
        let rate = sample_rate.max(1) as f32;
        // The upper bound is at least the lower one even for a nonsense rate, so the clamp can never
        // panic on an inverted range.
        let maximum = (rate * 0.45).max(1.0);
        let cutoff = if hz.is_finite() {
            hz.clamp(0.1, maximum)
        } else {
            maximum
        };
        self.coeff = (-std::f32::consts::TAU * cutoff / rate)
            .exp()
            .clamp(0.0, 0.9999);
    }

    #[inline]
    pub fn process(&mut self, sample: f32) -> f32 {
        self.state = sample + self.coeff * (self.state - sample);
        self.state
    }

    pub fn clear(&mut self) {
        self.state = 0.0;
    }
}

/// A free-running sine oscillator, phase in cycles (`0.0 .. 1.0`).
///
/// One type for the flanger's sweep and the phaser's notch movement: the phase is advanced by
/// `cycles_per_sample` per frame, which lets an effect compute the increment once per block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SineLfo {
    phase: f64,
}

impl SineLfo {
    pub fn new() -> Self {
        Self::default()
    }

    /// `sin(2π phase)`, in `-1.0 ..= 1.0`.
    #[inline]
    pub fn value(&self) -> f32 {
        (TAU * self.phase).sin() as f32
    }

    /// The same sine at a phase offset, for the second channel of a stereo pair.
    #[inline]
    pub fn value_at(&self, offset: f64) -> f32 {
        (TAU * (self.phase + offset)).sin() as f32
    }

    #[inline]
    pub fn phase(&self) -> f64 {
        self.phase
    }

    /// Advances by `cycles`, wrapping into one period. A non-finite step is ignored.
    #[inline]
    pub fn advance(&mut self, cycles: f64) {
        if cycles.is_finite() {
            let next = self.phase + cycles;
            self.phase = next - next.floor();
        }
    }

    pub fn set_phase(&mut self, phase: f64) {
        if phase.is_finite() {
            self.phase = phase - phase.floor();
        }
    }

    pub fn reset(&mut self) {
        self.set_phase(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_returns_what_was_pushed_delay_frames_ago() {
        let mut line = DelayLine::with_frames(16);
        for i in 0..8 {
            assert_eq!(line.read(1.0), if i == 0 { 0.0 } else { (i - 1) as f32 });
            line.push(i as f32);
        }
        assert_eq!(line.read(4.0), 4.0);
        assert_eq!(line.max_delay(), 14.0);
    }

    #[test]
    fn the_read_interpolates_between_frames() {
        let mut line = DelayLine::with_frames(8);
        line.push(0.0);
        line.push(1.0);
        line.push(2.0);
        line.push(3.0);
        // Behind the head: 3, 2, 1, 0.
        assert_eq!(line.read(1.0), 3.0);
        assert_eq!(line.read(2.0), 2.0);
        assert!((line.read(1.5) - 2.5).abs() < 1e-6);
        assert!((line.read(1.25) - 2.75).abs() < 1e-6);
        // A delay of zero is clamped to the newest reachable sample rather than the write slot.
        assert_eq!(line.read(0.0), 3.0);
        assert_eq!(line.read(f32::NAN), 0.0);
        // Past the line's history is silence, not the oldest stored sample.
        assert_eq!(line.read(1e9), 0.0);
    }

    #[test]
    fn a_line_wraps_cleanly_and_clears() {
        let mut line = DelayLine::with_frames(8);
        for i in 0..40 {
            line.push(i as f32);
        }
        assert_eq!(line.read(1.0), 39.0);
        assert_eq!(line.read(2.0), 38.0);
        line.clear();
        assert_eq!(line.read(1.0), 0.0);
        assert_eq!(line.frames(), 8, "clearing keeps the allocation");
        line.resize(32);
        assert_eq!(line.frames(), 32);
        assert_eq!(line.read(1.0), 0.0);
    }

    #[test]
    fn the_one_pole_smooths_and_converges() {
        let mut pole = OnePole::new();
        pole.set_cutoff(100.0, 44_100);
        let mut out = 0.0;
        for _ in 0..4_000 {
            out = pole.process(1.0);
        }
        assert!((out - 1.0).abs() < 1e-3, "pole settled at {out}");
        // After a clear the first output is one step of the ramp, which must be small for a 100 Hz
        // pole at 44.1 kHz.
        pole.clear();
        let first = pole.process(1.0);
        assert!(first > 0.0 && first < 0.05, "a slow pole moved by {first}");
        // A wide-open pole is transparent; nonsense is clamped, not propagated.
        pole.set_cutoff(1e9, 44_100);
        pole.clear();
        assert!(
            pole.process(1.0) > 0.9,
            "a maximum cutoff must pass almost at once"
        );
        pole.set_cutoff(f32::NAN, 44_100);
        assert!(pole.process(1.0).is_finite());
        pole.set_cutoff(100.0, 0);
        assert!(pole.process(1.0).is_finite());
    }

    #[test]
    fn the_lfo_cycles_and_holds_still() {
        let mut lfo = SineLfo::new();
        assert_eq!(lfo.value(), 0.0);
        lfo.advance(0.25);
        assert!((lfo.value() - 1.0).abs() < 1e-6);
        assert!((lfo.value_at(0.25) - 0.0).abs() < 1e-6);
        lfo.advance(1.0);
        assert!((lfo.phase() - 0.25).abs() < 1e-9, "phase must wrap");
        lfo.advance(f64::NAN);
        assert!((lfo.phase() - 0.25).abs() < 1e-9);
        lfo.set_phase(-0.25);
        assert!((lfo.phase() - 0.75).abs() < 1e-9);
        lfo.reset();
        assert_eq!(lfo.phase(), 0.0);
    }

    /// The line's documented read-before-push order, exercised the way an echo uses it.
    #[test]
    fn feedback_keeps_repeating_at_the_same_spacing() {
        let mut line = DelayLine::with_frames(32);
        let mut output = Vec::new();
        for i in 0..120 {
            let input = if i == 0 { 1.0 } else { 0.0 };
            let delayed = line.read(10.0);
            line.push(input + delayed * 0.5);
            output.push(delayed);
        }
        for repeat in 1..=6 {
            let at = repeat * 10;
            let expected = 0.5f32.powi(repeat as i32 - 1);
            assert!(
                (output[at] - expected).abs() < 1e-5,
                "repeat {repeat} was {} not {expected}",
                output[at]
            );
        }
    }
}
