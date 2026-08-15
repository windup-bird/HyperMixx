//! 引擎级集成模拟：真实引擎核心（双 deck + ops 队列 + master 混音）。
//! 与 deck 级 playback_sim 的区别：覆盖 EngineState 的 ops 应用、双 deck 混音、
//! 以及一个 33ms 轮询线程模拟 UI 的 poll_controls（速率扫掠/音量/EQ 写入）。
//!
//! 用法: engine_sim <音频路径> [mode] [seek_sec]
//!   mode: fake10  = 定时器驱动 10× 加速（默认，~40s 跑完）
//!         real    = 真实 cpal 声卡时钟（真实时间，~5.5 分钟；master 静音不打扰）
//!         sync    = 双 deck 同步：deck0 = leader、deck1 = follower（sync 开），
//!                   额外参数 <音频2> <leader_bpm> <follower_bpm>
//!         keylock = UI 模拟线程周期开关 keylock + pitch ±12 三角扫掠
//!                   （反复跨越 ±3 半音 profile 重建阈值）
//!         fx      = 60s FX 折磨：moog+echo 常开，UI 线程 100ms 扫 cutoff、
//!                   2s 翻 echo enable、3s 扫干湿、5s 换型 echo↔flanger
//!   seek_sec: 在 t=10s 时跳转到该秒数（fake10/real/keylock）
//! 退出码 1 = 提前停止或播放头停住

use std::time::{Duration, Instant};

use hypermixx_audio::{Engine, EngineHandle};
use hypermixx_core::{ControlBus, paths};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("用法: engine_sim <音频路径> [fake10|real|keylock|fx] [seek_sec]");
        eprintln!("      engine_sim <音频1> sync <音频2> <leader_bpm> <follower_bpm>");
        std::process::exit(2);
    }
    let path = &args[0];
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("fake10");
    let seek_sec: Option<f64> = args.get(2).and_then(|s| s.parse().ok());

    let bus = ControlBus::default();
    // master 静音（模拟过程中不打扰）；deck 音量 1.0 保证 deck VU 有意义
    bus.set(paths::master_volume(), 0.0);
    bus.set(&paths::deck_volume(0), 1.0);
    bus.set(&paths::deck_volume(1), 1.0);

    match mode {
        "real" => run_real(&bus, path, seek_sec),
        "keylock" => run_keylock_mode(&bus, path, seek_sec),
        "fx" => run_fx_mode(&bus, path),
        "sync" => run_sync_mode(
            &bus,
            path,
            args.get(2).expect("sync 模式需要 <音频2>"),
            args.get(3)
                .and_then(|s| s.parse().ok())
                .expect("sync 模式需要 <leader_bpm>"),
            args.get(4)
                .and_then(|s| s.parse().ok())
                .expect("sync 模式需要 <follower_bpm>"),
        ),
        _ => run_fake10(&bus, path, seek_sec),
    }
}

