//! Backend-agnostic analysis: raw beat times + key, before grid compilation.
//!
//! `analyze` runs the configured backends (beat detection and key detection are configured
//! separately — `[preference.analysis]` in config.toml names each one directly; there is no
//! auto), then `refine` fits a rigid [`BeatSpec`], then the grid compiler turns it into a
//! runtime [`BeatGrid`]. The backends are pure functions over a mono buffer, so they can be
//! tested without a deck or a file.

mod refine;
mod stratum;
mod timestretch;

pub use hypermixx_core::Backend;
pub use refine::{FitQuality, RefineConfig, fit_rigid};

use hypermixx_core::{Key, TrackAnalysis};
use hypermixx_media::Source;
use std::sync::Arc;

/// Which backend runs which analysis path. Beats and key are configured separately; every
/// backend is named directly — there is no auto fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backends {
    /// The grid path: beat detection + rigid fit.
    pub beats: Backend,
    /// The key path: key detection only.
    pub key: Backend,
}

impl Default for Backends {
    fn default() -> Self {
        Self {
            beats: Backend::default(),
            key: Backend::default(),
        }
    }
}

/// Raw, uncompiled analysis output: beat times in seconds plus a tempo hint and key, and the
/// backend's own verdicts about how much the grid deserves trust.
#[derive(Clone, Debug)]
pub struct RawAnalysis {
    /// Beat times in seconds (backend-native order — tracked sequences are ascending).
    pub beats_sec: Vec<f64>,
    pub key: Option<Key>,
    pub bpm_hint: Option<f32>,
    pub duration_sec: f64,
    /// The backend's dual-estimator verdict that the grid phase should not be trusted (e.g.
    /// timestretch's kick-band corroboration disagreed with its tracker). Marks the fit degraded.
    pub phase_untrusted: bool,
    /// Overall grid confidence the backend attached to its own beat list, if it reports one.
    pub grid_confidence: Option<f32>,
}

/// Why an analysis produced no usable result.
#[derive(Debug)]
pub enum AnalyserError {
    /// The chosen backend is not available in this build.
    Unsupported(&'static str),
    /// The backend failed or panicked.
    Failed(String),
    /// The backend returned nothing usable.
    NoBeats,
    /// Refinement could not fit a grid.
    Refine(String),
}

impl std::fmt::Display for AnalyserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalyserError::Unsupported(b) => write!(f, "backend {b} not available"),
            AnalyserError::Failed(m) => write!(f, "analysis failed: {m}"),
            AnalyserError::NoBeats => write!(f, "analysis produced no beats"),
            AnalyserError::Refine(m) => write!(f, "refinement failed: {m}"),
        }
    }
}

impl std::error::Error for AnalyserError {}

/// Analyzes a decoded source end-to-end: configured backends → rigid fit → compiled grid, ready
/// for a deck. The beat path and the key path run independently, per configuration.
pub fn analyze(
    source: Arc<dyn Source>,
    sample_rate: u32,
    backends: Backends,
) -> Result<TrackAnalysis, AnalyserError> {
    let mono = downmix_to_mono(source.as_ref());
    let raw = dispatch_beats(backends.beats, &mono, sample_rate)?;

    // The key path runs the configured key backend. When it is the same backend as the beat run
    // and that run already produced a key, it rides along; backends that do not detect key as
    // part of their beat pass (timestretch) run the standalone detection here.
    let key = match raw.key {
        Some(key) if backends.key == backends.beats => Some(key),
        _ => dispatch_key(backends.key, &mono, sample_rate)?,
    };

    let (spec, _quality) = fit_rigid(
        &raw,
        sample_rate,
        source.total_frames(),
        &RefineConfig::default(),
    )
    .map_err(AnalyserError::Refine)?;

    let compiler = crate::grid_compiler::GridCompiler::new(sample_rate, source.total_frames());
    let beatgrid = compiler.compile(&spec);
    if beatgrid.is_empty() {
        return Err(AnalyserError::NoBeats);
    }
    // The reported BPM is the *fitted* one, folded into the DJ band — display, sync and the grid
    // must agree.
    Ok(TrackAnalysis {
        beatgrid,
        key,
        bpm: Some(spec.segments[0].bpm as f32),
    })
}

/// Beat path: the configured backend produces a tracked beat sequence.
fn dispatch_beats(
    backend: Backend,
    mono: &[f32],
    sample_rate: u32,
) -> Result<RawAnalysis, AnalyserError> {
    match backend {
        Backend::Stratum => stratum::analyze(mono, sample_rate),
        Backend::Timestretch => timestretch::analyze_beats(mono, sample_rate),
    }
}

/// Key path: the configured backend's key detection, standalone.
fn dispatch_key(
    backend: Backend,
    mono: &[f32],
    sample_rate: u32,
) -> Result<Option<Key>, AnalyserError> {
    match backend {
        Backend::Stratum => Ok(stratum::analyze(mono, sample_rate)?.key),
        Backend::Timestretch => timestretch::detect_key(mono, sample_rate),
    }
}

/// Downmixes an interleaved stereo source to a mono buffer for analysis.
pub fn downmix_to_mono(source: &dyn Source) -> Vec<f32> {
    const CH: usize = hypermixx_core::CHANNELS;
    let total = source.total_frames() as usize;
    let mut mono = Vec::with_capacity(total);
    let mut block = vec![0.0f32; 4096 * CH];
    let mut start = 0u64;
    while start < total as u64 {
        let read = source.read_frames(start, &mut block);
        if read == 0 {
            break;
        }
        for i in (0..read * CH).step_by(CH) {
            mono.push((block[i] + block[i + 1]) * 0.5);
        }
        start += read as u64;
    }
    mono
}
