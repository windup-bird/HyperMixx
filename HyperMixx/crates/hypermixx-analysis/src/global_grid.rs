//! Full-track rigid beat-grid post-processing.
//!
//! `timestretch` tracks local tempo segments because that is useful for live
//! material.  DJ transport, sync, and quantize need a stable phase, however,
//! so this module fits one BPM/phase model and expands it across the track.

use timestretch::analysis::beat::BeatGrid as DetectedGrid;
use timestretch::analysis::rigid_grid::fit_rigid_grid;

/// Result of fitting one rigid grid over the complete track.
#[derive(Debug, Clone)]
pub struct GlobalGridFit {
    pub bpm: f64,
    pub offset_secs: f64,
    pub beats_secs: Box<[f64]>,
    pub phase_lock: f32,
    pub downbeat_rotation: Option<usize>,
}

/// Fits a single rigid grid and expands it over `[0, duration_secs]`.
///
/// The detector's dynamic beat list supplies the tempo seed, while the rigid
/// fitter uses the low-band onset envelope to choose a phase that is not
/// systematically delayed by the transient tracker.  Dynamic segments are
/// deliberately not copied into the result.
pub fn fit_global_grid(
    samples: &[f32],
    sample_rate: u32,
    detected: &DetectedGrid,
    duration_secs: f64,
) -> Option<GlobalGridFit> {
    if detected.bpm <= 0.0 || duration_secs <= 0.0 {
        return None;
    }
    let (bpm, phase, phase_lock) =
        if let Some(fit) = fit_rigid_grid(samples, sample_rate, detected.bpm) {
            (fit.bpm, fit.phase_secs, fit.phase_lock)
        } else {
            let period = 60.0 / detected.bpm;
            let first = detected.beats.first().copied()? / sample_rate as f64;
            let reference = first.rem_euclid(period);
            let mut residuals: Vec<f64> = detected
                .beats
                .iter()
                .map(|&beat| {
                    let phase = (beat / sample_rate as f64).rem_euclid(period);
                    let delta = phase - reference;
                    if delta > period * 0.5 {
                        delta - period
                    } else if delta < -period * 0.5 {
                        delta + period
                    } else {
                        delta
                    }
                })
                .collect();
            residuals.sort_by(f64::total_cmp);
            let median = residuals[residuals.len() / 2];
            (detected.bpm, (reference + median).rem_euclid(period), 0.0)
        };
    let period = 60.0 / bpm;
    if !period.is_finite() || period <= 0.0 {
        return None;
    }

    // The fitted phase is modulo one period. Start at the first grid point at
    // or after zero, then walk backwards only when the phase itself is zero.
    // This makes offset_secs the first grid point in the audio timeline while
    // still producing all grid points through the end of the track.
    let first = phase.rem_euclid(period);
    let mut beats = Vec::new();
    let mut k = 0usize;
    let mut t = first;
    while t <= duration_secs + 1e-9 {
        beats.push(t);
        k += 1;
        t = first + k as f64 * period;
    }
    if beats.len() < 2 {
        return None;
    }

    let downbeat_rotation = if detected.downbeats.is_empty() {
        None
    } else {
        let mut counts = [0usize; 4];
        for &index in &detected.downbeats {
            let Some(&sample) = detected.beats.get(index) else {
                continue;
            };
            let time = sample / sample_rate as f64;
            let beat_index = ((time - first) / period).round() as i64;
            counts[beat_index.rem_euclid(4) as usize] += 1;
        }
        counts
            .iter()
            .enumerate()
            .max_by_key(|(_, count)| **count)
            .filter(|(_, count)| **count > 0)
            .map(|(rotation, _)| rotation)
    };

    Some(GlobalGridFit {
        bpm,
        offset_secs: first,
        beats_secs: beats.into_boxed_slice(),
        phase_lock,
        downbeat_rotation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use timestretch::TempoTrackingOptions;
    use timestretch::analysis::beat::detect_beats_with_options;

    fn click_track(secs: f64, bpm: f64, delay: f64) -> Vec<f32> {
        let sr = 48_000.0;
        let period = 60.0 / bpm;
        let mut samples = vec![0.0; (secs * sr) as usize];
        for (i, sample) in samples.iter_mut().enumerate() {
            let t = i as f64 / sr;
            let since = (t - delay).rem_euclid(period);
            if since < 0.012 {
                *sample = ((2.0 * std::f64::consts::PI * 1500.0 * since).sin()
                    * (-(since / 0.004)).exp()) as f32;
            }
        }
        samples
    }

    #[test]
    fn expands_one_period_over_full_track() {
        let samples = click_track(24.0, 120.0, 1.0);
        let detected = detect_beats_with_options(
            &samples,
            48_000,
            &TempoTrackingOptions {
                hint_range: Some((100.0, 160.0)),
                ..Default::default()
            },
        );
        let fit = fit_global_grid(&samples, 48_000, &detected, 24.0).expect("rigid fit");
        assert!((fit.bpm - 120.0).abs() < 1.0);
        let period = 60.0 / fit.bpm;
        assert!(
            fit.beats_secs
                .windows(2)
                .all(|w| (w[1] - w[0] - period).abs() < 1e-6)
        );
        let nearest = fit
            .beats_secs
            .iter()
            .min_by(|a, b| (*a - 1.0).abs().total_cmp(&(*b - 1.0).abs()))
            .copied()
            .unwrap();
        assert!((nearest - 1.0).abs() < 0.03, "phase 应接近拍头: {nearest}");
        assert!(fit.beats_secs.last().copied().unwrap() >= 23.5);
    }

    #[test]
    fn dynamic_segments_are_collapsed_to_one_period() {
        let detected = DetectedGrid {
            beats: (0..20).map(|i| i as f64 * 24_000.0).collect(),
            downbeats: vec![0, 4, 8, 12, 16],
            segments: vec![
                timestretch::core::preanalysis::TempoSegment {
                    start_beat: 0,
                    bpm: 120.0,
                },
                timestretch::core::preanalysis::TempoSegment {
                    start_beat: 10,
                    bpm: 124.0,
                },
            ],
            bpm: 120.0,
            confidence: 0.8,
            downbeat_confidence: 0.8,
            sample_rate: 48_000,
            tempo_candidates: Vec::new(),
        };
        let fit = fit_global_grid(&[], 48_000, &detected, 12.0).expect("fallback fit");
        assert_eq!(fit.downbeat_rotation, Some(0));
        let period = 60.0 / fit.bpm;
        assert!(
            fit.beats_secs
                .windows(2)
                .all(|w| (w[1] - w[0] - period).abs() < 1e-9)
        );
    }
}
