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

/// How the runtime is set up: threads and memory. **None of this changes the audio.**
///
/// Kept apart from [`SeparateOptions`] on purpose: those take part in the separator's id and so in
/// the cache key (they produce different samples), while a thread count only decides how the same
/// arithmetic is scheduled. A low-end device needs this dial for a reason that has nothing to do with
/// quality: ONNX Runtime takes every core by default, and a separation that saturates the machine
/// starves the audio callback — the mix glitches *while* the stems are being prepared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeOptions {
    /// Intra-op threads, or `None` for "let ONNX Runtime decide" (every core).
    ///
    /// On a 4-core laptop, `Some(2)` leaves two cores for playback; the separation takes longer and
    /// the music does not stutter, which is the right trade when someone is listening.
    pub threads: Option<usize>,
    /// Trade throughput for peak memory: constant folding materialises several gigabytes of index
    /// tensors at load time on HTDemucs, which does not fit a small device.
    pub low_memory: bool,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            threads: None,
            // The bundled model is configured low-memory already; this keeps that the default.
            low_memory: true,
        }
    }
}

impl RuntimeOptions {
    /// Parse `[stems] threads`: `0` (or a missing key) means "the runtime decides".
    pub fn threads_from(value: usize) -> Option<usize> {
        (value > 0).then_some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_dials_are_defaults_that_do_not_touch_the_audio() {
        let runtime = RuntimeOptions::default();
        assert_eq!(runtime.threads, None, "let ONNX Runtime decide unless asked");
        assert!(runtime.low_memory, "the small-device default is the low-memory one");
        // `0` means auto, so a config file can say "no limit" without a special case.
        assert_eq!(RuntimeOptions::threads_from(0), None);
        assert_eq!(RuntimeOptions::threads_from(2), Some(2));
    }

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