/// 双 deck 同步模式（10× 定时器驱动）：deck0 = leader、deck1 = follower。
/// 验证：follower 速率锁定（deck1 显示 BPM → leader_bpm）、双播头持续推进、
/// follower 滑杆（−8%）被 sync 覆盖。
fn run_sync_mode(
    bus: &ControlBus,
    path: &str,
    path2: &str,
    leader_bpm: f64,
    follower_bpm: f64,
) -> anyhow::Result<()> {
    let (mut state, handle) = Engine::core(bus);
    handle.load(0, std::path::PathBuf::from(path));
    handle.load(1, std::path::PathBuf::from(path2));
    // 合成网格（engine_sim 不跑分析，BPM 由参数指定）
    bus.set(&paths::deck_grid_bpm(0), leader_bpm);
    bus.set(&paths::deck_grid_offset(0), 0.0);
    bus.set(&paths::deck_grid_bpm(1), follower_bpm);
    bus.set(&paths::deck_grid_offset(1), 0.0);
    bus.set(&paths::deck_sync(1), 1.0);
    bus.set(&paths::deck_rate(1), -8.0); // 滑杆应被 sync 覆盖

    let sim_bus = bus.clone();
    std::thread::spawn(move || {
        let mut out = vec![0.0f32; Engine::BLOCK_FRAMES * 2];
        loop {
            for _ in 0..(48000 / Engine::BLOCK_FRAMES) {
                state.process(&mut out);
                std::thread::sleep(Duration::from_micros(533));
            }
            if sim_bus.get(&paths::deck_play(0)) < 0.5
                && sim_bus.get(&paths::deck_playhead(0)) > 1.0
            {
                break;
            }
        }
    });

    let head0 = bus.control(&paths::deck_playhead(0));
    let head1 = bus.control(&paths::deck_playhead(1));
    let bpm1 = bus.control(&paths::deck_bpm(1));
    let dur1 = bus.control(&paths::deck_duration(1));
    let mut last0 = 0.0f64;
    let mut last1 = 0.0f64;
    let mut stalled0 = 0u32;
    let mut stalled1 = 0u32;
    let mut problem = false;
    let t0 = Instant::now();
    for sec in 0..2400u64 {
        std::thread::sleep(Duration::from_secs(1));
        let h0 = head0.get();
        let h1 = head1.get();
        let b = bpm1.get();
        println!(
            "sync t={sec:>4}s leader={h0:>8.3}s follower={h1:>8.3}s bpm1={b:>6.2}（目标 {leader_bpm:.1}）dur1={:.1}",
            dur1.get()
        );
        // follower 停止（EOF）后 bpm 显示回落滑杆，不再校验
        if sec >= 10 && bus.get(&paths::deck_play(1)) > 0.5 && (b - leader_bpm).abs() > 0.5 {
            println!("!!! follower BPM 未锁定：{b:.2} ≠ {leader_bpm:.2}");
            problem = true;
            break;
        }
        if h0 > 1.0 {
            if (h0 - last0).abs() < 0.01 {
                stalled0 += 1;
            } else {
                stalled0 = 0;
            }
        }
        if h1 > 1.0 {
            if (h1 - last1).abs() < 0.01 {
                stalled1 += 1;
            } else {
                stalled1 = 0;
            }
        }
        if stalled0 >= 3 || stalled1 >= 3 {
            println!("!!! 播放头停住 leader={h0:.3}s follower={h1:.3}s");
            problem = true;
            break;
        }
        if bus.get(&paths::deck_play(0)) < 0.5 && h0 > 1.0 {
            println!("leader 播完 @ {h0:.3}s；follower @ {h1:.3}s");
            break;
        }
        last0 = h0;
        last1 = h1;
    }
    println!("墙钟 {:.2}s", t0.elapsed().as_secs_f64());
    if problem {
        std::process::exit(1);
    }
    Ok(())
}

