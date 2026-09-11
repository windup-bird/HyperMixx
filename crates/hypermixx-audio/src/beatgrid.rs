//! BeatGrid: absolute beat positions on the frame grid.
//!
//! The grid stores one absolute frame position per beat. Intervals are *not* stored, so rounding
//! can never accumulate across a track, and BPM is a derived value rather than a second source of
//! truth that could drift out of sync with the grid.

use crate::SAMPLE_RATE;

/// Everything the deck knows about a track's musical analysis.
#[derive(Debug, Clone)]
pub struct TrackAnalysis {
    pub beatgrid: BeatGrid,
    pub key: Option<KeyReport>,
    pub bpm: Option<f32>,
}

/// Detected musical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    Major,
    Minor,
}

/// A key detection result: pitch class + mode + confidence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyReport {
    /// 0 = C, 1 = C#, ..., 11 = B.
    pub pc: u8,
    pub mode: KeyMode,
    /// 0.0–1.0, from the analyser.
    pub confidence: f32,
}

impl KeyReport {
    /// Standard name: `"C"`, `"Am"`, `"F#"`.
    pub fn name(&self) -> String {
        const PITCH_NAMES: [&str; 12] = [
            "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
        ];
        let root = PITCH_NAMES[self.pc as usize % 12];
        match self.mode {
            KeyMode::Major => root.to_owned(),
            KeyMode::Minor => format!("{root}m"),
        }
    }
}

/// Strictly increasing absolute beat positions, in frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeatGrid {
    /// Absolute frame position of every beat (strictly increasing).
    pub beat_frames: Vec<u64>,
    pub sample_rate: u32,
}

