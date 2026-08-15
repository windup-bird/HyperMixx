//! 波形渐进分析基准：对比整曲 analyze() 与渐进 start_analysis() 的
//! 首段延迟（= 用户感知的"波形多久出现"）与全程耗时。
//! 用法: analysis_bench <音频路径> [priority_seg]

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use hypermixx_analysis::{AnalysisEvent, start_analysis};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("用法: analysis_bench <音频路径> [priority_seg]");
        std::process::exit(2);
    }
    let path = std::path::PathBuf::from(&args[0]);
    let prio: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

    // 参照：整曲一次性分析（旧路径）
    let t0 = Instant::now();
    let full = hypermixx_analysis::analyze(&path).expect("整曲分析失败");
    println!(
        "整曲 analyze(): {:.2}s，{} 列（旧加载路径的完整延迟）",
        t0.elapsed().as_secs_f64(),
        full.detail.len()
    );

    // 渐进分析（新路径）
    let (tx, rx) = mpsc::channel();
    start_analysis(
        path,
        Arc::new(AtomicU64::new(prio)),
        Arc::new(AtomicBool::new(false)),
        1,
        tx,
    );
    let t0 = Instant::now();
    let mut n_seg = 0usize;
    let mut first = None;
    loop {
        match rx.recv().expect("通道断开") {
            AnalysisEvent::Segment { seg, detail, .. } => {
                if first.is_none() {
                    first = Some(t0.elapsed().as_secs_f64());
                    println!(
                        "+{:.3}s 首段就绪：段 {seg}（{} 列）← 用户感知的加载延迟",
                        first.unwrap(),
                        detail.len()
                    );
                }
                n_seg += 1;
            }
            AnalysisEvent::TrackAnalysis {
                bpm,
                key,
                offset_secs,
                beats_secs,
                downbeats_secs,
                confidence,
                ..
            } => {
                println!(
                    "+{:.2}s 单遍分析：BPM {bpm:.2}（置信 {confidence:.2}），key {}（Camelot {}），\
                     首拍 {offset_secs:.3}s，{} 拍 / {} 下拍",
                    t0.elapsed().as_secs_f64(),
                    key.as_ref().map(|k| k.name()).unwrap_or_default(),
                    key.as_ref().map(|k| k.camelot()).unwrap_or_default(),
                    beats_secs.len(),
                    downbeats_secs.len()
                );
            }
            AnalysisEvent::Done { wave, .. } => {
                println!(
                    "+{:.2}s Done：{} 段全部分析完，{} 列 / {} 帧",
                    t0.elapsed().as_secs_f64(),
                    n_seg,
                    wave.detail.len(),
                    wave.duration_frames
                );
                break;
            }
            AnalysisEvent::Failed { msg, .. } => {
                eprintln!("分析失败: {msg}");
                std::process::exit(1);
            }
        }
    }
}
