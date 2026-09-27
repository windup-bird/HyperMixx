//! Offline stem separation: the trait, a mock, the model table + downloader, and the result cache.
//!
//! Separation is *offline and off-thread*: it takes a whole decoded track, spends tens of seconds
//! on the CPU (and a gigabyte or two of RAM), and returns four frame-aligned streams. Nothing here
//! may ever run on the audio thread, and nothing here is on the audio thread's critical path — the
//! engine only ever receives the finished [`StemSet`] through a `Command`.
//!
//! Layering: this crate is a peer of `hypermixx-library` (offline work over decoded PCM), and the
//! ML dependency is behind the `onnx` feature so a build that never separates anything does not pay
//! for `ort`. Selection of *which* separator to use belongs to the front-end; this crate only offers
//! the choices and the plumbing.
//!
//! ## Why a mock is not optional
//!
//! [`MockSeparator`] splits a mix without any model, which is what lets the engine's stem path be
//! tested in CI (and developed) with no 300 MB download and no ONNX Runtime. It is the reason the
//! playback side of stems could be built and verified before the ML side existed.

pub mod cache;
pub mod mock;
pub mod model;

#[cfg(feature = "onnx")]
mod onnx;
#[cfg(feature = "onnx")]
pub use onnx::CharonSeparator;

pub use cache::{cache_key, cache_root, load_cached, store_cached, CachedStems};
pub use mock::{MockMode, MockSeparator};
pub use model::{ensure_model, model_path, models_root, ModelSpec, HTDEMUCS};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use hypermixx_core::{Source, StemSet, CHANNELS};

/// Why a separation could not be produced.
#[derive(Debug, Clone, PartialEq)]
pub enum StemError {
    /// Reading the source, the cache or the model file failed.
    Io(String),
    /// The model could not be obtained or failed its integrity check.
    Model(String),
    /// The inference backend refused or failed the job.
    Backend(String),
    /// This build has no separator compiled in (`--features onnx`).
    NoBackend,
    /// The caller cancelled.
    Cancelled,
}

impl StemError {
    pub fn message(&self) -> String {
        match self {
            StemError::Io(what) => format!("stem io: {what}"),
            StemError::Model(what) => format!("stem model: {what}"),
            StemError::Backend(what) => format!("stem backend: {what}"),
            StemError::NoBackend => {
                "this build has no stem separator (rebuild with --features onnx)".into()
            }
            StemError::Cancelled => "stem separation cancelled".into(),
        }
    }
}

impl std::fmt::Display for StemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for StemError {}

/// Where progress goes. An `Arc<dyn Fn>` rather than `&mut dyn FnMut` because the backend's own
/// callback has to be `'static + Send + Sync`; see `onnx.rs` for the bridge.
pub type ProgressSink = Arc<dyn Fn(f32) + Send + Sync>;

/// A progress sink that ignores everything, for callers that do not report.
pub fn no_progress() -> ProgressSink {
    Arc::new(|_| {})
}

/// A cancellation flag a front-end flips; a separator observes it between model windows.
///
/// The separation itself cannot be stopped mid-window, so cancelling means "stop as soon as
/// possible and do not install the result" — the worker checks this before handing the set over.
#[derive(Clone, Default, Debug)]
pub struct Cancelled(Arc<AtomicBool>);

impl Cancelled {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Separates one decoded track into four frame-aligned streams.
///
/// Implementations are `Send + Sync` and stateless per call, so a front-end can keep one in an
/// `Arc` and run a job from whichever worker thread it likes.
pub trait StemSeparator: Send + Sync {
    /// A stable id, part of the cache key: two separators that produce different audio must not
    /// share a cache entry.
    fn id(&self) -> String;

    /// Separates `mix`. `progress` is called with `0.0..=1.0` (monotonically, though callers must
    /// not rely on every value); `cancelled` short-circuits the job.
    ///
    /// The returned set is **frame-aligned with `mix`**: four streams, each `mix.total_frames()`
    /// long. The engine's four streams share one clock, so this is a contract, not a nicety.
    fn separate(
        &self,
        mix: &dyn Source,
        progress: &ProgressSink,
        cancelled: &Cancelled,
    ) -> Result<StemSet, StemError>;

    /// The model this separator needs, if any — so a front-end can offer to fetch it up front.
    fn model(&self) -> Option<&'static ModelSpec> {
        None
    }
}

/// The separator a front-end should use by default: HTDemucs on ONNX Runtime, with the model
/// obtained (and verified) first. `progress` covers the model download when one is needed.
#[cfg(feature = "onnx")]
pub fn default_separator(progress: &ProgressSink) -> Result<Box<dyn StemSeparator>, StemError> {
    let model = ensure_model(&HTDEMUCS, None, progress)?;
    Ok(Box::new(CharonSeparator::new(model)))
}

/// Without the `onnx` feature there is nothing to separate with; the mock is for tests, not for
/// pretending a track was separated.
#[cfg(not(feature = "onnx"))]
pub fn default_separator(_progress: &ProgressSink) -> Result<Box<dyn StemSeparator>, StemError> {
    Err(StemError::NoBackend)
}

