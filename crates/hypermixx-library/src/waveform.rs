//! Stereo, three-band waveform peaks for overview rendering.
//!
//! Pure functions over a [`Source`]; lives in `library` because this is derived display data, not
//! a playback concern. Nothing here runs on the audio path.
//!
//! Why a pyramid instead of one flat `Vec`:
//! The TUI redraws at up to 60Hz at an arbitrary zoom. If it scanned raw frames per column, the
//! cost would grow with how far out the user zoomed. A 2:1 mipmap of bucket peaks lets the renderer
//! pick a level whose buckets are one or two per screen column, so each frame is O(columns)
//! regardless of zoom, and the per-tick pass is an iterator that never allocates.
//!
//! Left and right are kept apart, so the two halves of a DJ waveform can show the stereo image.
//! Peaks are quantised to `u8` at build time: that is the precision a terminal dot or an image
//! pixel can show, and it halves the memory/bandwidth of the stereo pyramid.
//!
//! The three bands come from two one-pole low-passes per channel, which is cheap enough to run
//! over every sample once at load time and good enough for a display envelope.

use hypermixx_core::CHANNELS;
use hypermixx_media::Source;

/// Lower crossover: kick and bass live below this.
pub const DEFAULT_LOW_HZ: f32 = 200.0;
/// Upper crossover: body below, hats/percussion above. 4kHz rather than 2kHz keeps the white
/// high band from swallowing the whole envelope on dense material.
pub const DEFAULT_HIGH_HZ: f32 = 4_000.0;

/// Bucket size (frames) at the finest pyramid level. 256 frames @ 44.1kHz is ~5.8ms, fine enough
/// that the tightest useful zoom (one braille dot = 256 frames) still has data.
pub const BASE_BUCKET: u64 = 256;

/// One bucket's amplitude in the three bands, each the max `|sample|` seen in that bucket.
///
/// `u8` rather than `f32`: a braille dot is binary and an image pixel is 8-bit, so a float peak
/// would only cost memory and bandwidth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BandPeaks {
    pub low: u8,
    pub mid: u8,
    pub high: u8,
}

impl BandPeaks {
    pub const ZERO: Self = Self {
        low: 0,
        mid: 0,
        high: 0,
    };

    /// Element-wise max, the only combine a peak pyramid needs.
    fn max_assign(&mut self, other: &Self) {
        self.low = self.low.max(other.low);
        self.mid = self.mid.max(other.mid);
        self.high = self.high.max(other.high);
    }
}

/// Both channels of one bucket. The renderer puts the left channel above the centre line and the
/// right channel below it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StereoPeaks {
    pub left: BandPeaks,
    pub right: BandPeaks,
}

impl StereoPeaks {
    pub const ZERO: Self = Self {
        left: BandPeaks::ZERO,
        right: BandPeaks::ZERO,
    };

    fn max_assign(&mut self, other: &Self) {
        self.left.max_assign(&other.left);
        self.right.max_assign(&other.right);
    }
}

/// A mipmap of [`StereoPeaks`]: `levels[0]` is the finest, each next level halves the bucket count.
#[derive(Clone, Debug)]
pub struct Waveform {
    levels: Vec<Vec<StereoPeaks>>,
    base_bucket: u64,
    total_frames: u64,
}

impl Waveform {
    /// Builds the pyramid for `source` at [`BASE_BUCKET`] resolution with the default crossovers.
    pub fn build(source: &dyn Source) -> Self {
        Self::build_with_crossovers(source, DEFAULT_LOW_HZ, DEFAULT_HIGH_HZ)
    }

    /// As [`build`](Self::build) with the two band crossovers spelled out. The UI reads them from
    /// `preference.toml`; the defaults are the shipping choice.
    pub fn build_with_crossovers(source: &dyn Source, low_hz: f32, high_hz: f32) -> Self {
        let level0 = analyse(source, BASE_BUCKET, low_hz, high_hz);
        Self::from_level0(level0, BASE_BUCKET, source.total_frames())
    }

    /// As [`build`](Self::build) with an explicit finest bucket size (tests, analysis tools).
    pub fn build_with_bucket(source: &dyn Source, bucket_frames: u64) -> Self {
        Self::build_with_bucket_and_crossovers(
            source,
            bucket_frames,
            DEFAULT_LOW_HZ,
            DEFAULT_HIGH_HZ,
        )
    }

