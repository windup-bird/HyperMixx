//! 渐进式波形分析：按 16s 分段解码，优先分析播放头附近，逐段推给 UI。
//! 与 waveform::analyze() 共享列累积/归一化助手；顺序模式（不 seek）下
//! 各段连续解码，Done 结果与整曲分析逐字节一致。
//!
//! 线程生命周期：UI 重载同 deck 时置 shutdown 标志（旧线程尽快退出）、
//! 递增 generation（旧事件被丢弃）；UI 销毁时 channel 断开，send 失败后线程同样退出。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use anyhow::Result;
use timestretch::analysis::beat::detect_beats_with_options;
use timestretch::analysis::key::detect_key;
use timestretch::analysis::rigid_grid::refine_grid_rigid;
use timestretch::core::preanalysis::KeyEstimate;
use timestretch::TempoTrackingOptions;

use hypermixx_audio::decode::{To48k, TrackDecoder};

use crate::mono::{MonoAccumulator, TRACK_MONO_RATE, mixdown_48k};
use crate::waveform::{self, BandFilters, ColPeak, Column, DETAIL_FRAMES_PER_COL, WaveformData};

/// 每段帧数（48kHz）：16s。段内列数 6000 % OVERVIEW_RATIO == 0，overview 对齐。
pub const SEG_FRAMES: u64 = 768_000;
/// 每段 detail 列数（128 帧/列）。
pub const SEG_COLS: usize = (SEG_FRAMES / DETAIL_FRAMES_PER_COL as u64) as usize;

pub enum AnalysisEvent {
    /// 一个分析完成的段（满刻度 √ 压缩的显示值；全曲完成后由 Done 替换）。
    Segment {
        generation: u64,
        seg: usize,
        detail: Box<[Column]>,
        overview: Box<[Column]>,
    },
    /// 单遍分析结果（BPM/调性/beatgrid），Done 之前发出。
    /// bpm=0 表示未检测到；beats/downbeats 为秒坐标（12k 时间轴）。
    TrackAnalysis {
        generation: u64,
        bpm: f64,
        key: Option<KeyEstimate>,
        /// 首拍秒偏移（grid.beats 空时为 0）。检测器会把网格外推回曲首，
        /// 此值可能落在首个实际拍点之前；拍点对齐性以 beats_secs 为准。
        offset_secs: f64,
        beats_secs: Box<[f64]>,
        downbeats_secs: Box<[f64]>,
        confidence: f32,
        /// 分段网格初值（自研算法的参考输入）：(起点秒, bpm, 刚性 0..1)。
        /// 取自 timestretch detect 的分段列表（refine_grid_rigid 采纳时
        /// 会丢弃它，故在 refine 之前捕获）。
        tempo_segments: Vec<(f64, f64, f32)>,
    },
    /// 全曲分析完成：与 analyze() 相同的全局归一化数据。
    Done {
        generation: u64,
        wave: WaveformData,
    },
    Failed {
        generation: u64,
        msg: String,
    },
}

/// 启动渐进分析线程。
/// - `priority`：播放头所在段索引（UI 每 tick 更新），未分析段按距其远近排序；
/// - `shutdown`：置位后线程在当前段结束后退出（重载场景）；
/// - `generation`：每 deck 代际，UI 用它丢弃旧线程的事件。
pub fn start_analysis(
    path: PathBuf,
    priority: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    generation: u64,
    tx: Sender<AnalysisEvent>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("wave-analysis".into())
        .spawn(move || {
            if let Err(e) = analyzer_main(&path, &priority, &shutdown, generation, &tx) {
                let _ = tx.send(AnalysisEvent::Failed {
                    generation,
                    msg: format!("{e:#}"),
                });
            }
        })
        .expect("spawn analysis thread")
}

