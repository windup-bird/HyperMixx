//! Helpers shared by the integration tests.

use std::f32::consts::PI;
use std::fs;
use std::io::{BufWriter, Write};

/// Minimal 16-bit PCM WAV writer, just enough to hand symphonia a real file.
pub fn write_wav(path: &str, sample_rate: u32, channels: u16, frames: usize) {
    let block_align = channels * 2;
    let data_bytes = (frames * block_align as usize) as u32;

    let mut pcm = Vec::with_capacity(data_bytes as usize);
    for frame in 0..frames {
        let value = (frame as f32 / sample_rate as f32 * 440.0 * 2.0 * PI).sin();
        for _ in 0..channels {
            pcm.extend_from_slice(&((value * i16::MAX as f32) as i16).to_le_bytes());
        }
    }

    let mut out = BufWriter::new(fs::File::create(path).unwrap());
    out.write_all(b"RIFF").unwrap();
    out.write_all(&(36 + data_bytes).to_le_bytes()).unwrap();
    out.write_all(b"WAVE").unwrap();
    out.write_all(b"fmt ").unwrap();
    out.write_all(&16u32.to_le_bytes()).unwrap();
    out.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    out.write_all(&channels.to_le_bytes()).unwrap();
    out.write_all(&sample_rate.to_le_bytes()).unwrap();
    out.write_all(&(sample_rate * block_align as u32).to_le_bytes())
        .unwrap();
    out.write_all(&block_align.to_le_bytes()).unwrap();
    out.write_all(&16u16.to_le_bytes()).unwrap();
    out.write_all(b"data").unwrap();
    out.write_all(&data_bytes.to_le_bytes()).unwrap();
    out.write_all(&pcm).unwrap();
    out.flush().unwrap();
}

/// A unique path under the system temp dir.
pub fn temp_path(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("hypermixx-it-{name}"))
        .to_string_lossy()
        .into_owned()
}
