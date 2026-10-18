//! Streaming separation: blocks out of the model as it finishes them, straight into a sink.
//!
//! The model already windows the track — 7.8 s at a time, advancing a stride, blending the overlap —
//! so *that* is the segmentation. A second, unrelated cutting of the track (a "chunk" with its own
//! margins) would then have to be aligned to the model's window grid and re-normalised to sound like
//! one piece; measured, a chunk whose window phase was off by half a stride differed from a
//! whole-track run by 5× in level. Streaming removes the question: the runtime lays its windows on
//! the track's own grid and emits each block once no future window can touch it, and its output is
//! **sample-for-sample identical** to the whole-track path (they test that; so do we, against the
//! real model).
//!
//! What is left for this module is bookkeeping:
//!
//! * **the cache**, which stores blocks as they arrive (`meta.frames` = what is on disk) so a
//!   finished track comes back at once;
//! * **resuming**, which is a *partial* stream: the runtime is handed the track from an earlier point
//!   on the same window grid and the blocks already on disk are dropped.
//!
//! Resuming is why the input scale is passed explicitly: normalising by the *partial* source's own
//! statistics would give the resumed audio a different model input than the original run, which is
//! audible as a level step exactly where the two meet. The whole track's statistics are cheap to
//! measure (one pass) and make the resumed blocks identical to the ones a fresh run would produce.

use std::sync::Arc;

use hypermixx_core::{Source, StemSet, CHANNELS};

use crate::{cache, Cancelled, ProgressSink, StemBlockSink, StemError, StemSeparator};

/// The window stride a model with `segment_frames` and `overlap` advances by.
///
/// Mirrors the runtime's own arithmetic (`round((1 - overlap) · segment)`, computed in f64), because
/// the emission cadence has to be *exactly* the stride the backend uses — a resumed stream starts on
/// a stride boundary, and a stride that disagreed by one sample would put every window off the grid.
pub fn stride_frames(segment_frames: u64, overlap: f32) -> u64 {
    let overlap = if overlap.is_finite() {
        overlap.clamp(0.0, 0.9)
    } else {
        0.25
    };
    (((1.0 - overlap as f64) * segment_frames as f64).round() as u64).max(1)
}

/// A model's windowing: the length it takes at a time, and the stride between windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub segment_frames: u64,
    pub stride_frames: u64,
}

/// Runs a streaming separation into `sink`, with no cache.
///
/// The scale is measured over the whole track and handed to the backend, so this and
/// [`stream_cached`] separate from *identically-scaled* input: whatever the caller does about the
/// cache, the audio is the same.
pub fn stream_uncached(
    separator: &dyn StemSeparator,
    mix: &dyn Source,
    sink: &mut dyn StemBlockSink,
    progress: &ProgressSink,
    cancelled: &Cancelled,
) -> Result<(), StemError> {
    let scale = InputScale::of(mix)?;
    separator.stream_with_scale(mix, sink, progress, cancelled, scale)
}

/// [`stream_uncached`] with the on-disk cache in front: what is already there is handed over as one
/// block, and the rest is streamed, appended and written out as it arrives.
///
/// A **complete** entry is a pure replay (no model at all). A partial one is resumed: the runtime is
/// pointed at an earlier frame on the model's own grid, and the blocks that are already on disk are
/// dropped on the way past. A cancellation leaves whatever landed, so the next run continues from
/// there instead of starting over.
pub fn stream_cached(
    separator: &dyn StemSeparator,
    mix: &dyn Source,
    sink: &mut dyn StemBlockSink,
    progress: &ProgressSink,
    cancelled: &Cancelled,
) -> Result<(), StemError> {
    let total = mix.total_frames();
    let key = cache_key_for(separator, mix, progress)?;
    let cached_frames = cache::cached_frames(&key).unwrap_or(0).min(total);

    // What is already separated, handed over as one block: a re-run of a finished track costs a file
    // read, and a resumed one starts from where it stopped.
    if cached_frames > 0 {
        if let Some(cached) = cache::load_cached(&key) {
            let blocks = flatten(&cached.stems, cached.frames as usize);
            progress(cached_frames as f32 / total.max(1) as f32);
            if !sink.block(0, &blocks) {
                return Ok(());
            }
        }
    }
    if cancelled.is_cancelled() {
        return Err(StemError::Cancelled);
    }
    if cached_frames >= total {
        return Ok(()); // nothing left to separate
    }

    // Resume: start the runtime at an earlier window-grid point so its windows land where they would
    // have in a fresh run, and let the sink drop the blocks that are already on disk.
    let from = resume_frame(cached_frames, separator.window());
    let source = OffsetSource { inner: mix, from };
    // The whole track's level, not the partial stream's: see the module docs.
    let scale = InputScale::of(mix)?;
    let resumed = cached_frames as f32 / total.max(1) as f32;
    let rescaled: ProgressSink = {
        let outer = Arc::clone(progress);
        Arc::new(move |fraction| {
            outer((resumed + (1.0 - resumed) * fraction.clamp(0.0, 1.0)).clamp(0.0, 1.0))
        })
    };
    let mut forward = Forward {
        sink,
        key: &key,
        total,
        skip_until: cached_frames,
        started: from,
        written: cached_frames,
    };
    separator.stream_with_scale(&source, &mut forward, &rescaled, cancelled, scale)?;
    Ok(())
}

