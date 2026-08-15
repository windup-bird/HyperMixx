//! fx_spike：FX rack 逐块 CPU/内存基准（M3 P6 验收依据）。
//!
//! 四臂（同一次解码后依次跑，各臂独立新 rack）：
//! - `const`    空 rack（skip 直通基线）
//! - `reverb`  单槽 reverb（最重单效果）
//! - `all4`    4 槽全开：moog + echo + reverb + distortion
//! - `torture` 4 槽 + 每块参数扫掠（cutoff/time/roomsize/duty 正弦折磨）
//!   每 8192 块槽 1 换型 echo↔flanger↔phaser（含音频线程分配点）
//!
//! 用法: fx_spike <音频|--sine> [const|reverb|all4|torture|all]
//!   默认 all。--sine 用 12s 440Hz 正弦替代音频文件。
//! 输出每臂 CPU 比 (p50/p99/p99.9/max)、RTF、NaN、|out|max、RSS 增量。
//! 验收判定（release 构建，本机 PC）：reverb p99≤0.30、4 槽 p99≤0.60、
//! RTF<1、NaN=0、|out|<4（torture 臂只验有界 <20）、RSS 增量 ≤8MB。
//! debug 构建只报告不判定。
//!
//! 例: fx_spike "yehno - Always.flac"
//!     fx_spike --sine reverb

use std::time::Instant;

use hypermixx_audio::decode::{To48k, TrackDecoder};
use hypermixx_audio::fx::{EffectId, FxContext, FxRack};

const SR: u32 = 48_000;
const BLOCK: usize = 256;
const BLOCK_SECS: f64 = BLOCK as f64 / SR as f64;
/// 合成网格（engine_sim 不跑分析，beat 上下文用固定 BPM）。
const GRID_BPM: f64 = 130.0;
/// torture 臂换型周期（块数；≈43.7s）。
const TYPE_CYCLE_BLOCKS: usize = 8192;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    Const,
    Reverb,
    All4,
    Torture,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::Const => "const 空 rack",
            Arm::Reverb => "reverb 单槽",
            Arm::All4 => "all4 四槽",
            Arm::Torture => "torture 折磨",
        }
    }
}

struct Stats {
    arm: Arm,
    /// 每块 CPU 比（耗时/块时长，1.0 = 实时）。
    ratios: Vec<f64>,
    nans: usize,
    peak: f32,
    rss_delta_kb: u64,
    wall: f64,
    blocks: usize,
}

/// 整曲解码到 48k 交织立体声 f32（喂入模拟 chunk ring 的源缓冲）。
fn decode_track(path: &str) -> Result<Vec<f32>, String> {
    let mut dec =
        TrackDecoder::open(std::path::Path::new(path)).map_err(|e| format!("打开失败: {e:#}"))?;
    let mut rs = To48k::new(dec.sample_rate, SR).map_err(|e| format!("重采样器构建失败: {e:#}"))?;
    let mut buf = Vec::new();
    while let Some(input) = dec.decode_next().map_err(|e| format!("解码失败: {e:#}"))? {
        buf.extend_from_slice(
            &rs.process(&input)
                .map_err(|e| format!("重采样失败: {e:#}"))?,
        );
    }
    buf.extend_from_slice(&rs.flush().map_err(|e| format!("冲刷失败: {e:#}"))?);
    Ok(buf)
}

/// 12s 440Hz 立体声正弦。
fn sine_440(seconds: usize) -> Vec<f32> {
    (0..SR as usize * seconds)
        .flat_map(|i| {
            let v =
                (0.7 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / SR as f64).sin()) as f32;
            [v, v]
        })
        .collect()
}

/// 峰值常驻内存（进程级，Linux）。
fn vm_hwm_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

/// 每块 CPU 统计：耗时/回调时长 比值的 (p50, p99, p99.9, max)。
fn percentile_ratios(mut ratios: Vec<f64>) -> (f64, f64, f64, f64) {
    ratios.sort_by(|a, b| a.total_cmp(b));
    let p = |pct: f64| {
        let rank = (pct * (ratios.len() as f64 - 1.0)).round() as usize;
        ratios[rank.min(ratios.len() - 1)]
    };
    (p(0.5), p(0.99), p(0.999), *ratios.last().unwrap_or(&0.0))
}

