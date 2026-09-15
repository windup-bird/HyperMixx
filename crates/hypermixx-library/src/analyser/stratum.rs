//! stratum-dsp backend: catches panics and maps `AnalysisResult` to [`RawAnalysis`].

use std::panic::{catch_unwind, AssertUnwindSafe};

use super::{AnalyserError, RawAnalysis};
use hypermixx_core::{Key, KeyMode};
use stratum_dsp::{analyze_audio, AnalysisConfig};

/// A config that trades accuracy headroom for predictable time and memory.
///
/// `enable_silence_trimming = false` is load-bearing: stratum returns beat times relative to the
/// *trimmed* signal and discards the offset, so leaving it on shifts the whole grid. The others cut
/// peak memory from ~1 GB to a few hundred MB per track.
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

/// Runs stratum on mono PCM, producing raw beat times (seconds) + key + tempo hint.
pub fn analyze(mono: &[f32], sample_rate: u32) -> Result<RawAnalysis, AnalyserError> {
    let config = reduced_config();
    let result = catch_unwind(AssertUnwindSafe(|| {
        analyze_audio(mono, sample_rate, config)
    }))
    .map_err(|panic| {
        let msg = panic
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        AnalyserError::Failed(msg)
    })?
    .map_err(|err| AnalyserError::Failed(err.to_string()))?;

    if result.bpm <= 0.0 || result.beat_grid.beats.is_empty() {
        return Err(AnalyserError::NoBeats);
    }

    Ok(RawAnalysis {
        beats_sec: result.beat_grid.beats.iter().map(|&t| t as f64).collect(),
        key: convert_key(result.key, result.key_confidence),
        bpm_hint: Some(result.bpm),
        duration_sec: result.metadata.duration_seconds as f64,
    })
}

/// Maps stratum's `Key` (0=C..11=B, plus a confidence) into the core `Key`.
fn convert_key(key: stratum_dsp::Key, confidence: f32) -> Option<Key> {
    let (pc, mode) = match key {
        stratum_dsp::Key::Major(pc) => (pc as u8, KeyMode::Major),
        stratum_dsp::Key::Minor(pc) => (pc as u8, KeyMode::Minor),
    };
    Some(Key {
        pc,
        mode,
        confidence,
    })
}
