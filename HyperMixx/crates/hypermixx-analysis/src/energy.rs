//! 能量信封：结构分段（intro/drop/break）的特征源（目标 2 自研算法用）。
//! 纯 additive 接口层，不进音频管线、不碰 segment.rs 解码路径。
//! 音频数据源二选一：TrackCache 原始 PCM（hop 粒度 RMS），或
//! 已归一化的波形 detail 列（128 帧/列的 √ 压缩显示值）。

use hypermixx_audio::track_cache::TrackCache;

use crate::waveform::WaveformData;

/// 默认 hop：512 帧（≈10.7ms @48k）。
pub const DEFAULT_HOP_FRAMES: usize = 512;

/// 从 TrackCache 提取 RMS 能量信封（hop 采样，交织立体声先混 mono）。
/// 只读已填的连续前缀（filled_frames）；缓存未就绪（不足一 hop）返回空。
/// 返回长度 = filled_frames / hop（尾部不满一 hop 丢弃）。
pub fn energy_envelope(cache: &TrackCache, hop_frames: usize) -> Vec<f32> {
    let hop = hop_frames.max(1);
    let filled = cache.filled_frames();
    if filled < hop as u64 {
        return Vec::new();
    }
    // 整块读（8192 帧 = 4×2048 块）到复用缓冲，再按 hop 细分算 RMS；
    // 跨块边界 hop 计数延续，不满一 hop 的尾部丢弃。
    let chunk = 8192usize;
    let mut buf = vec![0.0f32; chunk * 2];
    let mut env = Vec::with_capacity((filled / hop as u64) as usize + 1);
    let mut sum_sq = 0.0f64;
    let mut in_hop = 0usize;
    let mut pos = 0u64;
    while pos < filled {
        let want = (chunk as u64).min(filled - pos) as usize;
        let got = cache.copy_ready(&mut buf[..want * 2], pos, want);
        if got == 0 {
            break; // 未填空洞（顺序填充下不会发生，防御性退出）
        }
        for i in 0..got {
            let m = (buf[i * 2] + buf[i * 2 + 1]) * 0.5;
            sum_sq += (m as f64) * (m as f64);
            in_hop += 1;
            if in_hop == hop {
                env.push((sum_sq / hop as f64).sqrt() as f32);
                sum_sq = 0.0;
                in_hop = 0;
            }
        }
        pos += got as u64;
    }
    env
}

/// 从已归一化的波形 detail 列提取能量信封：列能量 = all_p 满刻度比
/// （√ 压缩显示值，结构形状保留）。`smooth` = 盒式移动平均窗口
/// （列数；0/1 = 不平滑）。返回与 detail 等长。
pub fn envelope_from_waveform(wave: &WaveformData, smooth: usize) -> Vec<f32> {
    let raw: Vec<f32> = wave
        .detail
        .iter()
        .map(|c| c.all_p as f32 / 255.0)
        .collect();
    if smooth <= 1 {
        return raw;
    }
    let half = smooth / 2;
    (0..raw.len())
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + half + 1).min(raw.len());
            raw[lo..hi].iter().sum::<f32>() / (hi - lo) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// 48k 立体声正弦 WAV（440Hz，振幅 0.5 → RMS ≈ 0.354）。
    fn synth_wav(path: &Path, secs: f64) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let n = (secs * 48_000.0) as usize;
        for i in 0..n {
            let t = i as f32 / 48_000.0;
            let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    /// TrackCache::open → 轮询 fill_done（5s 超时）→ 经 lookup 取 Arc。
    /// （open 返回的 Arc 持有 filler 线程引用，lookup 返回 clone 视图，
    /// 线程结束后两者皆可释放。）
    fn open_filled(path: &Path) -> std::sync::Arc<TrackCache> {
        let cache = TrackCache::open(path, 48_000).expect("open 应成功");
        let t0 = Instant::now();
        while !cache.fill_done() {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "填充超时（1s 曲 5s 内必完）"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        TrackCache::lookup(path).expect("lookup 应有登记")
    }

    #[test]
    fn energy_envelope_tracks_sine_rms() {
        let p = std::env::temp_dir().join("hypermixx-energy-test.wav");
        synth_wav(&p, 1.0);
        let cache = open_filled(&p);
        let env = energy_envelope(&cache, 512);
        assert_eq!(env.len(), 48_000 / 512, "1s 曲 hop512 应 93 点");
        // 440Hz 正弦 ±0.5 → 每 hop RMS ≈ 0.5/√2 ≈ 0.3536（±块内相位抖动）
        for (i, e) in env.iter().enumerate() {
            assert!(
                (*e - 0.3536).abs() < 0.02,
                "env[{i}] = {e}，应 ≈0.354"
            );
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn energy_envelope_empty_before_fill() {
        let p = std::env::temp_dir().join("hypermixx-energy-empty-test.wav");
        synth_wav(&p, 1.0);
        let cache = TrackCache::open(&p, 48_000).expect("open 应成功");
        // 首块同步填充可能已就绪；仅验证 API 不 panic 且不产生假值
        let env = energy_envelope(&cache, 512);
        assert!(env.len() <= 48_000 / 512);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn envelope_from_waveform_smooths() {
        use crate::waveform::Column;
        // 交替 0/1 的方块列 → 平滑后中间值 ≈ 0.5
        let cols: Vec<Column> = (0..20)
            .map(|i| Column {
                all_p: if i % 2 == 0 { 255 } else { 0 },
                ..Default::default()
            })
            .collect();
        let wave = WaveformData {
            detail: cols.clone(),
            overview: vec![],
            frames_per_col: 128,
            sample_rate: 48_000,
            duration_frames: 20 * 128,
        };
        let raw = envelope_from_waveform(&wave, 1);
        assert_eq!(raw.len(), 20);
        assert!((raw[0] - 1.0).abs() < 1e-6);
        assert!(raw[1].abs() < 1e-6);
        // 窗口 9（±4）→ 中间列 0.444..0.556 ≈ 0.5（边界列窗口截短除外）
        let sm = envelope_from_waveform(&wave, 9);
        assert_eq!(sm.len(), 20);
        for (i, v) in sm.iter().enumerate().skip(2).take(16) {
            assert!((v - 0.5).abs() < 0.1, "sm[{i}] 应 ≈0.5，实得 {v}");
        }
    }
}
