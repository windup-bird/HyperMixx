//! Loop state: the `virtual → actual` position mapping, its atomic cell, the Source wrapper that
//! reads *through* the mapping, and the beat quantization the manual/beat loop paths share.
//!
//! The whole loop feature reduces to one formula. `virtual_pos` (the engine playhead) only ever
//! increases; what the listener hears is `map(virtual_pos)`:
//!
//! ```text
//! None        → actual = virtual
//! Some(in,out)→ virtual < out ? virtual : in + (virtual - in) % (out - in)
//! ```
//!
//! The engine never learns a loop exists — it just keeps pulling positions forward while
//! [`LoopSource`] folds them back into the range, so a wrap is a splice *inside the fed stream*
//! rather than a re-seek. Keylock's overlap windows cross that splice like any other content
//! discontinuity: no reset, no transient, and the slipped `virtual_pos` keeps running for free.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use hypermixx_core::{BeatGrid, LoopQuantum, Source, CHANNELS};

/// A loop as a half-open frame range `[in_frame, out_frame)`, `out > in`, `out ≤` track end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopRange {
    pub in_frame: u64,
    /// Exclusive.
    pub out_frame: u64,
}

impl LoopRange {
    /// A range. Callers must pass `out > in`; a degenerate range is dropped by
    /// [`LoopRangeCell::store`] rather than mapped (a zero-width modulo would never terminate).
    pub fn new(in_frame: u64, out_frame: u64) -> Self {
        Self {
            in_frame,
            out_frame,
        }
    }

    /// Length in frames (`0` only for a degenerate range the cell refuses to store).
    pub fn len(&self) -> u64 {
        self.out_frame.saturating_sub(self.in_frame)
    }

    pub fn is_empty(&self) -> bool {
        self.out_frame <= self.in_frame
    }

    /// The mapping formula. Identity below `out` (the first pass plays through), modulo above it.
    pub fn map(&self, virtual_frame: u64) -> u64 {
        if virtual_frame < self.out_frame {
            return virtual_frame;
        }
        let span = self.out_frame - self.in_frame;
        debug_assert!(span > 0, "a degenerate range must never reach the mapping");
        if span == 0 {
            return virtual_frame;
        }
        self.in_frame + (virtual_frame - self.in_frame) % span
    }

    /// The read segment starting at `virtual_frame`: where in the source it lands and how many
    /// frames it may cover before hitting the `out` boundary. Splitting here is what makes a
    /// wrap-splitting read exact for any buffer size and any loop length.
    pub fn segment(&self, virtual_frame: u64, want: usize) -> (u64, usize) {
        let actual = self.map(virtual_frame);
        debug_assert!(actual < self.out_frame, "map() must land inside the range");
        let to_boundary = self.out_frame - actual;
        (actual, (want as u64).min(to_boundary) as usize)
    }
}

/// The loop range of one flow: two `u64`s a producer thread stores whole.
///
/// "Atomic update" is literal — a writer replaces the entire range (in, then out) in one call,
/// so the reader on the feed path never sees a half-moved range: a length change, a shift and a
/// single-ended move are all just a new [`LoopRange`] stored here. Single writer, single reader,
/// both on the producer thread, hence `Relaxed` and no lock on the hot path.
///
/// `out_frame == 0` encodes "no loop": a live range always has `out > in ≥ 0`, so `out ≥ 1`.
pub struct LoopRangeCell {
    in_frame: AtomicU64,
    out_frame: AtomicU64,
}

impl LoopRangeCell {
    pub fn new() -> Self {
        Self {
            in_frame: AtomicU64::new(0),
            out_frame: AtomicU64::new(0),
        }
    }

    pub fn store(&self, range: Option<LoopRange>) {
        match range {
            // `out ≤ in` is degenerate (a zero-width modulo): drop it instead of storing a range
            // that could never terminate a read.
            Some(range) if !range.is_empty() => {
                self.in_frame.store(range.in_frame, Ordering::Relaxed);
                self.out_frame.store(range.out_frame, Ordering::Relaxed);
            }
            // Disable first (`out = 0`), so no reader can observe in from a stale range.
            _ => {
                self.out_frame.store(0, Ordering::Relaxed);
                self.in_frame.store(0, Ordering::Relaxed);
            }
        }
    }

    pub fn load(&self) -> Option<LoopRange> {
        let out = self.out_frame.load(Ordering::Relaxed);
        if out == 0 {
            return None;
        }
        Some(LoopRange {
            in_frame: self.in_frame.load(Ordering::Relaxed),
            out_frame: out,
        })
    }

