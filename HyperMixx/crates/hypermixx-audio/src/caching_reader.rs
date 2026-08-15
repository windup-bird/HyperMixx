//! CachingReader：解码 worker 线程。
//! symphonia 解码 → rubato 转 48kHz → 按 2048 帧 chunk 写入 SPSC ring，
//! 音频线程只从 ring 读，解码永不进实时线程。

use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::Result;
use ringbuf::traits::{Observer, Producer};

use crate::deck::TrackFrames;
use crate::decode::{To48k, TrackDecoder};

/// 每 chunk 帧数（48kHz 时间轴，交织立体声）。
pub const CHUNK_FRAMES: usize = 2048;
/// 每 deck ring 容量（256 × 16KB = 4MB，预解码窗口 ~10.9s）。
/// P22-C：64 → 256——±8/16 拍跳（BPM 90 下 8 拍 ≈ 5.3s）基本命中
/// 保留窗口；超窗走全排 refill 兜底（旧行为）。
pub const RING_CAPACITY: usize = 256;
/// P22-B 回填侧环容量（256 × 16KB = 4MB）：ManualLoop 激活时的前区回填
/// 解码结果推这里，deck 侧每块排空到环缓冲前区。超容走 push_or_cmd
/// 节流（reader 停驻等 deck 排空，不丢数据）。
pub const SIDE_RING_CAPACITY: usize = 256;

pub struct Chunk {
    /// 数据世代：seek/load 后递增，旧 chunk 被 deck 丢弃。
    pub epoch: u32,
    /// 本 chunk 首帧在 48kHz 时间轴上的位置。
    pub start_frame: u64,
    /// 交织立体声 f32，长度 = CHUNK_FRAMES*2。
    pub data: Box<[f32]>,
}

pub enum ReaderCmd {
    /// P22-C `resume = Some(r)`：deck 保留了 ring 已解码窗口（落点在窗
    /// 内），r = 窗口末块 end——reader 已推到 r 及之后时保持当前解码位置
    /// 继续推（窗口内容连续且位置正确，重解码只会产生被位置规则丢弃的
    /// 重复段）；落后于 r 时才 seek 重解码补齐。None = 正常 seek。
    Seek { epoch: u32, frame: u64, resume: Option<u64> },
    /// P22-B：回填 [from, to) 到侧环（不改变主环世代/位置）。已在回填中
    /// 时替换 from/to、保留 resume（主环续推点）；Seek 弃回填。epoch
    /// 不符（seek/load 后陈旧命令）忽略。
    Backfill { epoch: u32, from: u64, to: u64 },
    Shutdown,
}

enum PushOutcome {
    Pushed,
    Command(ReaderCmd),
}

