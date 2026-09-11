//! Beat/key analysis adapter: stratum-dsp → hypermixx `TrackAnalysis`.
//!
//! Takes mono f32 PCM (already decoded by the engine), feeds it to stratum-dsp's `analyze_audio`
//! with a reduced config, and converts the second-based beat grid + key into absolute frames.

use std::panic::{catch_unwind, AssertUnwindSafe};

use hypermixx_audio::beatgrid::{BeatGrid, KeyMode, KeyReport, TrackAnalysis};
use stratum_dsp::{analyze_audio, compute_confidence, AnalysisConfig, AnalysisResult};

/// Analysis failure reasons surfaced to the caller.
#[derive(Debug)]
pub enum AnalysisError {
    /// stratum-dsp returned an error.
    Stratum(String),
    /// The analysis thread panicked (caught, not propagated).
    Panicked(String),
    /// stratum-dsp "succeeded" but produced no usable output (bpm=0, empty grid).
    NoGrid,
}

impl std::fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalysisError::Stratum(msg) => write!(f, "analysis failed: {msg}"),
            AnalysisError::Panicked(msg) => write!(f, "analysis panicked: {msg}"),
            AnalysisError::NoGrid => write!(f, "analysis produced no usable beat grid"),
        }
    }
}

impl std::error::Error for AnalysisError {}

/// Reads all frames from a `Source` and downmixes interleaved stereo to mono.
pub fn downmix_to_mono(
    source: &dyn hypermixx_audio::Source,
    total_frames: u64,
    channels: usize,
) -> Vec<f32> {
    let mut mono = Vec::with_capacity(total_frames as usize);
    let mut block = vec![0.0f32; 4096 * channels];
    let mut start = 0u64;
    while start < total_frames {
        let read = source.read_frames(start, &mut block);
        if read == 0 {
            break;
        }
        for frame in block[..read * channels].chunks_exact(channels) {
            let sum: f32 = frame.iter().sum();
            mono.push(sum / channels as f32);
        }
        start += read as u64;
    }
    mono
}

/// Runs stratum-dsp on mono PCM and converts the result to a `TrackAnalysis`.
pub fn analyze(mono: &[f32], sample_rate: u32) -> Result<TrackAnalysis, AnalysisError> {
    let config = reduced_config();
    let result = catch_unwind(AssertUnwindSafe(|| {
        analyze_audio(mono, sample_rate, config)
    }))
    .map_err(|panic| {
        let msg = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        AnalysisError::Panicked(msg)
    })?
    .map_err(|err| AnalysisError::Stratum(err.to_string()))?;

    validate(&result)?;
    Ok(to_track_analysis(&result, sample_rate))
}

/// A config that trades accuracy headroom for predictable time and memory.
///
/// The important flag is `enable_silence_trimming = false`: stratum-dsp returns beat times
/// relative to the *trimmed* signal and discards the offset, so leaving it on would shift the
/// entire grid. The other flags reduce peak memory from ~1 GB to a few hundred MB per track.
fn reduced_config() -> AnalysisConfig {
    AnalysisConfig {
        enable_silence_trimming: false,
        enable_tempogram_multi_resolution: false,
        enable_tempogram_band_fusion: false,
        enable_tempogram_mel_novelty: false,
        key_stft_frame_size: 4096,
        ..AnalysisConfig::default()
    }
}

/// stratum-dsp is "best effort": it returns Ok even with bpm=0 or an empty grid.
fn validate(result: &AnalysisResult) -> Result<(), AnalysisError> {
    if result.bpm <= 0.0 {
        return Err(AnalysisError::NoGrid);
    }
    if result.beat_grid.beats.is_empty() {
        return Err(AnalysisError::NoGrid);
    }
    Ok(())
}

fn to_track_analysis(result: &AnalysisResult, sample_rate: u32) -> TrackAnalysis {
    let beatgrid = BeatGrid::from_seconds(&result.beat_grid.beats, sample_rate);
    let confidence = compute_confidence(result);

    let key = KeyReport {
        pc: match result.key {
            stratum_dsp::Key::Major(pc) => pc as u8,
            stratum_dsp::Key::Minor(pc) => pc as u8,
        },
        mode: match result.key {
            stratum_dsp::Key::Major(_) => KeyMode::Major,
            stratum_dsp::Key::Minor(_) => KeyMode::Minor,
        },
        confidence: confidence.key_confidence,
    };

    TrackAnalysis {
        beatgrid,
        key: Some(key),
        bpm: Some(result.bpm),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_audio::Source;

    /// Generates a mono click track at `bpm` with a sine body, so stratum-dsp has something to lock onto.
    fn click_track(bpm: f32, seconds: f64, sr: u32) -> Vec<f32> {
        let beat_sec = 60.0 / bpm as f64;
        let total = (seconds * sr as f64) as usize;
        let mut samples = vec![0.0f32; total];
        let mut t = 0.0f64;
        while (t * sr as f64) < total as f64 {
            let idx = (t * sr as f64) as usize;
            let click_len = ((0.05 * sr as f64) as usize).min(total.saturating_sub(idx));
            for i in 0..click_len {
                let env = (-(i as f64) / (0.02 * sr as f64)).exp() as f32;
                samples[idx + i] +=
                    (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin() * env;
            }
            t += beat_sec;
        }
        samples
    }

    struct FakeSource {
        samples: Vec<f32>,
        channels: usize,
    }

    impl Source for FakeSource {
        fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize {
            let start = start_frame as usize;
            let total = self.samples.len() / self.channels;
            if start >= total {
                return 0;
            }
            let want = output.len() / self.channels;
            let frames = want.min(total - start);
            let src = &self.samples[start * self.channels..(start + frames) * self.channels];
            output[..src.len()].copy_from_slice(src);
            frames
        }
        fn total_frames(&self) -> u64 {
            (self.samples.len() / self.channels) as u64
        }
    }

    #[test]
    fn downmix_halves_stereo() {
        let stereo = vec![0.4f32, -0.2, 0.6, 0.0];
        let src = FakeSource {
            samples: stereo,
            channels: 2,
        };
        let mono = downmix_to_mono(&src, 2, 2);
        assert_eq!(mono, vec![0.1, 0.3]);
    }

    #[test]
    fn detects_bpm_on_click_track() {
        let sr = 48_000u32;
        let mono = click_track(120.0, 10.0, sr);
        let result = analyze(&mono, sr).expect("analysis should succeed on a clean click track");
        let bpm = result.bpm.expect("bpm should be present");
        assert!((bpm - 120.0).abs() < 2.0, "expected ~120 BPM, got {bpm}");
    }

    #[test]
    fn grid_covers_the_track() {
        let sr = 48_000u32;
        let mono = click_track(120.0, 10.0, sr);
        let result = analyze(&mono, sr).expect("analysis should succeed");
        let first = result.beatgrid.beat_frames[0];
        assert!(
            first < sr as u64,
            "first beat should be near frame 0, got {first}"
        );
        assert!(
            result.beatgrid.beat_frames.len() > 10,
            "expected many beats"
        );
    }

    #[test]
    fn silence_returns_err() {
        let silence = vec![0.0f32; 48_000 * 5];
        let result = analyze(&silence, 48_000);
        assert!(result.is_err(), "silence should not produce a grid");
    }
}
