//! Three-band waveform peaks for overview rendering.
//!
//! Pure functions over a [`Source`]; lives in `library` because this is derived display data, not
//! a playback concern. Nothing here runs on the audio path.
//!
//! Why a pyramid instead of one flat `Vec`:
//! The TUI redraws at 30Hz at an arbitrary zoom. If it scanned raw frames per column, the cost
//! would grow with how far out the user zoomed. A 2:1 mipmap of bucket peaks lets the renderer
//! pick a level whose buckets are one or two per screen column, so each frame is O(columns)
//! regardless of zoom, and the per-tick pass is an iterator that never allocates.
//!
//! The three bands come from two one-pole low-passes, which is cheap enough to run over every
//! sample once at load time and good enough for a display envelope.

use hypermixx_core::CHANNELS;
use hypermixx_media::Source;

/// Lower crossover: kick and bass live below this.
const LOW_HZ: f32 = 200.0;
/// Upper crossover: body below, hats/percusssion above.
const HIGH_HZ: f32 = 2_000.0;

/// Bucket size (frames) at the finest pyramid level. 256 frames @ 44.1kHz is ~5.8ms, fine enough
/// that the tightest useful zoom (one braille dot = 256 frames) still has data.
pub const BASE_BUCKET: u64 = 256;

/// One bucket's amplitude in the three bands, each the max `|sample|` seen in that bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BandPeaks {
    pub low: f32,
    pub mid: f32,
    pub high: f32,
}

impl BandPeaks {
    pub const ZERO: Self = Self {
        low: 0.0,
        mid: 0.0,
        high: 0.0,
    };

    /// Element-wise max, the only combine a peak pyramid needs.
    fn max_assign(&mut self, other: &Self) {
        self.low = self.low.max(other.low);
        self.mid = self.mid.max(other.mid);
        self.high = self.high.max(other.high);
    }
}

/// A mipmap of [`BandPeaks`]: `levels[0]` is the finest, each next level halves the bucket count.
#[derive(Clone, Debug)]
pub struct Waveform {
    levels: Vec<Vec<BandPeaks>>,
    base_bucket: u64,
    total_frames: u64,
}

impl Waveform {
    /// Builds the pyramid for `source` at [`BASE_BUCKET`] resolution.
    pub fn build(source: &dyn Source) -> Self {
        Self::build_with_bucket(source, BASE_BUCKET)
    }

    /// As [`build`](Self::build) with an explicit finest bucket size (tests, analysis tools).
    pub fn build_with_bucket(source: &dyn Source, bucket_frames: u64) -> Self {
        let level0 = analyse(source, bucket_frames.max(1));
        Self::from_level0(level0, bucket_frames.max(1), source.total_frames())
    }

    /// Builds a pyramid from precomputed finest-level buckets. The rendering tests use this to
    /// avoid needing a `Source` at all.
    pub fn from_buckets(buckets: Vec<BandPeaks>, base_bucket: u64, total_frames: u64) -> Self {
        Self::from_level0(buckets, base_bucket.max(1), total_frames)
    }

    fn from_level0(level0: Vec<BandPeaks>, base_bucket: u64, total_frames: u64) -> Self {
        let mut levels = vec![level0];
        while levels.last().is_some_and(|level| level.len() > 1) {
            let prev = levels.last().expect("just checked");
            let mut next = Vec::with_capacity(prev.len().div_ceil(2));
            for pair in prev.chunks(2) {
                let mut peak = pair[0];
                if let Some(other) = pair.get(1) {
                    peak.max_assign(other);
                }
                next.push(peak);
            }
            levels.push(next);
        }
        Self {
            levels,
            base_bucket,
            total_frames,
        }
    }

    /// Frame length the peaks were built from.
    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Frames per bucket at the finest level.
    pub fn base_bucket(&self) -> u64 {
        self.base_bucket
    }

    /// True when the source had no frames.
    pub fn is_empty(&self) -> bool {
        self.levels.first().is_none_or(Vec::is_empty)
    }

