//! [`Bus`]: the unit of signal every stage of the mixer reads and writes.
//!
//! De-interleaved (L and R as separate planes) rather than interleaved like the deck's output and
//! the ring buffer. Effects touch each channel's samples in sequence, so a plane removes a stride
//! multiply from the innermost loop of every filter; the one place that wants interleaving is the
//! output ring, and [`Bus::write_interleaved`] is exactly that boundary.
//!
//! Buffers are allocated once at construction. Nothing in this module allocates, which is what lets
//! the mixer run on the producer thread without ever hitting a malloc.

use crate::fx::Ramp;
use crate::CHANNELS;

/// One block of de-interleaved stereo audio.
#[derive(Clone, Debug, PartialEq)]
pub struct Bus {
    /// Left plane, `frames` samples.
    pub l: Vec<f32>,
    /// Right plane, `frames` samples.
    pub r: Vec<f32>,
}

impl Bus {
    /// A silent bus of `frames` frames.
    pub fn new(frames: usize) -> Self {
        Self {
            l: vec![0.0; frames],
            r: vec![0.0; frames],
        }
    }

    /// The same thing as [`Bus::new`], named for call sites that mean "stereo".
    pub fn stereo(frames: usize) -> Self {
        Self::new(frames)
    }

    #[inline]
    pub fn frames(&self) -> usize {
        self.l.len()
    }

    /// Resizes both planes together.
    ///
    /// A no-op in the hot path: the mixer asks for the same block length every time, so this only
    /// reallocates when a device or a test changes it. Needed because `l`/`r` are `pub` (effects
    /// iterate a plane directly), which means the type cannot enforce equal lengths on its own.
    pub fn ensure_frames(&mut self, frames: usize) {
        if self.frames() != frames {
            self.l.resize(frames, 0.0);
            self.r.resize(frames, 0.0);
        }
    }

    #[inline]
    pub fn is_silent(&self) -> bool {
        self.l.iter().all(|s| *s == 0.0) && self.r.iter().all(|s| *s == 0.0)
    }

    /// Zeroes both planes, keeping the allocation.
    #[inline]
    pub fn clear(&mut self) {
        self.l.fill(0.0);
        self.r.fill(0.0);
    }

    /// Overwrites both planes with the same constant, per channel.
    #[inline]
    pub fn fill_from(&mut self, left: f32, right: f32) {
        self.l.fill(left);
        self.r.fill(right);
    }

    #[inline]
    pub fn channel(&self, index: usize) -> &[f32] {
        if index & 1 == 0 {
            &self.l
        } else {
            &self.r
        }
    }

    #[inline]
    pub fn channel_mut(&mut self, index: usize) -> &mut [f32] {
        if index & 1 == 0 {
            &mut self.l
        } else {
            &mut self.r
        }
    }

    /// `self += other`, element-wise. `other` is truncated to this bus's length if it is longer.
    #[inline]
    pub fn add_from(&mut self, other: &Bus) {
        let n = self.frames().min(other.frames());
        add_in_place(&mut self.l[..n], &other.l[..n]);
        add_in_place(&mut self.r[..n], &other.r[..n]);
    }

    /// `self += other * gain`, in one pass — the mixer's normal sum, since every channel arrives
    /// with a fader already on it.
    #[inline]
    pub fn add_scaled(&mut self, other: &Bus, gain: f32) {
        let n = self.frames().min(other.frames());
        add_scaled_in_place(&mut self.l[..n], &other.l[..n], gain);
        add_scaled_in_place(&mut self.r[..n], &other.r[..n], gain);
    }

    /// Multiplies both planes in place.
    #[inline]
    pub fn scale(&mut self, gain: f32) {
        if gain == 1.0 {
            return;
        }
        if gain == 0.0 || !gain.is_finite() {
            self.clear();
            return;
        }
        for s in self.l.iter_mut() {
            *s *= gain;
        }
        for s in self.r.iter_mut() {
            *s *= gain;
        }
    }

    /// Per-sample gain, for a fader that moves inside the block (a crossfader ramp).
    /// `next` is called once per sample index and applied to both planes, so left and right always
    /// see the same gain curve.
    #[inline]
    pub fn scale_ramped(&mut self, mut next: impl FnMut(usize) -> f32) {
        for ((i, l), r) in self.l.iter_mut().enumerate().zip(self.r.iter_mut()) {
            let g = next(i);
            *l *= g;
            *r *= g;
        }
    }

