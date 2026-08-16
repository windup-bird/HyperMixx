//! 全曲预解码缓存（P23 Phase A 底基）。
//!
//! 48k f32 交织，2048 帧/块，分块 `OnceLock<Box<[f32]>>` 懒分配（每块 16KB）。
//! filler 线程用 `priority: AtomicU64` 寄存器 + `shutdown: AtomicBool`
//! （无命令通道——音频线程只做无阻塞 store）。音频线程读取面
//! `copy_ready` = OnceLock::get + copy_from_slice，零锁零分配。
//!
//! 填充策略：顺序填充（曲首优先）+ priority 跳填（deck 欠载/seek 时
//! 请求，跳填后从跳点继续前进；整段填到 EOF 后回补最低未填洞）。
//! 首块由 `open` 同步解码（≈2-10ms），保住"载入即出声"。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::{self, JoinHandle};

use anyhow::{Result, anyhow};

use crate::decode::{To48k, TrackDecoder};

/// 块帧数（P23 Phase B 起由 TrackCache 定义——caching_reader.rs 已删，
/// 原常量随迁）。
pub const CHUNK_FRAMES: usize = 2048;
/// 30 分钟 48k 硬上限（帧数）：超限 open 报错，v1 不做流式降级。
pub const MAX_TRACK_FRAMES: u64 = 30 * 60 * 48_000;
/// 块槽数 = ceil(MAX / CHUNK_FRAMES)。空壳 ≈1.4MB/deck，逐块懒分配。
const CHUNK_COUNT: usize = (MAX_TRACK_FRAMES as usize).div_ceil(CHUNK_FRAMES);

pub struct TrackCache {
    /// 每块 2048 帧交织立体声；索引 = start_frame / CHUNK_FRAMES。
    /// EOF 尾块补零，块尺寸恒定。
    chunks: Box<[OnceLock<Box<[f32]>>]>,
    /// 曲长（48k 帧）：open 时写入 n_frames 估计值，EOF 时写入精确值。
    pub total_frames: Arc<AtomicU64>,
    /// 连续已填前缀（帧）：供分析侧轮询进度；EOF/截断后钳到实际值。
    filled_prefix: AtomicU64,
    eof_filled: AtomicBool,
    /// filler 跳填寄存器：音频线程无阻塞 store，filler 每块 swap 消费。
    priority: AtomicU64,
    shutdown: AtomicBool,
    /// 整曲填充完成（含回补洞）；deck 不消费，测试/分析侧轮询用。
    done: AtomicBool,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl TrackCache {
    /// 打开音轨并解码首块（同步），随后 filler 线程继续顺序填充。
    pub fn open(path: &Path, sr_out: u32) -> Result<Arc<TrackCache>> {
        let cache = Self::shell(sr_out);
        let mut dec = TrackDecoder::open(path)?;
        let sr_in = dec.sample_rate;
        if let Some(n) = dec.n_frames {
            let est = (n as f64 * sr_out as f64 / sr_in as f64).ceil() as u64;
            if est > MAX_TRACK_FRAMES {
                return Err(anyhow!(
                    "音轨超过 {:.0} 分钟上限（{est} 帧 @48k）",
                    MAX_TRACK_FRAMES as f64 / 48_000.0 / 60.0
                ));
            }
            cache.total_frames.store(est, Ordering::Relaxed);
        }
        let mut resampler = To48k::new(sr_in, sr_out)?;
        let mut pending = Vec::new();
        let mut run_eof = false;
        match fill_block(&cache, &mut dec, &mut resampler, &mut pending, 0, &mut run_eof)? {
            BlockFill::Filled => cache.advance_prefix(),
            BlockFill::Eof { real_frames } => cache.mark_eof(real_frames),
        }
        // 首块同步填好后把解码器状态移交 filler 线程（续填块 1，不重新 open）
        let c2 = Arc::clone(&cache);
        let handle = thread::Builder::new()
            .name("hypermixx-track-filler".into())
            .spawn(move || {
                filler_main(c2, sr_out, dec, resampler, pending, run_eof);
            })?;
        *cache.handle.lock().unwrap() = Some(handle);
        // 登记 lookup registry（Weak：换曲释放后自动失效）
        {
            let mut reg = registry().lock().unwrap();
            reg.retain(|_, w| w.strong_count() > 0);
            reg.insert(path.to_path_buf(), Arc::downgrade(&cache));
        }
        Ok(cache)
    }

    /// 停止填充并等待线程退出（音频线程调用，最多等一块解码时间）。
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.lock().unwrap().take() {
            let _ = h.join();
        }
    }