fn analyzer_main(
    path: &Path,
    priority: &AtomicU64,
    shutdown: &AtomicBool,
    generation: u64,
    tx: &Sender<AnalysisEvent>,
) -> Result<()> {
    let mut dec = TrackDecoder::open(path)?;
    let sr_in = dec.sample_rate;
    let mut rs = To48k::new(sr_in, 48_000)?;

    // 格式不报帧数（罕见）：退回整曲一次性分析
    let Some(n_in) = dec.n_frames else {
        let wave = waveform::analyze(path)?;
        let _ = tx.send(AnalysisEvent::Done { generation, wave });
        return Ok(());
    };

    let total_frames = (n_in as f64 * 48_000.0 / sr_in as f64) as u64;
    let n_segs = total_frames.div_ceil(SEG_FRAMES) as usize;

    // 原始峰值按段保留（Done 时拼接做全局归一化，与 analyze() 同路径）
    let mut raw: Vec<Option<Box<[ColPeak]>>> = vec![None; n_segs];
    // 12k 单声道按段保留（key + 粗 tempo 源；段边界重建 FIR）
    let mut monos: Vec<Option<Box<[f32]>>> = vec![None; n_segs];
    // 48k 单声道按段保留（细拍位 superflux + rigid 拟合源；段边界重建）。
    // 16s/段 ≈3MB，329s 曲 ≈63MB 瞬态，双 deck 并发 ≈126MB——RK3399 4GB 可承受。
    let mut monos48: Vec<Option<Box<[f32]>>> = vec![None; n_segs];
    let mut done: Vec<bool> = vec![false; n_segs];
    let mut filters = BandFilters::new(48_000.0);
    let mut last_contig: Option<u64> = None; // 上次解码停止的 48k 帧位置
    let mut carry: Vec<f32> = Vec::new(); // 段边界处多转换的样本（下一段续用）
    let mut actual_total = 0u64;

    while done.iter().any(|d| !*d) {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        // 选离 priority 最近的未完成段（等距取小号，顺序模式即 0,1,2,…）
        let p = priority.load(Ordering::Relaxed).min(n_segs as u64 - 1) as usize;
        let seg = (0..n_segs)
            .filter(|i| !done[*i])
            .min_by_key(|i| ((*i as i64 - p as i64).abs(), *i))
            .expect("存在未完成段");

        let seg_start = seg as u64 * SEG_FRAMES;
        let seg_end = ((seg as u64 + 1) * SEG_FRAMES).min(total_frames);

        // 恰好延续上次解码位置 → 不 seek，滤波器/重采样器状态续用
        if last_contig != Some(seg_start) {
            // seek 越界（n_frames 标称偏长/文件损坏）：无更多数据，全部视为完成
            if dec.seek(seg_start as f64 / 48_000.0).is_err() {
                for d in done.iter_mut() {
                    *d = true;
                }
                continue;
            }
            rs = To48k::new(sr_in, 48_000)?;
            filters = BandFilters::new(48_000.0);
            carry.clear();
        }

        // 解码到段边界（末段到 EOF 即止）
        let mut cols = vec![ColPeak::default()];
        let mut mono_acc = MonoAccumulator::new();
        let mut mono = Vec::with_capacity((SEG_FRAMES / 4) as usize);
        let mut mono48 = Vec::with_capacity(SEG_FRAMES as usize);
        let mut got = 0u64;
        let seg_eof = loop {
            if got >= seg_end - seg_start {
                break false;
            }
            let rem = (seg_end - seg_start - got) as usize;
            if !carry.is_empty() {
                let c = waveform::accumulate_upto(&mut cols, &mut filters, &carry, rem);
                mono_acc.process(&carry[..c * 2], &mut mono);
                mixdown_48k(&carry[..c * 2], &mut mono48);
                got += c as u64;
                carry.drain(..c * 2);
                continue;
            }
            let Some(native) = dec.decode_next()? else {
                break true; // 流比 n_frames 标称短（损坏/尾帧误差）
            };
            let converted = rs.process(&native)?;
            let c = waveform::accumulate_upto(&mut cols, &mut filters, &converted, rem);
            mono_acc.process(&converted[..c * 2], &mut mono);
            mixdown_48k(&converted[..c * 2], &mut mono48);
            got += c as u64;
            if c * 2 < converted.len() {
                carry = converted[c * 2..].to_vec();
            }
        };
        waveform::pop_trailing_empty(&mut cols);
        actual_total += got;

        if !cols.is_empty() {
            let display = waveform::fixed_scale(&cols);
            let overview = waveform::build_overview(&display);
            raw[seg] = Some(cols.into_boxed_slice());
            let _ = tx.send(AnalysisEvent::Segment {
                generation,
                seg,
                detail: display.into_boxed_slice(),
                overview: overview.into_boxed_slice(),
            });
        }
        monos[seg] = Some(mono.into_boxed_slice());
        monos48[seg] = Some(mono48.into_boxed_slice());
        done[seg] = true;
        last_contig = Some(seg_start + got);
        // EOF 只是流结束：优先序下最后一段可能最先分析，更早的段仍可 seek 回去
        // 解码；若本段一帧都没解出（seek 已过 EOF），剩余段也无数据。
        if seg_eof && got == 0 {
            for d in done.iter_mut() {
                *d = true;
            }
        }
    }

    // 单遍分析：拼接各段 12k mono → BPM/beatgrid/key（timestretch 分析）。
    // shutdown 置位（UI 已重载）→ 跳过，线程直接退出。
    if shutdown.load(Ordering::Relaxed) {
        return Ok(());
    }
    let ta0 = std::time::Instant::now();
    let analysis = track_analysis(&monos, &monos48);
    if let Some(a) = analysis {
        log::info!(
            "单遍分析完成：BPM {:.1}，{} 拍 / {} 下拍，key {:?}，{:.2}s",
            a.bpm,
            a.beats_secs.len(),
            a.downbeats_secs.len(),
            a.key.as_ref().map(|k| k.name()),
            ta0.elapsed().as_secs_f64()
        );
        let _ = tx.send(AnalysisEvent::TrackAnalysis {
            generation,
            bpm: a.bpm,
            key: a.key,
            offset_secs: a.offset_secs,
            beats_secs: a.beats_secs,
            downbeats_secs: a.downbeats_secs,
            confidence: a.confidence,
            tempo_segments: a.tempo_segments,
        });
    }

    // 收尾：raw 拼接 → 全局归一化（与 analyze() 完全相同的代码路径）
    let mut all: Vec<ColPeak> =
        Vec::with_capacity((actual_total / DETAIL_FRAMES_PER_COL as u64) as usize + 1);
    for s in raw.iter().flatten() {
        all.extend_from_slice(s);
    }
    let detail = waveform::normalize_detail(&all);
    let overview = waveform::build_overview(&detail);

    log::info!(
        "波形分析完成（渐进）：{} 帧，detail {} 列，{} 段",
        actual_total,
        detail.len(),
        done.iter().filter(|d| **d).count()
    );
    let _ = tx.send(AnalysisEvent::Done {
        generation,
        wave: WaveformData {
            detail,
            overview,
            frames_per_col: DETAIL_FRAMES_PER_COL as u32,
            sample_rate: 48_000,
            duration_frames: actual_total,
        },
    });
    Ok(())
}