/// Where a resumed stream starts: **a point on the model's window grid**, far enough back that every
/// window covering the cached watermark exists in the resumed run.
///
/// Both halves matter. On the grid, because the runtime lays its windows from the start of whatever
/// it is handed — off the grid, and every window in the resumed part is phase-shifted, which is a
/// timbre step against the cached part. Far enough back, because a sample is influenced by windows
/// starting up to a *segment* earlier: starting at the watermark itself would leave the first
/// `segment − stride` frames separated without their context.
///
/// A backend without fixed windowing may start at the watermark: there is no grid to be off.
fn resume_frame(cached_frames: u64, window: Option<Window>) -> u64 {
    let Some(window) = window else {
        return cached_frames;
    };
    let stride = window.stride_frames.max(1);
    let back = cached_frames.saturating_sub(window.segment_frames);
    (back / stride) * stride
}

/// Splits the cached set into one interleaved buffer per stream.
fn flatten(set: &StemSet, frames: usize) -> Vec<Vec<f32>> {
    hypermixx_core::Stem::ALL
        .iter()
        .map(|stem| {
            let source = set.get(*stem);
            let mut pcm = vec![0.0f32; frames * CHANNELS];
            let mut done = 0;
            while done < frames {
                let read = source.read_frames(done as u64, &mut pcm[done * CHANNELS..]);
                if read == 0 {
                    break;
                }
                done += read;
            }
            pcm
        })
        .collect()
}

/// A view of a source starting at `from`: what a resumed stream is handed.
struct OffsetSource<'a> {
    inner: &'a dyn Source,
    from: u64,
}

impl Source for OffsetSource<'_> {
    fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize {
        self.inner.read_frames(self.from + start_frame, output)
    }

    fn total_frames(&self) -> u64 {
        self.inner.total_frames().saturating_sub(self.from)
    }

    fn available_frames(&self) -> u64 {
        self.inner.available_frames().saturating_sub(self.from)
    }
}

/// Hands each streamed block to the caller's sink, dropping the ones a resumed run already has on
/// disk and appending the rest to the cache.
struct Forward<'a> {
    sink: &'a mut dyn StemBlockSink,
    key: &'a str,
    total: u64,
    /// Everything below this frame is already on disk (or is the lead-in a resume needed).
    skip_until: u64,
    started: u64,
    written: u64,
}

impl StemBlockSink for Forward<'_> {
    fn block(&mut self, start_frame: u64, blocks: &[Vec<f32>]) -> bool {
        // The runtime's own frames are relative to the source it was handed; the track's frames are
        // what the sink (and the deck) speak.
        let start = self.started + start_frame;
        let frames = blocks.first().map(Vec::len).unwrap_or(0) / CHANNELS;
        if frames == 0 {
            return true;
        }
        let end = start + frames as u64;
        if end <= self.skip_until {
            return true; // already on disk: the lead-in a resume re-runs
        }
        let (start, blocks) = if start < self.skip_until {
            // The block straddles the watermark (a resume re-runs a little of what is already on
            // disk): hand over and store only the part that is new.
            let skip = (self.skip_until - start) as usize * CHANNELS;
            (
                self.skip_until,
                blocks
                    .iter()
                    .map(|block| block[skip..].to_vec())
                    .collect::<Vec<Vec<f32>>>(),
            )
        } else {
            (start, blocks.to_vec())
        };
        let frames = blocks.first().map(Vec::len).unwrap_or(0) / CHANNELS;
        if frames == 0 {
            return true;
        }
        // Persist before handing it over: a cancellation (or a crash) after the hand-over must not
        // lose audio the deck is already playing.
        let _ = cache::append_cached(self.key, self.total, start, &blocks);
        self.written = start + frames as u64;
        self.sink.block(start, &blocks)
    }
}