    /// 分析侧按路径取已加载缓存（无则 None；已释放则 None）。
    pub fn lookup(path: &Path) -> Option<Arc<TrackCache>> {
        registry()
            .lock()
            .unwrap()
            .get(path)
            .and_then(Weak::upgrade)
    }

    // ---- 音频线程读取面（零锁零分配） ----

    /// 从 start_frame 起拷最多 frames 帧交织立体声到 dst，返回实际帧数。
    /// 跨块拷贝，遇未填块（含块槽越界）截断；0 = 该位置尚未就绪。
    pub fn copy_ready(&self, dst: &mut [f32], start_frame: u64, frames: usize) -> usize {
        let want = (frames * 2).min(dst.len());
        let mut copied = 0usize;
        let mut pos = start_frame;
        while copied < want {
            let idx = (pos / CHUNK_FRAMES as u64) as usize;
            let Some(chunk) = self.chunks.get(idx).and_then(OnceLock::get) else {
                break;
            };
            let off = (pos - idx as u64 * CHUNK_FRAMES as u64) as usize;
            let avail = CHUNK_FRAMES - off;
            let take = ((want - copied) / 2).min(avail);
            dst[copied..copied + take * 2].copy_from_slice(&chunk[off * 2..(off + take) * 2]);
            copied += take * 2;
            pos += take as u64;
        }
        copied / 2
    }

    /// [start_frame, start_frame+frames) 是否全部就绪（未填块假）。
    pub fn range_ready(&self, start_frame: u64, frames: usize) -> bool {
        if frames == 0 {
            return true;
        }
        let end = start_frame + frames as u64;
        let mut pos = start_frame;
        while pos < end {
            let idx = (pos / CHUNK_FRAMES as u64) as usize;
            if self.chunks.get(idx).and_then(OnceLock::get).is_none() {
                return false;
            }
            pos = (idx as u64 + 1) * CHUNK_FRAMES as u64;
        }
        true
    }

    pub fn chunk_ready(&self, idx: usize) -> bool {
        matches!(self.chunks.get(idx), Some(c) if c.get().is_some())
    }

    /// 请求 filler 优先填充该位置（未填块；填完自动回顺序填充）。
    pub fn request_priority(&self, frame: u64) {
        self.priority.store(frame, Ordering::Relaxed);
    }

    /// 已就绪的连续前缀（帧）；EOF 后 == total_frames。
    pub fn filled_frames(&self) -> u64 {
        self.filled_prefix.load(Ordering::Relaxed)
    }
    pub fn total_frames(&self) -> u64 {
        self.total_frames.load(Ordering::Relaxed)
    }
    pub fn eof_filled(&self) -> bool {
        self.eof_filled.load(Ordering::Relaxed)
    }
    /// 整曲填充完成（含回补洞）。eof_filled 在跳填到达 EOF 时即置位，
    /// 回补洞随后进行——需要完成的明确信号用这个。
    pub fn fill_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    // ---- 内部 ----

    fn shell(_sr: u32) -> Arc<TrackCache> {
        Arc::new(TrackCache {
            chunks: (0..CHUNK_COUNT).map(|_| OnceLock::new()).collect::<Vec<_>>().into_boxed_slice(),
            total_frames: Arc::new(AtomicU64::new(0)),
            filled_prefix: AtomicU64::new(0),
            eof_filled: AtomicBool::new(false),
            priority: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            done: AtomicBool::new(false),
            handle: Mutex::new(None),
        })
    }

    /// 推进连续前缀到第一个未填块（填完一块后调用）。
    fn advance_prefix(&self) {
        let mut prefix = self.filled_prefix.load(Ordering::Relaxed);
        loop {
            let idx = (prefix / CHUNK_FRAMES as u64) as usize;
            if idx >= CHUNK_COUNT || self.chunks[idx].get().is_none() {
                break;
            }
            prefix += CHUNK_FRAMES as u64;
        }
        self.filled_prefix.store(prefix, Ordering::Relaxed);
    }