/// keylock 模式（10× 定时器驱动）：UI 模拟线程每 5s 翻转 keylock、
/// 每 2s pitch 步进 ±2（±12 三角扫掠，反复跨越 ±3 半音 profile 重建阈值）。
/// 验证：重建轰炸不冻结播放头；BPM 显示与 pitch 无关（grid × rate，
/// keylock 开关两态均成立）。
fn run_keylock_mode(bus: &ControlBus, path: &str, seek_sec: Option<f64>) -> anyhow::Result<()> {
    let (mut state, handle) = Engine::core(bus);
    handle.load(0, std::path::PathBuf::from(path));
    // 合成网格（不跑分析）：BPM 显示不变性校验基准
    bus.set(&paths::deck_grid_bpm(0), 130.0);
    bus.set(&paths::deck_grid_offset(0), 0.0);

    let sim_bus = bus.clone();
    std::thread::spawn(move || {
        let mut out = vec![0.0f32; Engine::BLOCK_FRAMES * 2];
        loop {
            for _ in 0..(48000 / Engine::BLOCK_FRAMES) {
                state.process(&mut out);
                std::thread::sleep(Duration::from_micros(533));
            }
            if sim_bus.get(&paths::deck_play(0)) < 0.5
                && sim_bus.get(&paths::deck_playhead(0)) > 1.0
            {
                break;
            }
        }
    });

    // UI 模拟线程：keylock 每 5s 翻转、pitch 每 2s 步进（48s 三角周期）
    const TRI: [f64; 24] = [
        0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 10.0, 8.0, 6.0, 4.0, 2.0, 0.0, -2.0, -4.0, -6.0,
        -8.0, -10.0, -12.0, -10.0, -8.0, -6.0, -4.0, -2.0,
    ];
    let ui_bus = bus.clone();
    let seek_done = seek_sec.is_some();
    std::thread::Builder::new()
        .name("ui-sim".into())
        .spawn(move || {
            let mut tick = 0u64;
            let mut keylock = true;
            loop {
                if tick > 0 && tick.is_multiple_of(150) {
                    keylock = !keylock;
                    ui_bus.set(&paths::deck_keylock(0), if keylock { 1.0 } else { 0.0 });
                }
                if tick > 0 && tick.is_multiple_of(60) {
                    let st = TRI[((tick / 60) % 24) as usize];
                    ui_bus.set(&paths::deck_pitch(0), st);
                }
                // 恒写（poll_controls 模式；值未变时 set 无操作）
                ui_bus.set(&paths::deck_rate(0), 0.0);
                ui_bus.set(&paths::deck_volume(0), 1.0);
                ui_bus.set(paths::master_volume(), 0.0);
                std::thread::sleep(Duration::from_millis(33));
                tick += 1;
            }
        })?;

    // 监视：每秒读 deck0 状态
    let head = bus.control(&paths::deck_playhead(0));
    let bpm = bus.control(&paths::deck_bpm(0));
    let pitch = bus.control(&paths::deck_pitch(0));
    let dur = bus.control(&paths::deck_duration(0));
    let mut last_head = 0.0f64;
    let mut stalled = 0u32;
    let mut problem = false;
    let t0 = Instant::now();
    for sec in 0..2400u64 {
        if let Some(target) = seek_sec
            && seek_done
            && sec == 10
        {
            println!("--- 执行 seek → {target}s ---");
            handle.seek(0, target);
        }
        std::thread::sleep(Duration::from_secs(1));
        let h = head.get();
        let playing = bus.get(&paths::deck_play(0)) > 0.5;
        println!(
            "kl t={sec:>4}s head={h:>8.3}s play={playing} pitch={:+.0}st keylock={} bpm={:>6.2}（目标 130.0）dur={:.1}s",
            pitch.get(),
            bus.get(&paths::deck_keylock(0)) > 0.5,
            bpm.get(),
            dur.get()
        );
        // BPM 显示不变性：无论 keylock 开关 / pitch 大小，显示 = grid × rate
        if sec >= 10 && playing && (bpm.get() - 130.0).abs() > 0.5 {
            println!("!!! BPM 显示被 pitch 污染：{} ≠ 130.0", bpm.get());
            problem = true;
            break;
        }
        if playing && h > 1.0 {
            if (h - last_head).abs() < 0.01 {
                stalled += 1;
                if stalled >= 3 {
                    println!("!!! 播放头停住 @ {h:.3}s（连续 {stalled}s 不动）");
                    problem = true;
                    break;
                }
            } else {
                stalled = 0;
            }
        }
        if !playing && h > 1.0 {
            println!("正常播完 @ {h:.3}s / 曲长 {:.1}s", dur.get());
            break;
        }
        last_head = h;
    }
    println!("墙钟 {:.2}s", t0.elapsed().as_secs_f64());
    if problem {
        std::process::exit(1);
    }
    Ok(())
}

