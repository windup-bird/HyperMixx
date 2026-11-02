//! Sample-rate conversion between the engine's clock and another one.
//!
//! Two consumers, one kernel:
//!
//! * the **stem separator**, whose model is pinned to 44.1 kHz while the engine may run at whatever
//!   `[audio] sample_rate` says. The mix is converted down to the model's rate, and the separated
//!   blocks are converted back up — so the material a model sees is always what it was trained on,
//!   and the audio the deck installs is always on the engine's own frame grid;
//! * [`ResampledSource`], a band-limited `Source` view for callers that read randomly instead of
//!   streaming (measuring a whole track's level, hashing it for the cache key).
//!
//! The kernel is a Hann-windowed sinc, [`HALF_TAPS`] input frames on each side, normalised per
//! output phase so a DC or low-frequency signal keeps its level exactly. Downsampling lowers the
//! cutoff to stay under the *output's* Nyquist (otherwise the discarded band would fold back as
//! aliasing); upsampling keeps the full input band, because the input is already band-limited and
//! interpolation adds nothing above it.
//!
//! This is deliberately not `rubato`: the job is a fixed ratio, a deterministic result for any
//! requested frame range (so a resumed separation can be re-derived from the same input), and no
//! dependency surface. Cost is ~33 multiply-adds per output frame per channel.

use hypermixx_core::{Source, CHANNELS};

/// Smallest kernel half-width, in input frames. Interpolation needs no band-limiting (the input is
/// already band-limited below the output's Nyquist), so the common 44.1k ↔ 48k case runs on 16.
pub const MIN_HALF_TAPS: usize = 16;
/// Largest half-width. Past this the filter is longer than the transition band is wide: the cost
/// stops buying rejection.
pub const MAX_HALF_TAPS: usize = 128;
/// Fraction of the lower Nyquist kept when downsampling, leaving room for the filter's transition
/// band instead of folding it back as aliasing.
const ROLLOFF: f64 = 0.92;

/// The conversion parameters for one rate pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Converter {
    from: u32,
    to: u32,
    /// Kernel cutoff in cycles per input sample: `1.0` is the input's Nyquist.
    cutoff: f64,
    /// Kernel half-width in input frames — enough taps to actually reach the stopband.
    half: usize,
}

impl Converter {
    pub fn new(from: u32, to: u32) -> Self {
        let (from, to) = (from.max(1), to.max(1));
        let cutoff = if to < from {
            ROLLOFF * f64::from(to) / f64::from(from)
        } else {
            1.0
        };
        // A Hann-windowed sinc of `n` taps has a transition band about `3.1 / n` cycles per input
        // sample wide (`6.2 / n` of the input's Nyquist). That has to fit between the cutoff and the
        // point where the discarded band would alias, which is `to/from` of the input's Nyquist.
        let half = if to < from {
            let transition = f64::from(to) / f64::from(from) * (1.0 - ROLLOFF);
            let taps = 6.2 / transition.max(1e-6);
            ((taps / 2.0).ceil() as usize).clamp(MIN_HALF_TAPS, MAX_HALF_TAPS)
        } else {
            MIN_HALF_TAPS
        };
        Self {
            from,
            to,
            cutoff,
            half,
        }
    }

    /// Kernel half-width in input frames.
    pub fn half_taps(&self) -> usize {
        self.half
    }

    pub fn from_rate(&self) -> u32 {
        self.from
    }

    pub fn to_rate(&self) -> u32 {
        self.to
    }

    pub fn is_identity(&self) -> bool {
        self.from == self.to
    }

    /// Input frames consumed per output frame.
    #[inline]
    pub fn input_per_output(&self) -> f64 {
        f64::from(self.from) / f64::from(self.to)
    }

    /// Output frames produced by `input_frames` of input, rounded up so the tail is not lost.
    pub fn output_frames(&self, input_frames: u64) -> u64 {
        if self.is_identity() {
            return input_frames;
        }
        // Integer maths: `44_100 * 48_000 / 44_100` in f64 rounds to 48000.00000000001, and the
        // ceil of that is a phantom extra frame at the end of every track.
        let numerator = u128::from(input_frames) * u128::from(self.to);
        let denominator = u128::from(self.from);
        numerator.div_ceil(denominator) as u64
    }

    /// Input frame a given output frame reads from (fractional).
    #[inline]
    fn position(&self, output_frame: u64) -> f64 {
        output_frame as f64 * self.input_per_output()
    }
}