/// torture 臂每块参数折磨 + 周期性换型（唯一分配点，用户操作频率模拟）。
fn torture_params(rack: &mut FxRack, b: usize, t: f64) {
    let x1 = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * t / 2.3).sin();
    let x2 = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * t / 5.0).sin();
    let x3 = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * t / 3.1).sin();
    // moog：cutoff log 域 20Hz..20kHz 扫 + res 0.2..0.8
    let cutoff = (20.0 * (2.0f64).powf(10.0 * x1)) as f32;
    rack.set_slot_params(0, true, 1.0, [cutoff, 0.2 + 0.6 * x2 as f32, 0.0, 0.0]);
    // echo：time 0.05..1.0s 扫（50ms 平滑 + 分数读滑音路径）
    rack.set_slot_params(1, true, 1.0, [0.05 + 0.95 * x2 as f32, 0.7, 0.4, 0.0]);
    // reverb：roomsize/damp 扫
    rack.set_slot_params(2, true, 1.0, [0.2 + 0.7 * x3 as f32, x2 as f32, 0.7, 0.0]);
    // gate：duty 0.3..0.7 扫（beat 上下文 130BPM）
    rack.set_slot_params(3, true, 1.0, [1.0, 0.3 + 0.4 * x3 as f32, 2.0, 0.0]);
    // 每 TYPE_CYCLE_BLOCKS 块槽 1 换型：echo ↔ flanger ↔ phaser
    if b > 0 && b.is_multiple_of(TYPE_CYCLE_BLOCKS) {
        let cycle = (b / TYPE_CYCLE_BLOCKS) % 3;
        let (id, p) = match cycle {
            0 => (EffectId::Flanger, [0.5, 2.0, 3.0, 0.4]),
            _ => (EffectId::Phaser, [0.5, 800.0, 0.5, 0.3]),
        };
        rack.set_slot_type(1, Some(id));
        rack.set_slot_params(1, true, 1.0, p);
    }
}

fn run_arm(audio: &[f32], arm: Arm) -> Stats {
    let mut rack = FxRack::new(SR as f32);
    match arm {
        Arm::Const => {}
        Arm::Reverb => {
            rack.set_slot_type(0, Some(EffectId::Reverb));
            rack.set_slot_params(0, true, 1.0, [0.5, 0.3, 0.7, 0.0]);
        }
        Arm::All4 => {
            for (slot, (id, p)) in [
                (EffectId::FilterMoog, [2000.0, 0.2, 0.0, 0.0]),
                (EffectId::Echo, [0.375, 0.35, 0.3, 0.0]),
                (EffectId::Reverb, [0.5, 0.3, 0.7, 0.0]),
                (EffectId::Distortion, [12.0, 0.0, 0.0, 0.0]),
            ]
            .into_iter()
            .enumerate()
            {
                rack.set_slot_type(slot, Some(id));
                rack.set_slot_params(slot, true, 1.0, p);
            }
        }
        Arm::Torture => {
            for (slot, id) in [
                EffectId::FilterMoog,
                EffectId::Echo,
                EffectId::Reverb,
                EffectId::Gate,
            ]
            .into_iter()
            .enumerate()
            {
                rack.set_slot_type(slot, Some(id));
            }
        }
    }
    let rss_base = vm_hwm_kb();
    let total_blocks = audio.len() / 2 / BLOCK;
    let period = 60.0 / GRID_BPM;
    let mut ratios = Vec::with_capacity(total_blocks);
    let mut nans = 0usize;
    let mut peak = 0.0f32;
    let mut out = vec![0.0f32; BLOCK * 2];
    let t0 = Instant::now();
    for b in 0..total_blocks {
        out.copy_from_slice(&audio[b * BLOCK * 2..(b + 1) * BLOCK * 2]);
        let t = b as f64 * BLOCK_SECS;
        let ctx = FxContext {
            beats_total: t / period,
            beat_phase_01: ((t / period) % 1.0) as f32,
            beat_period_secs: period,
            grid_valid: true,
        };
        if arm == Arm::Torture {
            torture_params(&mut rack, b, t);
        }
        let start = Instant::now();
        rack.process(&mut out, BLOCK, &ctx);
        ratios.push(start.elapsed().as_secs_f64() / BLOCK_SECS);
        nans += out.iter().filter(|v| !v.is_finite()).count();
        for v in out.iter() {
            peak = peak.max(v.abs());
        }
    }
    Stats {
        arm,
        ratios,
        nans,
        peak,
        rss_delta_kb: vm_hwm_kb().saturating_sub(rss_base),
        wall: t0.elapsed().as_secs_f64(),
        blocks: total_blocks,
    }
}