/// fx 模式（10× 定时器驱动）：slot0 moog + slot1 echo 常开，跑 60s 内容。
/// UI 模拟线程折磨：100ms 扫 moog cutoff（log 域 20Hz..20kHz）、2s 翻
/// echo enable、3s 扫干湿、5s 换型 echo↔flanger（含音频线程分配点）。
/// 验证：FX 轰炸下播放头持续推进、无提前停止、无 NaN（VU 恒为有限值）。
fn run_fx_mode(bus: &ControlBus, path: &str) -> anyhow::Result<()> {
    let (mut state, handle) = Engine::core(bus);
    handle.load(0, std::path::PathBuf::from(path));
    // 合成网格（不跑分析）：gate/sync 类效果与 BPM 显示用
    bus.set(&paths::deck_grid_bpm(0), 130.0);
    bus.set(&paths::deck_grid_offset(0), 0.0);
    bus.set(&paths::deck_fx_type(0, 0), 7.0); // moog
    bus.set(&paths::deck_fx_type(0, 1), 1.0); // echo
    bus.set(&paths::deck_fx_drywet(0, 0), 1.0);
    bus.set(&paths::deck_fx_drywet(0, 1), 1.0);

    let sim_bus = bus.clone();
    std::thread::spawn(move || {
        let mut out = vec![0.0f32; Engine::BLOCK_FRAMES * 2];
        loop {
            for _ in 0..(48000 / Engine::BLOCK_FRAMES) {
                state.process(&mut out);
                std::thread::sleep(Duration::from_micros(533));
            }
            if sim_bus.get(&paths::deck_play(0)) < 0.5
                && sim_bus.get(&paths::deck_playhead(0)) > 1.0
            {
                break;
            }
        }
    });

    // UI 模拟线程：FX 折磨（33ms 轮询，复刻 poll_controls 写入模式）
    let ui_bus = bus.clone();
    std::thread::Builder::new()
        .name("ui-sim".into())
        .spawn(move || {
            let mut tick = 0u64;
            loop {
                // 100ms 扫 moog cutoff（log 域三角：20Hz ↔ 20kHz）
                if tick.is_multiple_of(3) {
                    let t = tick as f64 * 0.033;
                    let x = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * t / 1.7).sin();
                    ui_bus.set(&paths::deck_fx_p(0, 0, 0), 20.0 * 1000.0f64.powf(x));
                }
                // 2s 翻 echo enable（旁路冻结往返）
                if tick > 0 && tick.is_multiple_of(60) {
                    let v = if (tick / 60) % 2 == 1 { 0.0 } else { 1.0 };
                    ui_bus.set(&paths::deck_fx_enable(0, 1), v);
                }
                // 3s 扫干湿（echo 槽）
                if tick.is_multiple_of(30) {
                    let t = tick as f64 * 0.033;
                    let x = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * t / 3.0).sin();
                    ui_bus.set(&paths::deck_fx_drywet(0, 1), 0.2 + 0.8 * x);
                }
                // 5s 换型 echo↔flanger（音频线程分配点）
                if tick > 0 && tick.is_multiple_of(150) {
                    let v = if (tick / 150) % 2 == 1 { 2.0 } else { 1.0 };
                    ui_bus.set(&paths::deck_fx_type(0, 1), v);
                }
                std::thread::sleep(Duration::from_millis(33));
                tick += 1;
            }
        })?;

    // 监视：每秒读 deck0 状态；60s 内容即完成（10× → ≈6.5s 墙钟）
    let head = bus.control(&paths::deck_playhead(0));
    let vu = bus.control(&paths::deck_vu(0));
    let mut last_head = 0.0f64;
    let mut stalled = 0u32;
    let mut problem = false;
    let t0 = Instant::now();
    for sec in 0..240u64 {
        std::thread::sleep(Duration::from_secs(1));
        let h = head.get();
        let playing = bus.get(&paths::deck_play(0)) > 0.5;
        println!(
            "fx t={sec:>4}s head={h:>8.3}s play={playing} vu={:.3} cutoff={:>7.1}Hz enable1={} type1={} drywet1={:.2}",
            vu.get(),
            bus.get(&paths::deck_fx_p(0, 0, 0)),
            bus.get(&paths::deck_fx_enable(0, 1)),
            bus.get(&paths::deck_fx_type(0, 1)),
            bus.get(&paths::deck_fx_drywet(0, 1)),
        );
        if !vu.get().is_finite() {
            println!("!!! VU 非有限值（NaN/Inf 泄漏）");
            problem = true;
            break;
        }
        if playing && h > 1.0 {
            if (h - last_head).abs() < 0.01 {
                stalled += 1;
                if stalled >= 3 {
                    println!("!!! 播放头停住 @ {h:.3}s（连续 {stalled}s 不动）");
                    problem = true;
                    break;
                }
            } else {
                stalled = 0;
            }
        }
        if !playing && h < 59.0 {
            println!("!!! 提前停止 @ {h:.3}s");
            problem = true;
            break;
        }
        if h >= 60.0 {
            println!("60s FX 折磨完成 @ {h:.3}s");
            break;
        }
        last_head = h;
    }
    println!("墙钟 {:.2}s", t0.elapsed().as_secs_f64());
    if problem {
        std::process::exit(1);
    }
    Ok(())
}

/// 真实声卡时钟：Engine::start 的内部回调由 cpal 驱动。
fn run_real(bus: &ControlBus, path: &str, seek_sec: Option<f64>) -> anyhow::Result<()> {
    let backend = hypermixx_audio::CpalBackend::new()?;
    let engine = Engine::start(&backend, bus, None)?;
    engine.handle.load(0, std::path::PathBuf::from(path));
    // UI 模拟线程 + 监视循环共用
    run_ui_sim_and_monitor(bus, &engine.handle, seek_sec, 48000u32)
}

