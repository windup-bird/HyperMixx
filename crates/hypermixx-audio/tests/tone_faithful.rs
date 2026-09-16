//! 引擎忠实度回归:Deck(含 Flow + timestretch 引擎)在 ratio=1.0 时必须透传,
//! 输出频率 = 输入频率。任何慢放/快放/伸缩都会直接体现为频率偏移。
//!
//! 无需声卡:直接驱动 `Deck::process_block` 采集输出。

use std::sync::Arc;

use hypermixx_audio::{Deck, DECK_MIX_GAIN};
use hypermixx_core::{Source, CHANNELS, SAMPLE_RATE};
use hypermixx_media::{DecodedAudio, PcmPool};

const TONE_HZ: f64 = 480.0;
const SECONDS: f64 = 2.0;

fn tone_pool() -> Arc<dyn Source> {
    let frames = (SECONDS * SAMPLE_RATE as f64) as usize;
    let mut pcm = Vec::with_capacity(frames * CHANNELS);
    for i in 0..frames {
        let s = (2.0 * std::f64::consts::PI * TONE_HZ * i as f64 / SAMPLE_RATE as f64).sin() as f32;
        pcm.extend_from_slice(&[s, s]);
    }
    Arc::new(PcmPool::from_decoded(DecodedAudio {
        pcm,
        total_frames: frames as u64,
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
    }))
}

/// 统计左声道过零点(剔除前 `skip` 块,避开引擎预热瞬态)。
fn measured_hz(samples: &[f32], skip_blocks: usize, block_frames: usize) -> f64 {
    let start = skip_blocks * block_frames * CHANNELS;
    let left: Vec<f32> = samples[start..].iter().step_by(CHANNELS).copied().collect();
    let crossings = left
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    let duration = left.len() as f64 / SAMPLE_RATE as f64;
    crossings as f64 / 2.0 / duration
}

#[test]
fn deck_is_faithful_at_ratio_one() {
    let mut deck = Deck::new(tone_pool());
    deck.play();

    let block_frames = 256;
    let mut out = vec![0.0f32; block_frames * CHANNELS];
    let mut collected = Vec::new();
    let blocks = (SECONDS * SAMPLE_RATE as f64) as usize / block_frames;
    for _ in 0..blocks {
        deck.process_block(&mut out);
        collected.extend_from_slice(&out);
    }

    // 位置推进必须是实时的:floor(t * SAMPLE_RATE)。
    let expected = (SECONDS * SAMPLE_RATE as f64) as u64;
    let got = deck.current_frame();
    assert!(
        (got as i64 - expected as i64).abs() <= block_frames as i64,
        "播放位置 {got} ≠ 实时 {expected}"
    );

    // 频率忠实: Tape@1.0 是直通,输出频率必须等于输入 480Hz(容差 1% ≈ 引擎圆润过渡)。
    let hz = measured_hz(&collected, 4, block_frames);
    assert!(
        (hz - TONE_HZ).abs() / TONE_HZ < 0.01,
        "输出 {hz:.1} Hz ≠ 输入 {TONE_HZ} Hz — 引擎在伸缩时间(慢放/快放)"
    );

    // 幅度忠实:直通不应整体衰减(混音增益在 pipeline 层,Deck 层保持原始幅度)。
    let start = 8 * block_frames * CHANNELS;
    let peak = collected[start..]
        .iter()
        .step_by(CHANNELS)
        .fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(
        (peak - 1.0).abs() < 0.15,
        "输出峰值 {peak:.2} 偏离 1.0 — 有意外衰减/增益(DECK_MIX_GAIN={DECK_MIX_GAIN} 属 pipeline 层)"
    );
}

/// 44.1kHz 素材(引擎原生率)解码后时长守恒:total_frames ≈ 时长 × 44100。
#[test]
#[ignore = "需要 test.mp3;运行: cargo test -p hypermixx-audio --test tone_faithful -- --ignored"]
fn decoded_44k_duration_is_preserved() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test.mp3");
    let Ok(decoded) = hypermixx_media::decode_file(path) else {
        panic!("无法解码 {path}");
    };
    let seconds = decoded.total_frames as f64 / SAMPLE_RATE as f64;
    assert!(
        (seconds - 418.648).abs() < 1.0,
        "重采样后时长 {seconds:.3}s,期望 ≈418.648s(418.648×44100 ≈ 18462369 帧,实际 {})",
        decoded.total_frames
    );
}