    /// The finest-level buckets. Mostly for tests and non-TUI consumers.
    pub fn buckets(&self) -> &[BandPeaks] {
        self.levels.first().map_or(&[], Vec::as_slice)
    }

    /// An iterator over `count` columns starting at `start_frame`, each covering
    /// `frames_per_column` frames. Columns outside the track read as [`BandPeaks::ZERO`], which is
    /// what makes the playhead-centered view show empty space before the first beat and after the
    /// last. No allocation: the caller consumes values as they are produced.
    pub fn columns(
        &self,
        start_frame: f64,
        frames_per_column: f64,
        count: usize,
    ) -> Columns<'_> {
        let frames_per_column = frames_per_column.max(1.0);
        // Pick the coarsest level whose buckets are still <= one column, so a column aggregates
        // one or two buckets. Capped at the last level: past that the whole track is one bucket.
        let mut level = 0usize;
        let mut bucket = self.base_bucket as f64;
        while bucket * 2.0 <= frames_per_column && level + 1 < self.levels.len() {
            bucket *= 2.0;
            level += 1;
        }
        Columns {
            level: self.levels.get(level).map_or(&[], Vec::as_slice),
            bucket,
            total_frames: self.total_frames,
            start_frame,
            frames_per_column,
            index: 0,
            count,
        }
    }
}

/// The per-column iterator returned by [`Waveform::columns`].
pub struct Columns<'a> {
    level: &'a [BandPeaks],
    bucket: f64,
    total_frames: u64,
    start_frame: f64,
    frames_per_column: f64,
    index: usize,
    count: usize,
}

impl Iterator for Columns<'_> {
    type Item = BandPeaks;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.count {
            return None;
        }
        let start = self.start_frame + self.index as f64 * self.frames_per_column;
        let end = start + self.frames_per_column;
        self.index += 1;

        // Entirely before the track, past its end, or an empty waveform.
        if end <= 0.0 || start >= self.total_frames as f64 || self.level.is_empty() {
            return Some(BandPeaks::ZERO);
        }

        let first = (start.max(0.0) / self.bucket).floor() as usize;
        let last = ((end / self.bucket).ceil() as usize).max(first + 1);
        let mut peak = BandPeaks::ZERO;
        for bucket in &self.level[first.min(self.level.len())..last.min(self.level.len())] {
            peak.max_assign(bucket);
        }
        Some(peak)
    }
}

/// Scans `source` once and reduces it to one [`BandPeaks`] per `bucket_frames`.
fn analyse(source: &dyn Source, bucket_frames: u64) -> Vec<BandPeaks> {
    let total = source.total_frames();
    let capacity = (total / bucket_frames) as usize + 1;
    let mut out = Vec::with_capacity(capacity);

    // One-pole coefficients and state: `mid_lp` is the 2kHz low-pass, `low_lp` the 200Hz one.
    let low_a = pole(LOW_HZ);
    let high_a = pole(HIGH_HZ);
    let mut low_lp = 0.0f32;
    let mut mid_lp = 0.0f32;

    let mut block = vec![0.0f32; 4096 * CHANNELS];
    let mut start = 0u64;
    let mut cur = BandPeaks::ZERO;
    let mut in_bucket = 0u64;
    while start < total {
        let read = source.read_frames(start, &mut block);
        if read == 0 {
            break;
        }
        for i in (0..read * CHANNELS).step_by(CHANNELS) {
            let mono = (block[i] + block[i + 1]) * 0.5;
            low_lp += low_a * (mono - low_lp);
            mid_lp += high_a * (mono - mid_lp);
            cur.low = cur.low.max(low_lp.abs());
            cur.mid = cur.mid.max((mid_lp - low_lp).abs());
            cur.high = cur.high.max((mono - mid_lp).abs());
            in_bucket += 1;
            if in_bucket >= bucket_frames {
                out.push(cur);
                cur = BandPeaks::ZERO;
                in_bucket = 0;
            }
        }
        start += read as u64;
    }
    if in_bucket > 0 {
        out.push(cur);
    }
    out
}

