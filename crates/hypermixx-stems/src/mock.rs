//! A separator that needs no model, for tests, CI and development.
//!
//! Not a stand-in for quality — a stand-in for *plumbing*. The engine's stem path (install, level,
//! mute, solo, loop, jump) is entirely independent of how the four streams were produced, so being
//! able to exercise it without a 300 MB download is worth more than any fidelity a mock could have.
//! Both modes are exactly reconstructing, which is what makes "the sum is the mix" assertions
//! possible.

use std::sync::Arc;

use hypermixx_core::{Source, Stem, StemSet, CHANNELS};
use hypermixx_media::{DecodedAudio, PcmPool};

use crate::{Cancelled, ProgressSink, StemError, StemSeparator};

/// How the mock arranges the mix across the four stems.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockMode {
    /// Everything the mix has, in one stem; the other three are digital silence.
    ///
    /// This is the mode for testing *routing*: soloing the chosen stem must reproduce the mix
    /// sample for sample, and soloing any other must be exactly silent. A stem sent to the wrong
    /// index is then impossible to miss.
    Passthrough(Stem),
    /// A quarter of the mix in every stem, so the four sum back to the mix exactly.
    ///
    /// This is the mode for testing the *sum*: the deck at unity must sound like the track it was
    /// separated from.
    QuarterEach,
}

/// A model-free separator. See [`MockMode`].
#[derive(Clone, Copy, Debug)]
pub struct MockSeparator {
    mode: MockMode,
}

impl MockSeparator {
    pub fn new(mode: MockMode) -> Self {
        Self { mode }
    }
}

impl Default for MockSeparator {
    fn default() -> Self {
        Self::new(MockMode::QuarterEach)
    }
}

impl StemSeparator for MockSeparator {
    fn id(&self) -> String {
        match self.mode {
            MockMode::Passthrough(stem) => format!("mock-passthrough-{stem}"),
            MockMode::QuarterEach => "mock-quarter".into(),
        }
    }

    fn separate(
        &self,
        mix: &dyn Source,
        progress: &ProgressSink,
        cancelled: &Cancelled,
    ) -> Result<StemSet, StemError> {
        let mut read = 0.0f32;
        let pcm = crate::read_source(mix, &mut |fraction| {
            read = fraction;
            progress(fraction * 0.5);
        })?;
        if cancelled.is_cancelled() {
            return Err(StemError::Cancelled);
        }
        let _ = read;
        let frames = mix.total_frames();
        let pool = |samples: Vec<f32>| -> Arc<dyn Source> {
            Arc::new(PcmPool::from_decoded(DecodedAudio {
                total_frames: frames,
                sample_rate: hypermixx_core::SAMPLE_RATE,
                channels: CHANNELS,
                pcm: samples,
            }))
        };

        // Built in `Stem::ALL` order, which is the engine's stream order.
        let mut stems: [Arc<dyn Source>; Stem::COUNT] = std::array::from_fn(|_| pool(Vec::new()));
        for stem in Stem::ALL {
            let samples = match self.mode {
                MockMode::QuarterEach => pcm.iter().map(|s| s * 0.25).collect(),
                MockMode::Passthrough(target) => {
                    if target == stem {
                        pcm.clone()
                    } else {
                        vec![0.0; pcm.len()]
                    }
                }
            };
            stems[stem.index()] = pool(samples);
            progress(0.5 + 0.5 * (stem.index() + 1) as f32 / Stem::COUNT as f32);
        }
        Ok(StemSet::new(stems))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{no_progress, Cancelled, StemSeparator};
    use hypermixx_media::{DecodedAudio, PcmPool};

    fn mix(frames: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..frames as usize)
                .flat_map(|i| [i as f32, -(i as f32)])
                .collect(),
            total_frames: frames,
            sample_rate: hypermixx_core::SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    fn read_all(source: &dyn Source) -> Vec<f32> {
        let mut out = vec![0.0; source.total_frames() as usize * CHANNELS];
        let n = source.read_frames(0, &mut out);
        out.truncate(n * CHANNELS);
        out
    }

    #[test]
    fn quarter_each_sums_back_to_the_mix_sample_for_sample() {
        let source = mix(1_000);
        let set = MockSeparator::new(MockMode::QuarterEach)
            .separate(source.as_ref(), &no_progress(), &Cancelled::new())
            .unwrap();
        let original = read_all(source.as_ref());
        for (i, want) in original.iter().enumerate() {
            let sum: f32 = set.stems.iter().map(|s| read_all(s.as_ref())[i]).sum();
            assert!((sum - want).abs() < 1e-4, "sample {i}: {sum} != {want}");
        }
    }

    #[test]
    fn passthrough_puts_everything_in_one_stem() {
        let source = mix(500);
        let set = MockSeparator::new(MockMode::Passthrough(Stem::Vocals))
            .separate(source.as_ref(), &no_progress(), &Cancelled::new())
            .unwrap();
        let original = read_all(source.as_ref());
        for stem in Stem::ALL {
            let got = read_all(set.get(stem).as_ref());
            if stem == Stem::Vocals {
                assert_eq!(got, original, "the target stem carries the mix");
            } else {
                assert!(got.iter().all(|s| *s == 0.0), "{stem} must be silent");
            }
        }
    }

    #[test]
    fn progress_is_monotonic_and_reaches_one() {
        let source = mix(2_000);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: ProgressSink = {
            let seen = Arc::clone(&seen);
            Arc::new(move |f| seen.lock().unwrap().push(f))
        };
        MockSeparator::default()
            .separate(source.as_ref(), &sink, &Cancelled::new())
            .unwrap();
        let seen = seen.lock().unwrap();
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
        assert!((seen.last().copied().unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn the_id_distinguishes_modes_because_it_keys_the_cache() {
        assert_ne!(
            MockSeparator::new(MockMode::QuarterEach).id(),
            MockSeparator::new(MockMode::Passthrough(Stem::Drums)).id()
        );
    }
}
