//! timestretch backend — placeholder.
//!
//! The vendored `timestretch` exposes offline analysis only through its `analysis` module, which
//! this crate does not yet depend on. Wiring it up is a follow-up (add a `timestretch` dependency
//! and map its `analysis::beat::BeatGrid`, whose `beats` are already absolute sample positions).
//! Until then this backend reports `Unsupported`, and `Backend::Auto` relies on stratum.

use super::{AnalyserError, RawAnalysis};

pub fn analyze(_mono: &[f32], _sample_rate: u32) -> Result<RawAnalysis, AnalyserError> {
    Err(AnalyserError::Unsupported("timestretch"))
}