/// Separates `mix`, with the on-disk cache in front of the model.
///
/// This is the entry point a front-end wants: a cache hit costs a disk read instead of a minute of
/// CPU, and a miss is stored for next time. The cache key is content-addressed, so it is correct
/// across file renames and wrong separators alike (see [`cache_key`]).
///
/// Progress: 0.0–0.05 is reading and hashing the mix, 0.05–1.0 the separation (a cache hit jumps
/// straight to 1.0).
pub fn separate_cached(
    separator: &dyn StemSeparator,
    mix: &dyn Source,
    progress: &ProgressSink,
    cancelled: &Cancelled,
) -> Result<StemSet, StemError> {
    let pcm = read_source(mix, &mut |fraction| progress(fraction * 0.05))?;
    let key = cache_key(&separator.id(), &pcm);
    if let Some(hit) = load_cached(&key) {
        progress(1.0);
        return Ok(hit.stems);
    }
    if cancelled.is_cancelled() {
        return Err(StemError::Cancelled);
    }
    let slice = PcmSlice::new(pcm);
    let inner: ProgressSink = {
        let outer = Arc::clone(progress);
        Arc::new(move |fraction| outer(0.05 + 0.95 * fraction))
    };
    let set = separator.separate(&slice, &inner, cancelled)?;
    // A cache write is best-effort: failing to persist must not fail a separation that worked.
    let _ = store_cached(&key, &set);
    progress(1.0);
    Ok(set)
}

/// Frames read per batch when pulling a whole source into memory.
const READ_CHUNK_FRAMES: usize = 65_536;

/// Reads a whole [`Source`] into interleaved stereo f32 — the separators' input form.
///
/// `progress` reports the read (a fifth of the total wait at most, but a long track still takes a
/// moment to pull out of a decoder's pool).
pub fn read_source(
    source: &dyn Source,
    progress: &mut dyn FnMut(f32),
) -> Result<Vec<f32>, StemError> {
    let frames = source.total_frames() as usize;
    let mut pcm = vec![0.0f32; frames * CHANNELS];
    let mut done = 0usize;
    while done < frames {
        let want = (frames - done).min(READ_CHUNK_FRAMES);
        let read = source.read_frames(
            done as u64,
            &mut pcm[done * CHANNELS..(done + want) * CHANNELS],
        );
        if read == 0 {
            return Err(StemError::Io(format!(
                "source reported {frames} frames but ran out at {done}"
            )));
        }
        done += read;
        progress(done as f32 / frames.max(1) as f32);
    }
    Ok(pcm)
}

/// A [`Source`] over a slice of interleaved stereo f32.
///
/// The separators need to hand the *mix* to their backends and build stems from raw buffers; this
/// is the cheap adapter both directions use, so nothing has to go back through a decoder.
#[derive(Clone)]
pub struct PcmSlice {
    pcm: Arc<Vec<f32>>,
    frames: u64,
}

impl PcmSlice {
    /// Wraps interleaved stereo samples. The length is rounded down to whole frames.
    pub fn new(pcm: Vec<f32>) -> Self {
        let frames = (pcm.len() / CHANNELS) as u64;
        Self {
            pcm: Arc::new(pcm),
            frames,
        }
    }

    pub fn as_slice(&self) -> &[f32] {
        &self.pcm
    }
}

impl Source for PcmSlice {
    fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize {
        let start = start_frame as usize;
        if start >= self.frames as usize {
            return 0;
        }
        let want = output.len() / CHANNELS;
        let frames = want.min(self.frames as usize - start);
        let src = &self.pcm[start * CHANNELS..(start + frames) * CHANNELS];
        output[..src.len()].copy_from_slice(src);
        frames
    }

    fn total_frames(&self) -> u64 {
        self.frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_media::{DecodedAudio, PcmPool};

    fn ramp(frames: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..frames as usize)
                .flat_map(|i| [i as f32, -(i as f32)])
                .collect(),
            total_frames: frames,
            sample_rate: hypermixx_core::SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    #[test]
    fn read_source_pulls_the_whole_thing_and_reports_progress() {
        let source = ramp(10_000);
        let mut seen = Vec::new();
        let pcm = read_source(source.as_ref(), &mut |p| seen.push(p)).unwrap();
        assert_eq!(pcm.len(), 10_000 * CHANNELS);
        assert_eq!(pcm[0], 0.0);
        assert_eq!(pcm[19_998], 9999.0);
        assert!((seen.last().copied().unwrap() - 1.0).abs() < 1e-6);
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "progress must not go back");
    }

    #[test]
    fn pcm_slice_reads_like_a_pool() {
        let slice = PcmSlice::new((0..20).map(|i| i as f32).collect());
        assert_eq!(slice.total_frames(), 10);
        let mut out = vec![0.0f32; 4 * CHANNELS];
        assert_eq!(slice.read_frames(2, &mut out), 4);
        assert_eq!(&out[..4], &[4.0, 5.0, 6.0, 7.0]);
        assert_eq!(slice.read_frames(9, &mut out), 1);
    }

    #[test]
    fn the_mock_splits_frame_exactly_and_sums_back_to_the_mix() {
        let source = ramp(5_000);
        for mode in [MockMode::QuarterEach, MockMode::Passthrough(hypermixx_core::Stem::Vocals)] {
            let set = MockSeparator::new(mode)
                .separate(source.as_ref(), &no_progress(), &Cancelled::new())
                .unwrap();
            assert!(set.is_frame_aligned());
            assert_eq!(set.total_frames(), 5_000);
        }
    }

    #[test]
    fn cancellation_stops_the_mock() {
        let source = ramp(5_000);
        let cancelled = Cancelled::new();
        cancelled.cancel();
        let err = MockSeparator::new(MockMode::QuarterEach)
            .separate(source.as_ref(), &no_progress(), &cancelled)
            .unwrap_err();
        assert_eq!(err, StemError::Cancelled);
    }
}
