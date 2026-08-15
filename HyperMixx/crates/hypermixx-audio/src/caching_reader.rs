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
/// 每 deck ring 容量（64 × 16KB = 1MB）。
pub const RING_CAPACITY: usize = 64;

pub struct Chunk {
    /// 数据世代：seek/load 后递增，旧 chunk 被 deck 丢弃。
    pub epoch: u32,
    /// 本 chunk 首帧在 48kHz 时间轴上的位置。
    pub start_frame: u64,
    /// 交织立体声 f32，长度 = CHUNK_FRAMES*2。
    pub data: Box<[f32]>,
}

pub enum ReaderCmd {
    Seek { epoch: u32, frame: u64 },
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
            )? {
                return Ok(()); // Shutdown
            }
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
) -> Result<bool> {
    match cmd {
        ReaderCmd::Shutdown => Ok(true),
        ReaderCmd::Seek { epoch: e, frame } => {
            *epoch = e;
            *frames_pushed = frame;
            pending.clear();
            *chunk_to_push = None;
            dec.seek(frame as f64 / sr_out as f64)?;
            *resampler = To48k::new(sr_in, sr_out)?;
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
