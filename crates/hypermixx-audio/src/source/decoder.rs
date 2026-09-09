//! Whole-file decoder: audio file -> in-memory 48kHz interleaved stereo f32.

use std::error::Error;
use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::{CHANNELS, SAMPLE_RATE};

/// Fully decoded audio, ready to be turned into a [`PcmPool`](crate::source::PcmPool).
pub struct DecodedAudio {
    /// Interleaved stereo PCM at `sample_rate`.
    pub pcm: Vec<f32>,
    pub total_frames: u64,
    /// Always [`SAMPLE_RATE`] (`decode_file` resamples if the source differs).
    pub sample_rate: u32,
    /// Always [`CHANNELS`].
    pub channels: usize,
}

/// Decodes an audio file (MP3 / WAV / FLAC / ...) into 48kHz interleaved stereo f32.
///
/// CPU and IO heavy: call from a background thread, never from the audio path.
pub fn decode_file(path: &str) -> Result<DecodedAudio, Box<dyn Error>> {
    let file = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let format_opts = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };
    let meta_opts = MetadataOptions::default();
    let probed = symphonia::default::get_probe().format(&hint, mss, &format_opts, &meta_opts)?;
    let mut format = probed.format;

    // Pick the first track whose codec symphonia can actually instantiate.
    let dec_opts = DecoderOptions::default();
    let mut picked = None;
    for track in format.tracks() {
        if symphonia::default::get_codecs()
            .make(&track.codec_params, &dec_opts)
            .is_ok()
        {
            picked = Some((track.id, track.codec_params.clone()));
            break;
        }
    }
    let (track_id, params) = picked.ok_or("no decodable audio track in file")?;
    let mut decoder = symphonia::default::get_codecs().make(&params, &dec_opts)?;

    // Decode everything into a scratch buffer at the native rate/channel count.
    let mut raw: Vec<f32> = Vec::new();
    let mut src_rate = params.sample_rate.unwrap_or(SAMPLE_RATE);
    let mut src_channels = params.channels.map(|c| c.count()).unwrap_or(CHANNELS);

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(_) => break, // end of stream
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(_) => continue, // skip a corrupt packet rather than failing the whole file
        };
        if decoded.frames() == 0 {
            continue;
        }
        let spec = *decoded.spec();
        src_rate = spec.rate;
        src_channels = spec.channels.count();

        let mut buffer = SampleBuffer::<f32>::new(decoded.frames() as u64, spec);
        buffer.copy_interleaved_ref(decoded);
        raw.extend_from_slice(buffer.samples());
    }

    if raw.is_empty() {
        return Err("file decoded to zero samples".into());
    }

    let stereo = into_stereo(raw, src_channels);
    let pcm = resample(stereo, src_rate, SAMPLE_RATE);
    let total_frames = (pcm.len() / CHANNELS) as u64;

    Ok(DecodedAudio {
        pcm,
        total_frames,
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
    })
}

/// Down/upper-mixes interleaved PCM to stereo. Sources with more than 2 channels keep L/R only.
fn into_stereo(pcm: Vec<f32>, channels: usize) -> Vec<f32> {
    let channels = channels.max(1);
    if channels == CHANNELS {
        return pcm;
    }
    let frames = pcm.len() / channels;
    let mut out = vec![0.0f32; frames * CHANNELS];
    for (i, frame) in pcm.chunks_exact(channels).enumerate() {
        let l = frame[0];
        let r = if channels >= 2 { frame[1] } else { l };
        out[i * CHANNELS] = l;
        out[i * CHANNELS + 1] = r;
    }
    out
}

/// Naive linear-interpolation resampler, interleaved in / out.
///
/// v1 deliberately trades quality for simplicity (spec allows "简单处理"): no anti-alias filter,
/// so upsampling is slightly bright and downsampling can alias. Replace with a proper
/// (polyphase / windowed-sinc) stage before shipping time-stretch quality work.
fn resample(pcm: Vec<f32>, from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == 0 || from_rate == to_rate {
        return pcm;
    }
    let frames_in = pcm.len() / CHANNELS;
    let frames_out = ((frames_in as f64) * (to_rate as f64) / (from_rate as f64)).floor() as usize;
    if frames_out == 0 {
        return Vec::new();
    }
    let step = (frames_in - 1) as f64 / (frames_out.max(2) - 1) as f64;
    let mut out = vec![0.0f32; frames_out * CHANNELS];
    for i in 0..frames_out {
        let pos = (i as f64) * step;
        let idx = pos.floor() as usize;
        let frac = (pos - idx as f64) as f32;
        let next = (idx + 1).min(frames_in - 1);
        for ch in 0..CHANNELS {
            let a = pcm[idx * CHANNELS + ch];
            let b = pcm[next * CHANNELS + ch];
            out[i * CHANNELS + ch] = a + (b - a) * frac;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_passthrough() {
        let pcm = vec![1.0, -1.0, 2.0, -2.0];
        assert_eq!(into_stereo(pcm.clone(), 2), pcm);
    }

    #[test]
    fn mono_is_duplexed() {
        assert_eq!(into_stereo(vec![0.5, -0.5], 1), vec![0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn upsample_grows_frame_count() {
        let pcm = vec![0.0, 0.0, 1.0, 1.0]; // 2 frames, ramp 0 -> 1
        let out = resample(pcm, 24_000, 48_000);
        assert_eq!(out.len() / CHANNELS, 4);
        assert!((out[0] - 0.0).abs() < 1e-6);
        assert!((out[2 * CHANNELS] - 2.0 / 3.0).abs() < 1e-6);
        assert!((out[3 * CHANNELS] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn downsample_keeps_duration_ratio() {
        let pcm: Vec<f32> = (0..100).flat_map(|i| [i as f32, i as f32]).collect();
        let out = resample(pcm, 96_000, 48_000);
        assert_eq!(out.len() / CHANNELS, 50);
        assert_eq!(out[0], 0.0);
        assert!((out[out.len() - CHANNELS] - 99.0).abs() < 1e-3);
    }

    #[test]
    fn same_rate_is_identity() {
        let pcm = vec![0.25, -0.25];
        assert_eq!(resample(pcm.clone(), 48_000, 48_000), pcm);
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(decode_file("/definitely/not/here.wav").is_err());
    }
}