fn report(s: &Stats) {
    let (p50, p99, p999, max) = percentile_ratios(s.ratios.clone());
    let audio_secs = s.blocks as f64 * BLOCK_SECS;
    println!("== {} ==", s.arm.name());
    println!(
        "CPU 比: p50={p50:.3} p99={p99:.3} p99.9={p999:.3} max={max:.3}（1.0=实时）| RTF: {:.3}（{:.1}s 音频耗时 {:.2}s）",
        s.wall / audio_secs,
        audio_secs,
        s.wall
    );
    println!(
        "NaN: {} | |out|max: {:.2} | RSS 增量: {:.1} MB | 块数: {}",
        s.nans,
        s.peak,
        s.rss_delta_kb as f64 / 1024.0,
        s.blocks
    );
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("用法: fx_spike <音频|--sine> [const|reverb|all4|torture|all]");
        std::process::exit(2);
    }
    let audio = if args[0] == "--sine" {
        sine_440(12)
    } else {
        match decode_track(&args[0]) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    };
    let total_secs = audio.len() as f64 / 2.0 / SR as f64;
    println!("源: {:.1}s @48k 立体声", total_secs);

    let sel = args.get(1).map(|s| s.as_str()).unwrap_or("all");
    let arms: Vec<Arm> = match sel {
        "const" => vec![Arm::Const],
        "reverb" => vec![Arm::Reverb],
        "all4" => vec![Arm::All4],
        "torture" => vec![Arm::Torture],
        "all" => vec![Arm::Const, Arm::Reverb, Arm::All4, Arm::Torture],
        other => {
            eprintln!("未知臂 {other}");
            std::process::exit(2);
        }
    };

    let mut stats = Vec::new();
    for arm in arms {
        let s = run_arm(&audio, arm);
        report(&s);
        stats.push(s);
    }

    if cfg!(debug_assertions) {
        println!("debug 构建：跳过验收判定（阈值以 release 为准）");
        return;
    }
    // 验收判定（本机 PC release 首测基线）
    let find = |arm: Arm| stats.iter().find(|s| s.arm == arm);
    let mut pass = true;
    let mut check = |cond: bool, msg: &str| {
        println!("[{}] {msg}", if cond { "PASS" } else { "FAIL" });
        if !cond {
            pass = false;
        }
    };
    for s in &stats {
        let rt = s.wall / (s.blocks as f64 * BLOCK_SECS);
        check(rt < 1.0, &format!("{} RTF<1（{:.3}）", s.arm.name(), rt));
        check(s.nans == 0, &format!("{} NaN=0（{}）", s.arm.name(), s.nans));
        // torture 臂刻意叠最坏参数（moog res 0.8 共振峰 × echo fb 0.7 环 ×
        // reverb roomsize 0.9 comb 共振）：有界但链增益高，只验证有界性；
        // 常规参数臂按 4.0 headroom 判定
        let out_bound = if s.arm == Arm::Torture { 20.0 } else { 4.0 };
        check(
            s.peak < out_bound,
            &format!(
                "{} |out|<{out_bound}（{:.2}）",
                s.arm.name(),
                s.peak
            ),
        );
        check(
            s.rss_delta_kb <= 8 * 1024,
            &format!(
                "{} RSS 增量 ≤8MB（{:.1} MB）",
                s.arm.name(),
                s.rss_delta_kb as f64 / 1024.0
            ),
        );
    }
    if let Some(r) = find(Arm::Reverb) {
        let p99 = percentile_ratios(r.ratios.clone()).1;
        check(p99 <= 0.30, &format!("reverb p99 ≤0.30 块预算（{p99:.3}）"));
    }
    if let Some(r) = find(Arm::All4) {
        let p99 = percentile_ratios(r.ratios.clone()).1;
        check(p99 <= 0.60, &format!("全 4 槽 p99 ≤0.60 块预算（{p99:.3}）"));
    }
    if !pass {
        std::process::exit(1);
    }
}