impl BeatGrid {
    /// Builds a grid from beat times in seconds (e.g. from an external analyser).
    pub fn from_seconds(beats_sec: &[f32], sample_rate: u32) -> Self {
        let beat_frames = beats_sec
            .iter()
            .map(|t| (*t * sample_rate as f32).round().max(0.0) as u64)
            .collect();
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
            return Self {
                beat_frames: Vec::new(),
                sample_rate,
            };
        }
        let frames_per_beat = f64::from(sample_rate) * 60.0 / f64::from(bpm);
        if !(frames_per_beat.is_finite() && frames_per_beat >= 1.0) {
            return Self {
                beat_frames: Vec::new(),
                sample_rate,
            };
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

    /// Index of the beat closest to `frame`. Before the first beat the answer is beat 0; past the
    /// stored grid the beat index keeps counting on the extrapolated grid.
    pub fn nearest_beat(&self, frame: u64) -> u64 {
        if self.beat_frames.is_empty() {
            return 0;
        }
        let index = self.floor_beat(frame);
        let at = self.frame_at_beat(index);
        let next = self.frame_at_beat(index + 1);
        if frame - at <= next - frame {
            index
        } else {
            index + 1
        }
    }

    /// Position of the beat at or before `frame` (the grid's first beat when `frame` precedes it).
    pub fn current_beat_frame(&self, frame: u64) -> u64 {
        if self.beat_frames.is_empty() {
            return 0;
        }
        self.frame_at_beat(self.floor_beat(frame))
    }

    /// Position of the next beat after `frame`, extrapolated when the grid runs out.
    pub fn next_beat_frame(&self, frame: u64) -> u64 {
        if self.beat_frames.is_empty() {
            return frame;
        }
        self.frame_at_beat(self.floor_beat(frame) + 1)
    }

    /// Index of the beat at or before `frame`, counting on the extrapolated grid past the end.
    fn floor_beat(&self, frame: u64) -> u64 {
        let last = self.beat_frames.len() - 1;
        if frame < self.beat_frames[0] {
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
        let start = self.current_beat_frame(frame);
        let end = self.next_beat_frame(frame);
        if end <= start {
            return 0.0;
        }
        (frame.saturating_sub(start) as f32) / (end - start) as f32
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

    /// Local BPM, measured between beat `beat` and the one after it.
    pub fn bpm_at_beat(&self, beat: u64) -> f32 {
        let frames = match self.beat_frames.len() {
            0 => return 0.0,
            1 => return self.average_bpm(),
            len => {
                // Measure forward when possible, otherwise fall back to the previous interval.
                let index = (beat as usize).min(len - 1);
                if index + 1 < len {
                    self.beat_frames[index + 1] - self.beat_frames[index]
                } else {
                    self.beat_frames[index] - self.beat_frames[index - 1]
                }
            }
        };
        self.bpm_from_frames(frames, 1)
    }

    /// Number of beats stored in the grid.
    pub fn beat_count(&self) -> usize {
        self.beat_frames.len()
    }

    /// Width of beat `beat` in frames, on the (possibly extrapolated) grid.
    pub fn beat_width(&self, beat: u64) -> u64 {
        if self.beat_frames.is_empty() {
            return 0;
        }
        self.frame_at_beat(beat + 1)
            .saturating_sub(self.frame_at_beat(beat))
    }

    /// Target frame of a phase-preserving jump of `beats` beats (negative goes back).
    ///
    /// The current beat plus `beats` selects the target beat, and the current phase is reused as a
    /// *relative* spot inside it — `phase × beat_width(target)` — so jumping from 30% into one beat
    /// lands 30% into the target beat even when the analysed grid's beats are not equally long.
    pub fn beatjump_target(&self, current_frame: u64, beats: i64) -> u64 {
        if self.beat_frames.is_empty() {
            return current_frame;
        }
        let target = (self.floor_beat(current_frame) as i64 + beats).max(0) as u64;
        let offset =
            (f64::from(self.phase(current_frame)) * self.beat_width(target) as f64).round() as u64;
        self.frame_at_beat(target) + offset
    }

    fn last_interval(&self) -> u64 {
        let len = self.beat_frames.len();
        if len >= 2 {
            self.beat_frames[len - 1] - self.beat_frames[len - 2]
        } else {
            // A single beat tells us nothing about spacing; assume the nominal 48kHz tempo.
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

impl Default for BeatGrid {
    fn default() -> Self {
        Self {
            beat_frames: Vec::new(),
            sample_rate: SAMPLE_RATE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 122 BPM @ 48kHz: 23606.56 frames per beat.
    fn grid(bpm: f32, total_frames: u64) -> BeatGrid {
        BeatGrid::from_constant_bpm(bpm, 0, total_frames, SAMPLE_RATE)
    }

    const FPB: f64 = 48_000.0 * 60.0 / 122.0;

    #[test]
    fn constant_bpm_spacing_is_absolute_and_monotonic() {
        let g = grid(122.0, 48_000 * 5);
        assert_eq!(
            g.beat_count(),
            10,
            "one beat per whole beat-length inside the track"
        );
        assert_eq!(g.beat_frames[0], 0);
        assert_eq!(g.beat_frames[1], FPB.round() as u64);
        assert_eq!(g.beat_frames[9], (9.0 * FPB).round() as u64);
        assert!(
            g.beat_frames.windows(2).all(|w| w[0] < w[1]),
            "beat positions must increase"
        );
    }

    #[test]
    fn degenerate_grids_are_empty_and_safe() {
        for bpm in [0.0f32, -1.0, f32::NAN] {
            assert_eq!(grid(bpm, 48_000).beat_count(), 0, "bpm {bpm}");
        }
        let empty = grid(0.0, 48_000);
        assert_eq!(empty.frame_at_beat(3), 0);
        assert_eq!(empty.nearest_beat(1000), 0);
        assert_eq!(empty.current_beat_frame(1000), 0);
        assert_eq!(empty.next_beat_frame(1000), 1000);
        assert_eq!(empty.phase(1000), 0.0);
        assert_eq!(empty.average_bpm(), 0.0);
        assert_eq!(empty.bpm_at_beat(0), 0.0);
        assert_eq!(
            empty.beatjump_target(5000, 4),
            5000,
            "no grid means no movement"
        );
        assert_eq!(empty.beat_width(0), 0);
    }

    #[test]
    fn sub_beat_track_still_gets_a_usable_grid() {
        let g = grid(122.0, 5_000); // shorter than one beat
        assert_eq!(
            g.beat_count(),
            2,
            "the floor of two beats keeps the interval known"
        );
        assert_eq!(g.frame_at_beat(1), FPB.round() as u64);
        assert!((g.average_bpm() - 122.0).abs() < 0.5);
        assert_eq!(g.beatjump_target(1_000, 1), FPB.round() as u64 + 1_000);
    }

    #[test]
    fn frame_at_beat_extrapolates_past_the_grid() {
        let g = grid(122.0, 48_000 * 5); // beats 0..=9
        let interval = g.beat_frames[9] - g.beat_frames[8];
        assert_eq!(g.frame_at_beat(9), g.beat_frames[9]);
        assert_eq!(g.frame_at_beat(10), g.beat_frames[9] + interval);
        assert_eq!(g.frame_at_beat(12), g.beat_frames[9] + 3 * interval);
    }

    #[test]
    fn nearest_beat_handles_both_grid_ends() {
        let g = grid(122.0, 48_000 * 5);
        assert_eq!(g.nearest_beat(0), 0);
        assert_eq!(
            g.nearest_beat(100),
            0,
            "before the first beat still maps to beat 0"
        );
        assert_eq!(g.nearest_beat(FPB.round() as u64), 1);
        assert_eq!(
            g.nearest_beat(FPB as u64 / 2),
            0,
            "half a beat in is still nearest to 0"
        );
        assert_eq!(g.nearest_beat(FPB as u64 / 2 + 100), 1);
        // Past the stored grid the index keeps counting on the extrapolated beats.
        let last = g.beat_count() as u64 - 1;
        assert_eq!(g.nearest_beat(g.beat_frames[last as usize]), last);
        assert_eq!(g.nearest_beat(g.frame_at_beat(last + 2)), last + 2);
        // No overflow even far past the end of a 64-bit frame counter.
        let far = u64::MAX / 2;
        let floor = g.floor_beat(far);
        assert!(g.frame_at_beat(floor) <= far && far < g.frame_at_beat(floor + 1));
    }

    #[test]
    fn current_and_next_beat_frame_bracket_the_frame() {
        let g = grid(122.0, 48_000 * 5);
        let frame = (2.5 * FPB) as u64;
        assert_eq!(g.current_beat_frame(frame), g.beat_frames[2]);
        assert_eq!(g.next_beat_frame(frame), g.beat_frames[3]);
        // Exactly on a beat: that beat is current, the next one is ahead.
        assert_eq!(g.current_beat_frame(g.beat_frames[3]), g.beat_frames[3]);
        assert_eq!(g.next_beat_frame(g.beat_frames[3]), g.beat_frames[4]);
        assert_eq!(g.next_beat_frame(g.beat_frames[9]), g.frame_at_beat(10));
    }

    #[test]
    fn phase_runs_zero_to_one_within_a_beat() {
        let g = grid(122.0, 48_000 * 5);
        assert_eq!(g.phase(g.beat_frames[4]), 0.0);
        assert!((g.phase(g.beat_frames[4] + FPB as u64 / 2) - 0.5).abs() < 0.01);
        assert!(g.phase(g.beat_frames[4] + FPB as u64 - 1) > 0.98);
        assert!((0.2..0.25).contains(&g.phase(g.beat_frames[4] + (0.22 * FPB) as u64)));
    }

    #[test]
    fn bpm_is_derived_from_the_grid() {
        let g = grid(122.0, 48_000 * 60);
        assert!(
            (g.average_bpm() - 122.0).abs() < 0.1,
            "average {}",
            g.average_bpm()
        );
        assert!(
            (g.bpm_at_beat(0) - 122.0).abs() < 0.5,
            "first {}",
            g.bpm_at_beat(0)
        );
        assert!(
            (g.bpm_at_beat(g.beat_count() as u64 - 1) - 122.0).abs() < 0.5,
            "last beat"
        );
        // Beyond the grid the final interval still defines the local tempo.
        assert!((g.bpm_at_beat(10_000) - 122.0).abs() < 0.5);
    }

    #[test]
    fn beatjump_moves_whole_beats() {
        let g = grid(122.0, 48_000 * 5);
        assert_eq!(g.beatjump_target(0, 4), g.frame_at_beat(4));
        assert_eq!(
            g.beatjump_target(g.frame_at_beat(4), -4),
            g.frame_at_beat(0)
        );
    }

    #[test]
    fn beatjump_keeps_phase_inside_the_beat() {
        let g = grid(122.0, 48_000 * 5);
        let offset = 5_000; // somewhere inside beat 0
        assert_eq!(g.beatjump_target(offset, 4) - g.frame_at_beat(4), offset);
        assert_eq!(
            g.beatjump_target(offset, -4),
            offset,
            "cannot go before beat 0"
        );
        // Mid-beat 2: the floor is beat 2, so +1 lands a beat later at the same relative spot.
        let frame = g.beat_frames[2] + 1_000;
        let forward = g.beatjump_target(frame, 1);
        assert_eq!(forward, g.frame_at_beat(3) + 1_000);
        assert!(
            (g.phase(forward) - g.phase(frame)).abs() < 0.001,
            "phase must survive the jump"
        );
        assert_eq!(g.beatjump_target(forward, -1), frame);
    }

    #[test]
    fn beatjump_scales_phase_into_the_target_beat() {
        // A hand-made uneven grid: beats 0 and 1 are 20k frames, beat 2 is 30k frames wide.
        let g = BeatGrid {
            beat_frames: vec![0, 20_000, 40_000, 70_000],
            sample_rate: SAMPLE_RATE,
        };
        assert_eq!((g.beat_width(0), g.beat_width(2)), (20_000, 30_000));

        // Halfway into beat 0 (phase 0.5) ...
        let frame = 10_000;
        assert!((g.phase(frame) - 0.5).abs() < 0.001);
        // ... two beats ahead is halfway into beat 2, i.e. 15k frames in, not 10k.
        assert_eq!(g.beatjump_target(frame, 2), 40_000 + 15_000);
        assert!((g.phase(g.beatjump_target(frame, 2)) - 0.5).abs() < 0.001);
        // Carrying the raw frame offset instead would have landed at 50_000, 17% off the beat.
        assert_ne!(g.beatjump_target(frame, 2), 40_000 + 10_000);

        // And back the same way, into the shorter beat.
        let at = g.beatjump_target(frame, 2);
        assert_eq!(g.beatjump_target(at, -2), 10_000);
    }

    #[test]
    fn beatjump_round_trips_from_an_off_grid_cue() {
        let g = grid(122.0, 48_000 * 5);
        let frame = (6.7 * FPB).round() as u64;
        assert_eq!(g.beatjump_target(g.beatjump_target(frame, 8), -8), frame);
    }
}