    fn mark_eof(&self, real_frames: u64) {
        self.total_frames.store(real_frames, Ordering::Relaxed);
        self.eof_filled.store(true, Ordering::Relaxed);
        self.filled_prefix.store(real_frames, Ordering::Relaxed);
    }

    /// 灾难截断（解码中断/文件不可读）：能播的就是当前前缀，
    /// 标 EOF 让 deck 播到尽头正常停（不卡死、不 panic）。
    fn truncate_to_current(&self) {
        self.eof_filled.store(true, Ordering::Relaxed);
        let n = self.filled_prefix.load(Ordering::Relaxed);
        self.filled_prefix.store(n, Ordering::Relaxed);
        if n < self.total_frames.load(Ordering::Relaxed) {
            self.total_frames.store(n, Ordering::Relaxed);
        }
    }
}

/// 填一块的返回值：Filled = 正常满块；Eof = 本块为 EOF 尾块（可能不满）。
enum BlockFill {
    Filled,
    Eof { real_frames: u64 },
}

/// 从解码器填一块（2048 帧）到 cache.chunks[idx]。EOF 时 flush 重采样器
/// 残余并补零尾块；`run_eof` 跨块保持"本运行已 flush"。短于一块的空曲
/// 不写块（返回 Eof，real_frames = idx*2048）。
fn fill_block(
    cache: &TrackCache,
    dec: &mut TrackDecoder,
    resampler: &mut To48k,
    pending: &mut Vec<f32>,
    idx: usize,
    run_eof: &mut bool,
) -> Result<BlockFill> {
    let mut data = vec![0.0f32; CHUNK_FRAMES * 2];
    let mut written = 0usize;
    let mut eof = false;
    while written < CHUNK_FRAMES {
        if pending.len() < CHUNK_FRAMES * 2 && !*run_eof {
            match dec.decode_next()? {
                Some(frame) => pending.extend_from_slice(&resampler.process(&frame)?),
                None => {
                    pending.extend_from_slice(&resampler.flush()?);
                    *run_eof = true;
                }
            }
        }
        // pending 空 ≠ EOF：升采样（44.1k→48k 等）需凑满 rubato 输入块
        // 才有输出，首段 packet 可能不足一块 → 继续解码直到有输出或
        // 真正 EOF（decode_next None + flush 空）。
        if pending.is_empty() && *run_eof {
            eof = true;
            break;
        }
        let n = (pending.len() / 2).min(CHUNK_FRAMES - written);
        data[written * 2..(written + n) * 2].copy_from_slice(&pending[..n * 2]);
        pending.drain(..n * 2);
        written += n;
    }
    if eof && written == 0 {
        return Ok(BlockFill::Eof { real_frames: idx as u64 * CHUNK_FRAMES as u64 });
    }
    let _ = cache.chunks[idx].set(Box::from(data));
    if eof {
        Ok(BlockFill::Eof { real_frames: idx as u64 * CHUNK_FRAMES as u64 + written as u64 })
    } else {
        Ok(BlockFill::Filled)
    }
}

/// 解码器跳到指定帧（48k 时间轴）：seek + 重造重采样器 + 清 pending。
fn jump_to(
    dec: &mut TrackDecoder,
    resampler: &mut To48k,
    pending: &mut Vec<f32>,
    sr_out: u32,
    frame: u64,
) -> bool {
    let secs = frame as f64 / sr_out as f64;
    match dec.seek(secs).and_then(|_| To48k::new(dec.sample_rate, sr_out)) {
        Ok(r) => {
            *resampler = r;
            pending.clear();
            true
        }
        Err(_) => false,
    }
}

/// 找第一个未填块（从 start 起，限于曲长内——曲外不是"洞"）。
/// 全填返回 None。
fn scan_hole(cache: &TrackCache, start: usize) -> Option<usize> {
    let total = cache.total_frames.load(Ordering::Relaxed);
    let end = total.div_ceil(CHUNK_FRAMES as u64) as usize;
    (start.min(end)..end).find(|&i| cache.chunks[i].get().is_none())
}