/// The input scale a job normalises with: the **whole track's**, not a range's.
///
/// Demucs' convention is to normalise the input by the mono reference's mean and standard deviation
/// and to undo that on the outputs. A resumed stream given only the track's tail would otherwise
/// measure its own statistics and separate the tail from a differently-scaled input — a level step
/// exactly where the resumed audio meets the cached audio.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputScale {
    pub mean: f32,
    pub std: f32,
}

impl Default for InputScale {
    fn default() -> Self {
        Self::unity()
    }
}

impl InputScale {
    /// No scaling: for a backend that does its own, and for tests.
    pub fn unity() -> Self {
        Self {
            mean: 0.0,
            std: 1.0,
        }
    }

    /// The whole-track scale: the mono reference's mean and sample standard deviation (the
    /// `n − 1` denominator and the `1e-8` floor are Demucs' own, so this is the same normalisation the
    /// reference implementation applies — computed once for the track instead of once per stream).
    pub fn of(mix: &dyn Source) -> Result<Self, StemError> {
        // Welford, one pass: numerically stable, and it needs no buffer for the whole track.
        let mut count = 0u64;
        let mut mean = 0.0f64;
        let mut m2 = 0.0f64;
        let mut buf = vec![0.0f32; crate::READ_CHUNK_FRAMES * CHANNELS];
        let mut at = 0u64;
        let total = mix.total_frames();
        while at < total {
            let want = (total - at).min(crate::READ_CHUNK_FRAMES as u64) as usize;
            let read = mix.read_frames(at, &mut buf[..want * CHANNELS]);
            if read == 0 {
                return Err(StemError::Io(format!(
                    "the source ran out at frame {at} of {total} while measuring its level"
                )));
            }
            for frame in 0..read {
                // The mono reference: the mean of the channels, like the backend's own stats.
                let mono = f64::from((buf[frame * CHANNELS] + buf[frame * CHANNELS + 1]) * 0.5);
                count += 1;
                let delta = mono - mean;
                mean += delta / count as f64;
                m2 += delta * (mono - mean);
            }
            at += read as u64;
        }
        let var = if count > 1 {
            m2 / (count - 1) as f64
        } else {
            0.0
        };
        Ok(Self {
            mean: mean as f32,
            std: (var.sqrt() + 1e-8) as f32,
        })
    }

    /// One sample in, one sample out.
    #[inline]
    pub fn apply(self, sample: f32) -> f32 {
        (sample - self.mean) / self.std
    }

    /// The inverse, for the separated audio.
    #[inline]
    pub fn undo(self, sample: f32) -> f32 {
        sample * self.std + self.mean
    }

    /// Whether this is the identity, so a caller can skip the work.
    #[inline]
    pub fn is_unity(self) -> bool {
        self.mean == 0.0 && self.std == 1.0
    }
}

