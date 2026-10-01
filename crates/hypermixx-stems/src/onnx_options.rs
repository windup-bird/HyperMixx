//! The knobs that change the audio, and the provider that runs it.
//!
//! Deliberately outside the `onnx` feature: a front-end parses `--provider cuda` and
//! `--overlap 0.1` whether or not this build can honour them, so that "this build has no separator"
//! and "you asked for a GPU on a CPU build" read as different errors instead of a parse failure.

/// `charon`'s own default, and where a listening judgement should start: a quarter of each window
/// overlaps the next.
pub const DEFAULT_OVERLAP: f32 = 0.25;

/// The most overlap worth offering. Beyond this windows overlap more than they advance, which costs
/// time for averaging the model does not need.
pub const MAX_OVERLAP: f32 = 0.5;

/// Which ONNX Runtime execution provider to run the model on.
///
/// CPU is the default and needs nothing beyond the model file. CUDA is measured at ~9.6× per window
/// (214 ms vs 2055 ms on an RTX 4050; ~8 s instead of ~81 s for a 3:39 track), at the cost of the
/// `cuda` cargo feature, CUDA 13 + cuDNN 9 on the host, and ~3.2 GB of VRAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Provider {
    /// The CPU provider: always available, no system libraries, no GPU.
    #[default]
    Cpu,
    /// CUDA (NVIDIA).
    Cuda,
}

impl Provider {
    /// The label that goes into the separator id, and so into the cache key.
    pub fn label(self) -> &'static str {
        match self {
            Provider::Cpu => "cpu",
            Provider::Cuda => "cuda",
        }
    }

    /// Parses a `--provider` token (`cuda`/`gpu` are the same thing).
    pub fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "cpu" => Some(Provider::Cpu),
            "cuda" | "gpu" => Some(Provider::Cuda),
            _ => None,
        }
    }
}

/// Everything a caller can ask for that changes the produced audio — and therefore the cache key.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeparateOptions {
    /// Ensemble shifts: `0` and `1` are one pass; `2` averages a second, shifted run for ~2× time.
    pub shifts: usize,
    /// Window overlap in `0.0..=`[`MAX_OVERLAP`]; lower is faster and weights window edges more.
    pub overlap: f32,
    pub provider: Provider,
}

impl Default for SeparateOptions {
    fn default() -> Self {
        Self {
            shifts: 1,
            overlap: DEFAULT_OVERLAP,
            provider: Provider::Cpu,
        }
    }
}

impl SeparateOptions {
    /// Clamps and canonicalises: `shifts` to [`crate::MAX_SHIFTS`] (and 0→1, which `charon`
    /// documents as the same single pass), `overlap` into range.
    pub fn sanitised(mut self) -> Self {
        self.shifts = self.shifts.clamp(1, crate::MAX_SHIFTS);
        self.overlap = if self.overlap.is_finite() {
            self.overlap.clamp(0.0, MAX_OVERLAP)
        } else {
            DEFAULT_OVERLAP
        };
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_parse_by_name_and_spell_gpu_as_cuda() {
        assert_eq!(Provider::parse("cpu"), Some(Provider::Cpu));
        assert_eq!(Provider::parse("CUDA"), Some(Provider::Cuda));
        assert_eq!(Provider::parse("gpu"), Some(Provider::Cuda));
        assert_eq!(Provider::parse("vulkan"), None);
        // CPU is the default: a machine without a GPU must never need a flag to work.
        assert_eq!(Provider::default(), Provider::Cpu);
        assert_eq!(Provider::Cpu.label(), "cpu");
        assert_eq!(Provider::Cuda.label(), "cuda");
    }

    #[test]
    fn options_canonicalise_instead_of_erroring() {
        // charon documents 0 and 1 as the same single pass, so they canonise to one key.
        assert_eq!(SeparateOptions { shifts: 0, ..Default::default() }.sanitised().shifts, 1);
        assert_eq!(
            SeparateOptions { shifts: 99, ..Default::default() }.sanitised().shifts,
            crate::MAX_SHIFTS
        );
        assert_eq!(
            SeparateOptions { overlap: -1.0, ..Default::default() }.sanitised().overlap,
            0.0
        );
        assert_eq!(
            SeparateOptions { overlap: 9.0, ..Default::default() }.sanitised().overlap,
            MAX_OVERLAP
        );
        assert_eq!(
            SeparateOptions { overlap: f32::NAN, ..Default::default() }.sanitised().overlap,
            DEFAULT_OVERLAP
        );
        // The defaults are the fastest honest setting: no extra shift averaging, charon's overlap.
        let default = SeparateOptions::default().sanitised();
        assert_eq!(default.shifts, 1);
        assert_eq!(default.overlap, DEFAULT_OVERLAP);
        assert_eq!(default.provider, Provider::Cpu);
    }
}
