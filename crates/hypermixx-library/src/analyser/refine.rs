//! Rigid-grid refinement: fit a single constant-tempo [`BeatSpec`] to a tracked beat sequence.
//!
//! The model is `t_i ≈ φ + k_i·P`. The one fact that makes this a *fit* rather than a *search*:
//! a tracked beat list is an **ordered sequence** — one entry per beat, ascending. The order
//! itself carries the beat numbers, so `k_i` is derived deterministically (`k_{i+1} = k_i + 1`,
//! plus an extra step per dropped beat) instead of guessed with `round((t−φ)/P)`, whose
//! mis-assignments fed back into the regression and produced stable wrong tempos (the 3:2
//! lock-in: an interleaved numbering pins the slope at 2P/3). With sequence indexing there is no
//! feedback loop, and `t = φ + k·P` over fixed `(k_i, t_i)` pairs is ordinary least squares —
//! iteratively reweighted (Tukey) so ghost beats and stray onsets contribute nothing.
//!
//! Responsibility split, following `docs/BEAT_ANALYSIS.md`:
//! * **Metrical level** (87 vs 174 BPM, DnB 3:2) is the *detector's* decision
//!   (timestretch `tempo_candidates` + its built-in metrical-level re-check). The fitter fits the
//!   sequence it is given, faithfully.
//! * **Octave folding to the DJ band** happens *after* the fit, as an explicit power-of-two
//!   transform recorded in [`FitQuality::octave_shift`] — a display convention, not a search.
//! * **Onbeat vs offbeat** is the detector's dual-estimator verdict (`phase_untrusted`), surfaced
//!   as a degraded gate here.
//!
//! The fit is best effort: gates only mark [`FitQuality::degraded`]; the only hard error is a
//! beat list too short to fit.

use crate::analyser::RawAnalysis;
use crate::beat_spec::BeatSpec;

/// Tuning knobs for [`fit_rigid`].
#[derive(Clone, Copy, Debug)]
pub struct RefineConfig {
    /// DJ-convention band the fitted tempo is folded into by powers of two — a display
    /// convention applied *after* the fit (`[90, 180)` keeps house at 124 and DnB at 174).
    /// Folding never feeds back into the fit.
    pub octave_band: (f64, f64),
    /// Floor on inlier beats (scaled down for short lists, see `fit_rigid`).
    pub min_inliers: usize,
    /// Fraction of raw beats that must sit within 8% of a period of a grid line.
    pub min_inlier_fraction: f64,
    /// Median inlier residual above this marks the grid degraded.
    pub max_median_residual_ms: f64,
    pub min_beats: usize,
}

impl Default for RefineConfig {
    fn default() -> Self {
        Self {
            octave_band: (90.0, 180.0),
            min_inliers: 64,
            min_inlier_fraction: 0.50,
            max_median_residual_ms: 25.0,
            min_beats: 16,
        }
    }
}

/// How well the raw beats support the fitted grid. Surfaced so callers (and the comparison
/// harness) can attribute failures instead of guessing from a bare BPM. `degraded` means the
/// grid did not clear every gate — best effort, not a hard reject.
#[derive(Clone, Debug, PartialEq)]
pub struct FitQuality {
    /// Fitted tempo *after* the octave-band fold (see [`Self::octave_shift`]).
    pub bpm: f64,
    /// Fraction of raw beats within 8% of a period of a grid line.
    pub inlier_fraction: f64,
    /// Fraction of equal time-windows across the track holding at least one inlier.
    pub coverage: f64,
    pub median_residual_ms: f64,
    pub p95_residual_ms: f64,
    /// Number of raw beats the fit consumed.
    pub beats: usize,
    /// Power-of-two steps applied by the octave-band fold (`+1` = doubled for display).
    pub octave_shift: i32,
    /// True when the grid failed at least one quality gate or the backend flagged its phase.
    pub degraded: bool,
    /// Which gates failed (empty when the grid passed everything).
    pub gates: Vec<&'static str>,
}

