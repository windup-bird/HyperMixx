//! Turns an editable [`BeatSpec`] into a runtime [`BeatGrid`] of absolute frame positions.
//!
//! Three stages, matching the pipeline the analyser feeds:
//! 1. **front-fill** — extrapolate the first segment's phase back to frame 0 so a track whose first
//!    downbeat is late still has beat positions from the top;
//! 2. **even spacing** — lay each segment out at its own BPM within its frame span;
//! 3. **boundary dedup + tail-fill** — drop beats a segment hands off too closely to the next,
//!    then extend the final segment to the end of the track.

use hypermixx_core::BeatGrid;

use crate::beat_spec::BeatSpec;

/// Compiles specs against a fixed track geometry.
#[derive(Clone, Copy, Debug)]
pub struct GridCompiler {
    pub sample_rate: u32,
    pub total_frames: u64,
    /// A generated beat is dropped if it lands within `gap < min(prev, cur) * dedup_ratio` of the
    /// previous one — kills the double beat two adjacent segments can produce at their seam.
    pub dedup_ratio: f64,
}

impl GridCompiler {
    pub fn new(sample_rate: u32, total_frames: u64) -> Self {
        Self {
            sample_rate,
            total_frames,
            dedup_ratio: 0.9,
        }
    }

    fn frames_per_beat(&self, bpm: f64) -> f64 {
        f64::from(self.sample_rate) * 60.0 / bpm
    }

    /// Compiles `spec` into absolute, strictly-increasing beat frames.
    pub fn compile(&self, spec: &BeatSpec) -> BeatGrid {
        let first = match spec.segments.first() {
            Some(seg) => *seg,
            None => return BeatGrid::empty(self.sample_rate),
        };

        let mut beats: Vec<u64> = Vec::new();

        // 1. front-fill: negative beats from the first segment back toward frame 0.
        let fpb0 = self.frames_per_beat(first.bpm);
        if fpb0 >= 1.0 {
            let back = (first.start_frame as f64 / fpb0).ceil() as i64;
            for k in (1..=back).rev() {
                let frame = first.start_frame as i64 - (k as f64 * fpb0).round() as i64;
                if frame >= 0 {
                    beats.push(frame as u64);
                }
            }
        }

        // 2. even spacing per segment, clipped to the next segment's start (or track end).
        for (i, seg) in spec.segments.iter().enumerate() {
            let fpb = self.frames_per_beat(seg.bpm);
            if fpb < 1.0 {
                continue;
            }
            let end = spec
                .segments
                .get(i + 1)
                .map(|n| n.start_frame)
                .unwrap_or(self.total_frames)
                .min(self.total_frames);
            let span_beats = (((end.saturating_sub(seg.start_frame)) as f64) / fpb).floor() as u64;
            let count = seg.beats.unwrap_or(span_beats).min(span_beats);
            for k in 0..=count {
                beats.push(seg.start_frame + (k as f64 * fpb).round() as u64);
            }

            // 3a. tail-fill only after the last segment: extend at its tempo to the track end.
            if i + 1 == spec.segments.len() {
                let mut k = count + 1;
                loop {
                    let frame = seg.start_frame + (k as f64 * fpb).round() as u64;
                    if frame > self.total_frames {
                        break;
                    }
                    beats.push(frame);
                    k += 1;
                }
            }
        }

        // Sort, clamp, and dedup at segment seams.
        beats.sort_unstable();
        let mut out: Vec<u64> = Vec::with_capacity(beats.len());
        for frame in beats {
            if frame > self.total_frames {
                break;
            }
            if let Some(&prev) = out.last() {
                // Local spacing: compare against whichever segment's beat width is smaller.
                let min_gap = (self.frames_per_beat(seg_bpm_at(spec, prev)) * self.dedup_ratio)
                    .min(self.frames_per_beat(seg_bpm_at(spec, frame)) * self.dedup_ratio)
                    as u64;
                if frame - prev < min_gap.max(1) {
                    continue;
                }
                if frame == prev {
                    continue;
                }
            }
            out.push(frame);
        }
        BeatGrid::from_frames(out, self.sample_rate)
    }
}

