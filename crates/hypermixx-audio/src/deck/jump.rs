//! Seek semantics: turn a musical intent into a target frame. No flow switching here — the deck
//! owns that. Everything reduces to `BeatGrid` queries.

use hypermixx_core::BeatGrid;

/// A seek expressed in musical or absolute terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seek {
    /// Jump straight to a frame.
    Frames(u64),
    /// Move a relative number of beats, keeping the position inside the beat (phase-preserving).
    Beats(i64),
    /// Land on the head of an absolute beat index.
    Beat(u64),
    /// Snap an arbitrary frame to the nearest beat head.
    Quantized { frame: u64 },
}

/// Resolves `seek` to an absolute target frame given the current position. `None` for an empty grid
/// or a beat index the grid cannot represent.
pub fn resolve(seek: &Seek, grid: &BeatGrid, current: u64) -> Option<u64> {
    match seek {
        Seek::Frames(frame) => Some(*frame),
        Seek::Beats(beats) => phase_preserving(grid, current, *beats),
        Seek::Beat(beat) => {
            if grid.is_empty() {
                None
            } else {
                Some(grid.frame_at_beat(*beat))
            }
        }
        Seek::Quantized { frame } => {
            if grid.is_empty() {
                return None;
            }
            let floor = grid.floor_beat(*frame);
            let here = grid.frame_at_beat(floor);
            let next = grid.frame_at_beat(floor + 1);
            // Snap to whichever beat head is closer.
            Some(if next - *frame < *frame - here {
                next
            } else {
                here
            })
        }
    }
}

/// Phase-preserving beat jump: from 30% into the current beat, `+n` lands 30% into the target
/// beat, even when beats are not equally long. Returns `None` for an empty grid.
pub fn phase_preserving(grid: &BeatGrid, current: u64, beats: i64) -> Option<u64> {
    if grid.is_empty() {
        return None;
    }
    let target = (grid.floor_beat(current) as i64 + beats).max(0) as u64;
    let offset = (f64::from(grid.phase(current)) * grid.beat_width(target) as f64).round() as u64;
    Some(grid.frame_at_beat(target) + offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_core::SAMPLE_RATE;

    // 122 BPM @ 48k => 23606.56 frames/beat, grid over a 5s clip.
    fn grid() -> BeatGrid {
        BeatGrid::from_constant_bpm(122.0, 0, 48_000 * 5, SAMPLE_RATE)
    }

    #[test]
    fn frames_seek_is_passthrough() {
        assert_eq!(resolve(&Seek::Frames(12345), &grid(), 0), Some(12345));
    }

    #[test]
    fn beats_land_on_beat_heads_from_a_cue() {
        let g = grid();
        assert_eq!(resolve(&Seek::Beats(4), &g, 0), Some(g.frame_at_beat(4)));
        // From beat 4's head, +4 beats lands on beat 8's head.
        assert_eq!(
            resolve(&Seek::Beats(4), &g, g.frame_at_beat(4)),
            Some(g.frame_at_beat(8))
        );
    }

    #[test]
    fn negative_beats_clamp_at_the_first_beat() {
        let g = grid();
        assert_eq!(resolve(&Seek::Beats(-4), &g, 5_000), Some(5_000));
    }

    #[test]
    fn phase_survives_the_jump() {
        let g = grid();
        let cue = g.frame_at_beat(2) + 1_000;
        let forward = resolve(&Seek::Beats(1), &g, cue).unwrap();
        assert_eq!(forward, g.frame_at_beat(3) + 1_000);
        assert!((g.phase(forward) - g.phase(cue)).abs() < 0.001);
        assert_eq!(resolve(&Seek::Beats(-1), &g, forward), Some(cue));
    }

    #[test]
    fn beat_seek_targets_the_head() {
        let g = grid();
        assert_eq!(resolve(&Seek::Beat(3), &g, 0), Some(g.frame_at_beat(3)));
    }

    #[test]
    fn quantized_snaps_to_the_closer_head() {
        let g = grid();
        let just_after = g.frame_at_beat(3) + 100;
        assert_eq!(
            resolve(&Seek::Quantized { frame: just_after }, &g, 0),
            Some(g.frame_at_beat(3))
        );
        let just_before = g.frame_at_beat(4) - 100;
        assert_eq!(
            resolve(&Seek::Quantized { frame: just_before }, &g, 0),
            Some(g.frame_at_beat(4))
        );
    }

    #[test]
    fn empty_grid_resolves_nothing_but_frames() {
        let g = BeatGrid::empty(SAMPLE_RATE);
        assert_eq!(resolve(&Seek::Beats(4), &g, 100), None);
        assert_eq!(resolve(&Seek::Beat(2), &g, 100), None);
        assert_eq!(resolve(&Seek::Quantized { frame: 100 }, &g, 100), None);
    }
}