/// Fits a constant tempo to `raw.beats_sec` and returns a one-segment [`BeatSpec`] plus quality
/// metrics. Best effort: quality gates only mark [`FitQuality::degraded`]; the only hard error
/// is a beat list too short to fit.
pub fn fit_rigid(
    raw: &RawAnalysis,
    sample_rate: u32,
    _total_frames: u64,
    cfg: &RefineConfig,
) -> Result<(BeatSpec, FitQuality), String> {
    let mut times: Vec<f64> = raw.beats_sec.clone();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if times.len() < cfg.min_beats {
        return Err(format!("only {} beats, need {}", times.len(), cfg.min_beats));
    }

    // Median interval, computed on intervals that could be beats at all: a tracked sequence can
    // interleave ghost detections a few ms after each true beat, and the raw median of such a
    // sequence lands halfway between beat and ghost. No musical beat is shorter than ~30 ms
    // (2000 BPM), so sub-30 ms intervals are ghosts by definition and excluded from the median.
    let median_interval = match median_beat_interval(&times) {
        Some(m) if m > 0.0 => m,
        _ => match raw.bpm_hint {
            Some(hint) if hint > 0.0 => f64::from(hint).recip() * 60.0,
            _ => return Err("no usable beat intervals (all detections are sub-30 ms)".into()),
        },
    };

    // Sequence cleanup: an entry whose gap from the last kept entry is under 3/4 of a beat is
    // either a ghost double-detection or a subdivision onset sitting between beats — both are
    // strays on a rigid grid and are dropped, keeping the earlier entry. The median is then
    // re-derived on the cleaned sequence (the cleanup only removes sub-beat entries, so the
    // median can only move toward the true beat interval).
    let times = dedup_strays(&times, median_interval);
    let median_interval = median_beat_interval(&times).unwrap_or(median_interval);
    if times.len() < cfg.min_beats {
        return Err(format!(
            "only {} beats after dedup, need {}",
            times.len(),
            cfg.min_beats
        ));
    }
    let indices = sequence_indices(&times, median_interval);

    // Robust least squares: the indices are fixed, so every pass is a closed-form weighted LS
    // over (k, t); the Tukey reweighting shrinks ghosts/strays to zero influence.
    let (period, phase) = robust_ls(&times, &indices, median_interval);

    // Final inlier statistics at the settled model.
    let residuals: Vec<f64> = times
        .iter()
        .zip(&indices)
        .map(|(&t, &k)| (t - phase - k as f64 * period).abs())
        .collect();
    let inlier_cut = 0.08 * period;
    let inlier_residuals: Vec<f64> = residuals
        .iter()
        .copied()
        .filter(|&e| e < inlier_cut)
        .collect();
    let inlier_fraction = inlier_residuals.len() as f64 / times.len() as f64;
    let median_residual_ms = percentile(&inlier_residuals, 0.5) * 1000.0;
    let p95_residual_ms = percentile(&inlier_residuals, 0.95) * 1000.0;
    let coverage = temporal_coverage(&times, &residuals, inlier_cut);

    // Gates mark, never reject.
    let min_inliers = cfg.min_inliers.min(times.len() / 2);
    let mut gates: Vec<&'static str> = Vec::new();
    if inlier_residuals.len() < min_inliers {
        gates.push("too few inliers");
    }
    if inlier_fraction < cfg.min_inlier_fraction {
        gates.push("low inlier fraction");
    }
    if median_residual_ms > cfg.max_median_residual_ms {
        gates.push("loose residuals");
    }
    if raw.phase_untrusted {
        gates.push("backend phase untrusted");
    }

    // Octave-band fold: an explicit power-of-two display transform, applied after the fit and
    // recorded — never fed back into the regression. Folding keeps the phase: a doubled grid is
    // the old lines plus their midpoints ({φ + k·P} ⊂ {φ + k·P/2}), a halved grid is the even
    // subset, so `φ` is a grid line of the folded grid either way.
    let bpm_raw = 60.0 / period;
    let (bpm, octave_shift) = fold_bpm(bpm_raw, cfg.octave_band);

    let quality = FitQuality {
        bpm,
        inlier_fraction,
        coverage,
        median_residual_ms,
        p95_residual_ms,
        beats: times.len(),
        octave_shift,
        degraded: !gates.is_empty(),
        gates,
    };
    let start_frame = (phase * f64::from(sample_rate)).round().max(0.0) as u64;
    Ok((BeatSpec::rigid(bpm, start_frame), quality))
}

