//! keylock spike：真实曲目上变速不变调引擎的基准（M2 P1 定案依据）。
//!
//! 三臂对比：
//! - `timestretch` 实时引擎（Keylock profile，256 帧回调，chunk→demand_hint 喂入）
//! - `signalsmith` 实时引擎（preset_default，输入长度/回调决定瞬时速率）
//! - `batch`       timestretch 离线整曲（MaxQuality，预渲染回退路径参照）
//!   （原"rubato 两级"臂不可行：纯重采样对 (音高,速度) 的作用是对角群，
//!   二者必然耦合，无变速不变调能力——已从候选删除）
//!
//! 用法: keylock_spike <音频|--sine> [engine] [rate] [mode] [out.wav]
//!   engine: timestretch(默认) | signalsmith | batch
//!   rate:   恒定速率倍率（默认 1.08）
//!   mode:   const(默认) | sweep（±8% / 2s 折磨扫掠；batch 不支持）
//!   out.wav: 变速结果输出（试听用，16-bit 48k）
//! 附加 --sine：用 12s 440Hz 正弦替代音频文件（音高保持测量，p95/max cents）
//!
//! 例: keylock_spike "yehno - Always.flac"
//!     keylock_spike --sine timestretch 1.08 const
//!     keylock_spike "yehno - Always.flac" timestretch 1.08 sweep out.wav

use std::time::Instant;

use hypermixx_audio::decode::{To48k, TrackDecoder};
use timestretch::engine::{Engine, EngineConfig, EngineProfile};
use timestretch::{QualityMode, StretchParams};

const SR: u32 = 48_000;
const CALLBACK: usize = 256; // 帧
const CALLBACK_SECS: f64 = CALLBACK as f64 / SR as f64;
/// 音高保持判定线（cent）：计划 P1 决策表对 timestretch 的要求。
const SINE_CENTS_LIMIT: f64 = 10.0;

enum Arm {
    Timestretch,
    Signalsmith,
    Batch,
}

enum Mode {
    Const(f64),
    Sweep, // 1 + 0.08·sin(2π t/2)（上游 torture ride）
}

impl Mode {
    fn rate_at(&self, t: f64) -> f64 {
        match self {
            Mode::Const(r) => *r,
            Mode::Sweep => 1.0 + 0.08 * (2.0 * std::f64::consts::PI * t / 2.0).sin(),
        }
    }
    fn max_rate(&self) -> f64 {
        match self {
            Mode::Const(r) => (*r).max(1.2),
            Mode::Sweep => 1.2, // 上游 WCET 的 Keylock 余量（ride 上限 1.1）
        }
    }
}

struct Config {
    audio: Vec<f32>, // 交织立体声 f32 @48k
    arm: Arm,
    mode: Mode,
    out_wav: Option<String>,
    is_sine: bool,
    wide: bool, // timestretch 臂用 WideKeylock profile（0.25–2.0 全域 keylock）
}

fn parse_args() -> Result<Config, String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let wide = args.iter().position(|a| a == "--wide").is_some();
    args.retain(|a| a != "--wide");
    if args.is_empty() {
        return Err(
            "用法: keylock_spike <音频|--sine> [--wide] [engine] [rate] [mode] [out.wav]".into(),
        );
    }
    let (audio, is_sine, rest): (Vec<f32>, bool, &[String]) = if args[0] == "--sine" {
        (sine_440(12), true, &args[1..])
    } else {
        (decode_track(&args[0])?, false, &args[1..])
    };
    let arm = match rest.first().map(|s| s.as_str()) {
        None | Some("timestretch") => Arm::Timestretch,
        Some("signalsmith") => Arm::Signalsmith,
        Some("batch") => Arm::Batch,
        Some(other) => return Err(format!("未知引擎 {other}")),
    };
    let rate: f64 = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(1.08);
    let mode = match rest.get(2).map(|s| s.as_str()) {
        None | Some("const") => Mode::Const(rate),
        Some("sweep") if matches!(arm, Arm::Batch) => {
            return Err("batch 为整曲离线处理，不支持 sweep".into());
        }
        Some("sweep") => Mode::Sweep,
        Some(other) => return Err(format!("未知模式 {other}")),
    };
    Ok(Config {
        audio,
        arm,
        mode,
        out_wav: rest.get(3).cloned(),
        is_sine,
        wide,
    })
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

