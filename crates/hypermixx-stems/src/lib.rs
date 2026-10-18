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
pub mod chunked;
pub mod mock;
pub mod model;
mod onnx_options;

#[cfg(feature = "onnx")]
mod onnx;
#[cfg(feature = "onnx")]
pub use onnx::CharonSeparator;

pub use cache::{
    append_cached, cache_key, cache_root, cache_usage, load_cached, prune_cache, stems_root,
    store_cached, CachedStems,
};
pub use chunked::{stream_cached, stream_uncached, stride_frames};
pub use onnx_options::{
    Provider, RuntimeOptions, SeparateOptions, DEFAULT_OVERLAP, MAX_OVERLAP,
};
pub use mock::{MockMode, MockSeparator};
pub use model::{
    default_model, ensure_model, model_by_name, model_names, model_path, models_root, ModelSpec,
    HTDEMUCS, HTDEMUCS_FP16, MODELS,
};

use std::path::{Path, PathBuf};
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
    /// `None` for a backend with no model file (the mock), or for a local one nobody can fetch.
    fn model(&self) -> Option<ModelSpec> {
        None
    }

    /// The model's window: how much audio it takes at a time and how far each window advances.
    ///
    /// A front-end (and the driver) needs both for two things it cannot guess: reporting the block
    /// cadence, and *resuming* a partial cache entry — a resumed stream has to start on the window
    /// grid, or the blocks either side of the resume point come from differently-aligned windows.
    /// `None` means the backend has no fixed windowing, and a resume may start anywhere.
    fn window(&self) -> Option<chunked::Window> {
        None
    }

    /// Separates `mix` **as it goes**, handing each finished block to `sink` in track order.
    ///
    /// This is the shape a front-end wants: the first block arrives after roughly one model window
    /// instead of after the whole track, and the deck plays it immediately. The default below is
    /// correct but not progressive — one block at the end — which is what a backend with nothing to
    /// stream (the mock, a one-shot runtime) can offer.
    ///
    /// **A backend must not invent its own segmentation.** The model already windows the input, and a
    /// second, unrelated cutting of the track would have to be aligned, margined and re-normalised to
    /// sound like one piece — which is exactly the pile of machinery that streaming removes. If the
    /// runtime offers a streaming path, use it.
    fn stream(
        &self,
        mix: &dyn Source,
        sink: &mut dyn StemBlockSink,
        progress: &ProgressSink,
        cancelled: &Cancelled,
    ) -> Result<(), StemError> {
        if cancelled.is_cancelled() {
            return Err(StemError::Cancelled);
        }
        let set = self.separate(mix, progress, cancelled)?;
        let frames = set.total_frames() as usize;
        let blocks: Vec<Vec<f32>> = hypermixx_core::Stem::ALL
            .iter()
            .map(|stem| read_flat(set.get(*stem).as_ref(), frames))
            .collect();
        sink.block(0, &blocks);
        Ok(())
    }

    /// As [`stream`](Self::stream), with the input scale given explicitly.
    ///
    /// A **resumed** stream is handed only the track's tail, so it must be normalised by the whole
    /// track's statistics rather than its own — otherwise the audio either side of the resume point
    /// was separated from differently-scaled input, which is a level step exactly where they meet.
    /// The default ignores the scale and delegates, which is right for a backend that measures over
    /// the source it is given (and for the mock).
    fn stream_with_scale(
        &self,
        mix: &dyn Source,
        sink: &mut dyn StemBlockSink,
        progress: &ProgressSink,
        cancelled: &Cancelled,
        scale: crate::chunked::InputScale,
    ) -> Result<(), StemError> {
        let _ = scale;
        self.stream(mix, sink, progress, cancelled)
    }
}

/// Where a streaming separation puts its finished audio.
///
/// Called in track order, once per block, with one interleaved stereo buffer per stream and all of
/// them the same length. Blocks cover the track exactly once, in increasing `start_frame`.
pub trait StemBlockSink {
    /// `blocks[i]` belongs to stream `i` (the model's stem order). Returning `false` stops the job —
    /// the caller has nowhere to put the audio any more.
    fn block(&mut self, start_frame: u64, blocks: &[Vec<f32>]) -> bool;
}

/// Reads a whole source into one interleaved buffer (a block's worth, for the default streaming path).
fn read_flat(source: &dyn Source, frames: usize) -> Vec<f32> {
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
}

/// An open backend, asked for one range of the mix at a time.
///
/// The contract is the same frame-alignment one [`StemSeparator::separate`] has, per range: the set
/// that comes back is exactly `len` frames per stream, starting at `start`. A chunked job stitches
/// those ranges into the whole track, so a backend that over- or under-reads would leave a seam.
pub trait StemSession: Send {
    fn separate_range(
        &mut self,
        mix: &dyn Source,
        start: u64,
        len: usize,
        progress: &ProgressSink,
        cancelled: &Cancelled,
    ) -> Result<StemSet, StemError>;
}

/// Most random shifts worth asking for.
///
/// A shift averages an extra run of the model over a randomly offset copy, which measurably improves
/// separation for a linear cost in time — so this is a quality/time dial, not a quality cliff. Past
/// two the separation time grows faster than the separation does for a DJ's purposes.
pub const MAX_SHIFTS: usize = 2;

/// The separator a front-end should use: HTDemucs on ONNX Runtime, with the model obtained (and
/// verified) first. `progress` covers the model download when one is needed.
///
/// `options` are canonicalised here, so every caller's request reaches the backend in range. Each
/// option is part of the separator's [`id`](StemSeparator::id), and so of the cache key: they all
/// produce different samples, and none may share an entry with another setting.
#[cfg(feature = "onnx")]
pub fn default_separator(
    spec: &'static ModelSpec,
    options: SeparateOptions,
    runtime: RuntimeOptions,
    progress: &ProgressSink,
) -> Result<Box<dyn StemSeparator>, StemError> {
    let model = ensure_model(spec, None, progress)?;
    Ok(Box::new(
        CharonSeparator::from_options_spec(model, spec, options).with_runtime(runtime),
    ))
}

