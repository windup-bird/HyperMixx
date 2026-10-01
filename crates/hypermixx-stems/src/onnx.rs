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
use charon_audio::{AudioBuffer, Separator, SeparatorConfig};
use hypermixx_core::{Source, Stem, StemSet, CHANNELS, SAMPLE_RATE};
use hypermixx_media::{DecodedAudio, PcmPool};
use ndarray::Array2;

use crate::model::{ModelSpec, HTDEMUCS};
use crate::{Cancelled, ProgressSink, StemError, StemSeparator};

/// HTDemucs, on ONNX Runtime's CPU execution provider.
///
/// The CPU provider is not a fallback: the ONNX export this crate pins runs its STFT in-graph, which
/// the export only supports on CPU. A GPU would need the split-transform export, which is a
/// different file with a different contract.
pub struct CharonSeparator {
    model: PathBuf,
    /// How many random shifts to average. `0` is fastest; the model's own default of 1 already
    /// improves separation noticeably for ~2× the time.
    shifts: usize,
}

impl CharonSeparator {
    /// A separator over an existing model file. Use [`crate::ensure_model`] to obtain one.
    pub fn new(model: impl Into<PathBuf>) -> Self {
        Self {
            model: model.into(),
            shifts: 1,
        }
    }

    /// Random-shift averaging count, clamped to [`crate::MAX_SHIFTS`] (`charon` itself allows more;
    /// see that constant for why this does not).
    pub fn with_shifts(mut self, shifts: usize) -> Self {
        self.shifts = shifts.min(crate::MAX_SHIFTS);
        self
    }

    pub fn model_path(&self) -> &Path {
        &self.model
    }
}

impl StemSeparator for CharonSeparator {
    /// The shift count is part of the id, which is part of the cache key: two shift counts produce
    /// different audio, so they must not share a cache entry.
    fn id(&self) -> String {
        format!("charon-{}-s{}", HTDEMUCS.name, self.shifts)
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

        let config = SeparatorConfig::htdemucs(&self.model)
            .with_shifts(self.shifts)
            .with_progress(false);
        let separator = Separator::new(config)
            .map_err(|e| StemError::Backend(format!("{}: {e}", self.model.display())))?;

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