/// 12s 440Hz 立体声正弦（音高保持测量源）。
fn sine_440(seconds: usize) -> Vec<f32> {
    (0..SR as usize * seconds)
        .flat_map(|i| {
            let v =
                (0.7 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / SR as f64).sin()) as f32;
            [v, v]
        })
        .collect()
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

/// 正过零插值测频（上游 engine_keylock.rs 算法；干净正弦上亚音分精度）。
fn zero_crossing_freq(window: &[f32], sr: u32) -> Option<f64> {
    let (mut first, mut last, mut count) = (None, None, 0usize);
    for i in 1..window.len() {
        let (a, b) = (window[i - 1] as f64, window[i] as f64);
        if a <= 0.0 && b > 0.0 {
            let t = (i - 1) as f64 + a / (a - b);
            if first.is_none() {
                first = Some(t);
            }
            last = Some(t);
            count += 1;
        }
    }
    match (first, last) {
        (Some(f), Some(l)) if count >= 3 && l > f => Some((count - 1) as f64 * sr as f64 / (l - f)),
        _ => None,
    }
}

/// 音高偏差轨迹（100ms 窗 / 25ms 跳，跳过热身 0.5s），返回 (p95, max, 估计数)。
/// `skip_frames`：排除区间（seek 缝附近相位跳变会污染窗），mono 帧坐标。
fn cents_deviation(
    output: &[f32],
    sr: u32,
    ref_hz: f64,
    skip: Option<(usize, usize)>,
) -> (f64, f64, usize) {
    let win = (0.1 * sr as f64) as usize;
    let hop = (0.025 * sr as f64) as usize;
    let start = (0.5 * sr as f64) as usize;
    let mut deviations = Vec::new();
    let mut pos = start;
    while pos + win <= output.len() / 2 {
        let in_skip = skip.is_some_and(|(a, b)| pos + win > a && pos < b);
        if !in_skip {
            // 只看左声道（output 为交织立体声）
            let mono: Vec<f32> = (0..win).map(|k| output[(pos + k) * 2]).collect();
            if let Some(freq) = zero_crossing_freq(&mono, sr) {
                deviations.push((1_200.0 * (freq / ref_hz).log2()).abs());
            }
        }
        pos += hop;
    }
    deviations.sort_by(|a, b| a.total_cmp(b));
    let n = deviations.len();
    let p95 = if n > 0 {
        deviations[((n - 1) as f64 * 0.95).round() as usize]
    } else {
        0.0
    };
    let max = deviations.last().copied().unwrap_or(0.0);
    (p95, max, n)
}

/// --sine 模式的音高保持判定：打印 (p95, max) 并按判定线给出 PASS/FAIL。
fn check_pitch(output: &[f32], skip: Option<(usize, usize)>, limit: f64) -> bool {
    let (p95, max, n) = cents_deviation(output, SR, 440.0, skip);
    let pass = n >= 50 && p95 <= limit;
    println!(
        "音高保持: p95={p95:.2} cents, max={max:.2} cents（{} 个估计）→ {}（判定线 {limit} cents）",
        n,
        if pass { "PASS" } else { "FAIL" }
    );
    pass
}

fn nan_count(buf: &[f32]) -> usize {
    buf.iter().filter(|v| !v.is_finite()).count()
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

fn write_wav(path: &str, samples: &[f32]) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    for &s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .map_err(|e| e.to_string())?;
    }
    w.finalize().map_err(|e| e.to_string())
}