pub fn reader_main(
    path: std::path::PathBuf,
    sr_out: u32,
    cmd_rx: Receiver<ReaderCmd>,
    mut side_prod: ringbuf::HeapProd<Chunk>,
    mut prod: ringbuf::HeapProd<Chunk>,
    mut epoch: u32,
    track_frames: TrackFrames,
) -> Result<()> {
    let mut dec = TrackDecoder::open(&path)?;
    log::info!(
        "读取 {}：{}Hz {}ch",
        path.display(),
        dec.sample_rate,
        dec.channels
    );
    let sr_in = dec.sample_rate;
    // 立即报告曲长（源采样率帧数换算到 48kHz），UI 不必等到播完才显示时长；
    // EOF 时再用精确值覆盖。
    if let Some(n_in) = dec.n_frames {
        let total = (n_in as f64 * sr_out as f64 / sr_in as f64) as u64;
        track_frames.store(total, Ordering::Relaxed);
    }
    let mut resampler = To48k::new(sr_in, sr_out)?;
    let mut frames_pushed: u64 = 0;
    let mut pending: Vec<f32> = Vec::new();
    let mut chunk_to_push: Option<Chunk> = None;
    // P22-B 回填状态：(from, to, resume)——resume = 主环续推点（首个
    // Backfill 到达时的解码位置，替换回填时保持；完成/EOF 后 seek 回）。
    let mut backfill: Option<(u64, u64, u64)> = None;

    loop {
        // 1. 服务命令
        while let Ok(cmd) = cmd_rx.try_recv() {
            if handle_cmd(
                cmd,
                &mut dec,
                &mut resampler,
                sr_in,
                &mut frames_pushed,
                &mut pending,
                &mut chunk_to_push,
                &mut epoch,
                sr_out,
                &mut backfill,
            )? {
                return Ok(()); // Shutdown
            }
        }

        // 1.5 P22-B 回填模式：解码推侧环，范围到点/EOF 后 seek 回
        // resume 续主环（命令经 push_or_cmd 节流实时处理）
        if backfill.is_some() {
            if step_backfill(
                &mut dec,
                &mut resampler,
                sr_in,
                sr_out,
                &mut frames_pushed,
                &mut pending,
                &mut chunk_to_push,
                &mut epoch,
                &mut side_prod,
                &cmd_rx,
                &mut backfill,
            )? {
                return Ok(());
            }
            continue; // 回填优先：主环推送/解码让位
        }

        // 2. 推送积压 chunk
        if let Some(chunk) = chunk_to_push.take() {
            match push_or_cmd(&mut prod, chunk, &cmd_rx) {
                PushOutcome::Pushed => {
                    frames_pushed += CHUNK_FRAMES as u64;
                }
                PushOutcome::Command(cmd) => {
                    // 命令优先：未推送的 chunk 保留在 pending 前部
                    // （重建 chunk 以便重试）
                    // 注意 seek 会在 handle_cmd 里丢弃积压数据
                    let rebuilt = rebuild_chunk(epoch, frames_pushed, &mut pending);
                    if let Some(c) = rebuilt {
                        chunk_to_push = Some(c);
                    }
                    if handle_cmd(
                        cmd,
                        &mut dec,
                        &mut resampler,
                        sr_in,
                        &mut frames_pushed,
                        &mut pending,
                        &mut chunk_to_push,
                        &mut epoch,
                        sr_out,
                        &mut backfill,
                    )? {
                        return Ok(());
                    }
                }
            }
        }

        // 3. 解码 + 重采样 + 打包
        match dec.decode_next()? {
            None => {
                // EOF：flush 重采样残余；整块照常推，最后不足一块补零
                let tail = resampler.flush()?;
                let real_end = frames_pushed + (tail.len() / 2) as u64;
                pending.extend_from_slice(&tail);
                while pending.len() >= CHUNK_FRAMES * 2 {
                    let data: Vec<f32> = pending.drain(..CHUNK_FRAMES * 2).collect();
                    let chunk = Chunk {
                        epoch,
                        start_frame: frames_pushed,
                        data: data.into_boxed_slice(),
                    };
                    if let PushOutcome::Command(cmd) = push_or_cmd(&mut prod, chunk, &cmd_rx)
                        && handle_cmd(
                            cmd,
                            &mut dec,
                            &mut resampler,
                            sr_in,
                            &mut frames_pushed,
                            &mut pending,
                            &mut chunk_to_push,
                            &mut epoch,
                            sr_out,
                            &mut backfill,
                        )?
                    {
                        return Ok(());
                    }
                    frames_pushed += CHUNK_FRAMES as u64;
                }
                if !pending.is_empty() {
                    pending.resize(CHUNK_FRAMES * 2, 0.0);
                    let data: Vec<f32> = pending.drain(..CHUNK_FRAMES * 2).collect();
                    let chunk = Chunk {
                        epoch,
                        start_frame: frames_pushed,
                        data: data.into_boxed_slice(),
                    };
                    let _ = push_or_cmd(&mut prod, chunk, &cmd_rx);
                }
                track_frames.store(real_end, Ordering::Relaxed);
                return Ok(());
            }
            Some(native) => {
                let converted = resampler.process(&native)?;
                pending.extend_from_slice(&converted);
                while pending.len() >= CHUNK_FRAMES * 2 {
                    let data: Vec<f32> = pending.drain(..CHUNK_FRAMES * 2).collect();
                    let chunk = Chunk {
                        epoch,
                        start_frame: frames_pushed,
                        data: data.into_boxed_slice(),
                    };
                    match push_or_cmd(&mut prod, chunk, &cmd_rx) {
                        PushOutcome::Pushed => {
                            frames_pushed += CHUNK_FRAMES as u64;
                        }
                        PushOutcome::Command(cmd) => {
                            if handle_cmd(
                                cmd,
                                &mut dec,
                                &mut resampler,
                                sr_in,
                                &mut frames_pushed,
                                &mut pending,
                                &mut chunk_to_push,
                                &mut epoch,
                                sr_out,
                                &mut backfill,
                            )? {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_cmd(
    cmd: ReaderCmd,
    dec: &mut TrackDecoder,
    resampler: &mut To48k,
    sr_in: u32,
    frames_pushed: &mut u64,
    pending: &mut Vec<f32>,
    chunk_to_push: &mut Option<Chunk>,
    epoch: &mut u32,
    sr_out: u32,
    backfill: &mut Option<(u64, u64, u64)>,
) -> Result<bool> {
    match cmd {
        ReaderCmd::Shutdown => Ok(true),
        ReaderCmd::Seek { epoch: e, frame, resume } => {
            *epoch = e;
            // P22-C：resume ≤ 当前解码位置 → 保持（deck 保留窗口已覆盖到
            // 此处，续推即无缝；r > frames_pushed 才重 seek 补齐窗口尾）。
            let frame = resume.filter(|r| *r > *frames_pushed).unwrap_or(frame);
            *frames_pushed = frame;
            pending.clear();
            *chunk_to_push = None;
            dec.seek(frame as f64 / sr_out as f64)?;
            *resampler = To48k::new(sr_in, sr_out)?;
            *backfill = None; // P22-B：seek 弃回填（侧环残留由 deck 按位置钳制/epoch 丢弃）
            Ok(false)
        }
        ReaderCmd::Backfill { epoch: e, from, to } => {
            if e == *epoch {
                // P22-B：启动/替换回填。resume = 主环续推点（首个
                // Backfill 到达时的解码位置；替换时保持原值）。
                let resume = if let Some((_, _, r)) = *backfill {
                    r
                } else {
                    *frames_pushed
                };
                *backfill = Some((from, to, resume));
                *frames_pushed = from;
                pending.clear();
                *chunk_to_push = None;
                dec.seek(from as f64 / sr_out as f64)?;
                *resampler = To48k::new(sr_in, sr_out)?;
            }
            Ok(false)
        }
    }
}

/// P22-B：回填一步——解码一个源包并推侧环；范围到点或 EOF → seek 回
/// resume 续主环并清状态。推送节流中收到的命令（Seek 弃回填 / 新
/// Backfill 替换 / Shutdown）实时处理。返回 true = Shutdown。
#[allow(clippy::too_many_arguments)]
fn step_backfill(
    dec: &mut TrackDecoder,
    resampler: &mut To48k,
    sr_in: u32,
    sr_out: u32,
    frames_pushed: &mut u64,
    pending: &mut Vec<f32>,
    chunk_to_push: &mut Option<Chunk>,
    epoch: &mut u32,
    side_prod: &mut ringbuf::HeapProd<Chunk>,
    cmd_rx: &Receiver<ReaderCmd>,
    backfill: &mut Option<(u64, u64, u64)>,
) -> Result<bool> {
    match dec.decode_next()? {
        None => {
            // EOF：flush 残余 + 尾部补零推侧环（与主环 EOF 同款），随后
            // seek 回 resume 续主环（回填范围不足由 deck 按位置钳制）。
            // 期间命令照常处理（Shutdown 不能丢——否则 stop_reader 的
            // join 会悬挂）。
            let mut cmd_handled = false;
            let tail = resampler.flush()?;
            pending.extend_from_slice(&tail);
            while pending.len() >= CHUNK_FRAMES * 2 {
                let data: Vec<f32> = pending.drain(..CHUNK_FRAMES * 2).collect();
                let chunk = Chunk {
                    epoch: *epoch,
                    start_frame: *frames_pushed,
                    data: data.into_boxed_slice(),
                };
                match push_or_cmd(side_prod, chunk, cmd_rx) {
                    PushOutcome::Pushed => {}
                    PushOutcome::Command(cmd) => {
                        cmd_handled = true;
                        if handle_cmd(
                            cmd,
                            dec,
                            resampler,
                            sr_in,
                            frames_pushed,
                            pending,
                            chunk_to_push,
                            epoch,
                            sr_out,
                            backfill,
                        )? {
                            return Ok(true);
                        }
                    }
                }
                *frames_pushed += CHUNK_FRAMES as u64;
            }
            if !pending.is_empty() {
                pending.resize(CHUNK_FRAMES * 2, 0.0);
                let data: Vec<f32> = pending.drain(..CHUNK_FRAMES * 2).collect();
                let chunk = Chunk {
                    epoch: *epoch,
                    start_frame: *frames_pushed,
                    data: data.into_boxed_slice(),
                };
                match push_or_cmd(side_prod, chunk, cmd_rx) {
                    PushOutcome::Pushed => {}
                    PushOutcome::Command(cmd) => {
                        cmd_handled = true;
                        if handle_cmd(
                            cmd,
                            dec,
                            resampler,
                            sr_in,
                            frames_pushed,
                            pending,
                            chunk_to_push,
                            epoch,
                            sr_out,
                            backfill,
                        )? {
                            return Ok(true);
                        }
                    }
                }
            }
            // 无命令介入才续主环（新 Backfill 已重定位，seek 会覆盖）
            if !cmd_handled
                && let Some((_, _, resume)) = *backfill
            {
                dec.seek(resume as f64 / sr_out as f64)?;
                *resampler = To48k::new(sr_in, sr_out)?;
                pending.clear();
                *chunk_to_push = None;
                *frames_pushed = resume;
                *backfill = None;
            }
            Ok(false)
        }
        Some(native) => {
            let converted = resampler.process(&native)?;
            pending.extend_from_slice(&converted);
            while pending.len() >= CHUNK_FRAMES * 2 {
                let data: Vec<f32> = pending.drain(..CHUNK_FRAMES * 2).collect();
                let chunk = Chunk {
                    epoch: *epoch,
                    start_frame: *frames_pushed,
                    data: data.into_boxed_slice(),
                };
                match push_or_cmd(side_prod, chunk, cmd_rx) {
                    PushOutcome::Pushed => {
                        *frames_pushed += CHUNK_FRAMES as u64;
                    }
                    PushOutcome::Command(cmd) => {
                        if handle_cmd(
                            cmd,
                            dec,
                            resampler,
                            sr_in,
                            frames_pushed,
                            pending,
                            chunk_to_push,
                            epoch,
                            sr_out,
                            backfill,
                        )? {
                            return Ok(true);
                        }
                    }
                }
            }
            // 范围到点 → seek 回 resume 续主环。状态可能已被命令替换
            //（新 Backfill 重置 frames_pushed），读当前值判定。
            if let Some((_, to, resume)) = *backfill
                && *frames_pushed >= to
            {
                dec.seek(resume as f64 / sr_out as f64)?;
                *resampler = To48k::new(sr_in, sr_out)?;
                pending.clear();
                *chunk_to_push = None;
                *frames_pushed = resume;
                *backfill = None;
            }
            Ok(false)
        }
    }
}

fn rebuild_chunk(epoch: u32, start_frame: u64, pending: &mut Vec<f32>) -> Option<Chunk> {
    if pending.len() >= CHUNK_FRAMES * 2 {
        let data: Vec<f32> = pending.drain(..CHUNK_FRAMES * 2).collect();
        Some(Chunk {
            epoch,
            start_frame,
            data: data.into_boxed_slice(),
        })
    } else {
        None
    }
}

/// 尝试推送；ring 满时等待并监听命令（seek 会先清 ring，不会死锁）。
fn push_or_cmd(
    prod: &mut ringbuf::HeapProd<Chunk>,
    chunk: Chunk,
    cmd_rx: &Receiver<ReaderCmd>,
) -> PushOutcome {
    loop {
        if !prod.is_full() {
            if prod.try_push(chunk).is_ok() {
                return PushOutcome::Pushed;
            }
            // 消费者已销毁
            return PushOutcome::Command(ReaderCmd::Shutdown);
        }
        match cmd_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(cmd) => return PushOutcome::Command(cmd),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return PushOutcome::Command(ReaderCmd::Shutdown);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::{Consumer, Split};
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc::channel;
    use std::sync::Arc;

    /// 写 10s 440Hz 立体声 32-bit float WAV。
    fn write_sine_wav(path: &std::path::Path, secs: f64) {
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
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    /// 排空消费主环，返回已消费 chunk 的 start_frame 序列（保持 reader
    /// 不因 ring 满而停顿）。超时返回 Err。
    fn drain_main(
        main_cons: &mut ringbuf::HeapCons<Chunk>,
        seen: &mut Vec<u64>,
        target: u64,
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        loop {
            while let Some(c) = main_cons.try_pop() {
                seen.push(c.start_frame);
            }
            let end = seen.last().map(|s| s + CHUNK_FRAMES as u64).unwrap_or(0);
            if end >= target {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("主环只推到 {end}，未达 {target}（{seen:?}）"));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// P22-B：Backfill 命令把 [from, to) 解码进侧环（连续区间、epoch 保持），
    /// 到点后 seek 回 resume（= 命令到达时的解码位置）续主环——不重播
    /// 前区、不产生重复帧。deck 侧回填测试（manual_loop_*_with_real_reader）
    /// 依赖此契约。
    #[test]
    fn reader_backfill_decodes_range_then_resumes() {
        let path = std::env::temp_dir()
            .join(format!("hypermixx_reader_backfill_{}.wav", std::process::id()));
        write_sine_wav(&path, 10.0);
        let (cmd_tx, cmd_rx) = channel::<ReaderCmd>();
        let (main_prod, mut main_cons) = ringbuf::HeapRb::<Chunk>::new(256).split();
        let (side_prod, mut side_cons) =
            ringbuf::HeapRb::<Chunk>::new(SIDE_RING_CAPACITY).split();
        let track_frames = Arc::new(AtomicU64::new(0));
        let epoch = 7u32;
        let reader_path = path.clone();
        let handle = std::thread::spawn(move || {
            reader_main(
                reader_path,
                48_000,
                cmd_rx,
                side_prod,
                main_prod,
                epoch,
                track_frames,
            )
            .unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        // 1. 主环正常推进 ≥ 64 chunk（128k 帧），等 reader 超前预解码
        let mut main_seen: Vec<u64> = Vec::new();
        drain_main(&mut main_cons, &mut main_seen, 128 * 1024, deadline)
            .expect("reader 应快速预解码 64 chunk");
        // 2. 发 Backfill：resume = 此刻解码位置（≥ main_seen 末端）
        let resume = main_seen.last().map(|s| s + CHUNK_FRAMES as u64).unwrap_or(0);
        cmd_tx
            .send(ReaderCmd::Backfill {
                epoch,
                from: 48_000,
                to: 72_000,
            })
            .unwrap();
        // 3. 侧环收到 [48000, 72000) 连续覆盖（12 chunk，epoch 保持）
        let mut side_seen: Vec<u64> = Vec::new();
        loop {
            while let Some(c) = side_cons.try_pop() {
                assert_eq!(c.epoch, epoch, "回填 chunk 世代应保持");
                side_seen.push(c.start_frame);
            }
            let end = side_seen.last().map(|s| s + CHUNK_FRAMES as u64).unwrap_or(0);
            if end >= 72_000 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "侧环回填超时（{side_seen:?}）"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(side_seen.first().copied(), Some(48_000), "回填从 from 起");
        for w in side_seen.windows(2) {
            assert_eq!(
                w[1],
                w[0] + CHUNK_FRAMES as u64,
                "回填区间必须连续无空洞"
            );
        }
        // 4. 主环从 resume 续推：不重播前区（无 start < resume 的块再出现）、
        //    续推序列连续
        let pre_len = main_seen.len();
        drain_main(&mut main_cons, &mut main_seen, resume + 2 * CHUNK_FRAMES as u64, deadline)
            .expect("回填完成后主环应从 resume 续推");
        let resumed: &[u64] = &main_seen[pre_len..];
        assert!(
            resumed.windows(2).all(|w| w[1] == w[0] + CHUNK_FRAMES as u64),
            "续推序列必须连续（{resumed:?}）"
        );
        assert!(
            resumed.first().copied().unwrap_or(u64::MAX) >= resume,
            "续推不得回到前区（resume={resume}，首块 {:?}）",
            resumed.first()
        );
        // 5. 收尾：关命令通道 → reader 退出（不悬挂）
        drop(cmd_tx);
        let _ = handle.join();
        let _ = std::fs::remove_file(&path);
    }
}