/// One kernel tap: the windowed sinc at `distance` input frames from the read position.
#[inline]
fn tap(distance: f64, cutoff: f64, half_taps: usize) -> f64 {
    let half = half_taps as f64;
    if distance.abs() >= half {
        return 0.0;
    }
    // Hann window over `-half ..= half`.
    let window = 0.5 + 0.5 * (std::f64::consts::PI * distance / half).cos();
    let angle = std::f64::consts::PI * cutoff * distance;
    let sinc = if angle == 0.0 {
        1.0
    } else {
        angle.sin() / angle
    };
    window * sinc * cutoff
}

/// Renders `out_frames` output frames starting at output frame `first_out` from an input buffer
/// that begins at input frame `input_start`. Samples outside the buffer read as zero, which is what
/// makes the head and tail of a track behave: the interpolation fades in and out instead of
/// wrapping.
fn render_into(
    conv: &Converter,
    input: &[f32],
    input_start: i64,
    first_out: u64,
    out_frames: usize,
    out: &mut [f32],
) {
    let half = conv.half as i64;
    for index in 0..out_frames {
        let position = conv.position(first_out + index as u64);
        let base = position.floor() as i64;
        let mut weights = 0.0f64;
        let mut sum = [0.0f64; CHANNELS];
        for offset in (-half + 1)..=half {
            let source = base + offset;
            let weight = tap(position - source as f64, conv.cutoff, conv.half);
            if weight == 0.0 {
                continue;
            }
            weights += weight;
            let at = source - input_start;
            if at < 0 || at as usize >= input.len() / CHANNELS {
                continue;
            }
            let frame = at as usize * CHANNELS;
            for (channel, slot) in sum.iter_mut().enumerate() {
                *slot += f64::from(input[frame + channel]) * weight;
            }
        }
        let scale = if weights.abs() > 1e-12 {
            1.0 / weights
        } else {
            0.0
        };
        let frame = index * CHANNELS;
        for (channel, value) in sum.iter().enumerate() {
            out[frame + channel] = (value * scale) as f32;
        }
    }
}

/// Converts one whole buffer. A convenience for callers that already have the audio in memory.
pub fn convert(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    let conv = Converter::new(from, to);
    if conv.is_identity() {
        return input.to_vec();
    }
    let frames_in = input.len() / CHANNELS;
    let frames_out = conv.output_frames(frames_in as u64) as usize;
    let mut out = vec![0.0f32; frames_out * CHANNELS];
    render_into(&conv, input, 0, 0, frames_out, &mut out);
    out
}

/// A `Source` view of `inner` at rate `to`. Random access is deterministic (every call derives its
/// output from the same input window), which is what lets a cache key be hashed through it.
pub struct ResampledSource<'a> {
    inner: &'a dyn Source,
    conv: Converter,
}

impl<'a> ResampledSource<'a> {
    pub fn new(inner: &'a dyn Source, to: u32, from: u32) -> Self {
        Self {
            inner,
            conv: Converter::new(from, to),
        }
    }
}

impl Source for ResampledSource<'_> {
    fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize {
        let total = self.total_frames();
        if start_frame >= total {
            return 0;
        }
        let frames = (output.len() / CHANNELS).min((total - start_frame) as usize);
        if frames == 0 {
            return 0;
        }
        if self.conv.is_identity() {
            return self
                .inner
                .read_frames(start_frame, &mut output[..frames * CHANNELS]);
        }
        // The input window this output range needs, with the kernel's context on both sides.
        let half = self.conv.half as i64;
        let first = self.conv.position(start_frame).floor() as i64 - half;
        let last = self.conv.position(start_frame + frames as u64).ceil() as i64 + half;
        let inner_total = self.inner.total_frames() as i64;
        let from = first.max(0);
        let to = last.min(inner_total);
        let mut input = vec![0.0f32; ((to - from).max(0) as usize) * CHANNELS];
        if to > from {
            let mut done = 0usize;
            while done < (to - from) as usize {
                let read = self
                    .inner
                    .read_frames(from as u64 + done as u64, &mut input[done * CHANNELS..]);
                if read == 0 {
                    break;
                }
                done += read;
            }
        }
        render_into(&self.conv, &input, from, start_frame, frames, output);
        frames
    }

    fn total_frames(&self) -> u64 {
        self.conv.output_frames(self.inner.total_frames())
    }

    fn available_frames(&self) -> u64 {
        self.conv.output_frames(self.inner.available_frames())
    }
}

