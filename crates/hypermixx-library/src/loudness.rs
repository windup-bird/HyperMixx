//! Track loudness for auto-gain: the average level of the track's **loudest section**.
//!
//! One pass over the PCM, a sliding window of [`WINDOW_SECONDS`] with a [`HOP_SECONDS`] hop, and
//! the *maximum* window RMS. The result is dBFS (a full-scale sine is `-3 dB`), which is the unit
//! auto-gain's target is written in.
//!
//! Why the loudest section rather than the whole-track average: a DJ cares about matching the loud
//! part of a track to the loud part of the next one, and a long quiet intro would drag a whole-track
//! average down and normalise the drop too hot. The window smooths over a single transient, so a
//! one-frame click cannot become the reference level.
//!
//! Deliberately cheap and model-free: no LUFS weighting, no gating. The mix already has a safety
//! limiter, and auto-gain is about evening out records, not about broadcast loudness compliance.

/// Length of the averaged window: long enough to include a bar or two, short enough to sit on the
/// drop rather than on the whole track.
pub const WINDOW_SECONDS: f64 = 3.0;
/// Hop between windows: a one-second grid means a loud section cannot fall between windows.
pub const HOP_SECONDS: f64 = 1.0;

/// The average level of the loudest `WINDOW_SECONDS` of `pcm`, in dBFS.
///
/// `pcm` is interleaved with `channels` samples per frame (the decoder's format). `None` when there
/// is nothing to measure or the track is digital silence — a gain derived from silence would be
/// nonsense, and "no measurement" makes auto-gain leave the track alone.
pub fn loudness_db(pcm: &[f32], sample_rate: u32, channels: usize) -> Option<f32> {
    let channels = channels.max(1);
    if sample_rate == 0 || pcm.len() < channels {
        return None;
    }
    let frames = pcm.len() / channels;
    let rate = f64::from(sample_rate);
    // A hop of one second, clamped to the track so a very short clip is one window.
    let hop = ((rate * HOP_SECONDS) as usize).clamp(1, frames);
    let per_window = ((WINDOW_SECONDS * rate) / hop as f64).ceil().max(1.0) as usize;

    // A track shorter than one hop can never fill a window, so cap the ring by the frames that
    // actually exist: `per_window` is huge there and would allocate far more than it ever uses.
    let mut hop_sums = std::collections::VecDeque::with_capacity(per_window.min(frames) + 1);
    let mut best = 0.0f64;
    let mut pos = 0usize;
    let mut any = false;
    while pos < frames {
        let take = hop.min(frames - pos);
        let slice = &pcm[pos * channels..(pos + take) * channels];
        let mut sum = 0.0f64;
        for frame in slice.chunks_exact(channels) {
            let mut power = 0.0f64;
            for sample in frame {
                power += f64::from(*sample) * f64::from(*sample);
            }
            sum += power / channels as f64;
        }
        let mean_square = sum / take as f64;
        any |= mean_square > 0.0;
        hop_sums.push_back(mean_square);
        if hop_sums.len() > per_window {
            hop_sums.pop_front();
        }
        // Only score a full window; a short track (or its tail) is scored once at the end if it
        // never filled one.
        if hop_sums.len() == per_window {
            let average = hop_sums.iter().sum::<f64>() / per_window as f64;
            if average > best {
                best = average;
            }
        }
        pos += take;
    }
    if best <= 0.0 {
        // Shorter than one window: average what there is, so a 2 s clip still normalises.
        if hop_sums.is_empty() {
            return None;
        }
        best = hop_sums.iter().sum::<f64>() / hop_sums.len() as f64;
    }
    if !any || best <= 0.0 {
        return None;
    }
    // 20·log10(rms) = 10·log10(mean square).
    Some((10.0 * best.log10()) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sine at `amplitude`: its RMS is `amplitude / √2`, so the expected dB is analytic.
    fn sine(seconds: f64, amplitude: f32, sample_rate: u32) -> Vec<f32> {
        let frames = (seconds * f64::from(sample_rate)) as usize;
        (0..frames)
            .flat_map(|i| {
                let s = (2.0 * std::f64::consts::PI * 1_000.0 * i as f64
                    / f64::from(sample_rate))
                    .sin() as f32
                    * amplitude;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn a_steady_sine_measures_its_own_rms() {
        let pcm = sine(6.0, 0.5, 44_100);
        let db = loudness_db(&pcm, 44_100, 2).expect("a measurable track");
        let expected = 20.0 * f64::from(0.5f32 / std::f32::consts::SQRT_2).log10();
        assert!(
            (f64::from(db) - expected).abs() < 0.1,
            "{db:.2} dB vs expected {expected:.2} dB"
        );
    }

    /// The loudest section is the reference: a quiet intro must not drag the gain up.
    #[test]
    fn the_loudest_section_wins() {
        let mut pcm = sine(10.0, 0.02, 44_100); // quiet intro
        pcm.extend(sine(10.0, 0.5, 44_100)); // the drop
        let db = loudness_db(&pcm, 44_100, 2).expect("a measurable track");
        let loud = 20.0 * f64::from(0.5f32 / std::f32::consts::SQRT_2).log10();
        assert!(
            (f64::from(db) - loud).abs() < 0.3,
            "the loud part should set the level: {db:.2} vs {loud:.2}"
        );
    }

    #[test]
    fn silence_is_unmeasurable_rather_than_minus_infinity() {
        let pcm = vec![0.0f32; 44_100 * 2 * 4];
        assert_eq!(loudness_db(&pcm, 44_100, 2), None);
        assert_eq!(loudness_db(&[], 44_100, 2), None);
    }

    #[test]
    fn a_clip_shorter_than_one_window_still_measures() {
        let pcm = sine(1.0, 0.5, 44_100);
        let db = loudness_db(&pcm, 44_100, 2).expect("a measurable clip");
        let expected = 20.0 * f64::from(0.5f32 / std::f32::consts::SQRT_2).log10();
        assert!((f64::from(db) - expected).abs() < 0.2, "{db:.2}");
    }
}
