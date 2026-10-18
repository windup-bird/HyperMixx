//! The real backend: HTDemucs through `charon-audio` (ONNX Runtime, CPU).
//!
//! Two things this module is responsible for, beyond calling the model:
//!
//! * **The progress/cancel bridge.** `charon`'s callback must be `'static + Send + Sync`, so our
//!   borrowed `ProgressSink` cannot be handed to it directly. A `CancelToken` is created here, the
//!   closure captures a clone of it plus our own [`Cancelled`] flag, and observing our flag turns
//!   into calling `cancel()` — which is how a front-end's cancel reaches the model between windows.
//! * **The frame-alignment contract.** The engine's four streams share one clock, so four streams of
//!   different lengths would silently diverge. The output is checked against the input length here,
//!   where the result is built, rather than trusted downstream.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use charon_audio::control::{CancelToken, Control};
use charon_audio::models::ExecutionProvider;
use charon_audio::{AudioBuffer, Separator, SeparatorConfig};
use hypermixx_core::{Source, Stem, StemSet, CHANNELS, SAMPLE_RATE};
use hypermixx_media::{DecodedAudio, PcmPool};
use ndarray::Array2;

use crate::model::{default_model, ModelSpec};
use crate::{Provider, RuntimeOptions, SeparateOptions, DEFAULT_OVERLAP, MAX_OVERLAP};
use crate::{Cancelled, ProgressSink, StemError, StemSeparator};

/// HTDemucs on ONNX Runtime.
///
/// One model file, one session, either execution provider. The graph's contract is
/// `mix [1,2,343980] -> stems [1,4,2,343980]`.
pub struct CharonSeparator {
    model: PathBuf,
    /// The table entry this file came from: its name goes into the cache key, and its window (segment
    /// length) decides the block cadence and where a resumed stream may start. An owned copy, because
    /// a **local** file (a user's own export) is described by a spec built at run time.
    spec: ModelSpec,
    /// Ensemble shifts. `0` and `1` are the same thing (one pass, no shift averaging); `2` adds an
    /// averaged shifted run for ~2× the time.
    shifts: usize,
    /// Window overlap. The window itself is pinned by the export (343_980 samples = 7.8 s), so this
    /// is the only stride dial; lower is faster and gives the model's window edges more weight.
    overlap: f32,
    provider: Provider,
    /// Threads and memory. Not part of `id()`: it changes how the work is scheduled, not what comes
    /// out of it.
    runtime: RuntimeOptions,
}

impl CharonSeparator {
    /// A separator over an existing model file, on the CPU. Use [`crate::ensure_model`] to obtain
    /// one.
    pub fn new(model: impl Into<PathBuf>) -> Self {
        Self::for_spec(model, *default_model())
    }

    /// As [`CharonSeparator::new`], for a model taken from the table (or described by a spec built at
    /// run time, which is how a local file is loaded).
    pub fn for_spec(model: impl Into<PathBuf>, spec: ModelSpec) -> Self {
        Self {
            model: model.into(),
            spec,
            shifts: 1,
            overlap: DEFAULT_OVERLAP,
            provider: Provider::Cpu,
            runtime: RuntimeOptions::default(),
        }
    }

    /// Random-shift averaging count, clamped to [`crate::MAX_SHIFTS`] (`charon` itself allows more;
    /// see that constant for why this does not).
    pub fn with_shifts(mut self, shifts: usize) -> Self {
        self.shifts = shifts.min(crate::MAX_SHIFTS);
        self
    }

    /// Window overlap, clamped to `[0, 0.5]`. `0.0` is the fastest setting — 24% faster than the
    /// 0.25 default, because it removes a quarter of the model windows — and gives each window's
    /// edges more weight, which is a listening judgement rather than a numeric one.
    pub fn with_overlap(mut self, overlap: f32) -> Self {
        self.overlap = if overlap.is_finite() {
            overlap.clamp(0.0, MAX_OVERLAP)
        } else {
            DEFAULT_OVERLAP
        };
        self
    }

