//! timestretch backend — the tracked-sequence detector.
//!
//! timestretch's analysis module is a two-role detector (`docs/BEAT_ANALYSIS.md` §4): a DP
//! beat tracker that follows the music, and an independent kick-band rigid grid that
//! corroborates the tracker's phase — disagreement on plausibly quantized material sets
//! `phase_untrusted`, which the refit surfaces as a degraded gate. It also commits to a
//! metrical level with an explicit 3:2 re-check (DnB 174 vs 87), so the refit can fit the
//! sequence it is given without re-deciding octaves.

use std::panic::{catch_unwind, AssertUnwindSafe};

use super::{AnalyserError, RawAnalysis, RefineConfig};
use hypermixx_core::{Key, KeyMode};

/// DJ-range tempo options: a wide prior breaks octave ties without hard folding, and the
/// committed level is whatever the tracker's evidence supports.
fn dj_options(cfg_band: (f64, f64)) -> timestretch::analysis::tempogram::TempoTrackingOptions {
    timestretch::analysis::tempogram::TempoTrackingOptions {
        // Search the hard validity band; the prior (and the tracker's own metrical check)
        // decides the level inside it.
        min_bpm: cfg_band.0.floor() - 5.0,
        max_bpm: cfg_band.1.ceil() + 5.0,
        prior_center_bpm: 125.0,
        prior_octave_sigma: 1.0,
        hint_range: None,
    }
}

/// Runs the tracker on mono PCM: raw beat times (seconds) + key + tempo hint + trust verdicts.
pub fn analyze_beats(mono: &[f32], sample_rate: u32) -> Result<RawAnalysis, AnalyserError> {
    let options = dj_options(RefineConfig::default().octave_band);
    let grid = catch_unwind(AssertUnwindSafe(|| {
        timestretch::analysis::beat::detect_beats_with_options(mono, sample_rate, &options)
    }))
    .map_err(|panic| {
        let msg = panic
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        AnalyserError::Failed(msg)
    })?;

    if grid.bpm <= 0.0 || grid.beats.is_empty() {
        return Err(AnalyserError::NoBeats);
    }

    Ok(RawAnalysis {
        // Tracked positions are fractional sample indices on the analyzed signal.
        beats_sec: grid.beats.iter().map(|&t| t / f64::from(sample_rate)).collect(),
        bpm_hint: Some(grid.bpm as f32),
        key: None,
        duration_sec: grid.beats.last().map(|&t| t / f64::from(sample_rate)).unwrap_or(0.0),
        phase_untrusted: grid.phase_untrusted,
        grid_confidence: Some(grid.confidence),
    })
}

/// Key detection only (the standalone key path).
pub fn detect_key(
    mono: &[f32],
    sample_rate: u32,
) -> Result<Option<Key>, AnalyserError> {
    let estimate = catch_unwind(AssertUnwindSafe(|| {
        timestretch::analysis::key::detect_key(mono, sample_rate)
    }))
    .map_err(|panic| {
        let msg = panic
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        AnalyserError::Failed(msg)
    })?;

    Ok(estimate.map(|estimate| Key {
        pc: estimate.root,
        mode: match estimate.mode {
            timestretch::KeyMode::Major => KeyMode::Major,
            timestretch::KeyMode::Minor => KeyMode::Minor,
        },
        confidence: estimate.confidence,
    }))
}
