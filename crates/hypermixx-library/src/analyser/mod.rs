//! Backend-agnostic analysis: raw beat times + key, before grid compilation.
//!
//! `analyze` runs a backend, then `refine` fits a rigid [`BeatSpec`], then the grid compiler turns
//! it into a runtime [`BeatGrid`]. The backends are pure functions over a mono buffer, so they can
//! be tested without a deck or a file.

mod refine;
mod stratum;
mod timestretch;

pub use hypermixx_core::Backend;
pub use refine::{fit_rigid, RefineConfig};

use hypermixx_core::{Key, TrackAnalysis};
use hypermixx_media::Source;
use std::sync::Arc;

/// Raw, uncompiled analysis output: beat times in seconds plus a tempo hint and key.
#[derive(Clone, Debug)]
pub struct RawAnalysis {
    /// Beat times in seconds (backend-native order).
    pub beats_sec: Vec<f64>,
    pub key: Option<Key>,
    pub bpm_hint: Option<f32>,
    pub duration_sec: f64,
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

/// Analyzes a decoded source end-to-end: backend → rigid fit → compiled grid, ready for a deck.
pub fn analyze(
    source: Arc<dyn Source>,
    sample_rate: u32,
    backend: Backend,
) -> Result<TrackAnalysis, AnalyserError> {
    let mono = downmix_to_mono(source.as_ref());
    let raw = dispatch(backend, &mono, sample_rate)?;
    let spec = fit_rigid(
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
    Ok(TrackAnalysis {
        beatgrid,
        key: raw.key,
        bpm: raw.bpm_hint,
    })
}

/// Routes to a backend. `Auto` prefers stratum and falls back to timestretch.
pub fn dispatch(
    backend: Backend,
    mono: &[f32],
    sample_rate: u32,
) -> Result<RawAnalysis, AnalyserError> {
    match backend {
        Backend::Stratum => stratum::analyze(mono, sample_rate),
        Backend::Timestretch => timestretch::analyze(mono, sample_rate),
        Backend::Auto => {
            stratum::analyze(mono, sample_rate).or_else(|_| timestretch::analyze(mono, sample_rate))
        }
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