    /// The execution provider.
    ///
    /// Asking for CUDA is a *requirement*, not a hint: if the provider cannot take the whole graph
    /// (no CUDA/cuDNN, the provider library missing, or an unsupported node) the separation fails
    /// with an explanation instead of quietly running on the CPU at a ninth of the speed.
    pub fn with_provider(mut self, provider: Provider) -> Self {
        self.provider = provider;
        self
    }

    /// The provider this separator will ask for.
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// Threads and memory — see [`RuntimeOptions`] for why a low-end device wants this.
    pub fn with_runtime(mut self, runtime: RuntimeOptions) -> Self {
        self.runtime = runtime;
        self
    }

    /// What the backend calls that provider: `charon` reports `"CPU"`/`"CUDA"`.
    fn expected_label(provider: Provider) -> &'static str {
        match provider {
            Provider::Cpu => "CPU",
            Provider::Cuda => "CUDA",
        }
    }

    /// A separator configured in one call, from the options a front-end collected.
    pub fn from_options(model: impl Into<PathBuf>, options: SeparateOptions) -> Self {
        Self::for_spec(model, *default_model()).with_options(options)
    }

    /// As [`CharonSeparator::from_options`], for a model taken from the table.
    pub fn from_options_spec(
        model: impl Into<PathBuf>,
        spec: &'static ModelSpec,
        options: SeparateOptions,
    ) -> Self {
        Self::for_spec(model, *spec).with_options(options)
    }

    fn with_options(mut self, options: SeparateOptions) -> Self {
        let options = options.sanitised();
        self.shifts = options.shifts;
        self.overlap = options.overlap;
        self.provider = options.provider;
        self
    }

    /// As [`CharonSeparator::with_options`], plus the runtime dials.
    pub fn with_runtime_options(
        mut self,
        options: SeparateOptions,
        runtime: RuntimeOptions,
    ) -> Self {
        self.runtime = runtime;
        self.with_options(options)
    }

    pub fn model_path(&self) -> &Path {
        &self.model
    }

    /// The table entry (or run-time description) this separator runs.
    pub fn spec(&self) -> ModelSpec {
        self.spec
    }
}

impl StemSeparator for CharonSeparator {
    /// Everything that changes the audio goes in the id, because the id is the cache key: shifts,
    /// overlap and the execution provider all produce *different samples*, so none of them may share
    /// an entry with another setting.
    ///
    /// `shifts` is canonicalised to at least 1 first — `charon` documents 0 and 1 as the same single
    /// pass, so treating them as one key avoids storing the same 300 MB twice.
    fn id(&self) -> String {
        format!(
            "charon-{}-s{}-o{}-{}",
            self.spec.name,
            self.shifts.max(1),
            self.overlap,
            self.provider.label()
        )
    }

    fn model(&self) -> Option<ModelSpec> {
        Some(self.spec)
    }

    /// HTDemucs's window, as the runtime will lay it: the export's fixed segment, and the stride its
    /// overlap implies. The driver uses this to place a resumed stream on the grid.
    fn window(&self) -> Option<crate::chunked::Window> {
        Some(crate::chunked::Window {
            segment_frames: self.spec.segment_frames,
            stride_frames: crate::stride_frames(self.spec.segment_frames, self.overlap),
        })
    }