/// No musical beat is shorter than ~30 ms (2000 BPM); shorter adjacent intervals are ghost
/// detections by definition and are excluded from the median.
const MIN_BEAT_SECS: f64 = 0.03;

/// Median of adjacent intervals that could be beats (≥ [`MIN_BEAT_SECS`]). `None` when every
/// interval is sub-beat (pure ghosts) or the list is too short.
fn median_beat_interval(times: &[f64]) -> Option<f64> {
    let intervals: Vec<f64> = times
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| d.is_finite() && *d >= MIN_BEAT_SECS)
        .collect();
    if intervals.is_empty() {
        None
    } else {
        Some(percentile(&intervals, 0.5))
    }
}

/// Drops entries closer than 3/4 of a median beat to the last kept entry.
///
/// Both stray shapes collapse into this one walk: a ghost double-detection sits a few ms after
/// its true beat (dropped, true beat kept), and a subdivision onset sits between two beats
/// (dropped, so the next true beat's incoming gap becomes the whole beat). The kept sequence
/// then has gaps that are whole multiples of the beat.
fn dedup_strays(times: &[f64], median_interval: f64) -> Vec<f64> {
    let min_gap = 0.75 * median_interval;
    let mut out: Vec<f64> = Vec::with_capacity(times.len());
    for &t in times {
        if let Some(&last) = out.last() {
            if t - last < min_gap {
                continue;
            }
        }
        out.push(t);
    }
    out
}

/// Beat numbers from the cleaned sequence: every gap is a whole multiple of the beat, so the
/// index advances by `round(gap/median)` — 1 for an adjacent beat, 2+ across a dropped beat.
fn sequence_indices(times: &[f64], median_interval: f64) -> Vec<u64> {
    let mut indices = Vec::with_capacity(times.len());
    let mut k = 0u64;
    for (i, &t) in times.iter().enumerate() {
        if i > 0 {
            let gap = t - times[i - 1];
            k += (gap / median_interval).round().max(1.0) as u64;
        }
        indices.push(k);
    }
    indices
}

/// IRLS least squares for `t = φ + k·P` over fixed `(k, t)` pairs.
///
/// Tukey biweight on the residual normalized by the period: beats inside the plateau keep full
/// weight, far outliers decay to zero, ghosts and strays are excluded entirely. Starts from the
/// median interval and the first beat; eight passes are far past convergence for closed-form
/// updates.
fn robust_ls(times: &[f64], indices: &[u64], median_interval: f64) -> (f64, f64) {
    let mut period = median_interval.max(1e-6);
    let mut phase = times.first().copied().unwrap_or(0.0);
    for pass in 0..8 {
        // Residual scale tightens as the model settles.
        let cut = period * if pass < 2 { 0.20 } else { 0.10 };
        let mut sum_w = 0.0;
        let mut sum_wk = 0.0;
        let mut sum_wt = 0.0;
        let mut sum_wkk = 0.0;
        let mut sum_wkt = 0.0;
        for (&t, &k) in times.iter().zip(indices) {
            let e = t - phase - k as f64 * period;
            let x = e / cut;
            let w = if x.abs() >= 1.0 {
                0.0
            } else {
                let s = 1.0 - x * x;
                s * s
            };
            if w <= 0.0 {
                continue;
            }
            let kf = k as f64;
            sum_w += w;
            sum_wk += w * kf;
            sum_wt += w * t;
            sum_wkk += w * kf * kf;
            sum_wkt += w * kf * t;
        }
        // The 2×2 normal equations for slope P and intercept φ.
        let det = sum_w * sum_wkk - sum_wk * sum_wk;
        if det.abs() < 1e-12 || sum_w <= 0.0 {
            break; // degenerate round: keep the previous model
        }
        period = (sum_w * sum_wkt - sum_wk * sum_wt) / det;
        phase = (sum_wt - period * sum_wk) / sum_w;
        if !(period.is_finite()) || period <= 0.0 {
            break;
        }
    }
    (period.max(1e-6), phase)
}