/// The BPM of the segment containing `frame` (last segment past the final start).
fn seg_bpm_at(spec: &BeatSpec, frame: u64) -> f64 {
    let mut bpm = spec.segments[0].bpm;
    for seg in &spec.segments {
        if seg.start_frame <= frame {
            bpm = seg.bpm;
        } else {
            break;
        }
    }
    bpm
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beat_spec::Segment;
    use hypermixx_core::SAMPLE_RATE;

    fn compiler(total: u64) -> GridCompiler {
        GridCompiler::new(SAMPLE_RATE, total)
    }

    // 122 BPM @ 48k => 23606.56 frames/beat.
    const FPB: f64 = 48_000.0 * 60.0 / 122.0;

    #[test]
    fn rigid_single_segment() {
        let spec = BeatSpec::rigid(122.0, 0);
        let g = compiler(48_000 * 5).compile(&spec);
        assert!(!g.is_empty());
        assert_eq!(g.beat_frames[0], 0);
        assert_eq!(g.beat_frames[1], FPB.round() as u64);
        // Tail covered to the end.
        assert!(*g.beat_frames.last().unwrap() <= 48_000 * 5);
        assert!(
            g.beat_frames.windows(2).all(|w| w[0] < w[1]),
            "strictly increasing"
        );
    }

    #[test]
    fn front_fill_covers_a_late_downbeat() {
        // First real beat at 3 seconds, but beats exist from frame 0.
        let spec = BeatSpec::rigid(122.0, 3 * 48_000);
        let g = compiler(48_000 * 10).compile(&spec);
        assert!(g.beat_frames[0] < g.beat_frames[1]);
        assert!(
            g.beat_frames[0] < 48_000,
            "front-fill should start near the top, got {}",
            g.beat_frames[0]
        );
    }

    #[test]
    fn dynamic_is_many_one_beat_segments() {
        // A "dynamic" grid is just per-beat segments; compile must still yield one beat each.
        let base = FPB.round() as u64;
        let spec = BeatSpec {
            segments: (0..6)
                .map(|i| Segment {
                    bpm: 122.0,
                    start_frame: i * base,
                    beats: Some(1),
                })
                .collect(),
        };
        let g = compiler(6 * base).compile(&spec);
        // Six beats, roughly one beat-width apart, no seams doubled out to fewer than 5.
        assert!(
            g.beat_frames.len() >= 5,
            "expected ≥5 beats, got {}",
            g.beat_frames.len()
        );
        assert!(g.beat_frames.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn mixed_segments_change_tempo_at_the_boundary() {
        let half = 48_000 * 5;
        let spec = BeatSpec {
            segments: vec![
                Segment {
                    bpm: 120.0,
                    start_frame: 0,
                    beats: None,
                },
                Segment {
                    bpm: 128.0,
                    start_frame: half,
                    beats: None,
                },
            ],
        };
        let g = compiler(48_000 * 10).compile(&spec);
        // Before the seam: ~0.5s beats; after: slightly tighter. Widths shrink across boundary.
        let i = g.beat_frames.iter().position(|&f| f >= half).unwrap();
        let before = g.beat_frames[i] - g.beat_frames[i - 1];
        let after = *g.beat_frames.last().unwrap() - g.beat_frames[g.beat_frames.len() - 2];
        assert!(
            after < before,
            "128 BPM beats ({after}) tighter than 120 BPM ({before})"
        );
    }

    #[test]
    fn seam_produces_no_duplicate_beat() {
        // Second segment starts exactly where the first's beat would land: only one beat there.
        let start2 = FPB.round() as u64 * 4;
        let spec = BeatSpec {
            segments: vec![
                Segment {
                    bpm: 122.0,
                    start_frame: 0,
                    beats: Some(4),
                },
                Segment {
                    bpm: 122.0,
                    start_frame: start2,
                    beats: None,
                },
            ],
        };
        let g = compiler(48_000 * 10).compile(&spec);
        assert!(
            g.beat_frames.iter().filter(|&&f| f == start2).count() <= 1,
            "double beat at seam"
        );
    }

    #[test]
    fn empty_spec_yields_empty_grid() {
        let g = compiler(48_000).compile(&BeatSpec::default());
        assert!(g.is_empty());
    }
}