    /// The streaming path: charon owns the segmentation (the model's own window grid), the margins
    /// and the overlap-add, and emits each block the moment no later window can change it.
    ///
    /// Its output is **sample-for-sample identical** to the whole-track path (charon tests that, and
    /// `tests/real_model.rs` checks it here too), which is the property the deck leans on: blocks can
    /// be installed as they arrive without the audio having a seam at every block boundary.
    ///
    /// Time shifts are the one thing the streaming processor refuses (`NotSupported`), so `shifts > 1`
    /// falls back to the whole-track path — one block, at the end. That is the honest trade: shifts
    /// is a quality dial, and paying for it costs the progressive install.
    fn stream_with_scale(
        &self,
        mix: &dyn Source,
        sink: &mut dyn crate::StemBlockSink,
        progress: &ProgressSink,
        cancelled: &Cancelled,
        scale: crate::chunked::InputScale,
    ) -> Result<(), StemError> {
        if cancelled.is_cancelled() {
            return Err(StemError::Cancelled);
        }
        if self.shifts > 1 {
            // The default implementation: whole track, one block.
            return self.stream_whole(mix, sink, progress, cancelled);
        }
        let separator = self.open_session()?;
        let mut source = MixSource {
            mix,
            scale,
            at: 0,
        };
        let token = CancelToken::new();
        let control = {
            // The callback has to be `'static`, so the sink is cloned into it rather than borrowed.
            let progress = Arc::clone(progress);
            let flag = cancelled.clone();
            let token = token.clone();
            Control::new().with_cancel(token.clone()).with_progress(move |p| {
                // Our cancel flag is the one a front-end can reach; forwarding it here is what makes
                // a cancel land between model windows instead of at the end of the job.
                if flag.is_cancelled() {
                    token.cancel();
                }
                progress(p.fraction() as f32);
            })
        };
        let mut adapter = BlockAdapter {
            sink,
            stems: Stem::COUNT,
            pending: Vec::new(),
            emitted: 0,
            scale,
        };
        separator
            .separate_stream(&mut source, &mut adapter, &control)
            .map_err(|e| match e {
                charon_audio::error::CharonError::Cancelled => StemError::Cancelled,
                other => StemError::Backend(other.to_string()),
            })?;
        if cancelled.is_cancelled() {
            return Err(StemError::Cancelled);
        }
        Ok(())
    }

    /// The one-shot form: the whole track through the same normalisation, so it agrees with the
    /// streaming path sample for sample.
    fn separate(
        &self,
        mix: &dyn Source,
        progress: &ProgressSink,
        cancelled: &Cancelled,
    ) -> Result<StemSet, StemError> {
        let scale = crate::chunked::InputScale::of(mix)?;
        self.separate_with_scale(mix, scale, progress, cancelled)
    }
}

impl CharonSeparator {
    /// The whole-track form, as one block.
    fn stream_whole(
        &self,
        mix: &dyn Source,
        sink: &mut dyn crate::StemBlockSink,
        progress: &ProgressSink,
        cancelled: &Cancelled,
    ) -> Result<(), StemError> {
        let frames = mix.total_frames() as usize;
        let scale = crate::chunked::InputScale::of(mix)?;
        let set = self.separate_with_scale(mix, scale, progress, cancelled)?;
        let blocks: Vec<Vec<f32>> = Stem::ALL
            .iter()
            .map(|stem| {
                let source = set.get(*stem);
                let mut pcm = vec![0.0f32; frames * CHANNELS];
                let read = source.read_frames(0, &mut pcm);
                pcm.truncate(read * CHANNELS);
                pcm
            })
            .collect();
        sink.block(0, &blocks);
        Ok(())
    }

    /// The whole-track path with an explicit scale — the same normalisation every streaming block
    /// gets, so the two paths agree.
    fn separate_with_scale(
        &self,
        mix: &dyn Source,
        scale: crate::chunked::InputScale,
        progress: &ProgressSink,
        cancelled: &Cancelled,
    ) -> Result<StemSet, StemError> {
        let frames = mix.total_frames() as usize;
        let pcm = crate::read_source(mix, &mut |fraction| progress(fraction * 0.05))?;
        let mut data = Array2::<f32>::zeros((CHANNELS, frames));
        for frame in 0..frames {
            for channel in 0..CHANNELS {
                data[[channel, frame]] = scale.apply(pcm[frame * CHANNELS + channel]);
            }
        }
        let buffer = AudioBuffer::new(data, SAMPLE_RATE);
        let separator = self.open_session()?;
        let token = CancelToken::new();
        let flag = cancelled.clone();
        let progress = Arc::clone(progress);
        let token2 = token.clone();
        let control = Control::new()
            .with_cancel(token2.clone())
            .with_progress(move |p| {
                if flag.is_cancelled() {
                    token2.cancel();
                }
                progress(0.05 + 0.95 * p.fraction() as f32);
            });
        let stems = separator
            .separate_with(&buffer, &control)
            .map_err(|e| match e {
                charon_audio::error::CharonError::Cancelled => StemError::Cancelled,
                other => StemError::Backend(other.to_string()),
            })?;
        build_set(stems.sources, frames, scale)
    }
}