/// 单遍分析的中间结果（秒坐标）。
struct TrackAnalysisData {
    bpm: f64,
    key: Option<KeyEstimate>,
    offset_secs: f64,
    beats_secs: Box<[f64]>,
    downbeats_secs: Box<[f64]>,
    confidence: f32,
    tempo_segments: Vec<(f64, f64, f32)>,
}

/// 置信低于此值不发布 BPM（grid 事件仍发 beats，但 bpm=0 防引擎
/// sync/loop 建在劣质网格上；bridge 侧另有同阈值兜底不写 grid 总线）。
const GRID_MIN_CONFIDENCE: f32 = 0.25;

/// 段刚性初值：段内相邻拍间隔的变异系数 → 1/(1+CV) ∈ (0.5, 1]。
/// 恒定拍距 → 1.0（完全刚性）；拍距漂移越大越接近 0.5。拍不足两拍
/// 无法估 CV，直接取 1.0。
fn segment_rigidity(beats: &[f64], start_beat: usize, end_beat: usize) -> f32 {
    if end_beat <= start_beat + 1 {
        return 1.0;
    }
    let mut sum = 0.0;
    let mut sumsq = 0.0;
    let mut n = 0usize;
    for i in start_beat..end_beat - 1 {
        let d = beats[i + 1] - beats[i];
        sum += d;
        sumsq += d * d;
        n += 1;
    }
    let mean = sum / n as f64;
    if mean <= 0.0 {
        return 1.0;
    }
    let var = (sumsq / n as f64 - mean * mean).max(0.0);
    let cv = var.sqrt() / mean;
    (1.0 / (1.0 + cv)).clamp(0.0, 1.0) as f32
}