    pub fn map(&self, virtual_frame: u64) -> u64 {
        self.load().map_or(virtual_frame, |range| range.map(virtual_frame))
    }
}

impl Default for LoopRangeCell {
    fn default() -> Self {
        Self::new()
    }
}

/// A `Source` that serves *virtual* positions, folding them through a [`LoopRangeCell`].
///
/// The engine's feed holds a handle to the same cell, so a range edit takes effect on the next
/// block without touching the engine. `total_frames` reports `u64::MAX` while a range is set:
/// a looped flow's virtual clock runs forever (laps), and the feed's end-of-track check must
/// never stop it — every read lands back inside the track through `map()`.
pub struct LoopSource {
    inner: Arc<dyn Source>,
    cell: Arc<LoopRangeCell>,
}

impl LoopSource {
    pub fn new(inner: Arc<dyn Source>, cell: Arc<LoopRangeCell>) -> Arc<Self> {
        Arc::new(Self { inner, cell })
    }

    /// The range handle shared with the flow (edits) and the engine (wrap re-anchoring).
    pub fn cell(&self) -> Arc<LoopRangeCell> {
        Arc::clone(&self.cell)
    }

    /// The un-mapped PCM below, for out-of-band readers that want raw track positions.
    pub fn inner(&self) -> Arc<dyn Source> {
        Arc::clone(&self.inner)
    }
}

impl Source for LoopSource {
    fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize {
        let Some(range) = self.cell.load() else {
            return self.inner.read_frames(start_frame, output);
        };
        let want = output.len() / CHANNELS;
        let mut filled = 0usize;
        let mut virtual_pos = start_frame;
        while filled < want {
            let (actual, segment) = range.segment(virtual_pos, want - filled);
            let read = self.inner.read_frames(
                actual,
                &mut output[filled * CHANNELS..(filled + segment) * CHANNELS],
            );
            filled += read;
            if read < segment {
                // The inner source ran dry (only possible past a mis-clamped `out`): report what
                // we have rather than spinning on a zero-length segment.
                break;
            }
            virtual_pos += segment as u64;
        }
        filled
    }

    fn total_frames(&self) -> u64 {
        if self.cell.load().is_some() {
            u64::MAX
        } else {
            self.inner.total_frames()
        }
    }
}

// -------------------------------------------------------------------------------------- quantize

/// The nearest beat head to `frame`. `None` on an empty grid — a loop without a grid has nothing
/// to quantize against, and a wrong grid is worse than no loop (same rule as `beatjump`).
pub fn quantize_to_beat(grid: &BeatGrid, frame: u64) -> Option<u64> {
    if grid.is_empty() {
        return None;
    }
    let floor = grid.floor_beat(frame);
    let here = grid.frame_at_beat(floor);
    let next = grid.frame_at_beat(floor + 1);
    Some(if next - frame < frame - here { next } else { here })
}

/// The out point: `in_frame + round((current - in) / quantum) * quantum`, i.e. the offset from
/// the in point snapped to whole quanta of the grid.
///
/// A degenerate result (`out ≤ in`: an early press, or rounding to zero) falls back to exactly
/// **one beat**, per the loop spec's boundary table. Empty grid → `None`.
pub fn quantize_offset(
    grid: &BeatGrid,
    in_frame: u64,
    current: u64,
    quantum: LoopQuantum,
) -> Option<u64> {
    if grid.is_empty() {
        return None;
    }
    let beat = grid.beat_width(grid.floor_beat(in_frame));
    if beat == 0 {
        return None;
    }
    let quantum_frames = ((beat as f64) * quantum.beats()).round().max(1.0) as u64;
    let delta = current as f64 - in_frame as f64;
    let steps = (delta / quantum_frames as f64).round() as i64;
    let out = steps
        .checked_mul(quantum_frames as i64)
        .and_then(|offset| (in_frame as i64).checked_add(offset))?;
    Some(if out <= in_frame as i64 {
        // out ≤ in → the minimum loop: one beat.
        in_frame + beat
    } else {
        out as u64
    })
}