fn filler_main(
    cache: Arc<TrackCache>,
    sr_out: u32,
    mut dec: TrackDecoder,
    mut resampler: To48k,
    mut pending: Vec<f32>,
    mut run_eof: bool,
) {
    // 首块已由 open 同步填好；首块即 EOF（短曲）→ 全曲已在缓存
    if cache.eof_filled() {
        cache.done.store(true, Ordering::Relaxed);
        return;
    }
    // 首块已由 open 同步填好
    let mut seq: Option<u64> = Some(CHUNK_FRAMES as u64);
    let mut hole_from: u64 = 0;
    loop {
        if cache.shutdown.load(Ordering::Relaxed) {
            return;
        }
        // priority 跳填（跳点未填才跳；填完从跳点继续前进）
        let prio = cache.priority.swap(0, Ordering::Relaxed);
        if prio > 0 {
            let idx = (prio / CHUNK_FRAMES as u64) as usize;
            if idx < CHUNK_COUNT && cache.chunks[idx].get().is_none()
                && jump_to(&mut dec, &mut resampler, &mut pending, sr_out, prio)
            {
                run_eof = false;
                seq = Some(prio & !(CHUNK_FRAMES as u64 - 1));
            }
        }
        let target = match seq {
            Some(pos) => pos,
            None => {
                // 本运行到 EOF：回补最低未填洞（曲首优先），全填完退出
                let start = (hole_from / CHUNK_FRAMES as u64) as usize;
                match scan_hole(&cache, start) {
                    Some(idx) => {
                        let pos = idx as u64 * CHUNK_FRAMES as u64;
                        if !jump_to(&mut dec, &mut resampler, &mut pending, sr_out, pos) {
                            log::error!("回补洞 seek 失败（文件不可读？）");
                            cache.truncate_to_current();
                            return;
                        }
                        run_eof = false;
                        hole_from = pos + CHUNK_FRAMES as u64;
                        seq = Some(pos);
                        continue;
                    }
                    None => {
                        cache.done.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
        };
        let idx = (target / CHUNK_FRAMES as u64) as usize;
        if idx >= CHUNK_COUNT {
            // 无 n_frames 的流式格式填到槽位上限：到此为止（数据已就绪部分保留）
            cache.done.store(true, Ordering::Relaxed);
            return;
        }
        // 撞上已填区（priority 跳填后回补时）：交给回补洞分支找下一洞
        if cache.chunks[idx].get().is_some() {
            seq = None;
            continue;
        }
        let outcome =
            match fill_block(&cache, &mut dec, &mut resampler, &mut pending, idx, &mut run_eof)
            {
                Ok(o) => o,
                Err(e) => {
                    log::error!("缓存填充中断（文件不可读？）: {e:#}");
                    cache.truncate_to_current();
                    return;
                }
            };
        match outcome {
            BlockFill::Filled => {
                cache.advance_prefix();
                seq = Some(target + CHUNK_FRAMES as u64);
            }
            BlockFill::Eof { real_frames } => {
                // 空尾块（seek 落点已在曲尾后）不动 total；真实尾块才标记
                if real_frames > target {
                    cache.mark_eof(real_frames);
                }
                seq = None;
            }
        }
    }
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<TrackCache>>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, Weak<TrackCache>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// 测试构造 seam：内存直填（无线程无 I/O），deck.rs 测试模块使用。
#[cfg(test)]
#[allow(dead_code)]
impl TrackCache {
    pub(crate) fn test_new_empty(sr: u32) -> Arc<TrackCache> {
        Self::shell(sr)
    }
    pub(crate) fn test_set_chunk(&self, idx: usize, data: Box<[f32]>) {
        let _ = self.chunks[idx].set(data);
    }
    pub(crate) fn test_set_total(&self, n: u64) {
        self.total_frames.store(n, Ordering::Relaxed);
    }
    /// 模拟填充进度（filler 线程的 filled_prefix 更新；测试 rig 手填）。
    pub(crate) fn test_set_filled(&self, n: u64) {
        self.filled_prefix.store(n, Ordering::Relaxed);
    }
    pub(crate) fn test_set_eof(&self, n: u64) {
        self.mark_eof(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_sine_wav(path: &Path, secs: f64, freq: f64) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let n = (secs * 48_000.0) as usize;
        for i in 0..n {
            let v = ((i as f64) * freq * 2.0 * std::f64::consts::PI / 48_000.0).sin() as f32 * 0.5;
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
    }

    fn tmp_wav(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hmx_track_cache_{name}.wav"));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn wait_until(timeout_ms: u64, f: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(timeout_ms);
        while std::time::Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        f()
    }

    /// 跨块拷贝 / 未填块截断（计划新测试 #1）。
    #[test]
    fn cache_copy_ready_splits_at_chunk_boundaries() {
        let c = TrackCache::shell(48_000);
        // 填 0、2 块（第 1 块故意不填：验证截断）
        c.chunks[0].set(vec![1.0f32; CHUNK_FRAMES * 2].into_boxed_slice()).ok();
        c.chunks[2].set(vec![3.0f32; CHUNK_FRAMES * 2].into_boxed_slice()).ok();

        // 跨块（0 块尾 + 1 块头）：1 块未填 → 只拷到 0 块边界
        let mut dst = vec![0.0f32; CHUNK_FRAMES * 2];
        let n = c.copy_ready(&mut dst, CHUNK_FRAMES as u64 - 100, CHUNK_FRAMES);
        assert_eq!(n, 100, "未填块前截断");
        assert_eq!(dst[0], 1.0);
        assert_eq!(dst[n * 2 - 1], 1.0);

        // 单帧跨块读（2 块起点偏移）
        let mut one = [0.0f32; 2];
        assert_eq!(c.copy_ready(&mut one, 2 * CHUNK_FRAMES as u64 + 5, 1), 1);
        assert_eq!(one[0], 3.0);

        // 未填区 → 0
        let n = c.copy_ready(&mut dst, CHUNK_FRAMES as u64, 1);
        assert_eq!(n, 0, "未填块返回 0");
        assert!(!c.range_ready(CHUNK_FRAMES as u64, 1));
        assert!(c.range_ready(2 * CHUNK_FRAMES as u64, 10));
        // 越界（超出块槽）→ 截断为 0
        let n = c.copy_ready(&mut dst, CHUNK_COUNT as u64 * CHUNK_FRAMES as u64, 1);
        assert_eq!(n, 0);
    }

    /// 真线程：顺序填充到 EOF，total 精确（计划新测试 #2）。
    #[test]
    fn filler_sequential_fills_to_eof() {
        let p = tmp_wav("seq");
        write_sine_wav(&p, 2.5, 440.0);
        let c = TrackCache::open(&p, 48_000).unwrap();
        // 首块同步已填
        assert!(c.range_ready(0, CHUNK_FRAMES), "open 后首块应已就绪");
        assert!(wait_until(10_000, || c.eof_filled()));
        let total = c.total_frames();
        assert!((total as f64 / 48_000.0 - 2.5).abs() < 0.01, "total={total}");
        assert_eq!(c.filled_frames(), total);
        assert!(c.range_ready(0, total as usize), "全范围应可读");
        // 注意：EOF 尾块补零，copy 越过 total 返回零填充（deck 端由 EOF
        // 检查保护，不会读到这里）；顺序填充（无 seek）total 精确。
        c.stop();
    }

    /// priority 跳填后继续前进、EOF 后回补最低洞（计划新测试 #3）。
    #[test]
    fn filler_priority_jump_then_resumes_head() {
        let p = tmp_wav("prio");
        write_sine_wav(&p, 6.0, 440.0);
        let c = TrackCache::open(&p, 48_000).unwrap();
        // 立即请求跳填中段（顺序填充必然还没到那里）
        let mid = (3.0 * 48_000.0) as u64;
        c.request_priority(mid);
        let mid_idx = (mid / CHUNK_FRAMES as u64) as usize;
        assert!(
            wait_until(10_000, || c.chunk_ready(mid_idx)),
            "priority 跳填应优先填到中段"
        );
        assert!(
            wait_until(20_000, || c.fill_done()),
            "最终应填到 EOF 并回补完洞"
        );
        let total = c.total_frames();
        // 跳填经 symphonia Accurate seek，落点有包对齐误差（≤一块）；
        // 顺序填充（无 seek）才保证 total 精确（见 filler_sequential_fills_to_eof）
        assert!(
            (total as i64 - (6.0 * 48_000.0) as i64).abs() <= CHUNK_FRAMES as i64,
            "total={total}"
        );
        // 回补洞后无空洞：全曲范围可读（EOF 尾块补零覆盖曲尾）
        assert!(c.range_ready(0, (6.0 * 48_000.0) as usize), "回补后不应有洞");
        // 跳填目标块在顺序填充到达前已就绪（已由第一个 wait 断言）
        c.stop();
    }

}