/// 拼接各段 mono → timestretch 检测：12k 粗链定 tempo + 48k superflux
/// 细链定拍位 + 48k rigid 拟合，key 用 12k。
/// 各段为 None（空段）跳过；总量 <10s 直接返回 None（检测器不可信）。
fn track_analysis(
    monos: &[Option<Box<[f32]>>],
    monos48: &[Option<Box<[f32]>>],
) -> Option<TrackAnalysisData> {
    let total: usize = monos.iter().flatten().map(|m| m.len()).sum();
    if total < TRACK_MONO_RATE as usize * 10 {
        return None;
    }
    let mut mono = Vec::with_capacity(total);
    for m in monos.iter().flatten() {
        mono.extend_from_slice(m);
    }
    let mut mono48 = Vec::with_capacity(total * 4);
    for m in monos48.iter().flatten() {
        mono48.extend_from_slice(m);
    }

    // 0.11.0 单分辨率 48k superflux + EDM hint 范围（100–160，crates.io
    // 版无 master 的双分辨率粗链定层级修复）；再对 48k kick 包络做 rigid
    // 拟合（八度守卫钉在 tempogram 决定的层级上）。
    let grid = detect_beats_with_options(&mono48, 48_000, &TempoTrackingOptions {
        hint_range: Some((100.0, 160.0)),
        ..Default::default()
    });
    // 分段初值在 refine 之前捕获：refine_grid_rigid 采纳时会把 segments
    // 替换成单一 rigid 段（丢弃 detect 的分段列表）。空列表（如 detect
    // 无拍）回退为单段 [0, bpm]。
    let sr = 48_000u32;
    let tempo_segments: Vec<(f64, f64, f32)> = if grid.segments.is_empty() {
        vec![(0.0, grid.bpm, 1.0)]
    } else {
        let mut out = Vec::with_capacity(grid.segments.len());
        for (i, seg) in grid.segments.iter().enumerate() {
            let end = grid
                .segments
                .get(i + 1)
                .map(|n| n.start_beat)
                .unwrap_or(grid.beats.len());
            out.push((
                grid.beats
                    .get(seg.start_beat)
                    .map(|&b| b / sr as f64)
                    .unwrap_or(0.0),
                seg.bpm,
                segment_rigidity(&grid.beats, seg.start_beat, end),
            ));
        }
        out
    };
    let (grid, adopted) = refine_grid_rigid(&mono48, 48_000, grid);
    log::debug!(
        "beatgrid：BPM {:.1}，rigid 细化采纳 = {adopted}，置信 {:.2}，分段 {} 个",
        grid.bpm,
        grid.confidence,
        tempo_segments.len()
    );
    let to_secs = |samples: f64| samples / sr as f64;
    let beats_secs: Box<[f64]> = grid.beats.iter().map(|&b| to_secs(b)).collect();
    let downbeats_secs: Box<[f64]> = grid
        .downbeats
        .iter()
        .filter_map(|&i| grid.beats.get(i))
        .map(|&b| to_secs(b))
        .collect();
    let key = detect_key(&mono, TRACK_MONO_RATE);
    Some(TrackAnalysisData {
        // 低置信 → bpm=0（事件仍发：UI 网格线可用 beats，引擎侧无 BPM 可 sync）。
        bpm: if grid.confidence < GRID_MIN_CONFIDENCE {
            0.0
        } else {
            grid.bpm
        },
        key,
        // 首拍秒：rigid 拟合把网格补齐回曲首，beats.first() 即网格锚点
        //（可能落在首个实际拍点之前，对齐性以 beats_secs 为准）。
        offset_secs: grid.beats.first().map(|&b| to_secs(b)).unwrap_or(0.0),
        beats_secs,
        downbeats_secs,
        confidence: grid.confidence,
        tempo_segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;
    use timestretch::core::preanalysis::KeyMode;

    /// 合成 44.1kHz 立体声 WAV：110/880/5000Hz 混音（三频段都有能量），
    /// 44.1k→48k 重采样路径也被覆盖。
    fn synth_wav(path: &Path, secs: f64) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let n = (secs * 44_100.0) as usize;
        for i in 0..n {
            let t = i as f32 / 44_100.0;
            let s = 0.6 * (2.0 * std::f32::consts::PI * 110.0 * t).sin()
                + 0.5 * (2.0 * std::f32::consts::PI * 880.0 * t).sin()
                + 0.4 * (2.0 * std::f32::consts::PI * 5000.0 * t).sin();
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    /// 跑完一次分析，收集全部事件直到 Done/Failed。
    fn run_analysis(path: &Path, priority: u64, timeout: Duration) -> Vec<AnalysisEvent> {
        let (tx, rx) = mpsc::channel();
        start_analysis(
            path.to_path_buf(),
            Arc::new(AtomicU64::new(priority)),
            Arc::new(AtomicBool::new(false)),
            1,
            tx,
        );
        let mut evs = Vec::new();
        loop {
            let ev = rx.recv_timeout(timeout).expect("事件超时");
            let last = matches!(
                ev,
                AnalysisEvent::Done { .. } | AnalysisEvent::Failed { .. }
            );
            evs.push(ev);
            if last {
                break;
            }
        }
        evs
    }

    fn seg_order(evs: &[AnalysisEvent]) -> Vec<usize> {
        evs.iter()
            .filter_map(|e| match e {
                AnalysisEvent::Segment { seg, .. } => Some(*seg),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn progressive_equals_full_sequential() {
        let p = std::env::temp_dir().join("hypermixx-eq-test.wav");
        synth_wav(&p, 37.0); // 1776000 帧 → 3 段（6000+6000+1875 列）
        let evs = run_analysis(&p, 0, Duration::from_secs(60));
        assert_eq!(seg_order(&evs), vec![0, 1, 2], "顺序模式应从 0 起连续分析");
        let Some(AnalysisEvent::Done { wave, .. }) = evs.last() else {
            panic!("应以 Done 结束");
        };
        let full = crate::waveform::analyze(&p).unwrap();
        assert_eq!(
            wave.detail, full.detail,
            "渐进 Done 的 detail 应与整曲分析逐字节一致"
        );
        assert_eq!(wave.overview, full.overview, "overview 应一致");
        assert_eq!(wave.duration_frames, full.duration_frames, "总帧数应一致");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn priority_orders_nearest_first() {
        let p = std::env::temp_dir().join("hypermixx-prio-test.wav");
        synth_wav(&p, 66.0); // 3168000 帧 → 5 段
        let evs = run_analysis(&p, 3, Duration::from_secs(120));
        // 距离 3：0/1/1/2/3，等距取小号
        assert_eq!(seg_order(&evs), vec![3, 2, 4, 1, 0]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_file_sends_failed() {
        let evs = run_analysis(
            Path::new("/nonexistent/hypermixx-404.wav"),
            0,
            Duration::from_secs(5),
        );
        assert!(
            matches!(evs.last(), Some(AnalysisEvent::Failed { .. })),
            "缺文件应发 Failed"
        );
    }

    /// 48k 立体声 WAV 点击轨：bpm 拍速、first_click 首拍秒。
    fn synth_clicks(path: &Path, secs: f64, bpm: f64, first_click: f64) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let n = (secs * 48_000.0) as usize;
        let period = 60.0 / bpm;
        for i in 0..n {
            let t = i as f64 / 48_000.0;
            let since = (t - first_click).rem_euclid(period);
            let s = if since < 0.012 {
                // 12ms 1.5kHz 脉冲、4ms 指数衰减（瞬态能量集中）
                ((2.0 * std::f64::consts::PI * 1500.0 * since).sin() * (-since / 0.004).exp() * 0.9)
                    as f32
            } else {
                0.0
            };
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    /// 48k 立体声 WAV 和弦进行：C–F–G–C（I-IV-V-I），secs_per_chord 每和弦时长。
    fn synth_chords(path: &Path, secs_per_chord: f64, cycles: usize) {
        let chords: [[f64; 3]; 4] = [
            [261.63, 329.63, 392.00], // C 大三
            [349.23, 440.00, 523.25], // F 大三
            [392.00, 493.88, 587.33], // G 大三
            [261.63, 329.63, 392.00], // C 大三
        ];
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let n = (secs_per_chord * 48_000.0) as usize;
        for _ in 0..cycles {
            for chord in &chords {
                for i in 0..n {
                    let t = i as f64 / 48_000.0;
                    let attack = (t / 0.02).min(1.0); // 20ms 起音防爆音
                    let mut s = 0.0;
                    for (k, f) in chord.iter().enumerate() {
                        let a = 0.35 / (k as f64 + 1.0); // 基频最响，和声递减
                        s += a
                            * ((2.0 * std::f64::consts::PI * f * t).sin()
                                + 0.5 * (2.0 * std::f64::consts::PI * f * 2.0 * t).sin());
                    }
                    let s = (s * attack) as f32;
                    w.write_sample(s).unwrap();
                    w.write_sample(s).unwrap();
                }
            }
        }
        w.finalize().unwrap();
    }

    /// 收集事件直到 Done/Failed，取 TrackAnalysis。
    fn track_analysis_of(evs: &[AnalysisEvent]) -> (f64, f64, f32) {
        evs.iter()
            .find_map(|e| match e {
                AnalysisEvent::TrackAnalysis {
                    bpm,
                    offset_secs,
                    confidence,
                    ..
                } => Some((*bpm, *offset_secs, *confidence)),
                _ => None,
            })
            .expect("应有 TrackAnalysis 事件")
    }

    #[test]
    fn track_analysis_detects_120bpm() {
        let p = std::env::temp_dir().join("hypermixx-bpm-test.wav");
        synth_clicks(&p, 20.0, 120.0, 0.0);
        let evs = run_analysis(&p, 0, Duration::from_secs(120));
        let (bpm, _, confidence) = track_analysis_of(&evs);
        assert!(
            (bpm - 120.0).abs() <= 0.3,
            "BPM 应 ≈120（实得 {bpm}，置信 {confidence}）"
        );
        // tempo_segments 透传：合成恒定拍距 → 首段 bpm ≈ 检测 BPM、刚性 ≈1
        let segs = evs
            .iter()
            .find_map(|e| match e {
                AnalysisEvent::TrackAnalysis { tempo_segments, .. } => Some(tempo_segments),
                _ => None,
            })
            .expect("应有 tempo_segments");
        assert!(!segs.is_empty(), "恒定拍距应产生至少一个分段");
        assert!(
            (segs[0].1 - bpm).abs() <= 0.5,
            "首段 bpm 应 ≈ 检测 bpm：{} vs {bpm}",
            segs[0].1
        );
        assert!(
            segs[0].2 > 0.99,
            "恒定拍距段刚性应 ≈1：{}",
            segs[0].2
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn track_analysis_detects_key_c_major() {
        let p = std::env::temp_dir().join("hypermixx-key-test.wav");
        synth_chords(&p, 2.0, 2); // 16s C-F-G-C
        let evs = run_analysis(&p, 0, Duration::from_secs(120));
        let key = evs
            .iter()
            .find_map(|e| match e {
                AnalysisEvent::TrackAnalysis { key, .. } => Some(*key),
                _ => None,
            })
            .expect("应有 TrackAnalysis 事件")
            .expect("应有调性估计");
        assert_eq!(key.root, 0, "根音应为 C（实得 {}）", key.name());
        assert_eq!(
            key.mode,
            KeyMode::Major,
            "调式应为大调（实得 {}）",
            key.name()
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn grid_offset_matches_click_phase() {
        let p = std::env::temp_dir().join("hypermixx-offset-test.wav");
        synth_clicks(&p, 20.0, 120.0, 1.0); // 首拍在 1.0s
        let evs = run_analysis(&p, 0, Duration::from_secs(120));
        let (bpm, offset, _) = track_analysis_of(&evs);
        // 检测器把网格外推回曲首（offset 可能落在首拍之前一拍），
        // 验收不变量是"点击落在网格上"：offset + k·period ≈ 1.0。
        let period = 60.0 / bpm;
        let k = ((1.0 - offset) / period).round();
        let on_grid = 1.0 - (offset + k * period);
        assert!(
            on_grid.abs() <= 0.01,
            "点击应落在网格上（实得 {on_grid:+.3}s，offset={offset:.3}，bpm={bpm:.2}）"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn generation_race_discards_stale() {
        let p = std::env::temp_dir().join("hypermixx-race-test.wav");
        synth_wav(&p, 40.0); // 3 段
        // 旧代际启动后立刻被 shutdown；新代际并发跑完。
        let (tx1, rx1) = mpsc::channel();
        let shut1 = Arc::new(AtomicBool::new(false));
        start_analysis(
            p.clone(),
            Arc::new(AtomicU64::new(0)),
            shut1.clone(),
            1,
            tx1,
        );
        shut1.store(true, Ordering::Relaxed);
        let (tx2, rx2) = mpsc::channel();
        start_analysis(
            p.clone(),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            2,
            tx2,
        );
        // 新代际必须完整发出 TrackAnalysis + Done
        let mut got_ta = false;
        loop {
            let ev = rx2
                .recv_timeout(Duration::from_secs(120))
                .expect("gen2 事件超时");
            if matches!(ev, AnalysisEvent::TrackAnalysis { generation: 2, .. }) {
                got_ta = true;
            }
            if matches!(
                ev,
                AnalysisEvent::Done { .. } | AnalysisEvent::Failed { .. }
            ) {
                break;
            }
        }
        assert!(got_ta, "gen2 应发出 TrackAnalysis");
        // 旧代际：最多发完当前段，绝不发出终态事件，通道最终关闭（线程退出）
        while let Ok(ev) = rx1.recv_timeout(Duration::from_secs(120)) {
            assert!(
                !matches!(
                    ev,
                    AnalysisEvent::Done { .. }
                        | AnalysisEvent::TrackAnalysis { .. }
                        | AnalysisEvent::Failed { .. }
                ),
                "被 shutdown 的旧代际不得发出终态事件"
            );
        }
        let _ = std::fs::remove_file(&p);
    }
}