    /// Applies one [`Ramp`] as a per-sample gain. A stationary ramp falls back to [`Bus::scale`],
    /// so a settled unity gain keeps its no-op. The ramp already starts where the previous block
    /// ended, which is what makes the gain continuous at every boundary — no zip on a fast move.
    #[inline]
    pub fn scale_ramp(&mut self, ramp: &Ramp) {
        if ramp.is_constant() {
            self.scale(ramp.end());
            return;
        }
        self.scale_ramped(|i| ramp.at(i));
    }

    /// `self += other * ramp.at(i)`. The cue send uses this so a fast send move does not step at
    /// block boundaries.
    #[inline]
    pub fn add_scaled_ramp(&mut self, other: &Bus, ramp: &Ramp) {
        let n = self.frames().min(other.frames());
        for i in 0..n {
            let g = ramp.at(i);
            self.l[i] += other.l[i] * g;
            self.r[i] += other.r[i] * g;
        }
    }

    /// Peak absolute sample across both planes — the meter and limiter sidechain read this.
    #[inline]
    pub fn peak(&self) -> f32 {
        let left = self.l.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let right = self.r.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        left.max(right)
    }

    /// Copies a deck's interleaved block into this bus.
    ///
    /// Returns the frames written, i.e. the smaller of the two buses' lengths: a deck that produced
    /// a short block leaves the tail of the bus exactly as it found it.
    pub fn write_interleaved(&mut self, interleaved: &[f32]) -> usize {
        let frames = self.frames().min(interleaved.len() / CHANNELS);
        for (i, frame) in interleaved[..frames * CHANNELS]
            .as_chunks::<CHANNELS>()
            .0
            .iter()
            .enumerate()
        {
            self.l[i] = frame[0];
            self.r[i] = frame[1];
        }
        frames
    }

    /// Writes this bus into an interleaved buffer (the ring buffer's shape).
    pub fn read_interleaved(&self, output: &mut [f32]) -> usize {
        let frames = self.frames().min(output.len() / CHANNELS);
        for (i, out) in output[..frames * CHANNELS]
            .as_chunks_mut::<CHANNELS>()
            .0
            .iter_mut()
            .enumerate()
        {
            out[0] = self.l[i];
            out[1] = self.r[i];
        }
        frames
    }

    /// Silence, for a stage that must hand something back without owning a bus.
    pub fn silence(frames: usize) -> Bus {
        Bus::new(frames)
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new(0)
    }
}

#[inline]
fn add_in_place(dst: &mut [f32], src: &[f32]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d += s;
    }
}