/// 定时器驱动引擎核心（10× 加速真实时间）。
fn run_fake10(bus: &ControlBus, path: &str, seek_sec: Option<f64>) -> anyhow::Result<()> {
    let (mut state, handle) = Engine::core(bus);
    handle.load(0, std::path::PathBuf::from(path));
    let sim_bus = bus.clone();
    let sim_handle = handle.clone();
    std::thread::spawn(move || {
        // 10×：48000 帧/sim 秒，256 帧/块 → 每块睡 533µs
        let mut out = vec![0.0f32; Engine::BLOCK_FRAMES * 2];
        let bus = sim_bus;
        loop {
            for _ in 0..(48000 / Engine::BLOCK_FRAMES) {
                state.process(&mut out);
                std::thread::sleep(Duration::from_micros(533));
            }
            // 每秒结束前检查是否已停止
            if bus.get(&paths::deck_play(0)) < 0.5 && bus.get(&paths::deck_playhead(0)) > 1.0 {
                break;
            }
        }
    });
    run_ui_sim_and_monitor(bus, &sim_handle, seek_sec, 48000u32)
}

/// UI 模拟线程（33ms 轮询，速率 ±8 扫掠 + 音量/EQ 写入）+ 监视主循环。
fn run_ui_sim_and_monitor(
    bus: &ControlBus,
    handle: &EngineHandle,
    seek_sec: Option<f64>,
    _sr: u32,
) -> anyhow::Result<()> {
    // UI 模拟线程：复刻 poll_controls 的写入模式
    let ui_bus = bus.clone();
    let seek_done = seek_sec.is_some();
    std::thread::Builder::new()
        .name("ui-sim".into())
        .spawn(move || {
            let mut tick = 0u64;
            loop {
                // 速率扫掠：每 2s 在 ±8 之间跳变（模拟快速拖拽滑杆）
                if tick > 0 && tick.is_multiple_of(60) {
                    let v = if (tick / 60) % 2 == 1 { -8.0 } else { 8.0 };
                    ui_bus.set(&paths::deck_rate(0), v);
                }
                // 音量/EQ/master 恒写（poll_controls 模式；值未变时 set 无操作）
                ui_bus.set(&paths::deck_eq_low(0), 0.0);
                ui_bus.set(&paths::deck_eq_mid(0), 0.0);
                ui_bus.set(&paths::deck_eq_high(0), 0.0);
                ui_bus.set(&paths::deck_volume(0), 1.0);
                ui_bus.set(paths::master_volume(), 0.0);
                std::thread::sleep(Duration::from_millis(33));
                tick += 1;
            }
        })?;

    // 监视：每秒读 deck1 状态
    let playhead = bus.control(&paths::deck_playhead(0));
    let play = bus.control(&paths::deck_play(0));
    let dur = bus.control(&paths::deck_duration(0));
    let vu = bus.control(&paths::deck_vu(0));
    let mut last_head = 0.0f64;
    let mut stalled = 0u32;
    let mut problem = false;
    let t0 = Instant::now();
    for sec in 0..2400u64 {
        if let Some(target) = seek_sec
            && seek_done
            && sec == 10
        {
            println!("--- 执行 seek → {target}s ---");
            handle.seek(0, target);
        }
        std::thread::sleep(Duration::from_secs(1));
        let head = playhead.get();
        let playing = play.get() > 0.5;
        let d = dur.get();
        println!(
            "eng t={sec:>4}s head={head:>8.3}s vu={:.3} play={playing} dur={d:.1}s",
            vu.get()
        );
        if !playing && d > 1.0 && head < d - 1.0 {
            println!(
                "!!! 提前停止 @ {head:.3}s（曲长 {d:.1}s，剩 {:.1}s 未播）",
                d - head
            );
            problem = true;
            break;
        }
        if playing && head > 1.0 {
            if (head - last_head).abs() < 0.01 {
                stalled += 1;
                if stalled >= 3 {
                    println!("!!! 播放头停住 @ {head:.3}s（连续 {stalled}s 不动）");
                    problem = true;
                    break;
                }
            } else {
                stalled = 0;
            }
        }
        if d > 1.0 && !playing && head >= d - 1.0 {
            println!("正常播完 @ {head:.3}s / 曲长 {d:.1}s");
            break;
        }
        last_head = head;
    }
    println!("墙钟 {:.2}s", t0.elapsed().as_secs_f64());
    if problem {
        std::process::exit(1);
    }
    Ok(())
}