/// One-pole low-pass coefficient `a = 1 - exp(-2*pi*fc/fs)`.
fn pole(hz: f32) -> f32 {
    let fs = hypermixx_core::SAMPLE_RATE as f32;
    (1.0 - (-std::f32::consts::TAU * hz / fs).exp()).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_media::Source;

    const SR: u32 = hypermixx_core::SAMPLE_RATE;

    /// A sine at `hz`, same value on both channels, generated on the fly so tests need no buffers.
    struct ToneSource {
        hz: f32,
        frames: u64,
    }

    impl Source for ToneSource {
        fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize {
            let want = output.len() / CHANNELS;
            let frames = want.min(self.frames.saturating_sub(start_frame) as usize);
            for frame in 0..frames {
                let t = (start_frame + frame as u64) as f32 / SR as f32;
                let s = (std::f32::consts::TAU * self.hz * t).sin();
                output[frame * CHANNELS] = s;
                output[frame * CHANNELS + 1] = s;
            }
            frames
        }

        fn total_frames(&self) -> u64 {
            self.frames
        }
    }

    fn tone(hz: f32, frames: u64) -> ToneSource {
        ToneSource { hz, frames }
    }

    #[test]
    fn sub_bass_lands_in_low() {
        let w = Waveform::build(&tone(100.0, 44_100));
        let p = w.buckets().iter().fold(BandPeaks::ZERO, |mut acc, b| {
            acc.max_assign(b);
            acc
        });
        assert!(p.low > p.mid && p.low > p.high, "low should dominate: {p:?}");
    }

    #[test]
    fn midrange_lands_in_mid() {
        let w = Waveform::build(&tone(1_000.0, 44_100));
        let p = w.buckets().iter().fold(BandPeaks::ZERO, |mut acc, b| {
            acc.max_assign(b);
            acc
        });
        assert!(p.mid > p.low && p.mid > p.high, "mid should dominate: {p:?}");
    }

    #[test]
    fn highs_land_in_high() {
        let w = Waveform::build(&tone(8_000.0, 44_100));
        let p = w.buckets().iter().fold(BandPeaks::ZERO, |mut acc, b| {
            acc.max_assign(b);
            acc
        });
        assert!(p.high > p.low && p.high > p.mid, "high should dominate: {p:?}");
    }

    #[test]
    fn bucket_count_covers_the_track() {
        let w = Waveform::build_with_bucket(&tone(440.0, 1000), 100);
        assert_eq!(w.buckets().len(), 10);
        assert_eq!(w.total_frames(), 1000);
    }

    #[test]
    fn columns_are_zero_outside_the_track() {
        let w = Waveform::from_buckets(vec![BandPeaks { low: 1.0, mid: 0.5, high: 0.25 }], 256, 256);
        let mut it = w.columns(-256.0, 256.0, 3);
        assert_eq!(it.next(), Some(BandPeaks::ZERO), "before the track");
        assert_eq!(
            it.next(),
            Some(BandPeaks { low: 1.0, mid: 0.5, high: 0.25 }),
            "the one real column"
        );
        assert_eq!(it.next(), Some(BandPeaks::ZERO), "past the track");
        assert_eq!(it.next(), None, "exactly `count` items");
    }

    #[test]
    fn columns_pick_a_level_with_one_or_two_buckets_each() {
        // 64 buckets at 256 frames; a column of 2048 frames should use level 3 (2048/bucket).
        let buckets = (0..64)
            .map(|i| BandPeaks {
                low: i as f32,
                mid: 0.0,
                high: 0.0,
            })
            .collect();
        let w = Waveform::from_buckets(buckets, 256, 64 * 256);
        // Columns start on bucket boundaries so the max is deterministic.
        let got: Vec<f32> = w.columns(0.0, 2048.0, 2).map(|p| p.low).collect();
        assert_eq!(got, vec![7.0, 15.0]);
    }

    #[test]
    fn empty_source_is_safe() {
        let w = Waveform::build(&tone(440.0, 0));
        assert!(w.is_empty());
        let mut it = w.columns(0.0, 256.0, 2);
        assert_eq!(it.next(), Some(BandPeaks::ZERO));
        assert_eq!(it.next(), Some(BandPeaks::ZERO));
    }
}