/// The cache key for a streaming job: the separator and the whole mix's content.
fn cache_key_for(
    separator: &dyn StemSeparator,
    mix: &dyn Source,
    progress: &ProgressSink,
) -> Result<String, StemError> {
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    hasher.update(separator.id().as_bytes());
    hasher.update(b"\0");
    hasher.update(hyperymixx_sample_rate().to_le_bytes());
    hasher.update(mix.total_frames().to_le_bytes());
    let mut buf = vec![0.0f32; crate::READ_CHUNK_FRAMES * CHANNELS];
    let mut at = 0u64;
    let total = mix.total_frames();
    while at < total {
        let want = (total - at).min(crate::READ_CHUNK_FRAMES as u64) as usize;
        let read = mix.read_frames(at, &mut buf[..want * CHANNELS]);
        if read == 0 {
            break;
        }
        for sample in &buf[..read * CHANNELS] {
            hasher.update(sample.to_le_bytes());
        }
        at += read as u64;
        progress(at as f32 / total.max(1) as f32 * 0.05);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

#[inline]
fn hyperymixx_sample_rate() -> u32 {
    hypermixx_core::SAMPLE_RATE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockMode, MockSeparator};
    use crate::StemSeparator;
    use hypermixx_media::{DecodedAudio, PcmPool};

    pub(super) const RATE: u32 = hypermixx_core::SAMPLE_RATE;

    /// A mix whose sample value *is* its frame index, so a stitched result is checkable frame by
    /// frame against a whole-track run.
    fn ramp(frames: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..frames as usize)
                .flat_map(|i| [i as f32, -(i as f32)])
                .collect(),
            total_frames: frames,
            sample_rate: RATE,
            channels: CHANNELS,
        }))
    }

    fn read_all(source: &dyn Source) -> Vec<f32> {
        let frames = source.total_frames() as usize;
        let mut out = vec![0.0f32; frames * CHANNELS];
        assert_eq!(source.read_frames(0, &mut out), frames);
        out
    }

    /// Collects what a streaming job hands over, and stitches it.
    #[derive(Default)]
    struct Stitch {
        blocks: Vec<(u64, usize)>,
        streams: Vec<Vec<f32>>,
    }

    impl StemBlockSink for Stitch {
        fn block(&mut self, start_frame: u64, blocks: &[Vec<f32>]) -> bool {
            let frames = blocks.first().map(Vec::len).unwrap_or(0) / CHANNELS;
            self.blocks.push((start_frame, frames));
            if self.streams.is_empty() {
                self.streams = vec![Vec::new(); blocks.len()];
            }
            for (acc, block) in self.streams.iter_mut().zip(blocks) {
                acc.extend_from_slice(block);
            }
            true
        }
    }

    /// The resume point is on the window grid and far enough back to cover the cached watermark's
    /// own context — the difference between a resumed run that matches a fresh one and one with a
    /// subtle step at the seam.
    #[test]
    fn a_resume_starts_on_the_window_grid() {
        // HTDemucs at overlap 0.25: segment 343 980, stride 257 985. (The stride does not divide the
        // segment, which is exactly why "back off one segment" was wrong.)
        let window = Window {
            segment_frames: 343_980,
            stride_frames: 257_985,
        };
        for blocks in 1..40u64 {
            let watermark = blocks * window.stride_frames;
            let from = resume_frame(watermark, Some(window));
            assert_eq!(from % window.stride_frames, 0, "off the grid at {watermark}: {from}");
            // Far enough back that the cached watermark's own context is inside the resumed run —
            // or from the very start of the track, which has no earlier context to miss (the same
            // situation a fresh run is in).
            assert!(
                from == 0 || from + window.segment_frames <= watermark,
                "not enough lead at {watermark}: {from} + {} > {watermark}",
                window.segment_frames
            );
            // And it is the *latest* such point — moving one stride up would break the lead
            // requirement — so a resume does not redo more than it must.
            assert!(
                from == 0 || from + window.stride_frames + window.segment_frames > watermark,
                "wasteful resume at {watermark}: {from}"
            );
        }
        // A watermark too small to have a window of lead resumes from the start.
        assert_eq!(resume_frame(0, Some(window)), 0);
        assert_eq!(resume_frame(window.stride_frames, Some(window)), 0);
        // A backend without windowing may start exactly at the watermark.
        assert_eq!(resume_frame(1_234, None), 1_234);
    }

    #[test]
    fn the_stride_is_the_runtime_arithmetic() {
        // HTDemucs: 343 980 samples at overlap 0.25 → stride 257 985 (5.85 s), the emission cadence.
        assert_eq!(stride_frames(343_980, 0.25), 257_985);
        assert_eq!(stride_frames(343_980, 0.0), 343_980);
        assert_eq!(stride_frames(343_980, 0.5), 171_990);
        assert_eq!(stride_frames(343_980, f32::NAN), 257_985, "nonsense falls back");
        assert!(stride_frames(343_980, 1.0) > 0, "never zero");
    }

    /// Blocks arrive in order, cover the track exactly once, and stitch back to the whole-track
    /// result — which is the property the deck relies on when it plays them as they land.
    #[test]
    fn blocks_are_ordered_contiguous_and_stitch_to_the_whole_track() {
        let mix = ramp(5_000);
        let separator = MockSeparator::new(MockMode::QuarterEach);
        let mut stitch = Stitch::default();
        stream_uncached(
            &separator,
            mix.as_ref(),
            &mut stitch,
            &crate::no_progress(),
            &Cancelled::new(),
        )
        .unwrap();

        // Contiguous, in order, covering everything.
        let mut at = 0u64;
        for (start, frames) in &stitch.blocks {
            assert_eq!(*start, at, "a block must start where the last one ended");
            at += *frames as u64;
        }
        assert_eq!(at, 5_000, "the blocks cover the track exactly once");
        assert!(stitch.blocks.len() > 1, "the mock streams in blocks, not one piece");

        let whole = separator
            .separate(mix.as_ref(), &crate::no_progress(), &Cancelled::new())
            .unwrap();
        for stem in hypermixx_core::Stem::ALL {
            let expected = read_all(whole.get(stem).as_ref());
            assert_eq!(
                stitch.streams[stem.index()],
                expected,
                "{stem}: streamed blocks must stitch to the whole-track audio"
            );
        }
    }

    /// A complete cache entry is a pure replay: one block, and no separation at all.
    #[test]
    fn a_complete_cache_entry_replays_without_separating() {
        crate::testing::with_isolated_cache("stream-replay", || {
            let mix = ramp(4_000);
            let separator = MockSeparator::new(MockMode::QuarterEach);
            let mut first = Stitch::default();
            stream_cached(
                &separator,
                mix.as_ref(),
                &mut first,
                &crate::no_progress(),
                &Cancelled::new(),
            )
            .unwrap();
            assert!(first.blocks.len() > 1, "a first run does the work in blocks");

            // A separator that must not be called. Only its `id` may be asked for (the cache key).
            struct Never;
            impl StemSeparator for Never {
                fn id(&self) -> String {
                    MockSeparator::new(MockMode::QuarterEach).id()
                }
                fn separate(
                    &self,
                    _mix: &dyn Source,
                    _progress: &ProgressSink,
                    _cancelled: &Cancelled,
                ) -> Result<StemSet, StemError> {
                    panic!("a complete cache entry must not separate anything");
                }
            }
            let mut second = Stitch::default();
            stream_cached(
                &Never,
                mix.as_ref(),
                &mut second,
                &crate::no_progress(),
                &Cancelled::new(),
            )
            .unwrap();
            assert_eq!(second.blocks.len(), 1, "one replayed block");
            assert_eq!(second.blocks[0], (0, 4_000));
            assert_eq!(second.streams, first.streams, "the replay is what was stored");
        });
    }

    /// A partial entry resumes: the blocks already on disk are dropped, and the stitched result is
    /// the whole track — the same audio a fresh run would have produced.
    #[test]
    fn a_partial_entry_resumes_where_it_stopped() {
        crate::testing::with_isolated_cache("stream-resume", || {
            let mix = ramp(5_000);
            let separator = MockSeparator::new(MockMode::QuarterEach);
            // First run, stopped after one block: whatever landed stays on disk.
            let cancelled = Cancelled::new();
            let mut partial = Stitch::default();
            // A sink that stops the job after the first block.
            struct StopAfterOne(Stitch);
            impl StemBlockSink for StopAfterOne {
                fn block(&mut self, start: u64, blocks: &[Vec<f32>]) -> bool {
                    self.0.block(start, blocks);
                    false
                }
            }
            let mut stopper = StopAfterOne(Stitch::default());
            stream_cached(
                &separator,
                mix.as_ref(),
                &mut stopper,
                &crate::no_progress(),
                &cancelled,
            )
            .unwrap();
            partial.blocks = stopper.0.blocks.clone();
            assert_eq!(partial.blocks.len(), 1, "the job stopped after one block");
            let landed = partial.blocks[0].1;

            // Second run: the cache has `landed` frames, and the rest is separated again.
            let mut resumed = Stitch::default();
            stream_cached(
                &separator,
                mix.as_ref(),
                &mut resumed,
                &crate::no_progress(),
                &Cancelled::new(),
            )
            .unwrap();
            let whole = separator
                .separate(mix.as_ref(), &crate::no_progress(), &Cancelled::new())
                .unwrap();
            assert_eq!(resumed.streams[0].len(), 5_000 * CHANNELS, "the whole track");
            for stem in hypermixx_core::Stem::ALL {
                assert_eq!(
                    resumed.streams[stem.index()],
                    read_all(whole.get(stem).as_ref()),
                    "{stem}: a resumed run must match a fresh one"
                );
            }
            assert!(
                resumed.blocks.iter().any(|(start, _)| *start == landed as u64),
                "the new blocks continue where the cached ones stopped: {:?}",
                resumed.blocks
            );
        });
    }

    /// A cancelled job stops and says so, having handed over what it finished.
    #[test]
    fn a_cancelled_job_stops_between_blocks() {
        let mix = ramp(5_000);
        let separator = MockSeparator::new(MockMode::QuarterEach);
        let cancelled = Cancelled::new();
        struct Stop(usize, Cancelled);
        impl StemBlockSink for Stop {
            fn block(&mut self, _start: u64, _blocks: &[Vec<f32>]) -> bool {
                self.0 += 1;
                self.1.cancel();
                false
            }
        }
        let mut sink = Stop(0, cancelled.clone());
        // A cancelled sink stops the job without an error; the *cancellation* itself is what the
        // driver reports.
        stream_uncached(
            &separator,
            mix.as_ref(),
            &mut sink,
            &crate::no_progress(),
            &cancelled,
        )
        .unwrap();
        assert_eq!(sink.0, 1, "the block it finished was handed over");
    }

    /// Progress never goes backwards and ends at 1.0.
    #[test]
    fn progress_is_monotonic() {
        let mix = ramp(4_000);
        let separator = MockSeparator::new(MockMode::QuarterEach);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: ProgressSink = {
            let seen = Arc::clone(&seen);
            Arc::new(move |fraction| seen.lock().unwrap().push(fraction))
        };
        let mut stitch = Stitch::default();
        stream_uncached(&separator, mix.as_ref(), &mut stitch, &sink, &Cancelled::new()).unwrap();
        let seen = seen.lock().unwrap().clone();
        assert!(seen.windows(2).all(|pair| pair[0] <= pair[1] + 1e-6), "{seen:?}");
        assert!(seen.last().copied().unwrap_or(0.0) >= 0.9, "{seen:?}");
    }
}