    /// As [`build_with_bucket`](Self::build_with_bucket), with explicit crossovers.
    pub fn build_with_bucket_and_crossovers(
        source: &dyn Source,
        bucket_frames: u64,
        low_hz: f32,
        high_hz: f32,
    ) -> Self {
        let level0 = analyse(source, bucket_frames.max(1), low_hz, high_hz);
        Self::from_level0(level0, bucket_frames.max(1), source.total_frames())
    }

    /// Builds a pyramid from precomputed finest-level buckets. The rendering tests use this to
    /// avoid needing a `Source` at all.
    pub fn from_buckets(buckets: Vec<StereoPeaks>, base_bucket: u64, total_frames: u64) -> Self {
        Self::from_level0(buckets, base_bucket.max(1), total_frames)
    }

    fn from_level0(level0: Vec<StereoPeaks>, base_bucket: u64, total_frames: u64) -> Self {
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
    pub fn buckets(&self) -> &[StereoPeaks] {
        self.levels.first().map_or(&[], Vec::as_slice)
    }

    /// An iterator over `count` columns starting at `start_frame`, each covering
    /// `frames_per_column` frames. Columns outside the track read as [`StereoPeaks::ZERO`], which is
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
    level: &'a [StereoPeaks],
    bucket: f64,
    total_frames: u64,
    start_frame: f64,
    frames_per_column: f64,
    index: usize,
    count: usize,
}

impl Iterator for Columns<'_> {
    type Item = StereoPeaks;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.count {
            return None;
        }
        let start = self.start_frame + self.index as f64 * self.frames_per_column;
        let end = start + self.frames_per_column;
        self.index += 1;

        // Entirely before the track, past its end, or an empty waveform.
        if end <= 0.0 || start >= self.total_frames as f64 || self.level.is_empty() {
            return Some(StereoPeaks::ZERO);
        }

        let first = (start.max(0.0) / self.bucket).floor() as usize;
        let last = ((end / self.bucket).ceil() as usize).max(first + 1);
        let mut peak = StereoPeaks::ZERO;
        for bucket in &self.level[first.min(self.level.len())..last.min(self.level.len())] {
            peak.max_assign(bucket);
        }
        Some(peak)
    }
}

