//! Rigid-grid refinement: collapse a raw (possibly jittery, possibly half/double-time) beat list
//! into a single constant-tempo [`BeatSpec`] via median + least-squares, with octave disambiguation.
//!
//! Deliberately simple — no RANSAC. The median handles isolated misses; least squares fits the
//! phase and period jointly.

use crate::analyser::RawAnalysis;
use crate::beat_spec::BeatSpec;

/// Tuning knobs for [`fit_rigid`].
#[derive(Clone, Copy, Debug)]
pub struct RefineConfig {
    pub min_bpm: f64,
    pub max_bpm: f64,
    /// A beat further than `period * inlier_ratio` from the model is dropped.
    pub inlier_ratio: f64,
    pub min_beats: usize,
}

impl Default for RefineConfig {
    fn default() -> Self {
        Self {
            min_bpm: 60.0,
            max_bpm: 200.0,
            inlier_ratio: 0.03,
            min_beats: 16,
        }
    }
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        0.5 * (xs[n / 2 - 1] + xs[n / 2])
    }
}

/// Fits a rigid tempo to `raw.beats_sec` and returns a one-segment [`BeatSpec`], or an error string
/// if there are too few beats or the tempo cannot be disambiguated into `[min_bpm, max_bpm]`.
pub fn fit_rigid(
    raw: &RawAnalysis,
    sample_rate: u32,
    _total_frames: u64,
    cfg: &RefineConfig,
) -> Result<BeatSpec, String> {
    let mut times: Vec<f64> = raw.beats_sec.clone();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    if times.len() < cfg.min_beats {
        return Err(format!(
            "only {} beats, need {}",
            times.len(),
            cfg.min_beats
        ));
    }

    let intervals: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
    let mut period = median(intervals);
    if period <= 0.0 {
        return Err("non-positive median beat period".into());
    }

    // Octave disambiguation: scale the period into the accepted BPM band by powers of two.
    let bpm_of = |p: f64| 60.0 / p;
    while bpm_of(period) < cfg.min_bpm {
        period /= 2.0;
    }
    while bpm_of(period) > cfg.max_bpm {
        period *= 2.0;
    }

    // Assign beat numbers from the first beat and refit phase+period by least squares, iterating so
    // the integer assignment settles (dropped beats simply skip a k).
    let t0 = times[0];
    let mut model = Fit { start: t0, period };
    for _ in 0..4 {
        model = fit_ls(&times, model);
    }
    // With the period settled, prune gross outliers and refit if enough beats survive.
    let kept: Vec<f64> = times
        .iter()
        .filter(|&&t| {
            let k = ((t - model.start) / model.period).round();
            (t - (model.start + k * model.period)).abs() <= model.period * cfg.inlier_ratio
        })
        .copied()
        .collect();
    if kept.len() >= cfg.min_beats {
        for _ in 0..2 {
            model = fit_ls(&kept, model);
        }
    }

    let bpm = 60.0 / model.period;
    if !(cfg.min_bpm..=cfg.max_bpm).contains(&bpm) {
        return Err(format!("fitted BPM {bpm:.2} out of band"));
    }
    let start_frame = (model.start * sample_rate as f64).round().max(0.0) as u64;
    Ok(BeatSpec::rigid(bpm, start_frame))
}

struct Fit {
    start: f64,
    period: f64,
}

/// Least-squares fit of `t = start + k * period`, assigning integer `k` against the current model.
fn fit_ls(times: &[f64], model: Fit) -> Fit {
    let assigned: Vec<(f64, f64)> = times
        .iter()
        .map(|&t| (t, ((t - model.start) / model.period).round()))
        .collect();
    if assigned.len() < 2 {
        return model;
    }
    // Regress t on k: slope = period, intercept = start.
    let n = assigned.len() as f64;
    let sum_k: f64 = assigned.iter().map(|(_, k)| k).sum();
    let sum_t: f64 = assigned.iter().map(|(t, _)| t).sum();
    let sum_kk: f64 = assigned.iter().map(|(_, k)| k * k).sum();
    let sum_kt: f64 = assigned.iter().map(|(t, k)| k * t).sum();
    let denom = n * sum_kk - sum_k * sum_k;
    if denom.abs() < 1e-12 {
        return model;
    }
    let period = (n * sum_kt - sum_k * sum_t) / denom;
    let start = (sum_t - period * sum_k) / n;
    Fit {
        start,
        period: period.max(1e-6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(beats_sec: Vec<f64>) -> RawAnalysis {
        RawAnalysis {
            beats_sec,
            key: None,
            bpm_hint: None,
            duration_sec: 0.0,
        }
    }

    /// Beats at exactly 128 BPM (period 0.46875s).
    fn steady(bpm: f64, count: usize) -> Vec<f64> {
        let p = 60.0 / bpm;
        (0..count).map(|i| i as f64 * p).collect()
    }

    #[test]
    fn fits_steady_128() {
        let spec = fit_rigid(&raw(steady(128.0, 60)), 48_000, 0, &RefineConfig::default())
            .expect("should fit");
        let bpm = spec.segments[0].bpm;
        assert!((bpm - 128.0).abs() < 0.01, "got {bpm}");
    }

    #[test]
    fn tolerates_jitter_and_dropped_beats() {
        // 128 BPM, ±20ms jitter, every 10th beat dropped. The jitter is pseudo-random (an LCG), not
        // a sine on the beat index — a sine aliased to `i` correlates with `k` and biases any
        // least-squares slope, which is an artifact of the test, not of the fit.
        let p = 60.0 / 128.0;
        let mut state = 0x9E37_79B9_u64;
        let mut jitter = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            // Take the high bits, map to [-1, 1).
            ((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0
        };
        let beats: Vec<f64> = (0..100)
            .filter(|i| i % 10 != 3)
            .map(|i| i as f64 * p + jitter() * 0.02)
            .collect();
        let spec = fit_rigid(&raw(beats), 48_000, 0, &RefineConfig::default()).unwrap();
        assert!(
            (spec.segments[0].bpm - 128.0).abs() < 0.01,
            "got {}",
            spec.segments[0].bpm
        );
    }

    #[test]
    fn disambiguates_half_time_to_in_band() {
        // 61 BPM input sits below min_bpm=60 boundary-ish; a 30 BPM (too slow) input must be doubled.
        let beats = steady(30.0, 60); // period 2.0s, bpm 30 < min
        let spec = fit_rigid(&raw(beats), 48_000, 0, &RefineConfig::default()).unwrap();
        assert!(
            spec.segments[0].bpm >= 60.0,
            "should double into band, got {}",
            spec.segments[0].bpm
        );
    }

    #[test]
    fn rejects_too_few_beats() {
        let err = fit_rigid(&raw(steady(120.0, 5)), 48_000, 0, &RefineConfig::default());
        assert!(err.is_err());
    }
}