/// Charon's random-access input, over our `Source`, with the job's normalisation applied.
///
/// The processor reads the input in blocks (and reads it once more for its statistics unless the
/// scale is given), so this is exactly the interface it wants: no whole-track buffer in memory.
struct MixSource<'a> {
    mix: &'a dyn Source,
    scale: crate::chunked::InputScale,
    at: u64,
}

impl charon_audio::stream::AudioSource for MixSource<'_> {
    fn channels(&self) -> usize {
        CHANNELS
    }

    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn len(&self) -> usize {
        self.mix.total_frames() as usize
    }

    fn read(
        &mut self,
        start: usize,
        mut out: ndarray::ArrayViewMut2<f32>,
    ) -> charon_audio::Result<()> {
        // `out` is `[channels, n]`; our sources are interleaved stereo.
        let frames = out.ncols();
        for frame in 0..frames {
            let mut sample = [0.0f32; CHANNELS];
            self.mix
                .read_frames((start + frame) as u64, &mut sample);
            for channel in 0..CHANNELS {
                out[[channel, frame]] = self.scale.apply(sample[channel]);
            }
        }
        self.at = (start + frames) as u64;
        Ok(())
    }
}

/// Collects charon's per-stem writes into one block per window and hands it to our sink.
///
/// Charon writes every stem of a block consecutively, so the block is complete when the last stem
/// arrives — which is also the moment the deck can play it.
struct BlockAdapter<'a> {
    sink: &'a mut dyn crate::StemBlockSink,
    stems: usize,
    pending: Vec<Vec<f32>>,
    emitted: u64,
    /// The job's normalisation, undone here so the caller is handed audio at the track's own level —
    /// the input was scaled on the way in, and charon's own undo is switched off.
    scale: crate::chunked::InputScale,
}

impl charon_audio::stream::StemSink for BlockAdapter<'_> {
    fn begin(
        &mut self,
        _stems: &[String],
        _channels: usize,
        _sample_rate: u32,
    ) -> charon_audio::Result<()> {
        self.pending.clear();
        self.emitted = 0;
        Ok(())
    }

    fn write(
        &mut self,
        stem: usize,
        block: ndarray::ArrayView2<f32>,
    ) -> charon_audio::Result<()> {
        let frames = block.ncols();
        if self.pending.len() <= stem {
            self.pending.resize_with(stem + 1, Vec::new);
        }
        // Planar [channels, frames] back to the interleaved form the engine reads.
        let mut pcm = vec![0.0f32; frames * CHANNELS];
        for frame in 0..frames {
            for channel in 0..CHANNELS {
                pcm[frame * CHANNELS + channel] = self.scale.undo(block[[channel, frame]]);
            }
        }
        self.pending[stem] = pcm;
        if stem + 1 == self.stems {
            // The block is complete: the scale is undone here, so the caller is handed audio at the
            // track's own level.
            let complete = std::mem::take(&mut self.pending);
            if complete.iter().all(|block| !block.is_empty()) {
                if !self.sink.block(self.emitted, &complete) {
                    return Err(charon_audio::error::CharonError::Cancelled);
                }
                self.emitted += complete[0].len() as u64 / CHANNELS as u64;
            }
        }
        Ok(())
    }
}