/// Scans `source` once and reduces it to one [`StereoPeaks`] per `bucket_frames`.
fn analyse(
    source: &dyn Source,
    bucket_frames: u64,
    low_hz: f32,
    high_hz: f32,
) -> Vec<StereoPeaks> {
    let total = source.total_frames();
    let capacity = (total / bucket_frames) as usize + 1;
    let mut out = Vec::with_capacity(capacity);

    // One-pole coefficients and per-channel state: `mid_lp` is the high crossover low-pass,
    // `low_lp` the low one. Keeping two states is the only extra work stereo costs over the old
    // mono scan.
    let low_a = pole(low_hz);
    let high_a = pole(high_hz);
    let mut low_lp = [0.0f32; CHANNELS];
    let mut mid_lp = [0.0f32; CHANNELS];

    let mut block = vec![0.0f32; 4096 * CHANNELS];
    let mut start = 0u64;
    let mut cur = StereoPeaks::ZERO;
    let mut in_bucket = 0u64;
    while start < total {
        let read = source.read_frames(start, &mut block);
        if read == 0 {
            break;
        }
        for i in (0..read * CHANNELS).step_by(CHANNELS) {
            for channel in 0..CHANNELS {
                let sample = block[i + channel];
                low_lp[channel] += low_a * (sample - low_lp[channel]);
                mid_lp[channel] += high_a * (sample - mid_lp[channel]);
                let peaks = if channel == 0 {
                    &mut cur.left
                } else {
                    &mut cur.right
                };
                peaks.low = peaks.low.max(quantise(low_lp[channel].abs()));
                peaks.mid = peaks.mid.max(quantise((mid_lp[channel] - low_lp[channel]).abs()));
                peaks.high = peaks.high.max(quantise((sample - mid_lp[channel]).abs()));
            }
            in_bucket += 1;
            if in_bucket >= bucket_frames {
                out.push(cur);
                cur = StereoPeaks::ZERO;
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

/// Quantises one absolute amplitude to the 0..=255 display range.
fn quantise(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
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

    /// A sine at `hz`, generated on the fly so tests need no buffers.
    struct ToneSource {
        hz: f32,
        frames: u64,
        left: f32,
        right: f32,
    }

    impl Source for ToneSource {
        fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize {
            let want = output.len() / CHANNELS;
            let frames = want.min(self.frames.saturating_sub(start_frame) as usize);
            for frame in 0..frames {
                let t = (start_frame + frame as u64) as f32 / SR as f32;
                let s = (std::f32::consts::TAU * self.hz * t).sin();
                output[frame * CHANNELS] = s * self.left;
                output[frame * CHANNELS + 1] = s * self.right;
            }
            frames
        }

        fn total_frames(&self) -> u64 {
            self.frames
        }
    }

    fn tone(hz: f32, frames: u64) -> ToneSource {
        ToneSource {
            hz,
            frames,
            left: 1.0,
            right: 1.0,
        }
    }

    fn stereo_tone(hz: f32, frames: u64, left: f32, right: f32) -> ToneSource {
        ToneSource {
            hz,
            frames,
            left,
            right,
        }
    }

    fn aggregate(w: &Waveform) -> StereoPeaks {
        w.buckets().iter().fold(StereoPeaks::ZERO, |mut acc, b| {
            acc.max_assign(b);
            acc
        })
    }

    #[test]
    fn sub_bass_lands_in_low() {
        let w = Waveform::build(&tone(100.0, 44_100));
        let p = aggregate(&w).left;
        assert!(p.low > p.mid && p.low > p.high, "low should dominate: {p:?}");
    }

    #[test]
    fn midrange_lands_in_mid() {
        let w = Waveform::build(&tone(1_000.0, 44_100));
        let p = aggregate(&w).left;
        assert!(p.mid > p.low && p.mid > p.high, "mid should dominate: {p:?}");
    }

    #[test]
    fn the_high_crossover_leaves_three_kilohertz_in_mid() {
        let w = Waveform::build(&tone(3_000.0, 44_100));
        let p = aggregate(&w).left;
        assert!(p.mid > p.low && p.mid > p.high, "3kHz belongs to mid: {p:?}");
    }

    #[test]
    fn highs_land_in_high() {
        let w = Waveform::build(&tone(8_000.0, 44_100));
        let p = aggregate(&w).left;
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
        let sample = StereoPeaks {
            left: BandPeaks {
                low: 255,
                mid: 128,
                high: 64,
            },
            right: BandPeaks {
                low: 32,
                mid: 16,
                high: 8,
            },
        };
        let w = Waveform::from_buckets(vec![sample], 256, 256);
        let mut it = w.columns(-256.0, 256.0, 3);
        assert_eq!(it.next(), Some(StereoPeaks::ZERO), "before the track");
        assert_eq!(it.next(), Some(sample), "the one real column");
        assert_eq!(it.next(), Some(StereoPeaks::ZERO), "past the track");
        assert_eq!(it.next(), None, "exactly `count` items");
    }

    #[test]
    fn columns_pick_a_level_with_one_or_two_buckets_each() {
        // 64 buckets at 256 frames; a column of 2048 frames should use level 3 (2048/bucket).
        let buckets = (0..64)
            .map(|i| StereoPeaks {
                left: BandPeaks {
                    low: i as u8,
                    mid: 0,
                    high: 0,
                },
                right: BandPeaks::ZERO,
            })
            .collect();
        let w = Waveform::from_buckets(buckets, 256, 64 * 256);
        // Columns start on bucket boundaries so the max is deterministic.
        let got: Vec<u8> = w.columns(0.0, 2048.0, 2).map(|p| p.left.low).collect();
        assert_eq!(got, vec![7, 15]);
    }

    #[test]
    fn left_and_right_channels_stay_separate() {
        let left_only = Waveform::build(&stereo_tone(1_000.0, 44_100, 1.0, 0.0));
        let peak = aggregate(&left_only);
        assert!(peak.left.mid > 0, "left channel should carry the tone: {peak:?}");
        assert_eq!(peak.right, BandPeaks::ZERO, "right channel must stay empty: {peak:?}");

        let right_only = Waveform::build(&stereo_tone(1_000.0, 44_100, 0.0, 1.0));
        let peak = aggregate(&right_only);
        assert!(peak.right.mid > 0, "right channel should carry the tone: {peak:?}");
        assert_eq!(peak.left, BandPeaks::ZERO, "left channel must stay empty: {peak:?}");
    }

    #[test]
    fn empty_source_is_safe() {
        let w = Waveform::build(&tone(440.0, 0));
        assert!(w.is_empty());
        let mut it = w.columns(0.0, 256.0, 2);
        assert_eq!(it.next(), Some(StereoPeaks::ZERO));
        assert_eq!(it.next(), Some(StereoPeaks::ZERO));
    }
}
