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

use crate::model::{ModelSpec, HTDEMUCS};
use crate::{Provider, SeparateOptions, DEFAULT_OVERLAP, MAX_OVERLAP};
use crate::{Cancelled, ProgressSink, StemError, StemSeparator};

/// HTDemucs on ONNX Runtime.
///
/// One model file, one session, either execution provider. The graph's contract is
/// `mix [1,2,343980] -> stems [1,4,2,343980]`.
pub struct CharonSeparator {
    model: PathBuf,
    /// Ensemble shifts. `0` and `1` are the same thing (one pass, no shift averaging); `2` adds an
    /// averaged shifted run for ~2× the time.
    shifts: usize,
    /// Window overlap. The window itself is pinned by the export (343_980 samples = 7.8 s), so this
    /// is the only stride dial; lower is faster and gives the model's window edges more weight.
    overlap: f32,
    provider: Provider,
}

impl CharonSeparator {
    /// A separator over an existing model file, on the CPU. Use [`crate::ensure_model`] to obtain
    /// one.
    pub fn new(model: impl Into<PathBuf>) -> Self {
        Self {
            model: model.into(),
            shifts: 1,
            overlap: DEFAULT_OVERLAP,
            provider: Provider::Cpu,
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

    /// What the backend calls that provider: `charon` reports `"CPU"`/`"CUDA"`.
    fn expected_label(provider: Provider) -> &'static str {
        match provider {
            Provider::Cpu => "CPU",
            Provider::Cuda => "CUDA",
        }
    }

    /// A separator configured in one call, from the options a front-end collected.
    pub fn from_options(model: impl Into<PathBuf>, options: SeparateOptions) -> Self {
        let options = options.sanitised();
        Self::new(model)
            .with_shifts(options.shifts)
            .with_overlap(options.overlap)
            .with_provider(options.provider)
    }

    pub fn model_path(&self) -> &Path {
        &self.model
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
            HTDEMUCS.name,
            self.shifts.max(1),
            self.overlap,
            self.provider.label()
        )
    }

    fn model(&self) -> Option<&'static ModelSpec> {
        Some(&HTDEMUCS)
    }

    fn separate(
        &self,
        mix: &dyn Source,
        progress: &ProgressSink,
        cancelled: &Cancelled,
    ) -> Result<StemSet, StemError> {
        if cancelled.is_cancelled() {
            return Err(StemError::Cancelled);
        }
        let frames = mix.total_frames() as usize;
        // Reading is the first ~0-5% of the wait; the model is the rest.
        let pcm = crate::read_source(mix, &mut |fraction| progress(fraction * 0.05))?;

        let mut data = Array2::<f32>::zeros((CHANNELS, frames));
        for frame in 0..frames {
            for channel in 0..CHANNELS {
                data[[channel, frame]] = pcm[frame * CHANNELS + channel];
            }
        }
        let buffer = AudioBuffer::new(data, SAMPLE_RATE);

        let mut config = SeparatorConfig::htdemucs(&self.model)
            .with_shifts(self.shifts.max(1))
            .with_overlap(self.overlap)
            .with_progress(false);
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
                progress(0.05 + 0.95 * p.fraction() as f32);
            })
        };
        let stems = separator
            .separate_with(&buffer, &control)
            .map_err(|e| match e {
                charon_audio::error::CharonError::Cancelled => StemError::Cancelled,
                other => StemError::Backend(other.to_string()),
            })?;
        if cancelled.is_cancelled() {
            return Err(StemError::Cancelled);
        }
        build_set(stems.sources, frames)
    }
}

/// Turns charon's per-name buffers into a [`StemSet`] in [`Stem::ALL`] order, checking the
/// frame-alignment contract on the way.
fn build_set(
    sources: HashMap<String, AudioBuffer>,
    frames: usize,
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
                pcm[frame * CHANNELS + channel] = buffer.data[[channel, frame]];
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
        let err = build_set(sources, 10).unwrap_err();
        assert!(format!("{err}").contains("frame-aligned"), "{err}");
    }

    #[test]
    fn a_missing_stem_is_refused() {
        let mut sources = HashMap::new();
        sources.insert(
            "drums".to_owned(),
            AudioBuffer::new(Array2::<f32>::zeros((CHANNELS, 10)), SAMPLE_RATE),
        );
        let err = build_set(sources, 10).unwrap_err();
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
        let set = build_set(sources, 4).unwrap();
        for (i, stem) in Stem::ALL.iter().enumerate() {
            let mut out = [0.0f32; 2];
            set.get(*stem).read_frames(0, &mut out);
            assert_eq!(out[0], (i + 1) as f32, "{stem} is in the wrong slot");
        }
    }
}