/// The range a *pending* loop would have if `loop out` were pressed with the playhead at
/// `position`: `quantize_offset(p_in, position, quantum)`, clamped to the track.
///
/// This is "the out point if pressed now" — the formula the LoopFlow's lockstep driver stores
/// before every chunk and the exact expression `loop_out` stores at the press, which is what
/// makes the promotion a no-op on the fed-but-unheard window. `None` on a degenerate result
/// (no grid, or a range that collapsed against the track end).
pub fn provisional_range(
    grid: &BeatGrid,
    in_frame: u64,
    position: u64,
    quantum: LoopQuantum,
    total: u64,
) -> Option<LoopRange> {
    let out = quantize_offset(grid, in_frame, position, quantum)?.min(total);
    (out > in_frame).then_some(LoopRange::new(in_frame, out))
}

/// The frame `beats` whole beats past `in_frame` on the grid (for beat loops and length edits).
/// Non-uniform grids included: this is an exact grid distance, not `beats × nominal fpb`.
pub fn beats_after(grid: &BeatGrid, in_frame: u64, beats: u64) -> Option<u64> {
    if grid.is_empty() || beats == 0 {
        return None;
    }
    let floor = grid.floor_beat(in_frame);
    let next = floor.checked_add(beats)?;
    Some(grid.frame_at_beat(next))
}