#[inline]
fn add_scaled_in_place(dst: &mut [f32], src: &[f32], gain: f32) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d += s * gain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus(left: &[f32], right: &[f32]) -> Bus {
        assert_eq!(left.len(), right.len());
        Bus {
            l: left.to_vec(),
            r: right.to_vec(),
        }
    }

    #[test]
    fn de_interleave_and_back_round_trips() {
        let interleaved = [0.1, -0.1, 0.2, -0.2, 0.3, -0.3];
        let mut b = Bus::new(3);
        assert_eq!(b.write_interleaved(&interleaved), 3);
        assert_eq!(b.l, vec![0.1, 0.2, 0.3]);
        assert_eq!(b.r, vec![-0.1, -0.2, -0.3]);

        let mut back = [0.0f32; 6];
        assert_eq!(b.read_interleaved(&mut back), 3);
        assert_eq!(back, interleaved);
    }

    #[test]
    fn summing_is_elementwise_and_length_safe() {
        let mut a = bus(&[1.0, 2.0], &[3.0, 4.0]);
        let b = bus(&[10.0, 20.0, 30.0], &[1.0, 1.0, 1.0]);
        a.add_from(&b); // `a` is shorter: the extra frame must be ignored, not panic.
        assert_eq!(a.l, vec![11.0, 22.0]);
        assert_eq!(a.r, vec![4.0, 5.0]);
    }

    #[test]
    fn scaled_sum_is_one_pass() {
        let mut a = bus(&[1.0], &[0.0]);
        a.add_scaled(&bus(&[2.0], &[4.0]), 0.5);
        assert_eq!(a.l, vec![2.0]);
        assert_eq!(a.r, vec![2.0]);
    }

    #[test]
    fn gain_of_zero_or_nan_silences_rather_than_propagating() {
        let mut a = bus(&[1.0, 2.0], &[3.0, 4.0]);
        a.scale(f32::NAN);
        assert!(a.is_silent());
        a.fill_from(1.0, 1.0);
        a.scale(0.0);
        assert!(a.is_silent());
        a.fill_from(1.0, 2.0);
        a.scale(1.0);
        // `scale(1.0)` is a documented no-op, so both planes are exactly what was filled.
        assert_eq!(a.l, vec![1.0, 1.0]);
        assert_eq!(a.r, vec![2.0, 2.0]);
    }

    #[test]
    fn per_sample_gain_moves_through_the_block() {
        let mut a = bus(&[1.0, 1.0, 1.0, 1.0], &[1.0; 4]);
        // 0 → 1 across four frames.
        a.scale_ramped(|i| i as f32 / 3.0);
        assert_eq!(a.l, vec![0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0]);
        assert_eq!(a.r, a.l, "both planes see the same gain curve");
    }

    #[test]
    fn scale_ramp_spreads_the_step_over_the_block() {
        let mut a = bus(&[1.0; 4], &[1.0; 4]);
        a.scale_ramp(&Ramp::new(0.25, 1.0, 4));
        assert_eq!(a.l[0], 0.25);
        assert_eq!(a.l[3], 1.0);
        assert!(a.l[0] < a.l[1] && a.l[1] < a.l[2] && a.l[2] < a.l[3]);
        assert_eq!(a.l, a.r);
        // A stationary ramp is just a plain scale.
        let mut b = bus(&[0.5; 4], &[0.5; 4]);
        b.scale_ramp(&Ramp::new(2.0, 2.0, 4));
        assert_eq!(b.l, vec![1.0; 4]);
    }

    #[test]
    fn add_scaled_ramp_ramps_the_added_copy() {
        let mut cue = bus(&[0.0; 4], &[0.0; 4]);
        cue.add_scaled_ramp(&bus(&[1.0; 4], &[1.0; 4]), &Ramp::new(0.0, 1.0, 4));
        assert_eq!(cue.l[0], 0.0);
        assert_eq!(cue.l[3], 1.0);
        assert_eq!(cue.l, cue.r);
    }

    #[test]
    fn a_fast_full_travel_fader_move_joins_across_block_boundaries() {
        use crate::fx::Param;
        use crate::BLOCK_SIZE;
        // FADER_TAU = 8 ms: the old path applied each block's *end* gain to the whole block,
        // stepping ~50% at every 5.8 ms boundary. Ramp endpoints must meet exactly.
        let p = Param::new(0.0, 0.008);
        p.set(1.0);
        let mut previous_last = None;
        for _ in 0..4 {
            let ramp = p.ramp(BLOCK_SIZE, 44_100);
            if let Some(previous) = previous_last {
                assert_eq!(ramp.start(), previous, "a block boundary must not step");
            }
            let mut b = Bus::new(BLOCK_SIZE);
            b.fill_from(1.0, 1.0);
            b.scale_ramp(&ramp);
            assert_eq!(b.l[0], ramp.start());
            assert_eq!(b.l[BLOCK_SIZE - 1], ramp.end());
            assert_ne!(b.l[0], b.l[BLOCK_SIZE - 1], "gain must move within a block");
            previous_last = Some(ramp.end());
        }
    }

    #[test]
    fn peak_reads_both_planes() {
        let a = bus(&[0.5, -0.2], &[-0.9, 0.1]);
        assert_eq!(a.peak(), 0.9);
        assert!(!a.is_silent());
        assert!(Bus::new(8).is_silent());
    }

    #[test]
    fn clear_keeps_capacity() {
        let mut a = bus(&[1.0, 2.0], &[3.0, 4.0]);
        a.clear();
        assert_eq!(a.frames(), 2);
        assert!(a.is_silent());
    }

    #[test]
    fn channel_accessors_split_stereo() {
        let mut a = bus(&[1.0], &[2.0]);
        assert_eq!(a.channel(0), &[1.0]);
        assert_eq!(a.channel(1), &[2.0]);
        a.channel_mut(1)[0] = 9.0;
        assert_eq!(a.r, vec![9.0]);
    }
}