/// A streaming converter: pushes of arbitrary length in, contiguous output frames out.
///
/// The caller says where the stream starts on both sides ([`StreamConverter::new`]), so the output
/// of a resumed run lands at the engine frame a fresh run would have produced — the reason a
/// resumed separation can be spliced onto what the cache already holds.
pub struct StreamConverter {
    conv: Converter,
    /// Interleaved input history; `history_start` is the input frame of its first frame.
    history: Vec<f32>,
    history_start: i64,
    /// Input frames handed to [`push`](Self::push) so far.
    received: u64,
    /// Next output frame index (global).
    next_out: u64,
}

impl StreamConverter {
    pub fn new(from: u32, to: u32, first_input: u64, first_output: u64) -> Self {
        Self {
            conv: Converter::new(from, to),
            history: Vec::new(),
            history_start: first_input as i64,
            received: first_input,
            next_out: first_output,
        }
    }

    /// The next output frame index, i.e. how much output the stream has produced so far plus the
    /// start offset it was created with.
    pub fn next_output_frame(&self) -> u64 {
        self.next_out
    }

    /// Appends one interleaved stereo block of input, extending `out` with every output frame that
    /// is now fully determined.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.conv.is_identity() {
            out.extend_from_slice(input);
            self.received += (input.len() / CHANNELS) as u64;
            self.next_out += (input.len() / CHANNELS) as u64;
            return;
        }
        self.history.extend_from_slice(input);
        self.received += (input.len() / CHANNELS) as u64;
        self.render_available(out, false);
    }

    /// Emits the tail, treating the end of the input as silence so the last frames are completed.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        self.render_available(out, true);
        self.history.clear();
    }

    fn render_available(&mut self, out: &mut Vec<f32>, flush: bool) {
        let half = self.conv.half as i64;
        if self.conv.is_identity() {
            return;
        }
        loop {
            let position = self.conv.position(self.next_out);
            // Past the end of the input there is nothing left to interpolate: the caller's own
            // frame count is the authority on how long the stream is.
            if position >= self.received as f64 {
                break;
            }
            let base = position.floor() as i64;
            // Without a flush the newest samples must have arrived: a tap can reach `half` frames
            // past the read position, and inventing them would put a zero in the middle of a block.
            if !flush && base + half >= self.received as i64 {
                break;
            }
            let mut weights = 0.0f64;
            let mut sum = [0.0f64; CHANNELS];
            for offset in (-half + 1)..=half {
                let source = base + offset;
                let weight = tap(position - source as f64, self.conv.cutoff, self.conv.half);
                if weight == 0.0 {
                    continue;
                }
                weights += weight;
                let at = source - self.history_start;
                if at < 0 || at as usize >= self.history.len() / CHANNELS {
                    continue;
                }
                let frame = at as usize * CHANNELS;
                for (channel, slot) in sum.iter_mut().enumerate() {
                    *slot += f64::from(self.history[frame + channel]) * weight;
                }
            }
            let scale = if weights.abs() > 1e-12 {
                1.0 / weights
            } else {
                0.0
            };
            for value in sum {
                out.push((value * scale) as f32);
            }
            self.next_out += 1;
        }
        // Drop history no future tap can reach, so a long stream stays a few dozen frames wide.
        let keep_from = self.conv.position(self.next_out) as i64 - half;
        let drop_frames = (keep_from - self.history_start).max(0) as usize;
        let drop_frames = drop_frames.min(self.history.len() / CHANNELS);
        if drop_frames > 0 {
            self.history.drain(..drop_frames * CHANNELS);
            self.history_start += drop_frames as i64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f64, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let value =
                    (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin() as f32;
                [value, -value]
            })
            .collect()
    }

    fn rms(pcm: &[f32]) -> f64 {
        let sum: f64 = pcm
            .as_chunks::<CHANNELS>()
            .0
            .iter()
            .map(|frame| f64::from(frame[0]) * f64::from(frame[0]))
            .sum();
        (sum / (pcm.len() / CHANNELS).max(1) as f64).sqrt()
    }

    #[test]
    fn frame_counts_follow_the_rate_ratio() {
        let up = Converter::new(44_100, 48_000);
        assert_eq!(up.output_frames(44_100), 48_000);
        let down = Converter::new(48_000, 44_100);
        assert_eq!(down.output_frames(48_000), 44_100);
        // An empty input stays empty rather than inventing a frame.
        assert_eq!(up.output_frames(0), 0);
        // Identity is exactly the input.
        assert_eq!(Converter::new(44_100, 44_100).output_frames(1_234), 1_234);
    }

    #[test]
    fn a_tone_keeps_its_level_up_and_down() {
        for (from, to) in [(44_100u32, 48_000u32), (48_000, 44_100), (44_100, 96_000)] {
            // A mid-band tone: far from both the passband edge and the stopband.
            let input = sine(1_000.0, from, from as usize);
            let out = convert(&input, from, to);
            let expected = 1.0 / 2f64.sqrt();
            let got = rms(&out);
            assert!(
                (got - expected).abs() / expected < 0.02,
                "{from}->{to}: level {got:.4} should be ~{expected:.4}"
            );
        }
    }

    #[test]
    fn downsampling_does_not_fold_the_high_band_back() {
        // 23 kHz exists at 96k but is above 44.1k's Nyquist: a naive resampler would mirror it to
        // ~21 kHz at full level. The cutoff has to remove it.
        let input = sine(23_000.0, 96_000, 96_000);
        let out = convert(&input, 96_000, 44_100);
        assert!(rms(&out) < 0.05, "aliased level {}", rms(&out));
        // ...and a tone just below the output's Nyquist survives, which is what separates a
        // low-pass from a mute.
        let input = sine(18_000.0, 96_000, 96_000);
        let out = convert(&input, 96_000, 44_100);
        assert!(rms(&out) > 0.5, "passband level {}", rms(&out));
    }

    #[test]
    fn random_access_matches_the_whole_buffer() {
        let input = sine(440.0, 44_100, 4_410);
        let source = TestSource(input.clone());
        let view = ResampledSource::new(&source, 48_000, 44_100);
        let whole = convert(&input, 44_100, 48_000);
        assert_eq!(view.total_frames(), (whole.len() / CHANNELS) as u64);
        for (start, frames) in [(0u64, 100usize), (1_000, 777), (4_000, 400)] {
            let mut out = vec![0.0f32; frames * CHANNELS];
            let read = view.read_frames(start, &mut out);
            assert_eq!(read, frames);
            let expected = &whole[start as usize * CHANNELS..(start as usize + frames) * CHANNELS];
            for (a, b) in out.iter().zip(expected) {
                assert!((a - b).abs() < 1e-6, "{a} vs {b} at {start}");
            }
        }
    }

    #[test]
    fn streaming_matches_a_whole_buffer_conversion() {
        let input = sine(700.0, 44_100, 10_000);
        let whole = convert(&input, 44_100, 48_000);
        // Feed it in deliberately awkward block sizes, the way the separator does.
        let mut stream = StreamConverter::new(44_100, 48_000, 0, 0);
        let mut out = Vec::new();
        let mut at = 0usize;
        for size in [3_000usize, 1, 4_096, 2_903] {
            let end = (at + size).min(input.len() / CHANNELS);
            stream.push(&input[at * CHANNELS..end * CHANNELS], &mut out);
            at = end;
        }
        stream.push(&input[at * CHANNELS..], &mut out);
        stream.finish(&mut out);
        assert_eq!(
            out.len(),
            whole.len(),
            "the stream must produce every frame"
        );
        for (index, (a, b)) in out.iter().zip(&whole).enumerate() {
            assert!((a - b).abs() < 1e-6, "frame {index}: {a} vs {b}");
        }
    }

    #[test]
    fn a_resumed_stream_lands_where_a_fresh_one_would() {
        let input = sine(500.0, 48_000, 20_000);
        let whole = convert(&input, 48_000, 44_100);
        // Resume at input frame 5_000, output frame `round(5_000 * 44.1/48)`.
        let first_out = 5_000f64 * 44_100.0 / 48_000.0;
        let first_out = first_out.round() as u64;
        let mut stream = StreamConverter::new(48_000, 44_100, 5_000, first_out);
        let mut out = Vec::new();
        stream.push(&input[5_000 * CHANNELS..], &mut out);
        stream.finish(&mut out);
        let start = first_out as usize * CHANNELS;
        // The first frames cannot match: a resumed stream has no history before its start, so the
        // kernel's leading taps read silence. A resume always starts a whole window early (see
        // `resume_frame`), so those frames are never handed to the deck.
        let warmup = MAX_HALF_TAPS * 2 * CHANNELS;
        for (index, value) in out.iter().enumerate().skip(warmup) {
            let reference = whole[start + index];
            assert!(
                (value - reference).abs() < 1e-5,
                "frame {index}: {value} vs {reference}"
            );
        }
    }

    struct TestSource(Vec<f32>);

    impl Source for TestSource {
        fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize {
            let start = start_frame as usize;
            let total = self.0.len() / CHANNELS;
            if start >= total {
                return 0;
            }
            let frames = (output.len() / CHANNELS).min(total - start);
            output[..frames * CHANNELS]
                .copy_from_slice(&self.0[start * CHANNELS..(start + frames) * CHANNELS]);
            frames
        }

        fn total_frames(&self) -> u64 {
            (self.0.len() / CHANNELS) as u64
        }
    }
}