/// timestretch 实时引擎臂：256 帧回调 + demand_hint 喂入；
/// 中途做一次 reset+warm_start seek（验证排空协议）。
fn run_timestretch(cfg: &Config) -> Result<bool, String> {
    let audio = &cfg.audio;
    let total_frames = audio.len() / 2;
    let profile = if cfg.wide {
        EngineProfile::WideKeylock
    } else {
        EngineProfile::Keylock
    };
    let handles = Engine::build(EngineConfig {
        sample_rate: SR,
        channels: 2,
        profile,
        initial_tempo_rate: cfg.mode.rate_at(0.0),
        max_block_frames: CALLBACK,
        source_capacity_frames: 8192,
        pre_analysis: None,
    })
    .map_err(|e| format!("引擎构建失败: {e:#}"))?;
    let (controller, mut processor, mut source) =
        (handles.controller, handles.processor, handles.source);
    source.set_track_position(0);
    let latency = processor.pipeline_latency_frames();

    let mut cursor = 0usize; // 待喂帧游标
    let mut out = vec![0.0f32; CALLBACK * 2];
    let collect = cfg.out_wav.is_some() || cfg.is_sine;
    let mut output_buf: Vec<f32> = Vec::new();
    let mut ratios = Vec::new();
    let mut nans = 0usize;
    let mut finished = false;
    let mut drained = 0u32;
    let mut last_pos = 0.0f64;
    let seek_at = total_frames / 2;
    let seek_target = total_frames / 4;
    let mut sought = false;
    let mut seek_out_idx: Option<usize> = None; // 输出缓冲中 seek 缝的位置（mono 帧）

    let t0 = Instant::now();
    let mut callbacks = 0u64;
    let trace = std::env::var("SPIKE_TRACE").is_ok();
    loop {
        let t = controller.delivered_frames() as f64 / SR as f64;
        controller.set_tempo_rate(cfg.mode.rate_at(t));

        // 中途 seek 验证：排空 + 预卷 + 重定位（P2 seek 协议的实证）
        if !sought && cursor >= seek_at {
            let preroll = processor.warm_start_preroll_frames();
            let feed_from = seek_target - preroll.min(seek_target);
            processor.reset();
            source.set_track_position(feed_from as u64);
            controller.warm_start(preroll as u32);
            cursor = feed_from;
            sought = true;
            seek_out_idx = Some(output_buf.len() / 2);
        }

        while source.occupied_frames() < source.demand_hint(CALLBACK, cfg.mode.max_rate())
            && cursor < total_frames
        {
            let end = (cursor + 2048).min(total_frames);
            let accepted = source.push(&audio[cursor * 2..end * 2]);
            cursor += accepted;
            if accepted == 0 {
                break;
            }
        }
        if cursor >= total_frames && !finished {
            source.finish();
            finished = true;
        }

        let start = Instant::now();
        processor.process(&mut out);
        let elapsed = start.elapsed().as_secs_f64();
        callbacks += 1;
        ratios.push(elapsed / CALLBACK_SECS);
        nans += nan_count(&out);
        if collect {
            output_buf.extend_from_slice(&out);
        }
        if trace
            && (callbacks <= 40 || (sought && callbacks <= 60) || callbacks.is_multiple_of(1000))
        {
            println!(
                "cb={callbacks:>6} occ={:>5} cursor={:>8} dlv={:>9} und={:>9} pos={:>9.0}",
                source.occupied_frames(),
                cursor,
                controller.delivered_frames(),
                controller.underrun_frames(),
                controller.source_position()
            );
        }

        // 排空判定：finish 后 position 不再推进 8 个回调即管线已空。
        // （欠载静音继续计入 delivered，position 却冻结——不能用 delivered 判停）
        if finished {
            let pos = controller.source_position();
            if pos > last_pos {
                drained = 0;
                last_pos = pos;
            } else {
                drained += 1;
            }
            if drained >= 8 {
                break;
            }
        }
        if callbacks > total_frames as u64 / CALLBACK as u64 * 2 + 4096 {
            break; // 硬上限（rate<1 时输出长于输入）
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    let audio_secs = total_frames as f64 / SR as f64;
    let (p50, p99, p999, max) = percentile_ratios(ratios);

    println!(
        "== timestretch 实时引擎（{} profile）==",
        if cfg.wide { "WideKeylock" } else { "Keylock" }
    );
    println!(
        "延迟: {latency} 帧 = {:.1} ms",
        latency as f64 / SR as f64 * 1000.0
    );
    println!(
        "回调: {callbacks} × 256 帧；CPU 比: p50={p50:.3} p99={p99:.3} p99.9={p999:.3} max={max:.3}（1.0=实时）"
    );
    println!(
        "RTF: {:.3}（{:.1}s 音频处理耗时 {:.2}s）",
        wall / audio_secs,
        audio_secs,
        wall
    );
    println!(
        "欠载: {} 帧（≈EOF 排空尾 {}×256 + 尾块；内容期应为 0）| NaN: {nans} | 中途 seek: {}",
        controller.underrun_frames(),
        drained,
        if sought {
            "已执行（无异常）"
        } else {
            "跳过"
        }
    );
    let sp = controller.source_position();
    // seek 后 position 是 reset 后喂入坐标（P2 的 feed_base + source_position）
    let pos_target = if sought {
        total_frames - seek_target
    } else {
        total_frames
    };
    println!(
        "输出: {} 帧 | EOF 收敛 source_position={sp:.0}/{pos_target}（{}{}）",
        controller.delivered_frames(),
        if sought {
            "post-seek 坐标"
        } else {
            "整曲坐标"
        },
        if sp >= pos_target as f64 - 1024.0 {
            "✓"
        } else {
            "?"
        }
    );

    let mut pitch_pass = true;
    if cfg.is_sine {
        // seek 缝前后各留 50/100ms（相位跳变 + declick 斜坡污染窗，非引擎误差）
        let skip = seek_out_idx.map(|i| (i.saturating_sub(2_400), i + 4_800));
        pitch_pass = check_pitch(&output_buf, skip, SINE_CENTS_LIMIT);
    }
    if let Some(p) = &cfg.out_wav {
        write_wav(p, &output_buf)?;
        println!("已写 {p}");
    }
    Ok(pitch_pass)
}

/// signalsmith 实时臂：每次 process 的输入长度 = 256/rate（长度比即瞬时速率）。
fn run_signalsmith(cfg: &Config) -> Result<bool, String> {
    let audio = &cfg.audio;
    let total_frames = audio.len() / 2;
    let mut st = signalsmith_stretch::Stretch::preset_default(2, SR);
    let latency = st.input_latency() + st.output_latency();

    let mut cursor = 0usize;
    let mut out = vec![0.0f32; CALLBACK * 2];
    let collect = cfg.out_wav.is_some() || cfg.is_sine;
    let mut output_buf: Vec<f32> = Vec::new();
    let mut ratios = Vec::new();
    let mut nans = 0usize;
    let mut callbacks = 0u64;

    let t0 = Instant::now();
    loop {
        let t = cursor as f64 / SR as f64;
        let r = cfg.mode.rate_at(t);
        let in_frames = ((CALLBACK as f64 / r).round() as usize).max(1);
        let end = (cursor + in_frames).min(total_frames);
        if end == cursor {
            break;
        }
        let start = Instant::now();
        st.process(&audio[cursor * 2..end * 2], &mut out);
        let elapsed = start.elapsed().as_secs_f64();
        callbacks += 1;
        ratios.push(elapsed / CALLBACK_SECS);
        nans += nan_count(&out);
        cursor = end;
        if collect {
            output_buf.extend_from_slice(&out);
        }
        if callbacks > total_frames as u64 / CALLBACK as u64 * 2 + 4096 {
            break;
        }
    }
    // 尾部冲刷（output_latency 长度）
    let mut fl = vec![0.0f32; st.output_latency() * 2];
    st.flush(&mut fl);
    if collect {
        output_buf.extend_from_slice(&fl);
    }
    let wall = t0.elapsed().as_secs_f64();
    let audio_secs = total_frames as f64 / SR as f64;
    let (p50, p99, p999, max) = percentile_ratios(ratios);

    println!("== signalsmith 实时（preset_default）==");
    println!(
        "延迟: {latency} 帧 = {:.1} ms（input {} + output {}）",
        latency as f64 / SR as f64 * 1000.0,
        st.input_latency(),
        st.output_latency()
    );
    println!("回调: {callbacks}；CPU 比: p50={p50:.3} p99={p99:.3} p99.9={p999:.3} max={max:.3}");
    println!(
        "RTF: {:.3}（处理耗时 {:.2}s） | NaN: {nans}",
        wall / audio_secs,
        wall
    );

    let mut pitch_pass = true;
    if cfg.is_sine {
        pitch_pass = check_pitch(&output_buf, None, SINE_CENTS_LIMIT);
    }
    if let Some(p) = &cfg.out_wav {
        write_wav(p, &output_buf)?;
        println!("已写 {p}");
    }
    Ok(pitch_pass)
}

/// 离线整曲臂：timestretch::stretch（MaxQuality，预渲染回退路径的参照）。
/// 注意 stretch 的比率语义与实时引擎相反：输出 = 输入 × ratio，
/// 故"1.08 倍加速"传 1/rate。
fn run_batch(cfg: &Config) -> Result<bool, String> {
    let Mode::Const(rate) = cfg.mode else {
        return Err("batch 不支持 sweep".into());
    };
    let params = StretchParams::new(1.0 / rate)
        .with_sample_rate(SR)
        .with_channels(2)
        .with_quality_mode(QualityMode::MaxQuality);
    let t0 = Instant::now();
    let output =
        timestretch::stretch(&cfg.audio, &params).map_err(|e| format!("stretch 失败: {e:#}"))?;
    let wall = t0.elapsed().as_secs_f64();
    let audio_secs = cfg.audio.len() as f64 / 2.0 / SR as f64;

    println!("== timestretch 离线整曲（MaxQuality）==");
    println!(
        "RTF: {:.3}（{:.1}s 音频耗时 {:.2}s） | NaN: {} | 输出 {} 帧（期望 ≈{}）",
        wall / audio_secs,
        audio_secs,
        wall,
        nan_count(&output),
        output.len() / 2,
        (cfg.audio.len() as f64 / 2.0 / rate) as u64
    );

    let mut pitch_pass = true;
    if cfg.is_sine {
        pitch_pass = check_pitch(&output, None, SINE_CENTS_LIMIT);
    }
    if let Some(p) = &cfg.out_wav {
        write_wav(p, &output)?;
        println!("已写 {p}");
    }
    Ok(pitch_pass)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cfg = match parse_args() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let total_frames = cfg.audio.len() / 2;
    println!(
        "源: {} 帧 = {:.1}s @48k 立体声 | 速率: {}{}",
        total_frames,
        total_frames as f64 / SR as f64,
        match &cfg.mode {
            Mode::Const(r) => format!("{r}"),
            Mode::Sweep => "±8% 扫掠".into(),
        },
        if cfg.is_sine { " | 440Hz 正弦" } else { "" }
    );

    let pitch_pass = match cfg.arm {
        Arm::Timestretch => run_timestretch(&cfg),
        Arm::Signalsmith => run_signalsmith(&cfg),
        Arm::Batch => run_batch(&cfg),
    };
    match pitch_pass {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("音高保持判定 FAIL");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("运行失败: {e}");
            std::process::exit(1);
        }
    }
    println!("峰值常驻内存: {} MB", vm_hwm_kb() as f64 / 1024.0);
}