impl CharonSeparator {
    /// Loads the model and checks the execution provider — the expensive half of a job, split out so
    /// it can happen once.
    fn open_session(&self) -> Result<Separator, StemError> {
        // Every model in the table is an HTDemucs-shaped export: `mix [1,2,segment] ->
        // stems [1,4,2,segment]`, the four sources in `Stem::ALL` order.
        let mut config = SeparatorConfig::htdemucs(&self.model)
            .with_shifts(self.shifts.max(1))
            .with_overlap(self.overlap)
            .with_progress(false);
        // The input scale is the job's, applied on the way in and undone on the way out, so charon
        // must not measure statistics of its own on top of it.
        config.process.normalize = false;
        // Scheduling and memory: the two dials a small device needs. Neither changes the audio.
        config.model.onnx.intra_threads = self.runtime.threads;
        if self.runtime.low_memory {
            config.model.onnx.memory_pattern = false;
            config.model.onnx.disabled_optimizers = vec!["ConstantFolding".to_string()];
        } else {
            config.model.onnx.memory_pattern = true;
            config.model.onnx.disabled_optimizers.clear();
        }
        config.model.onnx.execution_provider = match self.provider {
            Provider::Cpu => ExecutionProvider::Cpu,
            Provider::Cuda => ExecutionProvider::Cuda,
        };
        let separator = Separator::new(config).map_err(|e| self.session_error(&e))?;
        // No silent downgrade. `with_execution_providers` reports a provider that *failed to
        // register* only through a log line, and ONNX Runtime falls back to the CPU for any node an
        // EP will not take — either way the run is correct and merely slow, which is exactly the
        // failure that is invisible without a check. The session used the provider we asked for, or
        // this is an error.
        let running = separator.provider();
        if running != Self::expected_label(self.provider) {
            return Err(StemError::Backend(format!(
                "asked for the {} execution provider but the session is on {running}; \
                 refusing to run a ninth as fast while claiming otherwise",
                self.provider.label()
            )));
        }
        Ok(separator)
    }
}

/// Turns charon's per-name buffers into a [`StemSet`] in [`Stem::ALL`] order, checking the
/// frame-alignment contract on the way.
fn build_set(
    sources: HashMap<String, AudioBuffer>,
    frames: usize,
    scale: crate::chunked::InputScale,
) -> Result<StemSet, StemError> {
    let mut stems: [Arc<dyn Source>; Stem::COUNT] =
        std::array::from_fn(|_| empty());
    for stem in Stem::ALL {
        let buffer = sources.get(stem.name()).ok_or_else(|| {
            StemError::Backend(format!(
                "the model did not return a `{stem}` stem (got {:?})",
                {
                    let mut names: Vec<&str> = sources.keys().map(String::as_str).collect();
                    names.sort_unstable();
                    names
                }
            ))
        })?;
        let got = buffer.samples();
        if got != frames {
            return Err(StemError::Backend(format!(
                "{stem} is {got} frames but the mix is {frames}; the engine's streams share one \
                 clock, so a set that is not frame-aligned would drift"
            )));
        }
        // Planar [channels, frames] back to the interleaved form the engine reads.
        let mut pcm = vec![0.0f32; frames * CHANNELS];
        for frame in 0..frames {
            for channel in 0..CHANNELS {
                // Undo the job's normalisation: what comes out is audio at the track's own level.
                pcm[frame * CHANNELS + channel] = scale.undo(buffer.data[[channel, frame]]);
            }
        }
        stems[stem.index()] = Arc::new(PcmPool::from_decoded(DecodedAudio {
            total_frames: frames as u64,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            pcm,
        }));
    }
    Ok(StemSet::new(stems))
}

impl CharonSeparator {
    /// Turns a session-construction failure into something a user can act on. A CUDA session fails
    /// for one of three reasons, and none of them is obvious from ONNX Runtime's own message.
    fn session_error(&self, err: &charon_audio::error::CharonError) -> StemError {
        let detail = err.to_string();
        if self.provider != Provider::Cuda {
            return StemError::Backend(format!("{}: {detail}", self.model.display()));
        }
        StemError::Backend(format!(
            "the CUDA execution provider could not run this model ({detail}). \n\
             A CUDA session needs: the `cuda` cargo feature, CUDA 13 + cuDNN 9 on the host, and \
             libonnxruntime_providers_cuda.so next to the executable. It also refuses to fall back \
             to the CPU, so any node the provider cannot take fails here — drop `--gpu` (or set the \
             provider to cpu) to separate on the CPU instead."
        ))
    }
}

