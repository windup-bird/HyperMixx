//! BeatGrid: absolute beat positions on the frame grid.
//!
//! The grid stores one absolute frame position per beat. Intervals are *not* stored, so rounding
//! can never accumulate across a track, and BPM is a derived value rather than a second source of
//! truth that could drift out of sync with the grid.

use crate::SAMPLE_RATE;

/// Strictly increasing absolute beat positions, in frames.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BeatGrid {
    /// Absolute frame position of every beat (strictly increasing).
    pub beat_frames: Vec<u64>,
    pub sample_rate: u32,
}

impl BeatGrid {
    /// An empty grid: every query degrades to "no movement".
    pub fn empty(sample_rate: u32) -> Self {
        Self {
            beat_frames: Vec::new(),
            sample_rate,
        }
    }

    /// Adopts precomputed absolute beat positions (what the grid compiler emits).
    pub fn from_frames(beat_frames: Vec<u64>, sample_rate: u32) -> Self {
        Self {
            beat_frames,
            sample_rate,
        }
    }

    /// Builds a grid of evenly spaced beats starting at `first_beat_frame`.
    ///
    /// Positions come from rounding `i * frames_per_beat` rather than accumulating an interval, so
    /// a three-hour track lands its final beat within half a sample of the true position. Beats
    /// past the generated count are extrapolated by [`frame_at_beat`](Self::frame_at_beat).
    pub fn from_constant_bpm(
        bpm: f32,
        first_beat_frame: u64,
        total_frames: u64,
        sample_rate: u32,
    ) -> Self {
        if !(bpm.is_finite() && bpm > 0.0) || sample_rate == 0 {
            return Self::empty(sample_rate);
        }
        let frames_per_beat = f64::from(sample_rate) * 60.0 / f64::from(bpm);
        if !(frames_per_beat.is_finite() && frames_per_beat >= 1.0) {
            return Self::empty(sample_rate);
        }
        let span = total_frames.saturating_sub(first_beat_frame);
        // One beat per whole beat-length inside the track. Two beats is the floor: with a single
        // entry there is no interval to extrapolate from on a track shorter than one beat.
        let count = (span as f64 / frames_per_beat).floor() as usize;
        let beat_frames = (0..count.max(2) as u64)
            .map(|beat| first_beat_frame + (beat as f64 * frames_per_beat).round() as u64)
            .collect();
        Self {
            beat_frames,
            sample_rate,
        }
    }

    /// True when the grid holds no beats.
    pub fn is_empty(&self) -> bool {
        self.beat_frames.is_empty()
    }

    /// Frame position of `beat`, extending past the grid at the final measured interval.
    pub fn frame_at_beat(&self, beat: u64) -> u64 {
        let Some(last) = self.beat_frames.len().checked_sub(1) else {
            return 0;
        };
        if beat <= last as u64 {
            return self.beat_frames[beat as usize];
        }
        self.beat_frames[last] + self.last_interval() * (beat - last as u64)
    }

    /// Index of the beat at or before `frame`, counting on the extrapolated grid past the ends.
    pub fn floor_beat(&self, frame: u64) -> u64 {
        let Some(&first) = self.beat_frames.first() else {
            return 0;
        };
        let last = self.beat_frames.len() - 1;
        if frame < first {
            return 0;
        }
        let last_frame = self.beat_frames[last];
        if frame >= last_frame {
            return last as u64 + (frame - last_frame) / self.last_interval().max(1);
        }
        (self.beat_frames.partition_point(|&beat| beat <= frame) - 1) as u64
    }

    /// Position inside the current beat, `0.0` on the beat and approaching `1.0` before the next.
    pub fn phase(&self, frame: u64) -> f32 {
        if self.beat_frames.is_empty() {
            return 0.0;
        }
        let start = self.frame_at_beat(self.floor_beat(frame));
        let end = self.frame_at_beat(self.floor_beat(frame) + 1);
        if end <= start {
            return 0.0;
        }
        (frame.saturating_sub(start) as f32) / (end - start) as f32
    }

    /// Width of beat `beat` in frames, on the (possibly extrapolated) grid.
    pub fn beat_width(&self, beat: u64) -> u64 {
        if self.beat_frames.is_empty() {
            return 0;
        }
        self.frame_at_beat(beat + 1)
            .saturating_sub(self.frame_at_beat(beat))
    }