/// The frame `beats` (may be negative) from `base`, as a grid distance in frames. `beats == 0`
/// returns `base`, which makes a zero shift an identity edit rather than an error.
pub fn beat_shift(grid: &BeatGrid, base: u64, beats: i64) -> Option<u64> {
    if grid.is_empty() {
        return None;
    }
    if beats == 0 {
        return Some(base);
    }
    let floor = grid.floor_beat(base);
    let target = if beats > 0 {
        floor.checked_add(beats as u64)?
    } else {
        floor.checked_sub(beats.unsigned_abs())?
    };
    // Keep the phase inside the beat, so a shift lands where the caller was within its beat.
    let offset = (f64::from(grid.phase(base)) * grid.beat_width(target) as f64).round() as u64;
    Some(grid.frame_at_beat(target) + offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_core::SAMPLE_RATE;

    fn grid() -> BeatGrid {
        // 122 BPM @ 44.1k → 21688.52 frames/beat.
        BeatGrid::from_constant_bpm(122.0, 0, 44_100 * 60, SAMPLE_RATE)
    }

    fn pool(total: u64) -> Arc<dyn Source> {
        use hypermixx_media::{DecodedAudio, PcmPool};
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..total as usize)
                .flat_map(|i| [i as f32, i as f32])
                .collect(),
            total_frames: total,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    #[test]
    fn identity_below_out_and_modulo_above() {
        let range = LoopRange::new(100, 200);
        // First pass plays through untouched…
        assert_eq!(range.map(0), 0);
        assert_eq!(range.map(199), 199);
        // …and from `out` on, the position folds back into the range.
        assert_eq!(range.map(200), 100);
        assert_eq!(range.map(201), 101);
        assert_eq!(range.map(299), 199);
        assert_eq!(range.map(300), 100, "lap 2 must land exactly on `in`");
        // A whole number of laps past `in` lands back on `in`: (1_000_000 − 100) % 100 == 0.
        assert_eq!(range.map(1_000_000), 100);
    }

    #[test]
    fn segment_stops_at_the_boundary_then_wraps() {
        let range = LoopRange::new(100, 200);
        // 10 frames before `out`: 8 frames fit, then the read must split.
        assert_eq!(range.segment(192, 64), (192, 8));
        // Past `out`, the segment starts at `in` and may cover the whole first lap.
        assert_eq!(range.segment(205, 64), (105, 64));
        assert_eq!(range.segment(199, 64), (199, 1));
    }

    #[test]
    fn cell_store_is_all_or_nothing() {
        let cell = LoopRangeCell::new();
        assert_eq!(cell.load(), None);
        cell.store(Some(LoopRange::new(10, 20)));
        assert_eq!(cell.map(25), 10 + (25 - 10) % 10);
        // A degenerate range is refused, not mapped (a zero span would never terminate).
        cell.store(Some(LoopRange::new(30, 30)));
        assert_eq!(cell.load(), None, "out ≤ in must disable, not half-store");
        assert_eq!(cell.map(77), 77, "and the mapping is identity again");
    }

    #[test]
    fn loop_source_wraps_and_reaches_every_lap() {
        let total = 1_000u64;
        let cell = Arc::new(LoopRangeCell::new());
        cell.store(Some(LoopRange::new(400, 500)));
        let source = LoopSource::new(pool(total), Arc::clone(&cell));
        let mut buf = [0.0f32; 64 * CHANNELS];

        // Reading across the boundary joins the tail to the head with no gap.
        let read = source.read_frames(496, &mut buf);
        assert_eq!(read, 64);
        assert_eq!(buf[0], 496.0, "starts just before `out`");
        assert_eq!(buf[8 * CHANNELS], 404.0, "frame 504 wraps to `in + 4`");
        // The wrapped flow's virtual clock runs past any track length…
        assert_eq!(source.total_frames(), u64::MAX);
        // …and every lap still maps inside the range (9_000 is a whole number of laps past `in`).
        let read = source.read_frames(9_000, &mut buf);
        assert_eq!(read, 64);
        assert_eq!(buf[0], 400.0, "lap of 9000 lands on `in + 0`");

        // With no range the wrapper is a transparent pass-through.
        cell.store(None);
        assert_eq!(source.total_frames(), total);
        assert_eq!(source.read_frames(700, &mut buf), 64);
        assert_eq!(buf[0], 700.0);
    }

    #[test]
    fn loop_source_splits_a_multi_lap_read() {
        // A 3-frame loop forces several wraps inside one 16-frame read.
        let cell = Arc::new(LoopRangeCell::new());
        cell.store(Some(LoopRange::new(10, 13)));
        let source = LoopSource::new(pool(100), Arc::clone(&cell));
        let mut buf = [0.0f32; 16 * CHANNELS];
        assert_eq!(source.read_frames(11, &mut buf), 16);
        let frames: Vec<u64> = (0..16)
            .map(|i| buf[i * CHANNELS] as u64)
            .collect();
        assert_eq!(frames, vec![11, 12, 10, 11, 12, 10, 11, 12, 10, 11, 12, 10, 11, 12, 10, 11]);
    }

    #[test]
    fn quantize_to_beat_snaps_to_the_nearest_head() {
        let g = grid();
        let beat = g.frame_at_beat(4);
        assert_eq!(quantize_to_beat(&g, beat), Some(beat));
        assert_eq!(
            quantize_to_beat(&g, beat + 100),
            Some(beat),
            "just past a head snaps back"
        );
        assert_eq!(
            quantize_to_beat(&g, beat + 21_000),
            Some(g.frame_at_beat(5)),
            "just before the next head snaps forward"
        );
        assert_eq!(quantize_to_beat(&BeatGrid::empty(SAMPLE_RATE), 0), None);
    }

    #[test]
    fn quantize_offset_rounds_to_whole_quanta() {
        let g = grid();
        let in_frame = g.frame_at_beat(10);
        let beat = g.beat_width(10);
        // 4.4 beats ahead, whole beats → 4.
        assert_eq!(
            quantize_offset(&g, in_frame, in_frame + (beat as f64 * 4.4) as u64, LoopQuantum::Beat),
            Some(in_frame + 4 * beat)
        );
        // 4.4 beats, half-beat quanta → 4.5 (in the rounded quantum's own units).
        let half = ((beat as f64) * 0.5).round() as u64;
        assert_eq!(
            quantize_offset(&g, in_frame, in_frame + (beat as f64 * 4.4) as u64, LoopQuantum::Half),
            Some(in_frame + 9 * half)
        );
        // An early press (out ≤ in) falls back to the one-beat minimum.
        assert_eq!(
            quantize_offset(&g, in_frame, in_frame - 500, LoopQuantum::Beat),
            Some(in_frame + beat)
        );
        // Exactly on `in`: zero steps → also the one-beat minimum.
        assert_eq!(
            quantize_offset(&g, in_frame, in_frame, LoopQuantum::Eighth),
            Some(in_frame + beat)
        );
    }

    #[test]
    fn grid_distances_beat_loops_and_shifts() {
        let g = grid();
        let base = g.frame_at_beat(7) + 500;
        // Four beats from an exact head…
        assert_eq!(
            beats_after(&g, g.frame_at_beat(3), 4),
            Some(g.frame_at_beat(7))
        );
        // …and from mid-beat, the distance still starts at the containing beat's head.
        assert_eq!(beats_after(&g, base, 2), Some(g.frame_at_beat(9)));

        assert_eq!(beat_shift(&g, base, 0), Some(base));
        assert_eq!(
            beat_shift(&g, base, 3),
            Some(g.frame_at_beat(10) + 500),
            "a forward shift keeps the phase inside the beat"
        );
        assert_eq!(beat_shift(&g, base, -7), Some(500), "clamped at the first beat");
        assert_eq!(beat_shift(&BeatGrid::empty(SAMPLE_RATE), 0, 1), None);
    }
}