#[cfg(test)]
mod scale_tests {
    use super::*;
    use hypermixx_media::{DecodedAudio, PcmPool};

    const RATE: u32 = hypermixx_core::SAMPLE_RATE;

    fn dc(values: &[(f32, f32)]) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: values.iter().flat_map(|(l, r)| [*l, *r]).collect(),
            total_frames: values.len() as u64,
            sample_rate: RATE,
            channels: CHANNELS,
        }))
    }

    /// The scale is the *mono reference's* mean and sample standard deviation (Demucs' convention),
    /// so it must not be a per-channel or a population statistic.
    #[test]
    fn the_input_scale_is_the_mono_mean_and_sample_std() {
        let scale = InputScale::of(dc(&[(1.0, -1.0), (1.0, -1.0), (1.0, -1.0)]).as_ref()).unwrap();
        assert_eq!(scale.mean, 0.0, "the mono reference cancels a symmetric stereo pair");
        // Every mono sample is 0 → std is the 1e-8 floor, not zero (a division by nothing would be
        // NaN on the audio the model then sees).
        assert!((scale.std - 1e-8).abs() < 1e-12, "{}", scale.std);

        // Mono values 1, 2, 3: mean 2, sample variance ((1+0+1)/2) = 1 → std 1.
        let scale = InputScale::of(dc(&[(1.0, 1.0), (2.0, 2.0), (3.0, 3.0)]).as_ref()).unwrap();
        assert!((scale.mean - 2.0).abs() < 1e-6);
        assert!((scale.std - 1.0).abs() < 1e-6, "{}", scale.std);
        // A left/right pair that differs averages to the mono reference, not to either channel.
        let scale = InputScale::of(dc(&[(0.0, 2.0), (0.0, 2.0), (0.0, 2.0)]).as_ref()).unwrap();
        assert!((scale.mean - 1.0).abs() < 1e-6, "{}", scale.mean);

        // Apply and undo are inverse, and unity is a no-op a caller can detect.
        let scale = InputScale {
            mean: 0.25,
            std: 2.0,
        };
        assert!((scale.undo(scale.apply(0.75)) - 0.75).abs() < 1e-6);
        assert!(InputScale::unity().is_unity());
        assert!(!scale.is_unity());
    }

    /// The scale is measured over the *whole* source, which is the point: a resumed stream must not
    /// be normalised by its own tail's level.
    #[test]
    fn a_quiet_range_does_not_scale_itself() {
        // 3 000 loud samples then 3 000 quiet ones: the whole-track scale is dominated by the loud
        // part, so the quiet range's audio is *not* normalised up to unit level.
        let mut values = vec![(0.5f32, 0.5f32); 3_000];
        values.extend(vec![(0.001f32, 0.001f32); 3_000]);
        let scale = InputScale::of(dc(&values).as_ref()).unwrap();
        assert!(scale.mean > 0.2, "the loud half dominates the mean: {}", scale.mean);
        let local = 0.001 / scale.std;
        assert!(local < 0.1, "a quiet range stays quiet instead of being lifted: {local}");
    }
}