    /// BPM of the beat at `beat`, derived from that beat's own width — so a grid whose tempo
    /// changes reports the tempo in force *there*, while a constant grid reports the same number
    /// everywhere. `0.0` when the width degenerates (empty grid, zero sample rate).
    pub fn bpm_at_beat(&self, beat: u64) -> f32 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        let width = self.beat_width(beat);
        if width == 0 {
            return 0.0;
        }
        (60.0 * f64::from(self.sample_rate) / width as f64) as f32
    }

    /// BPM in force at `frame` — the width of the beat containing it, converted to a tempo.
    ///
    /// Sync reads this every block: the leader's tempo at the position it is actually at, not a
    /// track-wide average that would drift against a grid with tempo changes.
    pub fn bpm_at_frame(&self, frame: u64) -> f32 {
        self.bpm_at_beat(self.floor_beat(frame))
    }

    /// Mean BPM over the whole grid, the value a static tempo display shows.
    pub fn average_bpm(&self) -> f32 {
        let (Some(&first), Some(&last)) = (self.beat_frames.first(), self.beat_frames.last())
        else {
            return 0.0;
        };
        let beats = self.beat_frames.len() - 1;
        if last <= first {
            return 0.0;
        }
        self.bpm_from_frames(last - first, beats as u64)
    }

    fn last_interval(&self) -> u64 {
        let len = self.beat_frames.len();
        if len >= 2 {
            self.beat_frames[len - 1] - self.beat_frames[len - 2]
        } else {
            // A single beat tells us nothing about spacing; assume the nominal tempo.
            (f64::from(self.sample_rate.max(SAMPLE_RATE)) * 60.0 / 120.0).round() as u64
        }
    }

    fn bpm_from_frames(&self, frames: u64, beats: u64) -> f32 {
        if frames == 0 || beats == 0 || self.sample_rate == 0 {
            return 0.0;
        }
        (60.0 * f64::from(self.sample_rate) * f64::from(beats as u32) / f64::from(frames as u32))
            as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SAMPLE_RATE as SR;

    // 122 BPM @ 44.1kHz: 21688.52 frames per beat.
    const FPB: f64 = 44_100.0 * 60.0 / 122.0;

    fn grid(bpm: f32, total_frames: u64) -> BeatGrid {
        BeatGrid::from_constant_bpm(bpm, 0, total_frames, SR)
    }

    #[test]
    fn constant_bpm_spacing_is_absolute_and_monotonic() {
        let g = grid(122.0, 48_000 * 5); // 10 beats
        assert_eq!(g.beat_frames[0], 0);
        assert_eq!(g.beat_frames[1], FPB.round() as u64);
        assert_eq!(g.beat_frames[9], (9.0 * FPB).round() as u64);
        assert!(g.beat_frames.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn empty_grid_is_safe() {
        let g = grid(0.0, 48_000);
        assert!(g.is_empty());
        assert_eq!(g.frame_at_beat(3), 0);
        assert_eq!(g.floor_beat(1000), 0);
        assert_eq!(g.phase(1000), 0.0);
        assert_eq!(g.beat_width(0), 0);
        assert_eq!(g.average_bpm(), 0.0);
    }

    #[test]
    fn frame_at_beat_extrapolates_past_the_grid() {
        let g = grid(122.0, 44_100 * 5);
        let last = g.beat_frames.len() - 1;
        let interval = g.beat_frames[last] - g.beat_frames[last - 1];
        assert_eq!(
            g.frame_at_beat(last as u64 + 1),
            g.beat_frames[last] + interval
        );
        assert_eq!(
            g.frame_at_beat(last as u64 + 3),
            g.beat_frames[last] + 3 * interval
        );
    }

    #[test]
    fn floor_beat_clamps_and_extrapolates() {
        let g = grid(122.0, 48_000 * 5);
        assert_eq!(g.floor_beat(100), 0, "before the first beat");
        assert_eq!(g.floor_beat(FPB.round() as u64), 1);
        // No overflow far past the end of a 64-bit counter.
        let far = u64::MAX / 2;
        let floor = g.floor_beat(far);
        assert!(g.frame_at_beat(floor) <= far && far < g.frame_at_beat(floor + 1));
    }

    #[test]
    fn phase_runs_zero_to_one_within_a_beat() {
        let g = grid(122.0, 48_000 * 5);
        assert_eq!(g.phase(g.beat_frames[4]), 0.0);
        assert!((g.phase(g.beat_frames[4] + FPB as u64 / 2) - 0.5).abs() < 0.01);
        assert!(g.phase(g.beat_frames[4] + FPB as u64 - 1) > 0.98);
    }

    #[test]
    fn average_bpm_is_derived() {
        let g = grid(122.0, 48_000 * 60);
        assert!(
            (g.average_bpm() - 122.0).abs() < 0.1,
            "average {}",
            g.average_bpm()
        );
    }

    #[test]
    fn from_frames_uses_positions_verbatim() {
        let g = BeatGrid::from_frames(vec![0, 20_000, 40_000, 70_000], SR);
        assert_eq!((g.beat_width(0), g.beat_width(2)), (20_000, 30_000));
        assert!(
            (g.phase(10_000) - 0.5).abs() < 0.001,
            "uneven beats still report phase"
        );
    }

    #[test]
    fn bpm_at_frame_follows_the_beat_that_contains_the_position() {
        let constant = grid(122.0, 48_000 * 5);
        for probe in [0, 10_000, FPB as u64 + 1, 44_100 * 4] {
            let bpm = constant.bpm_at_frame(probe);
            assert!((bpm - 122.0).abs() < 0.01, "at {probe}: {bpm}");
        }

        // A grid that doubles its tempo: the reported BPM must follow the beat, not the average.
        let half = (f64::from(SR) * 60.0 / 122.0).round() as u64;
        let g = BeatGrid::from_frames(vec![0, half, half * 2, half * 2 + half / 2], SR);
        let slow = g.bpm_at_frame(0);
        let fast = g.bpm_at_frame(half * 2 + 1);
        assert!((slow - 122.0).abs() < 0.01, "first segment: {slow}");
        assert!((fast - 244.0).abs() < 0.1, "second segment: {fast}");
    }

    #[test]
    fn bpm_at_frame_is_zero_without_a_grid() {
        let g = grid(0.0, 48_000);
        assert_eq!(g.bpm_at_beat(0), 0.0);
        assert_eq!(g.bpm_at_frame(44_100), 0.0);
    }
}
