//! 端到端播放模拟：真实 CachingReader 线程 + Deck，不接声卡。
//! 用法: playback_sim <音频路径> [rate_pct] [mode] [seek_sec] [pitch_st]
//!   rate_pct: 初始速率滑杆值（0 = 1.0×，100 = 2.0×）
//!   mode:     fast10 = 10× 加速真实时间（默认，曲目 33s 跑完）
//!             paced  = 1× 真实时间
//!             stress = 全速压测（deck 跑得比解码快，恒定欠载）
//!   seek_sec: 在模拟 t=10s 时跳转到该秒数（测 seek 竞态）
//!   pitch_st: 初始 key shift 半音（-12..12，0 = 不启用，默认 0）
//!   若 mode 以 sweep 开头（sweep/sweep10）：每 2s 把速率在 ±8 之间跳跃
//!   （模拟用户快速拖拽速率滑杆），sweep10 同时用 10× 加速
//! 输出每秒的 playhead/VU/播放状态；发现异常打 !!!
//! 退出码 1 = 发现提前停止或播放头停住

use std::time::{Duration, Instant};

use hypermixx_audio::deck::Deck;
use hypermixx_core::ControlBus;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("用法: playback_sim <音频路径> [rate_pct] [mode] [seek_sec] [pitch_st]");
        std::process::exit(2);
    }
    let path = &args[0];
    let rate_pct: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let mode = args.get(2).map(|s| s.as_str()).unwrap_or("fast10");
    let sweep = mode.starts_with("sweep");
    let block_us = match mode {
        "paced" => 5333u64,
        "fast10" | "sweep10" => 533u64,
        _ => 0u64, // stress：不睡觉
    };
    let seek_sec: Option<f64> = args.get(3).and_then(|s| s.parse().ok());
    let pitch_st: f64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0.0);

    let bus = ControlBus::default();
    let mut deck = Deck::new(0, 48000, &bus);
    deck.ctl.rate.set(rate_pct);
    deck.ctl.volume.set(1.0);
    deck.ctl.keylock.set(1.0);
    if pitch_st != 0.0 {
        deck.ctl.pitch.set(pitch_st);
    }
    deck.load(path.into());

    let mut out = vec![0.0f32; 256 * 2];
    let mut stalled_secs = 0u32;
    let mut last_head = 0.0;
    let mut problem = false;
    let t0 = Instant::now();
    let mut sec = 0u64;
    loop {
        for _ in 0..(48000 / 256) {
            deck.update_params();
            deck.process(&mut out, 256);
            if block_us > 0 {
                std::thread::sleep(Duration::from_micros(block_us));
            }
        }
        // 每秒动作：可选跳转（测 seek 竞态）
        if let Some(target) = seek_sec
            && sec == 10
        {
            println!("--- 执行 seek → {target}s ---");
            deck.seek_seconds(target);
        }
        // 速率扫掠：每 2s 在 ±8 之间跳变（模拟拖拽滑杆）
        if sweep && sec > 2 && sec.is_multiple_of(2) {
            let v = if (sec / 2).is_multiple_of(2) {
                8.0
            } else {
                -8.0
            };
            deck.ctl.rate.set(v);
            println!("--- 速率 → {v}% ---");
        }
        let head = deck.ctl.playhead.get();
        let playing = deck.ctl.play.get() > 0.5;
        let dur = deck.ctl.duration.get();
        println!(
            "sim t={sec:>4}s head={head:>8.3}s vu={:.3} play={playing} dur={dur:.1}s",
            deck.ctl.vu.get()
        );
        // 提前停止：未到结尾 play 变 false
        if !playing && dur > 1.0 && head < dur - 1.0 {
            println!(
                "!!! 提前停止 @ {head:.3}s（曲长 {dur:.1}s，剩 {:.1}s 未播）",
                dur - head
            );
            problem = true;
            break;
        }
        // 播放头停住：连续 3s 不动（play=true 且有数据）
        if playing && head > 1.0 {
            if (head - last_head).abs() < 0.01 {
                stalled_secs += 1;
                if stalled_secs >= 3 {
                    println!("!!! 播放头停住 @ {head:.3}s（连续 {stalled_secs}s 不动）");
                    problem = true;
                    break;
                }
            } else {
                stalled_secs = 0;
            }
        }
        // 正常播完
        if dur > 1.0 && !playing && head >= dur - 1.0 {
            println!("正常播完 @ {head:.3}s / 曲长 {dur:.1}s");
            break;
        }
        last_head = head;
        sec += 1;
        if sec > 3600 {
            println!("!!! 超时（1 小时模拟仍未结束）");
            problem = true;
            break;
        }
    }
    println!("墙钟 {:.2}s", t0.elapsed().as_secs_f64());
    if problem {
        std::process::exit(1);
    }
    Ok(())
}
