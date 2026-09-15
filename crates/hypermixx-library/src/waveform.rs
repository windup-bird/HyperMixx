//! Waveform peak extraction for overview rendering.
//!
//! Pure function over a [`Source`]; lives in `library` because it is derived display data, not a
//! playback concern. Kept trivial: min/max per bucket of the mono downmix.

use hypermixx_media::Source;

/// Per-bucket `(min, max)` peaks of a mono signal, plus the bucket size in frames.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Waveform {
    pub peaks: Vec<(f32, f32)>,
    pub bucket_frames: u64,
}

/// Reduces `source` to one `(min, max)` pair per `bucket_frames` of audio.
pub fn peaks(source: &dyn Source, bucket_frames: u64) -> Waveform {
    const CH: usize = hypermixx_core::CHANNELS;
    let bucket = bucket_frames.max(1);
    let total = source.total_frames();
    let mut out = Vec::with_capacity((total / bucket) as usize + 1);
    let mut block = vec![0.0f32; 4096 * CH];
    let mut start = 0u64;
    let mut cur = (f32::MAX, f32::MIN);
    let mut in_bucket = 0u64;
    while start < total {
        let read = source.read_frames(start, &mut block);
        if read == 0 {
            break;
        }
        for i in (0..read * CH).step_by(CH) {
            let mono = (block[i] + block[i + 1]) * 0.5;
            cur.0 = cur.0.min(mono);
            cur.1 = cur.1.max(mono);
            in_bucket += 1;
            if in_bucket >= bucket {
                out.push(cur);
                cur = (f32::MAX, f32::MIN);
                in_bucket = 0;
            }
        }
        start += read as u64;
    }
    if in_bucket > 0 {
        out.push(cur);
    }
    Waveform {
        peaks: out,
        bucket_frames: bucket,
    }
}