fn empty() -> Arc<dyn Source> {
    Arc::new(PcmPool::from_decoded(DecodedAudio {
        pcm: Vec::new(),
        total_frames: 0,
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every option that changes the samples has to change the id, because the id is the cache key.
    ///
    /// The specific case this guards: a CPU-produced entry must never be reused for a `--gpu`
    /// request (and vice versa) — the two are different audio, and silently serving one for the
    /// other would make the flag look like it did nothing.
    #[test]
    fn the_id_carries_every_option_that_changes_the_audio() {
        let variants = || {
            [
                CharonSeparator::new("m.onnx"),
                CharonSeparator::new("m.onnx").with_shifts(2),
                CharonSeparator::new("m.onnx").with_overlap(0.0),
                CharonSeparator::new("m.onnx").with_provider(Provider::Cuda),
                CharonSeparator::new("m.onnx").with_overlap(0.1).with_shifts(2),
            ]
        };
        let ids: Vec<String> = variants().iter().map(CharonSeparator::id).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "options collide in the cache key: {ids:?}");
        assert!(ids[0].ends_with("-cpu"), "{}", ids[0]);
        assert!(ids[3].ends_with("-cuda"), "{}", ids[3]);
        // The id names the model, so a future second model cannot collide either.
        assert!(ids[0].starts_with("charon-htdemucs-"), "{}", ids[0]);
        // 0 and 1 are one pass, so they must *not* be two entries.
        assert_eq!(
            CharonSeparator::new("m.onnx").with_shifts(0).id(),
            CharonSeparator::new("m.onnx").with_shifts(1).id()
        );
    }

    /// The alignment check is the contract the engine relies on, and it is cheaper to test it here
    /// than to discover a drift in the mixer.
    #[test]
    fn a_short_stem_is_refused() {
        let mut sources = HashMap::new();
        for stem in Stem::ALL {
            let frames = if stem == Stem::Vocals { 9 } else { 10 };
            sources.insert(
                stem.name().to_owned(),
                AudioBuffer::new(Array2::<f32>::zeros((CHANNELS, frames)), SAMPLE_RATE),
            );
        }
        let err = build_set(sources, 10, crate::chunked::InputScale::unity()).unwrap_err();
        assert!(format!("{err}").contains("frame-aligned"), "{err}");
    }

    #[test]
    fn a_missing_stem_is_refused() {
        let mut sources = HashMap::new();
        sources.insert(
            "drums".to_owned(),
            AudioBuffer::new(Array2::<f32>::zeros((CHANNELS, 10)), SAMPLE_RATE),
        );
        let err = build_set(sources, 10, crate::chunked::InputScale::unity()).unwrap_err();
        assert!(format!("{err}").contains("did not return"), "{err}");
    }

    #[test]
    fn stems_land_in_model_order_not_name_order() {
        let mut sources = HashMap::new();
        for (i, stem) in Stem::ALL.iter().enumerate() {
            let mut data = Array2::<f32>::zeros((CHANNELS, 4));
            data[[0, 0]] = (i + 1) as f32;
            sources.insert(stem.name().to_owned(), AudioBuffer::new(data, SAMPLE_RATE));
        }
        let set = build_set(sources, 4, crate::chunked::InputScale::unity()).unwrap();
        for (i, stem) in Stem::ALL.iter().enumerate() {
            let mut out = [0.0f32; 2];
            set.get(*stem).read_frames(0, &mut out);
            assert_eq!(out[0], (i + 1) as f32, "{stem} is in the wrong slot");
        }
    }
}