/// Fraction of equal time-windows over the track span that hold at least one inlier.
fn temporal_coverage(times: &[f64], residuals: &[f64], inlier_cut: f64) -> f64 {
    const WINDOWS: usize = 32;
    let (Some(&first), Some(&last)) = (times.first(), times.last()) else {
        return 0.0;
    };
    let span = last - first;
    if span <= 0.0 {
        return 0.0;
    }
    let mut bins = vec![false; WINDOWS];
    for (&t, &e) in times.iter().zip(residuals) {
        if e >= inlier_cut {
            continue;
        }
        let idx = (((t - first) / span * WINDOWS as f64) as usize).min(WINDOWS - 1);
        bins[idx] = true;
    }
    bins.iter().filter(|bin| **bin).count() as f64 / WINDOWS as f64
}

/// Folds a BPM into `[lo, hi)` by powers of two, returning the folded value and the shift in
/// octaves (`+1` = doubled). Guarded against a degenerate band.
fn fold_bpm(bpm: f64, band: (f64, f64)) -> (f64, i32) {
    let (lo, hi) = band;
    if !(hi > lo) {
        return (bpm, 0);
    }
    let mut b = bpm;
    let mut shift = 0i32;
    let mut guard = 0;
    while b < lo {
        b *= 2.0;
        shift += 1;
        guard += 1;
        if guard > 64 {
            return (lo, shift);
        }
    }
    while b >= hi {
        b /= 2.0;
        shift -= 1;
        guard += 1;
        if guard > 64 {
            return (lo, shift);
        }
    }
    (b, shift)
}

