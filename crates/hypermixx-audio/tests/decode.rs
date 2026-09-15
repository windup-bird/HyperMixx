//! Decoder tests against real files on disk (written to a temp dir, so no fixtures to commit).

mod common;

use std::fs;

use common::{temp_path, write_wav};
use hypermixx_audio::{CHANNELS, SAMPLE_RATE};
use hypermixx_media::decode_file;

#[test]
fn decodes_native_rate_stereo() {
    let path = temp_path("native.wav");
    write_wav(&path, SAMPLE_RATE, CHANNELS as u16, 24_000); // half a second
    let audio = decode_file(&path).unwrap();
    assert_eq!(audio.sample_rate, SAMPLE_RATE);
    assert_eq!(audio.channels, CHANNELS);
    assert_eq!(audio.total_frames, 24_000);
    assert_eq!(audio.pcm.len(), 48_000);
    let _ = fs::remove_file(path);
}

#[test]
fn resamples_foreign_rate() {
    let path = temp_path("44k.wav");
    write_wav(&path, 44_100, CHANNELS as u16, 44_100); // one second
    let audio = decode_file(&path).unwrap();
    assert_eq!(audio.sample_rate, SAMPLE_RATE);
    // A 44.1kHz second must become a 48kHz second, within a block of rounding.
    assert!(
        (audio.total_frames as i64 - SAMPLE_RATE as i64).abs() < 256,
        "got {}",
        audio.total_frames
    );
    let _ = fs::remove_file(path);
}

#[test]
fn upmixes_mono_to_stereo() {
    let path = temp_path("mono.wav");
    write_wav(&path, SAMPLE_RATE, 1, 4_800);
    let audio = decode_file(&path).unwrap();
    assert_eq!(audio.total_frames, 4_800);
    assert_eq!(audio.pcm.len(), 9_600);
    assert_eq!(
        audio.pcm[0], audio.pcm[1],
        "mono channels should be identical"
    );
    let _ = fs::remove_file(path);
}

#[test]
fn rejects_garbage_and_missing_files() {
    let path = temp_path("garbage.wav");
    fs::write(&path, b"not an audio file at all").unwrap();
    assert!(decode_file(&path).is_err());
    assert!(decode_file("/nope/nothing.wav").is_err());
    let _ = fs::remove_file(path);
}