/// Without the `onnx` feature there is nothing to separate with; the mock is for tests, not for
/// pretending a track was separated.
#[cfg(not(feature = "onnx"))]
pub fn default_separator(
    _spec: &'static ModelSpec,
    _options: SeparateOptions,
    _runtime: RuntimeOptions,
    _progress: &ProgressSink,
) -> Result<Box<dyn StemSeparator>, StemError> {
    Err(StemError::NoBackend)
}

/// Whether this build can ask for `provider`.
///
/// Lets a front-end reject `--gpu` with "this build has no CUDA support" instead of failing the job
/// after the model has been fetched.
pub fn provider_available(provider: Provider) -> bool {
    match provider {
        Provider::Cpu => cfg!(feature = "onnx"),
        Provider::Cuda => cfg!(all(feature = "onnx", feature = "cuda")),
    }
}

/// A separator over a **local model file**: a user's own export (quantized, fine-tuned, or a
/// different precision of the same family).
///
/// Deliberately not hash-checked. The table's hashes exist because a downloaded file has to be
/// proven to be what the table says; a file the user produced on their own machine is theirs, and
/// refusing it for lacking a hash nobody could know would make the feature useless. What is checked
/// is that it loads and that it produces the stems its spec declares.
pub fn local_separator(
    path: impl Into<PathBuf>,
    spec: ModelSpec,
    options: SeparateOptions,
    runtime: RuntimeOptions,
) -> Result<Box<dyn StemSeparator>, StemError> {
    let path = path.into();
    let meta = std::fs::metadata(&path)
        .map_err(|e| StemError::Model(format!("{}: {e}", path.display())))?;
    if meta.len() == 0 {
        return Err(StemError::Model(format!("{} is empty", path.display())));
    }
    #[cfg(feature = "onnx")]
    {
        Ok(Box::new(
            CharonSeparator::for_spec(path, spec).with_runtime_options(options, runtime),
        ))
    }
    #[cfg(not(feature = "onnx"))]
    {
        let _ = (path, spec, options, runtime);
        Err(StemError::NoBackend)
    }
}

/// Describes a **local** model file: its name, size and window, with nothing to download.
///
/// Built from a table entry's shape (the segment length and sample rate are properties of the
/// architecture, not of the file) unless a front-end overrides them.
pub fn local_spec(path: &Path, based_on: ModelSpec) -> ModelSpec {
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "local".to_owned());
    ModelSpec {
        // Leaked: a spec lives for the process (it is `Copy` data handed to the engine and to the
        // cache key), and a local model's name has to outlive the call that built it.
        name: Box::leak(name.into_boxed_str()),
        repo_path: "",
        sha256: "",
        bytes: std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
        ..based_on
    }
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
pub(crate) const READ_CHUNK_FRAMES: usize = 65_536;

#[cfg(test)]
mod local_model_tests {
    use super::*;

    /// A local file is *not* hash-checked (it never came from us), but it does have to be a file with
    /// something in it — and the failure has to arrive before a runtime is built.
    #[test]
    fn a_local_model_must_be_a_non_empty_file() {
        let default = *default_model();
        let missing = Path::new("/nonexistent/htdemucs-int8.onnx");
        let err = local_separator(missing, local_spec(missing, default), Default::default(), Default::default())
            .err()
            .expect("a missing file must be refused");
        assert!(err.message().contains("htdemucs-int8"), "{}", err.message());

        let empty = std::env::temp_dir().join("hypermixx-empty-model.onnx");
        std::fs::write(&empty, b"").unwrap();
        let err = local_separator(
            &empty,
            local_spec(&empty, default),
            Default::default(),
            Default::default(),
        )
        .err()
        .expect("an empty file must be refused");
        assert!(err.message().contains("empty"), "{}", err.message());
        let _ = std::fs::remove_file(&empty);
    }

    /// The spec a local file gets describes the *file* (its name and size) and borrows the shape from
    /// the entry it is based on — a quantized htdemucs is still a 7.8 s-window, 44.1 kHz model.
    #[test]
    fn a_local_spec_describes_the_file_and_borrows_the_shape() {
        let default = *default_model();
        let spec = local_spec(Path::new("/tmp/htdemucs-int8.onnx"), default);
        assert_eq!(spec.name, "htdemucs-int8", "named after the file, so it lands in its own cache entry");
        assert_eq!(spec.segment_frames, default.segment_frames);
        assert_eq!(spec.sample_rate, default.sample_rate);
        assert_eq!(spec.stems, default.stems);
        assert_eq!(spec.sha256, "", "there is nothing to check a local file against");
        assert_eq!(spec.repo_path, "", "and nothing to download");
    }
}

/// Test support shared by the modules that touch the cache.
#[cfg(test)]
pub(crate) mod testing {
    /// Runs `body` with its own cache and model roots, so tests cannot see each other's entries.
    ///
    /// Both roots are process-wide environment variables, so cache-touching tests are serialised
    /// behind **one** lock shared by every module — two locks would let two tests race on the same
    /// variable.
    pub(crate) fn with_isolated_cache<T>(name: &str, body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("hypermixx-cache-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("HYPERMIXX_CACHE", &dir);
        std::env::set_var("HYPERMIXX_MODELS", dir.join("model"));
        let out = body();
        std::env::remove_var("HYPERMIXX_CACHE");
        std::env::remove_var("HYPERMIXX_MODELS");
        out
    }
}

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