/// Nearest-rank percentile of a copy of `xs` (0.5 = median, 0.95 = p95). 0.0 for empty input.
fn percentile(xs: &[f64], q: f64) -> f64 {
    let mut sorted: Vec<f64> = xs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    let idx = (((n - 1) as f64) * q).round().clamp(0.0, (n - 1) as f64) as usize;
    sorted[idx]
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
            phase_untrusted: false,
            grid_confidence: None,
        }
    }

    fn default_fit(beats: Vec<f64>) -> Result<(BeatSpec, FitQuality), String> {
        fit_rigid(&raw(beats), 48_000, 0, &RefineConfig::default())
    }

    /// Beats at exactly `bpm` BPM.
    fn steady(bpm: f64, count: usize) -> Vec<f64> {
        let p = 60.0 / bpm;
        (0..count).map(|i| i as f64 * p).collect()
    }

    /// Pseudo-random jitter in [-1, 1) (an LCG; a sine on the beat index would alias against the
    /// regression and bias any slope — an artifact of the test, not of the fit).
    fn jitter() -> impl FnMut() -> f64 {
        let mut state = 0x9E37_79B9_u64;
        move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0
        }
    }

    #[test]
    fn fits_steady_128() {
        let (spec, q) = default_fit(steady(128.0, 60)).expect("should fit");
        assert!(!q.degraded, "gates: {:?}", q.gates);
        assert!((spec.segments[0].bpm - 128.0).abs() < 0.01, "got {}", q.bpm);
        assert_eq!(q.octave_shift, 0);
        assert!(q.median_residual_ms < 1.0);
        assert!(q.coverage > 0.9);
    }

    #[test]
    fn tolerates_jitter_and_dropped_beats() {
        // 128 BPM, ±20ms jitter, every 10th beat dropped: the index expansion must land the
        // dropped beats' numbers on the regression without any rounding feedback.
        let p = 60.0 / 128.0;
        let mut jitter = jitter();
        let beats: Vec<f64> = (0..100)
            .filter(|i| i % 10 != 3)
            .map(|i| i as f64 * p + jitter() * 0.02)
            .collect();
        let (spec, q) = default_fit(beats).unwrap();
        assert!(
            (spec.segments[0].bpm - 128.0).abs() < 0.01,
            "got {}",
            q.bpm
        );
        assert!(q.inlier_fraction > 0.9);
    }

    #[test]
    fn half_time_folds_up_into_the_dj_band() {
        // A detector that committed to the half-time level reports 87; the post-fit fold must
        // lift the grid to 174 and record the shift.
        let (spec, q) = default_fit(steady(87.0, 200)).unwrap();
        assert!(
            (spec.segments[0].bpm - 174.0).abs() < 0.1,
            "got {}",
            q.bpm
        );
        assert_eq!(q.octave_shift, 1);
    }

    #[test]
    fn disambiguates_half_time_to_in_band() {
        // 30 BPM (far below any band) folds all the way to 120.
        let (spec, q) = default_fit(steady(30.0, 60)).unwrap();
        assert!((spec.segments[0].bpm - 120.0).abs() < 0.1, "got {}", q.bpm);
        assert_eq!(q.octave_shift, 2);
    }

    #[test]
    fn ghost_beats_are_deduped_not_fit() {
        // Ghost double-detections a few ms after each true beat: dedup removes them, and the fit
        // stays at the true tempo instead of doubling.
        let p = 60.0 / 128.0;
        let mut jitter = jitter();
        let beats: Vec<f64> = (0..100)
            .flat_map(|i| {
                let t = i as f64 * p;
                vec![t, t + 0.010 + jitter() * 0.01]
            })
            .collect();
        let (spec, q) = default_fit(beats).unwrap();
        assert!(
            (spec.segments[0].bpm - 128.0).abs() < 0.05,
            "got {}",
            q.bpm
        );
        assert_eq!(q.octave_shift, 0);
    }

    #[test]
    fn stray_onsets_get_zero_regression_weight() {
        // 85% clean beats plus spurious detections at 2P/3 offsets: sequence indexing puts the
        // strays at the wrong rows and the Tukey reweighting excludes them entirely.
        let p = 60.0 / 128.0;
        let mut beats: Vec<f64> = (0..170).map(|i| i as f64 * p).collect();
        beats.extend((0..30).map(|i| i as f64 * p + 2.0 * p / 3.0));
        let (spec, q) = default_fit(beats).unwrap();
        assert!(
            (spec.segments[0].bpm - 128.0).abs() < 0.05,
            "got {}",
            q.bpm
        );
        assert!(q.inlier_fraction > 0.8);
    }

    #[test]
    fn phase_untrusted_marks_degraded() {
        let mut r = raw(steady(128.0, 60));
        r.phase_untrusted = true;
        let (_, q) = fit_rigid(&r, 48_000, 0, &RefineConfig::default()).unwrap();
        assert!(q.degraded);
        assert!(q.gates.contains(&"backend phase untrusted"));
    }

    #[test]
    fn spurious_early_beat_does_not_steal_the_phase() {
        // One noise detection well before the music: indexing puts it at k=0, the regression
        // reweights it away, and the grid stays on the true phase.
        let p = 60.0 / 128.0;
        let mut beats: Vec<f64> = (0..100).map(|i| 10.0 + i as f64 * p).collect();
        beats.push(10.0 - 0.37 * p);
        let (spec, q) = default_fit(beats).unwrap();
        assert!(
            (spec.segments[0].bpm - 128.0).abs() < 0.05,
            "got {}",
            q.bpm
        );
        assert!(q.median_residual_ms < 5.0);
    }

    #[test]
    fn long_track_period_does_not_drift() {
        // 5000 beats, ±15ms jitter, every 13th dropped: the settled slope must hold to 1e-4.
        let p = 60.0 / 128.0;
        let mut jitter = jitter();
        let beats: Vec<f64> = (0..5000)
            .filter(|i| i % 13 != 5)
            .map(|i| i as f64 * p + jitter() * 0.015)
            .collect();
        let (spec, q) = default_fit(beats).unwrap();
        assert!(
            (spec.segments[0].bpm - 128.0).abs() < 128.0 * 1e-4,
            "got {}",
            q.bpm
        );
        assert!(q.median_residual_ms < 20.0);
    }

    #[test]
    fn random_times_degrade_instead_of_erroring() {
        // No coherent sequence: the fit still emits something (best effort) but degraded.
        let mut jitter = jitter();
        let beats: Vec<f64> = (0..200).map(|_| jitter() * 200.0).collect();
        let (_, q) = default_fit(beats).expect("degraded fit, not an error");
        assert!(q.degraded, "gates: {:?}", q.gates);
    }

    #[test]
    fn rejects_too_few_beats() {
        assert!(default_fit(steady(120.0, 5)).is_err());
    }
}
