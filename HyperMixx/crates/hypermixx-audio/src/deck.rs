//! Deck：单个播放通道的实时状态（只被音频线程 + 引擎操作段触碰）。
//!
//! 播放链（keylock 引擎可用时）：
//! 缓存读取器 → 喂入(keylocker) → 引擎整块渲染（变速不变调）→ pitch（key shift）
//! → EQ → deck 滤波（旋钮 LP/HP）→ FX rack（8 槽）→ gain（音量 × 通道增益 dB）。
//! 引擎构建失败时回退线性插值路径（read_stereo + pos += rate）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use crate::caching_reader::{CHUNK_FRAMES, Chunk, ReaderCmd};
#[cfg(test)]
use ringbuf::traits::Producer as _;
use crate::dsp::deck_filter::DeckFilter;
use crate::dsp::eq::ThreeBandEq;
use crate::dsp::pitch::PitchShifter;
use crate::dsp::smoother::Smoother;
use crate::fx::{EffectId, FxContext, FxRack, manifest};
use crate::keylocker::{Keylocker, TimestretchLocker};
use hypermixx_core::{BeatClock, BeatGrid, ControlHandle};
use ringbuf::traits::{Consumer, Split};

/// 引擎块帧数（与 Engine::BLOCK_FRAMES 一致；deck 侧不依赖 engine 模块）。
const ENGINE_BLOCK: usize = 256;
/// timestretch 引擎速率上限（MIN/MAX_TEMPO_RATE = 4.0），喂入 demand 用。
const MAX_ENGINE_RATE: f64 = 4.0;
/// profile 切换阈值（半音）：|key shift| 超过它时 r/p 可能超出 Keylock
/// profile ±20.5% 全 keylock 带 → 重建 WideKeylock。±3 半音内窄频
/// 引擎（r/p ∈ [0.84, 1.19] @ r=1）质量无损，省 3.3× CPU。
const WIDE_SHIFT_ST: f64 = 3.0;
/// EOF 判停：finish() 后 source_position 连续冻结块数（spike 实测
/// 排空尾 ≈8×256+尾块，8 块足够且不会误判内容期欠载——欠载时
/// eof_fed 为 false）。
const EOF_STALL_BLOCKS: u32 = 8;
/// 对拍临时加减速倍率（按住按钮期间生效；-8% 用 1/1.08 保持上下对称）。
const NUDGE_UP: f64 = 1.08;
const NUDGE_DOWN: f64 = 1.0 / NUDGE_UP;

// ---- P14 sync 一次性对齐参数（初值，调参见实现方案.md P14 参数表）----
/// 对齐时间常数（拍）：sync 开启沿触发一次性 nudge，corr = err/τ 使
/// 相位差指数衰减（τ ≈ 1 拍 → 0.25 拍偏移约 1.6s 收敛进死区 @120BPM），
/// 收敛即停、此后仅速率锁。
const SYNC_ALIGN_BEATS: f64 = 1.0;
/// 对齐 nudge 速率修正上限（±50%）：仅对齐窗口内生效（半拍偏移
/// 约 1 拍拉齐），收敛后速率回落 target。
const SYNC_MAX_CORR: f64 = 0.5;
/// 相位死区（拍）：|err| 低于它视为对齐（锁定目标速率、不再追相位）。
/// 0.01 拍 ≈ 5ms @120BPM（保证收敛残差 < 10ms 验收线）。
const SYNC_DEADZONE_BEATS: f64 = 0.01;

// ---- P15 推子软接管（回位后推子才有效）----
/// 回位带（速率百分点）：推子位置与当前速率差距 ≤ 此值视为"在同步速率
/// 位置"（回位）。带内小步离开 = 穿过目标位置（接管/锁定切换）；带外
/// 小步跨过目标（符号翻转）同义。
const FADER_TAKEOVER_EPS: f64 = 0.5;
/// 回位判定的小步上限（百分点/块）：连续拖动每块步长 ≤ 此值才视为拖动
/// 穿过；触摸跳变（大步）不视为穿过/离开——推子未回位不生效。
const FADER_STEP_MAX: f64 = 3.0;

// ---- P10.3 loop 无 reset 回绕（deck 侧缓冲喂入）----
/// 环缓冲帧上限：min(64 拍, 30s)。超限环不缓冲（切环回退 reset 路径）。
const LOOP_BUF_MAX_BEATS: f64 = 64.0;
const LOOP_BUF_MAX_SECS: f64 = 30.0;

/// P22-A 接缝交叉淡化长度（帧）：4ms@48k。覆盖圈首 wrap 接缝（blend_at=0）
/// 与 P22-B 偏移入环接缝（blend_at=d）；等功率 cos²+sin²=1 淡入淡出，
/// 任意波形跳变在 bl 帧内平滑过渡（残余瞬态 ≤ bl·斜率，无感）。
const LOOP_BLEND_FRAMES: usize = 192;

/// P10.3 loop 喂入状态机（切环不动 keylocker 引擎：缓冲喂入无 reset、
/// 无欠载缝隙——vendored graph.rs loop_wrap_is_gapless_across_ratios 同款用法）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LoopFeed {
    /// 无环或环超限：直喂 ring（超限环切环回退 reset 路径）。
    Idle,
    /// 环激活：喂 ring 的同时把 [loop_in, loop_out) 帧拷入缓冲。
    Capturing,
    /// 已切环：改从缓冲循环喂入（播头按 loop_sp_anchor 折返显示）。
    FromBuffer,
}

/// P10.3 环缓冲：[loop_in, loop_out) 原始立体声帧（交织 f32）。
/// 捕获完成 = frames == 环长（复用判定）；cursor = FromBuffer 喂入游标。
///
/// P22-A 接缝淡化：blend 是 data 中 [blend_at, blend_at+blend_len)
/// 区域的替代切片——圈首 wrap 接缝（blend_at=0）或 P22-B 偏移入环
/// 接缝（blend_at=d）处喂 blend 而非 data。cursor 语义不变：一圈仍
/// 恰好喂 frames 帧，播头公式与 P11.1 退出锚点不受影响。
struct LoopBuf {
    data: Vec<f32>,
    frames: usize,
    cursor: usize,
    blend: Vec<f32>,
    blend_len: usize,
    blend_at: usize,
    /// P22-B 未排空回填帧数：>0 时 data 前区 [frames − pending, frames)
    ///（或全圈布防时整个 data）等侧环回填；排空到 0 才允许入环判定
    ///（loop_wrap 守卫 / 偏移入环）。seek/load/去激活清 0。
    backfill_pending: usize,
    /// P22-B 回填布防型（arm 时定）：true = 全圈回填（feed ≥ out 的等待
    /// 窗，回填完成即偏移入环）；false = 部分回填。仅 pending > 0 时有
    /// 意义；回填停滞兜底的放行阈值按此分流（全圈 2s / 部分 1 chunk）。
    backfill_full: bool,
}

impl LoopBuf {
    fn empty() -> Self {
        Self {
            data: Vec::new(),
            frames: 0,
            cursor: 0,
            blend: Vec::new(),
            blend_len: 0,
            blend_at: 0,
            backfill_pending: 0,
            backfill_full: false,
        }
    }

    fn with_capacity(frames: usize) -> Self {
        Self {
            data: Vec::with_capacity(frames * 2),
            frames: 0,
            cursor: 0,
            blend: Vec::with_capacity(LOOP_BLEND_FRAMES * 2),
            blend_len: 0,
            blend_at: 0,
            backfill_pending: 0,
            backfill_full: false,
        }
    }

    /// P22-A：cursor 是否落在 blend 区（区内喂 blend 切片）。
    fn blend_covers(&self, cursor: usize) -> bool {
        cursor >= self.blend_at && cursor < self.blend_at + self.blend_len
    }

    /// P22-A：cursor 所在喂入段的终点——blend 区边界或圈尾
    ///（cursor 在区前/区内/区后三段的界限）。
    fn segment_end(&self, cursor: usize) -> usize {
        if cursor < self.blend_at {
            self.blend_at
        } else if cursor < self.blend_at + self.blend_len {
            self.blend_at + self.blend_len
        } else {
            self.frames
        }
    }

    /// P22-A：预计算圈首接缝 blend——圈尾 [n−bl, n) 等功率淡出 ×
    /// 圈头 [0, bl) 淡入。blend 替代 data[0..bl)，每圈仍喂 n 帧；
    /// 捕获内容恒定 → 计算一次全程复用（含首圈 Capturing→FromBuffer
    /// 接缝）。短环钳半（blend 区不得与圈尾区重叠取错样本）。
    fn rebuild_wrap_blend(&mut self, n_frames: usize) {
        let bl = LOOP_BLEND_FRAMES.min(n_frames / 2);
        self.blend.clear();
        for i in 0..bl {
            let t = ((i as f32 + 0.5) / bl as f32) * (std::f32::consts::PI / 2.0);
            let (g_out, g_in) = (t.cos(), t.sin());
            for ch in 0..2 {
                let v = self.data[(n_frames - bl + i) * 2 + ch] * g_out
                    + self.data[i * 2 + ch] * g_in;
                self.blend.push(v);
            }
        }
        self.blend_len = bl;
        self.blend_at = 0;
    }
}

/// 环缓冲帧上限（帧）：min(64 拍, 30s) @sr。
fn loop_buf_cap_frames(bpm: f64, sr: f64) -> u64 {
    let by_beats = (LOOP_BUF_MAX_BEATS * 60.0 / bpm * sr) as u64;
    let by_secs = (LOOP_BUF_MAX_SECS * sr) as u64;
    by_beats.min(by_secs)
}

/// 音轨总帧数（48kHz 时间轴），由读取线程在 EOF 时写入。
pub type TrackFrames = Arc<AtomicU64>;

pub struct Deck {
    pub index: usize,
    sr: f64,

    // 传输
    playing: bool,
    rate: f64,
    pos: f64, // 当前帧位置（48kHz，f64 精度足够）
    track_frames: TrackFrames,

    // 数据源
    loaded: bool,
    epoch: u32,
    chunk_rx: Option<ringbuf::HeapCons<Chunk>>,
    /// 最近两个 chunk 的 (起始帧, 数据)：插值读 idx/idx+1 跨边界时
    /// 会同时需要新旧两块，只缓存一块会在 rate<1 时把仍在用的旧块挤掉。
    chunks: [(Option<u64>, Box<[f32]>); 2],
    chunk_slot: usize, // 下一 chunk 写入哪个槽（轮换）
    reader_cmd: Option<Sender<ReaderCmd>>,
    _reader_handle: Option<JoinHandle<()>>,
    /// 当前音轨路径（EOF 后读取线程退出，seek 需据此重生线程）。
    path: Option<std::path::PathBuf>,

    // keylock 引擎 + key shift（None = 构建失败，回退线性插值路径）
    keylocker: Option<Box<dyn Keylocker>>,
    pitch_shifter: PitchShifter,
    /// 引擎渲染暂存：升调（p>1）时每块渲染 256×p 帧（上限 513），
    /// load 时预分配，运行时零分配。
    engine_scratch: Vec<f32>,
    /// 引擎渲染帧数的分数余量：256×p 非整数时用累加器均摊，
    /// 长期供给恰好等于 pitch 级消费，carry 不漂移。
    shifter_frac: f64,
    /// 喂入状态（fed 坐标：播头 = feed_base + source_position()）。
    feed_pos: u64, // 下一个待喂入的音轨帧
    feed_base: u64, // 引擎 fed 0 对应的音轨帧（reset 时重锚定）
    feed_chunk: Option<(u64, Box<[f32]>, usize)>, // (start, data, 帧游标)
    /// P22-B 回填侧环（reader 回填 [loop_in, feed_pos) 时推这里，deck 每块
    /// 排空到环缓冲前区）。load/respawn 时随 reader 重建，stop_reader 置 None。
    side_rx: Option<ringbuf::HeapCons<Chunk>>,
    /// P22-B 最近喂入尾部（LOOP_BLEND_FRAMES 帧交织 f32，每次 push 尾随
    /// 更新）：偏移入环 entry blend 的淡出分量——自然尾已在 keylocker
    /// 内不可撤回，用尾部缓冲重建。
    feed_tail: [f32; LOOP_BLEND_FRAMES * 2],
    eof_fed: bool,  // finish() 已成功推入冲刷 padding
    eof_stall: u32, // eof_fed 后连续 position 冻结块数
    last_sp: f64,
    /// 引擎控制去重：事件只在值变化时发送（避免每块事件积压）。
    last_sent_rate: Option<f64>,
    keylock_sent: Option<bool>,
    /// 上一块重建失败标记：同一 shift 状态段内不重试（防每块分配轰炸）。
    rebuild_pending: bool,
    // 参数快照
    pitch: f64, // key shift 半音
    keylock_on: bool,
    /// beat sync 开关（P5；engine.rs 在 update_params 后调 apply_sync）。
    sync: bool,
    /// P14 一次性对齐完成标记：sync 开启沿复位，apply_sync 相位差收敛进
    /// 死区后置位；置位后不再追相位（仅速率锁），sync 下 seek/微调不再
    /// 被拉回。
    sync_align_done: bool,
    /// P15 推子脱开锁存（非 sync）：取消 sync 后 rate 保持 sync 期间值
    /// （推子仅解锁、播放状态不变，P14），推子位置与当前速率可能脱开；
    /// 置位后滑杆移动需先回位（进入当前速率 ±FADER_TAKEOVER_EPS 带）
    /// 才恢复直通（软接管，防触摸跳变直接拉速）。
    fader_detached: bool,
    /// P15 推子暂时接管（sync 期间）：false = 速率锁（rate = target）；
    /// 推子小步穿过目标速率带后翻转（离开带/带外跨过）→ true = 暂时
    /// 加减速（rate = 推子），回到带内重新锁定。接管/锁定均置位
    /// sync_align_done——操作后不再自动对拍（只在 sync 开启沿对齐）。
    /// sync 开启沿/换曲复位。
    fader_armed: bool,
    /// P15 上一块 apply_sync 时的滑杆位置（速率值，纯推子不含 nudge）：
    /// 穿过判定（小步 + 符号翻转/带内离开）基准，apply_sync 块尾更新
    /// （update_params 不更新，保证 step 是真实跨块步长）。
    fader_prev_rate: f64,
    /// P12 最近滑杆速率（纯推子值）：sync 开启时滑杆不再每块重写 rate，
    /// 改为检测变化——变化即用户接管意图，在 sync 失效（leader 停播/
    /// 无网格）时恢复滑杆调速；apply_sync 活跃覆写后清接管标志（同步优先）。
    last_slider_rate: Option<f64>,
    slider_takeover: bool,

    // beat loop（秒；外部跳转出环时引擎清零 active，bus 与字段同步写）
    loop_active: bool,
    loop_in: f64,
    loop_out: f64,
    /// P18 manual loop 边沿检测用的上一块总线快照（bus 激活/改边界 →
    /// 引擎同步捕获状态机；见 update_params）。
    loop_active_prev: bool,
    loop_in_prev: f64,
    loop_out_prev: f64,
    /// P10.3 loop 喂入状态（缓冲捕获/回喂；见 LoopFeed）。
    loop_feed: LoopFeed,
    /// P10.3 环缓冲（[loop_in, loop_out) 帧；去激活保留，同尺寸复用）。
    loop_buf: LoopBuf,
    /// 缓冲对应的 loop_in 帧号（复用判定：与现环一致才可跳过捕获）。
    loop_buf_anchor: u64,
    /// 切环瞬间 kl.source_position()：FromBuffer 显示播头折返基准。
    loop_sp_anchor: f64,
    /// P11.1 收尾圈完成后的线性显示锚点：pos = pos_base + source_position()。
    /// 退出瞬间锚点 = feed_pos − feed_pos_at_loop_start − loop_pushed
    /// （P22-B 改：引擎标签 = 原始喂入计数 − 管线延迟，set_track_position
    /// 只重锚工件不锚标签；Δ = feed_pos − pushed_total 吸收环喂入与
    /// feed_pos 推进的差——常规环 Δ = −W×len、偏移入环 Δ = +d，同式
    /// 覆盖，延迟精确抵消 → 播头 = 正在出声的内容位置）。seek/load/
    /// 引擎重建清空（坐标系重建）。
    pos_base: Option<f64>,
    /// P22-B 偏移入环的圈内偏移（帧）：d = (feed_pos − loop_out×sr) mod n。
    /// 播头公式映射入环相位；退出续点每圈 +n（P + k×n）。非偏移入环
    /// 恒 0（loop_wrap 常规路径）；seek/load/去激活清 0。
    loop_offset: u64,
    /// P22-B FromBuffer 起点 feed_pos（退出锚点 Δ 的基准；常规 = loop_out、
    /// 偏移 = 入环位置 P）。与 loop_pushed 一起恢复环期间 pushed_total。
    feed_pos_at_loop_start: u64,
    /// P22-B 自 FromBuffer 起累计的环喂入帧数（feed_from_loop_buf 每次
    /// 接受 +accepted；FromBuffer 进入时清零）。
    loop_pushed: u64,
    /// P22-C 保留窗口的旧世代：seek_internal 落点命中 ring 已解码窗口时
    /// 记窗口尾块世代——接受规则同时认 self.epoch 与 preserve，首块新
    /// 世代 chunk 接受后清空（reader 已续推到窗口尾，之后世代对齐）。
    preserve: Option<u32>,

    // DSP
    eq: ThreeBandEq,
    filter: DeckFilter,
    gain: Smoother,
    /// 通道增益（dB 平滑到线性倍率；调音台 trim，跨曲保持，load 不重置）。
    gain_db: Smoother,
    tone_phase: f64,
    /// FX rack（EQ 之后、gain 之前；槽位换型仅用户操作触发）。
    rack: FxRack,
    /// 槽位类型快照（换型去抖，仿 rebuild_pending）。
    fx_type_snapshot: [Option<EffectId>; 8],

    // 控制句柄（每块快照）
    pub ctl: DeckControls,
}

pub struct DeckControls {
    pub play: ControlHandle,
    pub rate: ControlHandle,
    pub eq_low: ControlHandle,
    pub eq_mid: ControlHandle,
    pub eq_high: ControlHandle,
    pub volume: ControlHandle,
    pub gain: ControlHandle,
    pub filter: ControlHandle,
    pub pitch: ControlHandle,
    pub keylock: ControlHandle,
    pub bpm: ControlHandle,
    pub grid_bpm: ControlHandle,
    pub grid_offset: ControlHandle,
    pub sync: ControlHandle,
    pub quantize: ControlHandle,
    pub nudge: ControlHandle,
    pub playhead: ControlHandle,
    pub vu: ControlHandle,
    pub duration: ControlHandle,
    pub loaded: ControlHandle,
    pub loop_active: ControlHandle,
    pub loop_in: ControlHandle,
    pub loop_out: ControlHandle,
    pub fx_type: [ControlHandle; 8],
    pub fx_enable: [ControlHandle; 8],
    pub fx_drywet: [ControlHandle; 8],
    pub fx_p: [[ControlHandle; 4]; 8],
}

impl DeckControls {
    pub fn new(bus: &hypermixx_core::ControlBus, index: usize) -> Self {
        use hypermixx_core::paths;
        Self {
            play: bus.control(&paths::deck_play(index)),
            rate: bus.control(&paths::deck_rate(index)),
            eq_low: bus.control(&paths::deck_eq_low(index)),
            eq_mid: bus.control(&paths::deck_eq_mid(index)),
            eq_high: bus.control(&paths::deck_eq_high(index)),
            volume: bus.control(&paths::deck_volume(index)),
            gain: bus.control(&paths::deck_gain(index)),
            filter: bus.control(&paths::deck_filter(index)),
            pitch: bus.control(&paths::deck_pitch(index)),
            keylock: bus.control(&paths::deck_keylock(index)),
            bpm: bus.control(&paths::deck_bpm(index)),
            grid_bpm: bus.control(&paths::deck_grid_bpm(index)),
            grid_offset: bus.control(&paths::deck_grid_offset(index)),
            sync: bus.control(&paths::deck_sync(index)),
            quantize: bus.control(&paths::deck_quantize(index)),
            nudge: bus.control(&paths::deck_nudge(index)),
            playhead: bus.control(&paths::deck_playhead(index)),
            vu: bus.control(&paths::deck_vu(index)),
            duration: bus.control(&paths::deck_duration(index)),
            loaded: bus.control(&paths::deck_loaded(index)),
            loop_active: bus.control(&paths::deck_loop_active(index)),
            loop_in: bus.control(&paths::deck_loop_in(index)),
            loop_out: bus.control(&paths::deck_loop_out(index)),
            fx_type: std::array::from_fn(|s| bus.control(&paths::deck_fx_type(index, s))),
            fx_enable: std::array::from_fn(|s| bus.control(&paths::deck_fx_enable(index, s))),
            fx_drywet: std::array::from_fn(|s| bus.control(&paths::deck_fx_drywet(index, s))),
            fx_p: std::array::from_fn(|s| {
                std::array::from_fn(|p| bus.control(&paths::deck_fx_p(index, s, p)))
            }),
        }
    }
}

/// leader deck 的同步快照（engine.rs 聚合只读；follower 由 apply_sync 消费）。
#[derive(Clone, Copy)]
pub struct SyncLeader {
    pub loaded: bool,
    pub playing: bool,
    pub grid_bpm: f64,
    pub grid_offset: f64,
    /// 实际播放速率（源时间/真实时间；keylock 链的输出节奏恒等于它）。
    pub tempo_rate: f64,
    /// 延迟补偿的当前出声位置（秒）。
    pub position_secs: f64,
}

impl Deck {
    pub fn new(index: usize, sr: u32, bus: &hypermixx_core::ControlBus) -> Self {
        let coeff = 1.0 - (-1.0 / (0.01 * sr as f64)).exp(); // 10ms 增益平滑
        Self {
            index,
            sr: sr as f64,
            playing: false,
            rate: 1.0,
            pos: 0.0,
            track_frames: Arc::new(AtomicU64::new(0)),
            loaded: false,
            epoch: 0,
            chunk_rx: None,
            chunks: [
                (None, vec![0.0; 0].into_boxed_slice()),
                (None, vec![0.0; 0].into_boxed_slice()),
            ],
            chunk_slot: 0,
            reader_cmd: None,
            _reader_handle: None,
            path: None,
            keylocker: None,
            pitch_shifter: PitchShifter::new(),
            engine_scratch: vec![0.0; (ENGINE_BLOCK * 2 + 2) * 2],
            shifter_frac: 0.0,
            feed_pos: 0,
            feed_base: 0,
            feed_chunk: None,
            side_rx: None,
            feed_tail: [0.0; LOOP_BLEND_FRAMES * 2],
            eof_fed: false,
            eof_stall: 0,
            last_sp: 0.0,
            last_sent_rate: None,
            keylock_sent: None,
            rebuild_pending: false,
            pitch: 0.0,
            keylock_on: true,
            sync: false,
            sync_align_done: false,
            fader_detached: false,
            fader_armed: false,
            fader_prev_rate: 0.0,
            last_slider_rate: None,
            slider_takeover: false,
            loop_active: false,
            loop_in: 0.0,
            loop_out: 0.0,
            loop_active_prev: false,
            loop_in_prev: 0.0,
            loop_out_prev: 0.0,
            loop_feed: LoopFeed::Idle,
            loop_buf: LoopBuf::empty(),
            loop_buf_anchor: 0,
            loop_sp_anchor: 0.0,
            pos_base: None,
            loop_offset: 0,
            feed_pos_at_loop_start: 0,
            loop_pushed: 0,
            preserve: None,
            eq: ThreeBandEq::new(sr as f32),
            filter: DeckFilter::new(sr as f32),
            gain: Smoother::new(1.0, coeff as f32),
            gain_db: Smoother::new(1.0, coeff as f32),
            rack: FxRack::new(sr as f32),
            fx_type_snapshot: [None; 8],
            tone_phase: 0.0,
            ctl: DeckControls::new(bus, index),
        }
    }

    /// 实际引擎速率：keylock 开 = r/p（key shift 链，推导见 dsp/pitch.rs），
    /// 关 = r。P15 起 nudge 倍率叠加在引擎轴（不并入 self.rate）——
    /// sync 速率锁与显示 BPM 不含临时加减速（"暂时加减速时波形不缩放"），
    /// 且 sync 速率锁不再覆盖对拍 nudge（sync 时也能暂时加减速）。
    fn engine_rate(&self) -> f64 {
        let shift = if self.keylock_on { self.pitch } else { 0.0 };
        let nudge = self.ctl.nudge.get();
        let nudge_factor = if nudge > 0.5 {
            NUDGE_UP
        } else if nudge < -0.5 {
            NUDGE_DOWN
        } else {
            1.0
        };
        self.rate / 2f64.powf(shift / 12.0) * nudge_factor
    }

    /// 滑杆速率（纯推子值，不含 nudge）：P15 起 nudge 移至引擎轴——软接管
    /// 回位判定用推子位置，nudge 不参与。sync 开启时由 apply_sync 覆写
    /// self.rate（同步优先）；无网格 fallback 分支用此值恢复滑杆控制。
    fn slider_rate(&self) -> f64 {
        1.0 + self.ctl.rate.get() * 0.01
    }

    /// 每块开始时从控制总线快照参数。
    pub fn update_params(&mut self) {
        self.playing = self.ctl.play.get() > 0.5;
        self.loop_active = self.ctl.loop_active.get() > 0.5;
        self.loop_in = self.ctl.loop_in.get().max(0.0);
        self.loop_out = self.ctl.loop_out.get().max(0.0);
        // P18 manual loop：总线边沿检测——UI 直接写 loop_in/loop_out/
        // loop_active 总线（ManualLoop 控件，零桥改动）。激活沿或激活中
        // 边界变化 → 按 set_beat_loop 同款逻辑准备捕获（缓冲复用判定/
        // 容量上限/越界回跳），否则 bus 激活只走线性回跳路径（每圈
        // 全预卷 seek ≈33ms 静音，且无缓冲循环）。
        let loop_active_was = self.loop_active_prev;
        let loop_in_was = self.loop_in_prev;
        let loop_out_was = self.loop_out_prev;
        self.loop_active_prev = self.loop_active;
        self.loop_in_prev = self.loop_in;
        self.loop_out_prev = self.loop_out;
        if self.loop_active
            && (!loop_active_was
                || self.loop_in != loop_in_was
                || self.loop_out != loop_out_was)
        {
            let li_frames = (self.loop_in * self.sr) as u64;
            let loop_frames = ((self.loop_out - self.loop_in) * self.sr) as u64;
            // P22-B：prepare_loop_capture 抽共用逻辑（set_beat_loop 同款），
            // 进入捕获时按 feed 相对 loop 的位置布防回填（三路）；
            // 激活瞬间已过 loop_out 由 arm 的全圈回填 + 偏移入环处理
            //（不再全预卷 seek 回跳；超限环仍由块首检查回跳）。
            self.prepare_loop_capture(li_frames, loop_frames);
        }
        // P14：sync 边沿检测（rate 段用上一块状态）。
        let sync_was_on = self.sync;
        self.sync = self.ctl.sync.get() > 0.5;
        if self.sync && !sync_was_on {
            self.sync_align_done = false; // 开启沿：一次性快速对齐
            self.fader_armed = false; // 开启沿：sync 速率锁复位
        }
        // P15 滑杆语义（软接管）：
        // - sync 开启：rate 由 apply_sync 决定（速率锁 + 推子软接管，见
        //   apply_sync）。这里仅检测滑杆变化（P12 接管补丁：leader 失效
        //   /无网格 fallback 时恢复滑杆调速；apply_sync 活跃时其覆写优先）。
        // - sync 刚关闭：推子仅解锁——rate 保持 sync 期间值（播放状态
        //   不变，P14）。推子位置与当前速率可能脱开 → fader_detached
        //   置位，此后滑杆移动需先回位（进入当前速率 ±EPS 带）才恢复
        //   直通（软接管，防触摸跳变直接拉速）。
        // - 非 sync 直通：rate = 滑杆。
        let slider = self.slider_rate();
        if self.sync {
            if self.last_slider_rate != Some(slider) {
                self.last_slider_rate = Some(slider);
                self.rate = slider;
                self.slider_takeover = true;
            }
        } else if sync_was_on {
            self.last_slider_rate = Some(slider); // 记录当前滑杆位置（≠ 实际速率）
            // 回位判定按百分点比较（EPS 定义即百分点，rate 单位差 ×100）
            self.fader_detached = (slider - self.rate).abs() * 100.0 > FADER_TAKEOVER_EPS;
            self.slider_takeover = false;
        } else if self.last_slider_rate != Some(slider) {
            // 非 sync：滑杆变化——已回位（进入当前速率 ±EPS 带）或已直通
            // → rate = 滑杆；未回位 → 保持速率（推子自由移动不生效）。
            if !self.fader_detached
                || (slider - self.rate).abs() * 100.0 <= FADER_TAKEOVER_EPS
            {
                self.rate = slider;
                self.fader_detached = false;
            }
            self.last_slider_rate = Some(slider);
            self.slider_takeover = false;
        } else if !self.fader_detached {
            // 非 sync 且滑杆未动：维持直通（rate 与滑杆同值，重写无害）
            self.rate = slider;
        }
        // 注：fader_prev_rate 不在本处更新——apply_sync 的穿过判定需要
        // "上一块 apply_sync 时的推子位置"，同块更新会令 step 恒为 0。
        self.keylock_on = self.ctl.keylock.get() > 0.5;
        self.pitch = self.ctl.pitch.get();
        self.eq.set_low_db(self.ctl.eq_low.get() as f32);
        self.eq.set_mid_db(self.ctl.eq_mid.get() as f32);
        self.eq.set_high_db(self.ctl.eq_high.get() as f32);
        self.gain
            .set_target(self.ctl.volume.get().clamp(0.0, 1.0) as f32);
        // 通道增益 dB → 线性倍率（10^0 = 1.0 精确 → 默认 bitwise 恒等）
        self.gain_db.set_target(
            10f64.powf(self.ctl.gain.get().clamp(-12.0, 12.0) / 20.0) as f32,
        );
        self.filter.set_knob(self.ctl.filter.get().clamp(-1.0, 1.0) as f32);

        let engine_rate = self.engine_rate();
        if let Some(kl) = self.keylocker.as_mut() {
            // key shift 仅 keylock 开启时生效（Mixxx 约定：关闭 = 纯 varispeed，
            // 音高随变速；pitch 级旁路）。
            let shift = if self.keylock_on { self.pitch } else { 0.0 };
            if self.keylock_sent != Some(self.keylock_on) {
                kl.set_keylock(self.keylock_on);
                self.keylock_sent = Some(self.keylock_on);
            }
            let need_wide = shift.abs() > WIDE_SHIFT_ST;
            if kl.is_wide() != need_wide {
                // profile 切换：一次性重建（用户操作触发，与 load/seek 同类代价；
                // 新引擎从 feed_pos 续喂，≈45ms 管线填充 + declick 淡入）
                if !self.rebuild_pending {
                    self.rebuild_keylocker(engine_rate);
                }
            } else {
                self.rebuild_pending = false;
                // sync 开启时滑杆速率不直发引擎：apply_sync 同块随后用
                // 有效速率覆写（P10.1），这里发滑杆值只会先污染一瞬。
                // 例外：滑杆接管（sync 已失效、apply_sync 不再覆写）直发，
                // 否则滑杆调速失联（P12 补丁）。
                if (!self.sync || self.slider_takeover) && self.last_sent_rate != Some(engine_rate) {
                    kl.set_rate(engine_rate);
                    self.last_sent_rate = Some(engine_rate);
                }
            }
            self.pitch_shifter.set_semitones(shift);
        }
        // 实时 BPM 显示（grid × 输出节奏；无网格显示 0）。
        // 输出节奏恒等于 self.rate（key shift 链保持节奏 = r，音高 = p 独立），
        // 与引擎内部 engine_rate = r/p 无关。
        let grid = self.ctl.grid_bpm.get();
        self.ctl.bpm.set(if grid > 0.0 { grid * self.rate } else { 0.0 });

        // FX rack：换型检测（一次性实例化，音频线程唯一允许的分配点）
        // + 每块参数快照。换型时把 manifest 默认值写回总线（读回一致，
        // 仿引擎已写的 playhead/vu/bpm），本块即按新默认值处理。
        // enable 不随换型置 1：只尊重 ON 开关（P8 修复——此前强制
        // 置 1 导致面板开关状态与引擎脱钩，"开关关着效果却响"）。
        for slot in 0..8 {
            let id = EffectId::from_bus(self.ctl.fx_type[slot].get());
            if id != self.fx_type_snapshot[slot] {
                self.fx_type_snapshot[slot] = id;
                self.rack.set_slot_type(slot, id);
                if let Some(id) = id {
                    let m = manifest(id);
                    for (i, param) in m.params.iter().enumerate() {
                        self.ctl.fx_p[slot][i].set(param.default as f64);
                    }
                }
            }
            let enabled = self.ctl.fx_enable[slot].get() > 0.5;
            let drywet = self.ctl.fx_drywet[slot].get() as f32;
            let params: [f32; 4] = std::array::from_fn(|i| self.ctl.fx_p[slot][i].get() as f32);
            self.rack.set_slot_params(slot, enabled, drywet, params);
        }

        // beat loop 回跳（仅播放中）。引擎路径：feed 越过 loop_out 时在
        // feed_keylocker 内切环（P10.3 缓冲喂入无 reset，精确到帧）；
        // legacy 线性路径与超限环（loop_feed == Idle，不缓冲）无 feed
        // 机制，沿用 audible 越界回跳（reset 路径）。
        if self.loop_active
            && self.playing
            && self.loop_out > self.loop_in
            && (self.keylocker.is_none() || self.loop_feed == LoopFeed::Idle)
            && self.pos >= self.loop_out * self.sr
        {
            self.seek_internal(self.loop_in, false);
        }
        // loop 停用（bus 关/seek 出环）：Capturing 直接回 Idle（坐标未污染）；
        // FromBuffer 收尾圈——喂完当前圈后在圈界切回 ring（见
        // feed_from_loop_buf），音频从 loop_out 无缝续进、播头不跳变。
        // 环缓冲保留复用（同尺寸二次激活跳过捕获）。
        if !self.loop_active && self.loop_feed == LoopFeed::Capturing {
            self.loop_feed = LoopFeed::Idle;
            self.loop_buf.cursor = 0;
            // 捕获中途中止：缓冲作废（frames 归零——含 P22-B out_past
            // 等待窗的"完整帧数"，否则同环二次激活会误判已完整而喂陈旧
            // 内容）；回填一并作废（侧环残留由下次 arm 清空/位置钳制）。
            self.loop_buf.frames = 0;
            self.loop_buf.backfill_pending = 0;
            self.loop_offset = 0;
            self.feed_pos_at_loop_start = 0;
            self.loop_pushed = 0;
        }
    }

    /// 块首拍上下文（process 内调用：sync 可能在 update_params 之后改
    /// rate，这里拿到的才是本块真实出声节奏；legacy 路径 pos 在读循环
    /// 内推进，须在读取前取）。
    fn fx_context(&self) -> FxContext {
        let grid = BeatGrid {
            bpm: self.ctl.grid_bpm.get(),
            offset_secs: self.ctl.grid_offset.get(),
        };
        if !grid.is_valid() {
            return FxContext::default();
        }
        let t = self.pos / self.sr;
        let ph = grid.phase_at(t);
        FxContext {
            beats_total: grid.beat_index_at(t) as f64 + ph,
            beat_phase_01: ph as f32,
            beat_period_secs: 60.0 / (grid.bpm * self.rate),
            grid_valid: true,
        }
    }

    /// 重建引擎切换 profile（key shift 跨 ±3 半音阈值）。
    fn rebuild_keylocker(&mut self, engine_rate: f64) {
        let need_wide = self.keylocker.as_ref().is_some_and(|kl| !kl.is_wide());
        match TimestretchLocker::build(self.sr as u32, need_wide) {
            Ok(mut kl) => {
                kl.set_track_position(self.feed_pos);
                kl.set_keylock(self.keylock_on);
                kl.set_rate(engine_rate);
                kl.warm_start(1); // 最小预卷：priming 收尾的 declick 淡入
                self.keylocker = Some(Box::new(kl));
                self.feed_base = self.feed_pos; // 新引擎 fed 坐标从 0 重新计
                self.pos_base = None; // P11.1：收尾圈锚点随引擎重建作废
                if self.loop_feed == LoopFeed::FromBuffer {
                    // 新引擎 source_position 从 0 重计：刷新折返锚点
                    //（喂入从缓冲续，环绕不因 profile 切换中断）
                    self.loop_sp_anchor = self.keylocker.as_ref().unwrap().source_position();
                }
                self.last_sent_rate = Some(engine_rate);
                self.keylock_sent = Some(self.keylock_on);
                self.rebuild_pending = false;
                self.eof_fed = false;
                self.eof_stall = 0;
                log::info!(
                    "deck {}: keylock 引擎切换为 {} profile",
                    self.index + 1,
                    if need_wide { "WideKeylock" } else { "Keylock" }
                );
            }
            Err(e) => {
                // 保留旧引擎（profile 不匹配但可用）；本段 shift 状态内不重试。
                // shift 回到匹配区时 rebuild_pending 复位。
                self.rebuild_pending = true;
                log::error!("deck {}: keylock 引擎重建失败: {e:#}", self.index + 1);
            }
        }
    }

    /// sync 开关快照（engine.rs 的 follower/leader 判定用）。
    pub fn sync_on(&self) -> bool {
        self.sync
    }

    /// leader 侧只读快照（engine.rs 聚合）。
    pub fn sync_leader_snapshot(&self) -> SyncLeader {
        SyncLeader {
            loaded: self.loaded,
            playing: self.playing,
            grid_bpm: self.ctl.grid_bpm.get(),
            grid_offset: self.ctl.grid_offset.get(),
            tempo_rate: self.rate,
            position_secs: self.pos / self.sr,
        }
    }

    /// beat sync（engine.rs 在 update_params 后、process 前调用；仅 follower）。
    ///
    /// P14 重写为「开启沿一次性快速对齐 + 持续速率锁」（旧 P10.1 每块
    /// 连续 PI 相位锁已删——它把 sync 下的手动微调/seek 拉回原位，
    /// 用户报"sync 下微调进度失败"）：
    /// - 目标速率 = leader.grid_bpm × leader.tempo_rate / follower.grid_bpm，
    ///   clamp [0.5, 2.0]。**速率锁每块（值变化时）直发引擎**——两侧
    ///   bpm 一致，leader 拉 tempo 推子后 follower 立即跟随。
    /// - 相位对齐仅 sync 开启沿做一次（update_params 复位
    ///   sync_align_done）：corr = err/SYNC_ALIGN_BEATS 指数衰减
    ///   （τ≈1 拍，封顶 SYNC_MAX_CORR），|err| < SYNC_DEADZONE_BEATS
    ///   即锁定；此后不再追相位，sync 下 seek/微调进度生效、不被拉回。
    /// - 修正下发到引擎轴：kl.set_rate(engine_rate_of(rate))，含 pitch 链
    ///   （keylock 开时引擎速率 = r/p）——旧实现把源轴 target 直接调度到
    ///   引擎轴，音高开启时修正量被 r/p 放偏。
    /// - 整拍相位偏移对 wrap(err) 不可见是固有语义（由 P10.2 网格锚点
    ///   精度缓解），sync_whole_beat_phase_offset_stays_wrapped 文档化。
    /// - P15 推子软接管（sync 期间暂时加减速）：推子小步穿过目标速率带
    ///   （带内离开 / 带外符号翻转）翻转 fader_armed（接管 ↔ 锁定）；
    ///   触摸跳变（大步）不视为穿过。接管后 rate = 推子（暂时加减速，
    ///   BPM 显示/波形窗口仍写目标锁值 → 波形不缩放）；接管/锁定均置位
    ///   sync_align_done → 操作后 sync 不再自动对拍（只在开启沿对齐）。
    pub fn apply_sync(&mut self, leader: &SyncLeader) {
        if !self.sync || !self.loaded || !self.playing || !leader.loaded || !leader.playing {
            return;
        }
        let fgrid = BeatGrid {
            bpm: self.ctl.grid_bpm.get(),
            offset_secs: self.ctl.grid_offset.get(),
        };
        let lgrid = BeatGrid {
            bpm: leader.grid_bpm,
            offset_secs: leader.grid_offset,
        };
        if !fgrid.is_valid() || !lgrid.is_valid() {
            // sync 无法启用（无网格）：回退滑杆速率——P12 起 update_params
            // 在 sync 开启时不再下发滑杆速率，这里每块重写保证滑杆可调速。
            self.rate = self.slider_rate();
            let e = self.engine_rate();
            if let Some(kl) = self.keylocker.as_mut()
                && self.last_sent_rate != Some(e)
            {
                kl.set_rate(e);
                self.last_sent_rate = Some(e);
            }
            return;
        }
        let target = (leader.grid_bpm * leader.tempo_rate / fgrid.bpm).clamp(0.5, 2.0);

        // P14：开启沿一次性快速对齐（相位差 wrap 到 ±0.5 拍，指数衰减
        // nudge）；收敛进死区后置位 sync_align_done，此后仅速率锁。
        let mut rate = if !self.sync_align_done {
            let lc = BeatClock::from_grid_at(&lgrid, leader.position_secs);
            let fc = BeatClock::from_grid_at(&fgrid, self.pos / self.sr);
            let mut err = lc.phase - fc.phase;
            if err > 0.5 {
                err -= 1.0;
            } else if err < -0.5 {
                err += 1.0;
            }
            if err.abs() < SYNC_DEADZONE_BEATS {
                self.sync_align_done = true; // 锁定：此后不再追相位
                target
            } else {
                let corr = (err / SYNC_ALIGN_BEATS).clamp(-SYNC_MAX_CORR, SYNC_MAX_CORR);
                (target * (1.0 + corr)).clamp(0.5, 2.0)
            }
        } else {
            target
        };

        // P15 推子软接管（sync 期间暂时加减速）：推子小步穿过目标速率带
        //（带内离开 / 带外符号翻转）→ fader_armed 翻转（接管 ↔ 锁定）；
        // 触摸跳变（大步）不视为穿过——推子需回位（拖动穿过当前速率
        // 位置）才有效。接管后 rate = 推子（暂时加减速），回到带内重新
        // 锁定。接管/锁定均不触发重新对拍（sync_align_done 不受影响）。
        let slider = self.slider_rate();
        let prev = self.fader_prev_rate;
        // 带判定/步长按百分点（EPS/STEP_MAX 定义即百分点，rate 单位 ×100）
        let slider_pct = (slider - 1.0) * 100.0;
        let prev_pct = (prev - 1.0) * 100.0;
        let target_pct = (target - 1.0) * 100.0;
        let step = (slider_pct - prev_pct).abs();
        if step > 0.0 && step <= FADER_STEP_MAX {
            let in_band_prev = (prev_pct - target_pct).abs() <= FADER_TAKEOVER_EPS;
            let in_band_now = (slider_pct - target_pct).abs() <= FADER_TAKEOVER_EPS;
            if (in_band_prev && !in_band_now)
                || (!in_band_prev
                    && !in_band_now
                    && (prev_pct - target_pct).signum() != (slider_pct - target_pct).signum())
            {
                self.fader_armed = !self.fader_armed;
                // 接管/锁定都是用户操作 → 停止对齐追相位（只在 sync
                // 开启沿对齐），操作后 sync 不再自动对拍。
                self.sync_align_done = true;
            }
        }
        if self.fader_armed {
            rate = slider.clamp(0.5, 2.0);
        }
        self.rate = rate;
        // 同步活跃覆写：清除滑杆接管标志（P12 滑杆接管只在 sync 失效时生效）
        self.slider_takeover = false;
        // 实时 BPM 显示：写速率锁目标（P15：暂时加减速时 BPM/波形不缩放
        // ——推子接管与 nudge 偏离不改变显示 BPM 与拍轴窗口）
        self.ctl.bpm.set(fgrid.bpm * target);

        // 速率每块下发（值变化时，last_sent_rate 去重）——含对齐 nudge
        // 后的实际值；线性回退路径无 keylocker：仅速率跟随（rate 只影响
        // 逐帧插值步进，见 process_legacy）。
        let e = self.engine_rate();
        if let Some(kl) = self.keylocker.as_mut()
            && self.last_sent_rate != Some(e)
        {
            kl.set_rate(e);
            self.last_sent_rate = Some(e);
        }
        // P15 穿过判定基准：记录本块推子位置（update_params 不更新此处，
        // 保证下块 step = |slider − 上一块| 是真实跨块步长）。
        self.fader_prev_rate = slider;
    }

    /// 处理一块立体声（frames 帧，写入 out[..frames*2]）。
    pub fn process(&mut self, out: &mut [f32], frames: usize) {
        let n = self.track_frames.load(Ordering::Relaxed);
        if self.playing && self.loaded && self.keylocker.is_some() {
            self.process_engine(out, frames);
        } else {
            self.process_legacy(out, frames, n);
        }

        // 输出控制（playhead / VU）
        if self.loaded {
            self.ctl.playhead.set(self.pos / self.sr);
            let d = n.max(1) as f64 / self.sr;
            if self.ctl.duration.get() != d {
                self.ctl.duration.set(d);
            }
        }
        let mut peak = 0.0f32;
        for v in out.iter().step_by(2).chain(out.iter().skip(1).step_by(2)) {
            let a = v.abs();
            if a > peak {
                peak = a;
            }
        }
        self.ctl.vu.set(peak as f64);
    }

    /// keylock 引擎路径：喂入 → 引擎渲染（256×p 帧）→ pitch → EQ → FX → gain。
    fn process_engine(&mut self, out: &mut [f32], frames: usize) {
        // 块首拍上下文（pos 在块尾才更新，此刻 = 上一块播头）
        let ctx = self.fx_context();
        // 本块引擎渲染帧数：升调（p>1）时 pitch 级每块消费 256×p 引擎帧，
        // 引擎须按比例多渲染（分数帧用累加器均摊，长期恰好相等，carry 不漂移）
        let p = self.pitch_shifter.pitch_factor();
        let engine_frames = if p > 1.0 {
            let want = ENGINE_BLOCK as f64 * p + self.shifter_frac;
            let f = want.floor() as usize;
            self.shifter_frac = want - f as f64;
            f
        } else {
            self.shifter_frac = 0.0;
            ENGINE_BLOCK
        };
        self.feed_keylocker(engine_frames);
        let kl = self.keylocker.as_mut().unwrap();
        kl.process(&mut self.engine_scratch[..engine_frames * 2]);
        // pitch 级：旁路直通 / 256×p 消费；EQ 在 pitch 之后
        //（引擎瞬态检测应吃原始音频，处理链不能前置）
        self.pitch_shifter
            .process(&self.engine_scratch[..engine_frames * 2], out, frames);
        for i in 0..frames {
            let (l, r) = self.eq.process(out[i * 2], out[i * 2 + 1]);
            out[i * 2] = l;
            out[i * 2 + 1] = r;
        }
        // deck 滤波：EQ 之后、FX 之前（旋钮 0 且稳定时内部整体旁路）
        self.filter.process(out, frames);
        // FX rack：滤波之后、gain 之前（链序：变速/keylock → EQ → 滤波 → FX → 音量）
        self.rack.process(out, frames, &ctx);
        for i in 0..frames {
            let g = self.gain.step() * self.gain_db.step();
            out[i * 2] *= g;
            out[i * 2 + 1] *= g;
        }
        // 播头 = fed 坐标 + 音轨锚点（source_position 延迟补偿、欠载冻结）；
        // P10.3 FromBuffer：缓冲循环进度折返映射回 [loop_in, loop_out)
        //（source_position 是累计帧，锚点不归零）；P11.1 收尾圈后：
        // pos_base 重锚 → 从 loop_out 无缝续进（重锚值可为负，不写 feed_base）
        let sp = kl.source_position();
        self.pos = if self.loop_feed == LoopFeed::FromBuffer {
            let len = (self.loop_out - self.loop_in) * self.sr;
            // P22-B：偏移入环时锚点 = li + loop_offset（入环相位起点），
            // 折返公式映射入环相位；非偏移入环 loop_offset == 0 恒不变。
            self.loop_in * self.sr + self.loop_offset as f64
                + (sp - self.loop_sp_anchor).rem_euclid(len)
        } else if let Some(base) = self.pos_base {
            base + sp
        } else {
            self.feed_base as f64 + sp
        };
        // EOF 判停：finish() 后 position 冻结 EOF_STALL_BLOCKS 块 → 停
        if self.eof_fed {
            if sp > self.last_sp {
                self.eof_stall = 0;
            } else {
                self.eof_stall += 1;
            }
            self.last_sp = sp;
            if self.eof_stall >= EOF_STALL_BLOCKS {
                self.playing = false;
                self.ctl.play.set(0.0);
            }
        }
    }

    /// 线性插值回退路径（引擎构建失败/未加载时的旧逻辑）。
    fn process_legacy(&mut self, out: &mut [f32], frames: usize, n: u64) {
        // 块首拍上下文（pos 在读循环内推进，须先取）
        let ctx = self.fx_context();
        for i in 0..frames {
            let (l, r) = if self.playing && self.loaded {
                if n > 0 && self.pos >= n as f64 {
                    // 播放到结尾：停；若 loop_out 钳制在曲尾则回跳续环
                    // （引擎路径块首检查覆盖此情形，legacy 逐帧推进须特判）
                    if self.loop_active && self.loop_out * self.sr >= n as f64 {
                        self.seek_internal(self.loop_in, false);
                        match self.read_stereo(self.pos) {
                            Some((l, r)) => {
                                self.pos += self.rate;
                                (l, r)
                            }
                            None => (0.0, 0.0),
                        }
                    } else {
                        self.playing = false;
                        self.ctl.play.set(0.0);
                        (0.0, 0.0)
                    }
                } else {
                    match self.read_stereo(self.pos) {
                        Some((l, r)) => {
                            self.pos += self.rate;
                            (l, r)
                        }
                        None => {
                            // 欠载：保持位置输出静音。若推进 pos，追赶目标越来越远，
                            // 一旦读取速度超过解码产能（高速播放/解码被抢占）就永久掉队。
                            (0.0, 0.0)
                        }
                    }
                }
            } else if self.playing && !self.loaded {
                // 测试音：440Hz 正弦，验证信号链
                self.tone_phase += 440.0 * 2.0 * std::f64::consts::PI / self.sr;
                if self.tone_phase > 2.0 * std::f64::consts::PI {
                    self.tone_phase -= 2.0 * std::f64::consts::PI;
                }
                let v = (self.tone_phase.sin() as f32) * 0.3;
                (v, v)
            } else {
                (0.0, 0.0)
            };
            out[i * 2] = l;
            out[i * 2 + 1] = r;
        }
        for i in 0..frames {
            let (l, r) = self.eq.process(out[i * 2], out[i * 2 + 1]);
            out[i * 2] = l;
            out[i * 2 + 1] = r;
        }
        // deck 滤波：EQ 之后、FX 之前（与引擎路径同位置）
        self.filter.process(out, frames);
        // FX rack：滤波之后、gain 之前（与引擎路径同位置）
        self.rack.process(out, frames, &ctx);
        for i in 0..frames {
            let g = self.gain.step() * self.gain_db.step();
            out[i * 2] *= g;
            out[i * 2 + 1] *= g;
        }
    }

    /// P22-B 捕获准备（set_beat_loop 与 P18 总线边沿共用）：容量就位 +
    /// 同环复用判定 + 进入捕获时布防回填。返回是否走缓冲路径。
    fn prepare_loop_capture(&mut self, li_frames: u64, loop_frames: u64) -> bool {
        let grid_bpm = self.ctl.grid_bpm.get();
        if loop_frames <= loop_buf_cap_frames(grid_bpm, self.sr) {
            if self.loop_buf_anchor != li_frames
                || self.loop_buf.frames != loop_frames as usize
            {
                self.loop_buf = LoopBuf::with_capacity(loop_frames as usize);
                self.loop_buf_anchor = li_frames;
            }
            // 同环重设且缓冲完整 → 保持 FromBuffer 环绕（不重捕）；
            // 否则进入捕获（buffer 容量已就位）并按 feed 位置布防回填。
            if !(self.loop_feed == LoopFeed::FromBuffer
                && self.loop_buf.frames == loop_frames as usize)
            {
                self.loop_feed = LoopFeed::Capturing;
                self.arm_loop_capture(li_frames, loop_frames);
            }
            true
        } else {
            // 超限环不缓冲：切环回退 reset 路径（块首检查越界回跳）
            self.loop_feed = LoopFeed::Idle;
            self.loop_buf = LoopBuf::empty();
            self.loop_buf_anchor = 0;
            false
        }
    }

    /// P22-B：进入捕获时按 feed 相对 loop 的位置布防回填（三路）。
    /// 覆盖 P21 的 In/Out 手动定界 UX：
    /// - 0 < backfill < loop_frames 且 feed < lo（Out 在前）：部分回填
    ///   [li, feed_pos) 补前区，捕获续写尾部——播到 Out 量化点才回绕。
    /// - feed ≥ lo（Out 已过/恰拍）：全圈回填 [li, lo)，捕获空转（feed
    ///   已过 Out 无帧可拷）——回填完成即带偏移入环，不跳回、不等圈界。
    /// - 否则（backfill == 0，beatloop 常态）：现语义，捕获即完整。
    ///   回填经 reader_cmd 发给读取线程（侧环 + Backfill 命令）；测试
    ///   deck 无 reader，由测试直接推侧环模拟。epoch 恒不变（回填不动
    ///   世代）。
    fn arm_loop_capture(&mut self, li_frames: u64, loop_frames: u64) {
        let feed_pos = self.feed_pos;
        let lo = li_frames + loop_frames;
        let backfill = feed_pos.saturating_sub(li_frames);
        let out_past = feed_pos >= lo;
        // 清上次未完成回填的残留（位置钳制本就保证内容有效，清空只为腾侧环）
        if let Some(rx) = self.side_rx.as_mut() {
            while rx.try_pop().is_some() {}
        }
        if backfill > 0 && backfill < loop_frames {
            // Out 在前：部分回填补 [li, feed_pos)，捕获从尾部续写。
            // 容量 = 全环（捕获续写尾区要 room），len 截到 backfill
            //（extend_from_slice 从 frames 处续写，frames = backfill）。
            self.loop_buf.data.resize(loop_frames as usize * 2, 0.0);
            self.loop_buf.data.truncate(backfill as usize * 2);
            self.loop_buf.frames = backfill as usize;
            self.loop_buf.backfill_pending = backfill as usize;
            self.loop_buf.backfill_full = false;
            self.send_backfill_or_respawn(li_frames, feed_pos);
        } else if out_past {
            // Out 已过/恰拍：全圈回填 [li, lo)；回填完成即偏移入环
            self.loop_buf.data.resize(loop_frames as usize * 2, 0.0);
            self.loop_buf.frames = loop_frames as usize;
            self.loop_buf.backfill_pending = loop_frames as usize;
            self.loop_buf.backfill_full = true;
            self.send_backfill_or_respawn(li_frames, lo);
        }
        // else：backfill == 0（激活于 loop_in 之前/恰在 in）→ 捕获即完整
    }

    /// P22-C：发回填命令；reader 已死（短曲 EOF 提前退出）时 send 失败 →
    /// 重生读取线程（从当前 feed_pos 续推主环）后重发。回填完成后 reader
    /// 从到达时解码位置续主环（= feed_pos 附近），主环无断点。
    fn send_backfill_or_respawn(&mut self, from: u64, to: u64) {
        let epoch = self.epoch;
        let tx = self.reader_cmd.clone();
        if tx.is_some_and(|tx| {
            tx.send(ReaderCmd::Backfill { epoch, from, to }).is_ok()
        }) {
            return;
        }
        log::warn!("回填命令发送失败（reader 已退出？），重生读取线程");
        self.respawn_reader(epoch, self.feed_pos);
        if let Some(tx) = self.reader_cmd.as_ref() {
            let _ = tx.send(ReaderCmd::Backfill { epoch, from, to });
        }
    }

    /// P22-B：排空侧环回填到环缓冲前区。非阻塞 try_pop；epoch 不符丢弃
    /// （seek/load 后陈旧）。内容按位置寻址：拷 [chunk 区间] ∩ [li,
    /// li+backfill_pending) 到 data 前区（偏移 = 帧位置 − li）——陈旧 arm
    /// chunk 无害（范围钳制）。每块喂入前调一次：量少块小，无分配。
    fn drain_backfill(&mut self) {
        let Some(rx) = self.side_rx.as_mut() else {
            return;
        };
        let li = (self.loop_in * self.sr) as u64;
        let cap = self.loop_buf.data.capacity() / 2;
        // 窗口上界用 frames（回填总量）而非 pending（剩余量）：chunk 按
        // 绝对位置寻址，多 chunk 排空时剩余 < 下一块起点（如 6×2048 后
        // pending=11712 < chunk@12288），用 pending 会误跳过全部后续块。
        // 两路 arm 的 frames == 回填总量（全圈 = 环长；部分 = 激活位−li）。
        // 部分回填时 frames 随捕获续写增长：晚到 chunk 可能越入已捕获区，
        // 内容同源（同一解码区间）覆盖无害。
        let total = li + self.loop_buf.frames as u64;
        while self.loop_buf.backfill_pending > 0 {
            let Some(chunk) = rx.try_pop() else {
                break;
            };
            if chunk.epoch != self.epoch {
                continue;
            }
            let chunk_end = chunk.start_frame + (chunk.data.len() / 2) as u64;
            let a = chunk.start_frame.max(li);
            let b = chunk_end.min(total);
            if a >= b {
                continue;
            }
            let n = ((b - a) as usize).min(cap); // 容量应已就位（arm resize），双保险防越界
            let dst = (a - li) as usize;
            let src = (a - chunk.start_frame) as usize;
            self.loop_buf.data[dst * 2..(dst + n) * 2]
                .copy_from_slice(&chunk.data[src * 2..(src + n) * 2]);
            self.loop_buf.backfill_pending -= n;
        }
    }

    /// P22-B：偏移入环判定（drain_backfill 后、喂入前）——Capturing 且
    /// 全圈回填完成（backfill_pending == 0、frames == 环长）且 feed 已过
    /// loop_out → 立即带偏移进入循环：不跳回、不等圈界，音频从当前相位
    /// 无缝映射入环。d = (feed_pos − lo) mod n；entry blend = feed_tail
    /// × data[d..d+bl)（自然尾已在 keylocker 内不可撤回，用尾部缓冲重建
    /// 淡出分量），blend_at = d 与圈首 wrap 接缝同构（首个 wrap 重算回
    /// 标准圈首 blend）。loop_offset = d：播头公式映射入环相位、退出续点
    /// 每圈 +n。feed_pos 停在 P（入环位置）——退出续点。feed_pos == lo
    /// 走 loop_wrap（圈界精确切环，避免 entry blend 与 wrap blend 双写）。
    fn maybe_enter_loop(&mut self) {
        let loop_frames = ((self.loop_out - self.loop_in) * self.sr) as u64;
        if !(self.loop_active
            && self.loop_feed == LoopFeed::Capturing
            && self.loop_buf.backfill_pending == 0
            && self.loop_buf.frames as u64 == loop_frames)
        {
            return;
        }
        let lo = (self.loop_out * self.sr) as u64;
        let li = (self.loop_in * self.sr) as u64;
        if self.feed_pos <= lo {
            return; // 未到量化 Out：等圈界 loop_wrap 切换
        }
        let d = ((self.feed_pos - lo) % loop_frames) as usize;
        let n_frames = self.loop_buf.frames;
        let bl = LOOP_BLEND_FRAMES.min(n_frames.saturating_sub(d));
        if bl > 0 {
            let blend = &mut self.loop_buf.blend;
            blend.clear();
            for i in 0..bl {
                let t = ((i as f32 + 0.5) / bl as f32) * (std::f32::consts::PI / 2.0);
                let (g_out, g_in) = (t.cos(), t.sin());
                for ch in 0..2 {
                    let v = self.feed_tail[i * 2 + ch] * g_out
                        + self.loop_buf.data[(d + i) * 2 + ch] * g_in;
                    blend.push(v);
                }
            }
            self.loop_buf.blend_len = bl;
            self.loop_buf.blend_at = d;
        }
        self.loop_offset = d as u64;
        self.loop_buf.cursor = d;
        // P22-B 退出锚点基准：feed_pos 停在 P（入环位置，此后每圈 +n）
        self.feed_pos_at_loop_start = self.feed_pos;
        self.loop_pushed = 0;
        let kl = self.keylocker.as_mut().unwrap();
        kl.set_track_position(li + d as u64);
        self.loop_sp_anchor = kl.source_position();
        self.loop_feed = LoopFeed::FromBuffer;
    }

    /// 喂入引擎：按 demand_hint（基于本块渲染帧数）补源帧；曲尾 finish() 冲刷。
    ///
    /// P10.3 loop 缓冲喂入：Capturing 时把 [loop_in, loop_out) 帧拷入
    /// loop_buf；feed 到达 loop_out（推送不越界、精确到帧）→ loop_wrap()
    /// 切 FromBuffer 无 reset 回喂（捕获不完整则回退 reset 重捕）；
    /// FromBuffer 时从缓冲循环喂入（reader 因 ring 满自然停驻）。
    ///
    /// P22-B：每块喂入前先排空侧环回填 + 偏移入环判定（feed 已过
    /// loop_out 且全圈回填完成 → 立即带偏移入环，不跳回不等圈界）。
    fn feed_keylocker(&mut self, engine_frames: usize) {
        if self.keylocker.is_none() {
            return;
        }
        self.drain_backfill();
        self.maybe_enter_loop();
        let n = self.track_frames.load(Ordering::Relaxed);
        let want = self
            .keylocker
            .as_ref()
            .unwrap()
            .demand_hint(engine_frames, MAX_ENGINE_RATE);
        while self.keylocker.as_ref().unwrap().occupied_frames() < want && !self.eof_fed {
            // P10.3 切环：feed 到达 loop_out → 缓冲喂入（无 reset）或回退。
            // P22-B 守卫：全圈布防（frames == loop_frames 且回填未排空）
            // 时让位偏移入环路径（maybe_enter_loop 先处理）；部分回填
            // 未排空时也放行 loop_wrap——其内部守卫兜底 reset（回填迟到
            // /reader 死亡安全网，= 今日行为）。
            // P22-B 停滞兜底：pending 卡住（reader 死亡/极慢）且 feed 已
            // 越过 out → 回兜底 reset 路径，播放头不得永久逃出 out。
            // 部分回填收紧到 1 chunk（43ms，其初始量 ≤ 环长，正常解码
            // 远快于此）；全圈回填放行 2s（长环回填在慢解码设备上可超
            // 1s，RK3399 余量）。
            if self.loop_active && self.loop_feed == LoopFeed::Capturing {
                let lo = (self.loop_out * self.sr) as u64;
                let loop_frames = ((self.loop_out - self.loop_in) * self.sr) as u64;
                if self.feed_pos >= lo
                    && (self.loop_buf.backfill_pending == 0
                        || (self.loop_buf.frames as u64) < loop_frames
                        || (self.loop_buf.backfill_full
                            && self.feed_pos - lo >= (2.0 * self.sr) as u64)
                        || (!self.loop_buf.backfill_full
                            && self.feed_pos - lo >= CHUNK_FRAMES as u64))
                {
                    self.loop_wrap();
                }
            }
            if self.loop_feed == LoopFeed::FromBuffer {
                if !self.feed_from_loop_buf() {
                    break; // 引擎 ring 满
                }
                continue;
            }
            // 曲尾：冲刷 resampler lookahead（finish 失败下一块重试）。
            // 环钳在曲尾时 feed_pos == lo == n，切环已先行处理。
            if n > 0 && self.feed_pos >= n {
                self.eof_fed = self.keylocker.as_mut().unwrap().finish();
                break;
            }
            if self.feed_chunk.is_none() {
                // ring 空（读取线程未赶上）：欠载等待，播头冻结
                let Some((start, data)) = self.next_feed_chunk() else {
                    break;
                };
                self.feed_chunk = Some((start, data, 0));
            }
            let Some((start, data, cursor)) = self.feed_chunk.as_mut() else {
                break;
            };
            if *start > self.feed_pos {
                // 数据超前于喂入位置（缺口）：欠载，等读取线程补上
                break;
            }
            // 跳过已被引擎消费的帧（reader 重生后的重叠数据）
            if *start + (data.len() / 2) as u64 <= self.feed_pos {
                self.feed_chunk = None;
                continue;
            }
            if *start < self.feed_pos {
                *cursor = (self.feed_pos - *start) as usize;
            }
            let rem = data.len() / 2 - *cursor;
            // P10.3 环边界：Capturing 时喂不越出 loop_out（切环在下一轮
            // 迭代精确发生）。P22-B：feed 已过 loop_out（全圈回填布防的
            // 等待窗）不钳——播放继续线性推进，入环瞬间的 feed 相位即
            // 偏移基准。
            let limit = if self.loop_active && self.loop_feed == LoopFeed::Capturing {
                let lo = (self.loop_out * self.sr) as u64;
                if self.feed_pos < lo {
                    (lo - self.feed_pos).min(rem as u64) as usize
                } else {
                    rem
                }
            } else {
                rem
            };
            let accepted = self
                .keylocker
                .as_mut()
                .unwrap()
                .push(&data[*cursor * 2..*cursor * 2 + limit * 2])
                .min(limit);
            if accepted == 0 {
                break; // 引擎 ring 满
            }
            let seg_start = *start + *cursor as u64;
            *cursor += accepted;
            self.feed_pos = *start + *cursor as u64;
            // P22-B 尾部缓冲：最近喂入 LOOP_BLEND_FRAMES 帧（偏移入环
            // entry blend 的淡出分量）。左移保留尾段 + 新帧写尾。
            if accepted > 0 {
                let n_keep = accepted.min(LOOP_BLEND_FRAMES);
                let tail_len = self.feed_tail.len();
                let tail = &mut self.feed_tail;
                tail.copy_within(n_keep * 2..tail_len, 0);
                let seg = &data[*cursor * 2 - accepted * 2..*cursor * 2];
                let from = seg.len() - n_keep * 2;
                tail[tail_len - n_keep * 2..].copy_from_slice(&seg[from..]);
            }
            // P10.3 捕获：把喂过的 [loop_in, loop_out) 帧拷入缓冲
            //（完整性在 loop_wrap 校验——帧数 == 环长 ⇔ 恰好覆盖全环）。
            // 须在 feed_chunk 释放前做（借 data 段）。
            if self.loop_active && self.loop_feed == LoopFeed::Capturing {
                let li = (self.loop_in * self.sr) as u64;
                let lo = (self.loop_out * self.sr) as u64;
                let a = seg_start.max(li);
                let b = self.feed_pos.min(lo);
                if a < b {
                    let room = (self.loop_buf.data.capacity() / 2 - self.loop_buf.frames)
                        .min((b - a) as usize);
                    let off = (a - *start) as usize;
                    self.loop_buf
                        .data
                        .extend_from_slice(&data[off * 2..(off + room) * 2]);
                    self.loop_buf.frames += room;
                }
            }
            if *cursor >= data.len() / 2 {
                self.feed_chunk = None;
            }
        }
    }

    /// P10.3 切环：缓冲完整（帧数 == 环长 ⇔ 恰好覆盖 [loop_in, loop_out)）
    /// → set_track_position 重锚 + FromBuffer 无 reset 回喂（引擎连续，
    /// 接缝零 click——vendored graph.rs loop_wrap_is_gapless_across_ratios
    /// 同款宿主用法）；不完整/回填未排空 → 回退 seek_internal reset，
    /// 下一圈从 loop_in 起重新捕获（P22-B：回填迟到/reader 死亡安全网，
    /// = 今日行为）。
    fn loop_wrap(&mut self) {
        let loop_frames = ((self.loop_out - self.loop_in) * self.sr) as u64;
        if self.loop_buf.frames as u64 == loop_frames && self.loop_buf.backfill_pending == 0 {
            // P22-A：预计算圈首接缝 blend（尾×头等功率淡化）。捕获内容
            // 恒定 → 一份 blend 全程复用（含首圈 Capturing→FromBuffer
            // 接缝与每圈 wrap 接缝同源）。
            self.loop_buf.rebuild_wrap_blend(self.loop_buf.frames);
            let kl = self.keylocker.as_mut().unwrap();
            kl.set_track_position((self.loop_in * self.sr) as u64);
            self.loop_sp_anchor = kl.source_position();
            self.loop_buf.cursor = 0;
            self.loop_feed = LoopFeed::FromBuffer;
            self.feed_pos = (self.loop_out * self.sr) as u64;
            // P22-B 退出锚点基准：feed_pos 恒停 loop_out（常规路径）
            self.feed_pos_at_loop_start = self.feed_pos;
            self.loop_pushed = 0;
            // feed_chunk 保留原位（游标已停在 loop_out）：退出时无缝续喂；
            // reader 因 ring 满自然停驻，epoch/世代不变。
        } else {
            // 捕获不完整（激活时已过 loop_in 或刚 seek）：reset 回跳重捕。
            // seek_internal 会把 loop_feed 置回 Capturing 并清空缓冲。
            let li = self.loop_in;
            self.seek_internal(li, false);
        }
    }

    /// P10.3 FromBuffer 喂入：从环缓冲循环取帧（游标 mod 环长环绕）。
    /// 返回 false = 引擎 ring 满（调用方 break 防忙旋）。
    ///
    /// P22-A 接缝淡化：cursor 落在 blend 区时喂 blend 切片（data 该处
    /// bl 帧的柔和替代），否则照旧喂 data——cursor/圈长语义不变（一圈
    /// 仍恰好喂 n 帧），播头公式与 P11.1 退出锚点不受影响；部分接受时
    /// cursor 停在 blend 区内续推 ✓。
    fn feed_from_loop_buf(&mut self) -> bool {
        let n_frames = self.loop_buf.frames;
        if n_frames == 0 {
            return true; // 空缓冲（不应发生：loop_wrap 已校验完整性）
        }
        let seg_end = self.loop_buf.segment_end(self.loop_buf.cursor);
        let use_blend = self.loop_buf.blend_covers(self.loop_buf.cursor);
        let rem = seg_end - self.loop_buf.cursor;
        // blend 切片以 blend_at 为基（圈首 blend_at=0；偏移入环 blend_at=d，
        // blend 缓冲只有 bl 帧，绝对下标越界）。data 基 = 0。
        let (src, base) = if use_blend {
            (&self.loop_buf.blend, self.loop_buf.blend_at)
        } else {
            (&self.loop_buf.data, 0usize)
        };
        let accepted = self
            .keylocker
            .as_mut()
            .unwrap()
            .push(&src[(self.loop_buf.cursor - base) * 2..(seg_end - base) * 2])
            .min(rem);
        self.loop_buf.cursor += accepted;
        // P22-B 退出锚点：环喂入累计（Δ = feed_pos − 基准 − 累计）
        self.loop_pushed += accepted as u64;
        if accepted > 0 {
            // P22-B 尾部缓冲（同 feed_keylocker push 侧，含 loop 喂入）：
            // 偏移入环的淡出分量须跟随实际出声内容（环内圈圈不同）。
            let n_keep = accepted.min(LOOP_BLEND_FRAMES);
            let tail_len = self.feed_tail.len();
            let tail = &mut self.feed_tail;
            tail.copy_within(n_keep * 2..tail_len, 0);
            let lo = (self.loop_buf.cursor - base) * 2 - accepted * 2;
            let seg = &src[lo..(self.loop_buf.cursor - base) * 2];
            let from = seg.len() - n_keep * 2;
            tail[tail_len - n_keep * 2..].copy_from_slice(&seg[from..]);
        }
        if self.loop_buf.cursor >= n_frames {
            self.loop_buf.cursor = 0;
            // P22-B：偏移入环后每圈推进退出续点（feed_pos 停在入环位置
            // P，每圈 +n——退出时 ring 续喂位置 = P + k×n）。仅偏移入环
            // （loop_offset > 0）生效；常规路径 feed_pos 恒停 loop_out。
            if self.loop_offset > 0 {
                self.feed_pos += n_frames as u64;
            }
            // P22-A：偏移入环（P22-B 的 blend_at=d）后首个 wrap 重算
            // 标准圈首 blend（blend_at → 0）；常规路径 blend_at 恒 0，
            // 计算幂等（内容恒定，192 帧便宜）。
            if self.loop_buf.blend_at != 0 {
                self.loop_buf.rebuild_wrap_blend(n_frames);
            }
            // P11.1 收尾圈完成（loop 已关）：切回 ring（feed_pos/feed_chunk
            // 停在 loop_out）并锚定线性显示。锚点由折返公式与当前标签反解：
            // 引擎位置标签自切环起累计 k×len（不重打标签，避免标签断层
            // 显示反跳），pos_base 吸收该偏移 → 播头从 loop_out 无缝续进，
            // 不再出现释放跳 N×len 的旧 bug（pos = feed_base + sp）。
            // P22-B：偏移入环时锚点 = li + loop_offset（入环相位起点），
            // 折返公式 + loop_offset 吸收 → 播头从入环位置线性续进
            //（非偏移入环 loop_offset == 0，公式不变）。
            if !self.loop_active {
                self.loop_feed = LoopFeed::Idle;
                // P22-B 退出锚点分路：
                // - 常规环（loop_offset == 0）：音频 = 环相位连续续进
                //   （loop 内容 = 音轨 [li, lo)，接 ring 于 lo 内容无缝），
                //   播头相位续进无跳变——P11.1 原锚点：折返公式与当前
                //   标签反解（标签 = 原始喂入计数 − 管线延迟，不重打
                //   标签避免标签断层反跳），相位 ≡ 标签折叠，精确抵消。
                // - 偏移入环（loop_offset > 0）：音频切回 ring 续点
                //   P + k×len（内容跳变 k×len + d，设计如此），播头必须
                //   随声音跳——锚 Δ = feed_pos − 基准 − 环喂入 = +d（首
                //   圈只喂 n−d 帧），显示 = 标签 + Δ = 正在出声的 ring
                //   内容位置；旧相位公式会让播头永久落后音频 k×len。
                //   （有符号中间量：偏移路径 feed_pos − 基准 = W×len，
                //   常规路径 0 − 累计为负，u64 直减溢出。）
                if self.loop_offset > 0 {
                    self.pos_base = Some(
                        (self.feed_pos as i64 - self.feed_pos_at_loop_start as i64
                            - self.loop_pushed as i64) as f64,
                    );
                } else {
                    let kl = self.keylocker.as_ref().unwrap();
                    let sp = kl.source_position();
                    let len = (self.loop_out - self.loop_in) * self.sr;
                    let folded = (sp - self.loop_sp_anchor).rem_euclid(len);
                    self.pos_base = Some(self.loop_in * self.sr + folded - sp);
                }
                // P22-B 退出续点安全网：偏移入环超圈后退出（k×n > ring
                // 解码窗口）→ 续点超出已解码内容，min-preroll 重锚替代
                // 无缝续喂（常规路径续点恒在 loop_out，窗口必覆盖）。
                let resume = self.feed_pos;
                let mut covered = false;
                if let Some((start, data, cursor)) = self.feed_chunk.as_mut() {
                    if *start <= resume && resume < *start + (data.len() / 2) as u64 {
                        *cursor = (resume - *start) as usize;
                        covered = true;
                    } else {
                        self.feed_chunk = None; // 陈旧（超窗）
                    }
                }
                if !covered
                    && let Some(rx) = self.chunk_rx.as_mut()
                {
                    // ring 前 chunk 窥视（不弹出）：内容恒连续 [front.start,
                    // front.start + n×2048)（reader 顺序解码、满则停驻），
                    // 覆盖 resume 即续喂可行；否则重锚。
                    let (a, b) = rx.as_slices();
                    let n_chunks = (a.len() + b.len()) as u64;
                    covered = a.first().is_some_and(|c| {
                        c.start_frame <= resume
                            && resume < c.start_frame + n_chunks * CHUNK_FRAMES as u64
                    });
                }
                if !covered {
                    // 超窗：min-preroll 重锚（seek_internal 顺带清缓存/回填）
                    self.seek_internal(resume as f64 / self.sr, true);
                }
                self.loop_offset = 0;
            }
        }
        accepted > 0
    }

    /// 从 ring 弹下一个当前世代 chunk。
    /// P22-C：保留窗口（preserve）内的旧世代 chunk 一并接受——seek 时
    /// 已滤净落点之前的陈旧数据，窗口连续且位置正确。
    fn next_feed_chunk(&mut self) -> Option<(u64, Box<[f32]>)> {
        let rx = self.chunk_rx.as_mut()?;
        loop {
            let chunk = rx.try_pop()?;
            if chunk.epoch != self.epoch && self.preserve != Some(chunk.epoch) {
                continue; // 过期数据（seek 前残留）
            }
            if chunk.epoch == self.epoch {
                self.preserve = None; // 世代已对齐，窗口使命结束
            }
            return Some((chunk.start_frame, chunk.data));
        }
    }

    /// 读取指定帧（48kHz 时间轴）的线性插值立体声采样。
    fn read_stereo(&mut self, pos: f64) -> Option<(f32, f32)> {
        let i0 = pos as usize;
        let frac = (pos - i0 as f64) as f32;
        let (l0, r0) = self.frame_at(i0)?;
        let (l1, r1) = self.frame_at(i0 + 1)?;
        Some((l0 + (l1 - l0) * frac, r0 + (r1 - r0) * frac))
    }

    /// 取单帧；缓存最近两个 chunk（插值读 idx/idx+1 跨边界时需要同时持有
    /// 新旧两块——只存一块会在 rate<1 时把仍在读的旧块挤掉，导致
    /// "数据超前→欠载死锁"：playhead 恰好停在 chunk 边界后冻结）。
    /// 用帧区间匹配而非序号整除：seek 落到非 2048 对齐的帧时也不会错位。
    fn frame_at(&mut self, idx: usize) -> Option<(f32, f32)> {
        let idx = idx as u64;
        for (start, data) in self.chunks.iter() {
            if let Some(start) = *start
                && idx >= start
                && idx < start + CHUNK_FRAMES as u64
            {
                let j = ((idx - start) as usize) * 2;
                return Some((data[j], data[j + 1]));
            }
        }
        // 缓存未命中：从 ring 前进到目标 chunk
        let rx = self.chunk_rx.as_mut()?;
        loop {
            let chunk = rx.try_pop()?;
            if chunk.epoch != self.epoch && self.preserve != Some(chunk.epoch) {
                continue; // 过期数据（seek 前残留）
            }
            if chunk.epoch == self.epoch {
                self.preserve = None; // 世代已对齐，窗口使命结束
            }
            let start = chunk.start_frame;
            if idx >= start && idx < start + CHUNK_FRAMES as u64 {
                // 轮换写槽：始终保留最近两块
                let slot = self.chunk_slot;
                self.chunks[slot] = (Some(start), chunk.data);
                self.chunk_slot ^= 1;
                let (_, data) = &self.chunks[slot];
                let j = ((idx - start) as usize) * 2;
                return Some((data[j], data[j + 1]));
            }
            if start > idx {
                // 数据比读位置新（读位置真正落后于缓存）：按欠载处理，
                // 保持 pos 等待追赶数据，而不是丢弃前进。
                return None;
            }
            // 落后于目标位置：丢弃继续
        }
    }

    /// 引擎操作：加载音轨（spawn 读取线程）。
    pub fn load(&mut self, path: std::path::PathBuf) {
        self.stop_reader();

        // keylock 引擎：构建失败 → None，回退线性插值路径（trait 缝的意义）
        let locker = match TimestretchLocker::build(self.sr as u32, false) {
            Ok(kl) => Some(Box::new(kl) as Box<dyn Keylocker>),
            Err(e) => {
                log::error!("keylock 引擎构建失败，回退线性插值路径: {e:#}");
                None
            }
        };
        self.keylocker = locker;
        self.pitch_shifter.set_semitones(0.0);
        self.feed_pos = 0;
        self.feed_base = 0;
        self.pos_base = None;
        self.feed_chunk = None;
        self.eof_fed = false;
        self.eof_stall = 0;
        self.last_sp = 0.0;
        self.last_sent_rate = None; // 首块强制 set_rate
        self.keylock_sent = None; // 首块强制 set_keylock
        self.rebuild_pending = false;
        self.pitch = 0.0;
        self.keylock_on = true;
        self.sync_align_done = false; // 换曲重对齐（sync 保持开启时）
        self.fader_detached = false; // 换曲后滑杆恢复直通（不继承旧同步速率）
        self.fader_armed = false; // 换曲后 sync 速率锁复位
        self.fader_prev_rate = 0.0;

        let (prod, cons) =
            ringbuf::HeapRb::<Chunk>::new(crate::caching_reader::RING_CAPACITY).split();
        // P22-B 回填侧环（reader 回填推这里，deck 每块排空到环缓冲前区）
        let (side_prod, side_cons) =
            ringbuf::HeapRb::<Chunk>::new(crate::caching_reader::SIDE_RING_CAPACITY).split();
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        self.track_frames.store(0, Ordering::Relaxed);

        self.chunk_rx = Some(cons);
        self.side_rx = Some(side_cons);
        self.loop_offset = 0; // 换曲：偏移入环量清零（坐标重建）
        self.feed_pos_at_loop_start = 0;
        self.loop_pushed = 0;
        self.preserve = None; // P22-C：ring 已换新，旧窗口世代作废
        self.path = Some(path.clone());
        self.reader_cmd = Some(self.spawn_reader(path, prod, side_prod, epoch));
        self.loaded = true;
        self.pos = 0.0;
        self.clear_cache();
        self.eq.reset();
        self.filter.reset();
        self.rack.reset();
        self.gain.set_immediate(1.0);
        // gain_db 不重置：通道增益是调音台 trim，跨曲保持
        self.playing = true;
        self.ctl.play.set(1.0);
        self.ctl.playhead.set(0.0);
        self.ctl.loaded.set(1.0);
        // beat loop 随载曲复位（新音轨的网格/时长都变了，旧环无意义）
        self.loop_active = false;
        self.loop_in = 0.0;
        self.loop_out = 0.0;
        self.loop_feed = LoopFeed::Idle;
        self.loop_buf = LoopBuf::empty();
        self.loop_buf_anchor = 0;
        self.loop_sp_anchor = 0.0;
        self.ctl.loop_active.set(0.0);
        self.ctl.loop_in.set(0.0);
        self.ctl.loop_out.set(0.0);
    }

    /// 生成解码读取线程（load 与 EOF 后 seek 重生共用）。
    /// 返回命令通道 sender；线程句柄存入 self._reader_handle。
    fn spawn_reader(
        &mut self,
        path: std::path::PathBuf,
        prod: ringbuf::HeapProd<Chunk>,
        side_prod: ringbuf::HeapProd<Chunk>,
        epoch: u32,
    ) -> Sender<ReaderCmd> {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<ReaderCmd>();
        let track_frames = self.track_frames.clone();
        let sr_out = self.sr as u32;
        let deck_name = format!("reader-{}", self.index + 1);
        let handle = std::thread::Builder::new()
            .name(deck_name)
            .spawn(move || {
                if let Err(e) = crate::caching_reader::reader_main(
                    path,
                    sr_out,
                    cmd_rx,
                    side_prod,
                    prod,
                    epoch,
                    track_frames,
                ) {
                    log::error!("读取线程退出: {e:#}");
                }
            })
            .expect("spawn reader thread");
        self._reader_handle = Some(handle);
        cmd_tx
    }

    /// EOF 后读取线程已退出（reader_main 排空曲尾即 return，旧 sender
    /// disconnected）：用相同音轨重生线程并立即跳到目标帧。不重生则
    /// EOF 后 seek 永久欠载（回归测试 eof_seek_restarts_reader）。
    fn respawn_reader(&mut self, epoch: u32, frame: u64) {
        let Some(path) = self.path.clone() else {
            log::error!("读取线程重生失败：无音轨路径");
            return;
        };
        // 新 ring：旧 prod 半部已随死线程丢弃，无法复用旧 ring
        let (prod, cons) =
            ringbuf::HeapRb::<Chunk>::new(crate::caching_reader::RING_CAPACITY).split();
        // P22-B：侧环随 reader 重建（旧 prod 已随死线程丢弃）
        let (side_prod, side_cons) =
            ringbuf::HeapRb::<Chunk>::new(crate::caching_reader::SIDE_RING_CAPACITY).split();
        let tx = self.spawn_reader(path, prod, side_prod, epoch);
        self.chunk_rx = Some(cons);
        self.side_rx = Some(side_cons);
        self.reader_cmd = Some(tx.clone());
        // 新线程从曲首起跑，loop 顶 try_recv 优先于解码 → Seek 立即生效；
        // 竞态窗口内先推出的曲首 chunk 由 feed_keylocker 按位置丢弃。
        if tx.send(ReaderCmd::Seek {
            epoch,
            frame,
            resume: None,
        })
        .is_err()
        {
            log::error!("读取线程重生后立即退出（解码失败？）");
        }
    }

    /// 引擎操作：跳转（quantize 开启时吸附到最近拍点）。
    pub fn seek_seconds(&mut self, seconds: f64) {
        if !self.loaded {
            return;
        }
        // quantize：网格有效时吸附到最近拍点。
        // 直读总线而非 update_params 快照：seek 经 ops 队列在块首
        // update_params 之前执行，快照值会滞后一拍。
        let mut seconds = seconds.max(0.0);
        if self.ctl.quantize.get() > 0.5 {
            let grid = BeatGrid {
                bpm: self.ctl.grid_bpm.get(),
                offset_secs: self.ctl.grid_offset.get(),
            };
            if grid.is_valid() {
                seconds = grid.snap(seconds);
            }
        }
        self.deactivate_loop_if_outside(seconds);
        self.seek_internal(seconds, false);
    }

    /// 引擎操作：精确跳转（不量化；cue/hotcue 召回用——量化会把
    /// 召回到点吸到邻近拍点）。外部跳转出环同样取消 loop。
    pub fn seek_exact(&mut self, seconds: f64) {
        if !self.loaded {
            return;
        }
        let seconds = seconds.max(0.0);
        self.deactivate_loop_if_outside(seconds);
        self.seek_internal(seconds, false);
    }

    /// 用户跳转落在 [loop_in, loop_out) 之外时取消 loop（环内回跳
    /// 由块首检查负责，不走这里）。
    fn deactivate_loop_if_outside(&mut self, seconds: f64) {
        if self.loop_active && !(seconds >= self.loop_in && seconds < self.loop_out) {
            self.loop_active = false;
            self.ctl.loop_active.set(0.0);
        }
    }

    /// 引擎操作：激活/调整 beat loop（拍数 → 量化起止；网格无效、
    /// 起点越过曲尾或环长非正则 no-op）。重复激活同尺寸的 toggle
    /// 语义由 UI 层判断（写 loop_active=0），引擎不做 toggle。
    pub fn set_beat_loop(&mut self, beats: f64) {
        if !self.loaded {
            return;
        }
        let grid = BeatGrid {
            bpm: self.ctl.grid_bpm.get(),
            offset_secs: self.ctl.grid_offset.get(),
        };
        if !grid.is_valid() || beats <= 0.0 {
            return;
        }
        let t = (self.pos / self.sr).max(0.0);
        let loop_in = grid.snap(t).max(0.0);
        let n = self.track_frames.load(Ordering::Relaxed);
        let dur = if n > 0 { n as f64 / self.sr } else { f64::INFINITY };
        if loop_in >= dur {
            return;
        }
        let loop_out = (loop_in + beats * grid.period_secs()).min(dur);
        if loop_out <= loop_in {
            return;
        }
        self.loop_active = true;
        self.loop_in = loop_in;
        self.loop_out = loop_out;
        self.ctl.loop_in.set(loop_in);
        self.ctl.loop_out.set(loop_out);
        self.ctl.loop_active.set(1.0);
        // P10.3 缓冲喂入准备：环长 ≤ min(64 拍, 30s) 才捕获；与现有
        // [loop_in, loop_out) 完全一致的缓冲直接复用（跳过捕获），否则
        // 清空重捕。超限环 loop_feed 置 Idle（切环回退 reset 路径）。
        // P22-B：激活瞬间已过 loop_out 由 arm 的全圈回填 + 偏移入环
        // 处理（不再全预卷 seek 回跳；超限环仍由块首检查回跳）。
        let li_frames = (loop_in * self.sr) as u64;
        let loop_frames = ((loop_out - loop_in) * self.sr) as u64;
        self.prepare_loop_capture(li_frames, loop_frames);
    }

    /// 引擎操作：按拍跳跃（P10.1 起源拍域整拍距离；P17 已否决落点 snap）。
    /// 拍长 = 60/grid_bpm（跳 N 拍 = N×60/bpm 源拍域距离，与播放速率
    /// 无关）——两轨同速时跳 N 拍相对相位恒不变（不开 sync 也不失去同步）。
    /// P17：落点量化拍线被用户否决（"更糟糕，因为有延迟导致 jump 后
    /// 更慢"——离拍起跳落点吸附使跳距缩短最多 0.5 拍，叠引擎延迟听感
    /// 更慢；P14 用户已确认"跳跃时长 = 60/bpm×beats"即整拍 = 跳距）：
    /// 回滚为 P16 精确跳距 target = pos + N·period，不吸附。P14：seek
    /// 走最小预卷（零 priming 静音）。网格无效 no-op。
    pub fn beatjump(&mut self, beats: f64) {
        if !self.loaded {
            return;
        }
        let grid = BeatGrid {
            bpm: self.ctl.grid_bpm.get(),
            offset_secs: self.ctl.grid_offset.get(),
        };
        if !grid.is_valid() {
            return;
        }
        let period = grid.period_secs(); // 60/grid_bpm（源拍域，与速率无关）
        let n = self.track_frames.load(Ordering::Relaxed);
        let dur = if n > 0 { n as f64 / self.sr } else { f64::INFINITY };
        let target = (self.pos / self.sr + beats * period).clamp(0.0, dur);
        self.deactivate_loop_if_outside(target);
        self.seek_internal(target, true); // P14：最小预卷，跳拍零静音
    }

    /// 跳转本体（量化/清环决策由调用方负责；seek 内部不动 playing）。
    /// `min_preroll`：beatjump 专用——1 帧预卷，priming 立即收尾出声
    ///（全预卷 1584 帧 priming 丢弃输出 ≈33ms 静音，是跳拍卡顿根因；
    /// P14）。seek/cue 保持全预卷（质量优先）。
    fn seek_internal(&mut self, seconds: f64, min_preroll: bool) {
        let frame = (seconds * self.sr) as u64;
        // P10.3：seek 使捕获状态失效——环激活时清空缓冲重捕（下一圈从
        // loop_in 起完整捕获，完整性校验兜底）；环未激活回 Idle（缓冲
        // 保留复用：内容 = 音轨帧，与播放位置无关。必须回 Idle——
        // 否则 beatjump 出环后 FromBuffer 会继续喂旧环内容）。
        if self.loop_active && self.loop_feed != LoopFeed::Idle {
            self.loop_feed = LoopFeed::Capturing;
            self.loop_buf.frames = 0;
            self.loop_buf.cursor = 0;
            // P22-A：缓冲作废 → blend 一并失效（重捕完成后 loop_wrap
            // 重算，喂入侧只读 blend_len > 0 且 FromBuffer 态的 blend）
            self.loop_buf.blend_len = 0;
            self.loop_buf.blend_at = 0;
            // P22-B：回填未排空数据随缓冲作废（侧环残留由 drain 按位置
            // 钳制丢弃）；偏移入环量清零（坐标重建）
            self.loop_buf.backfill_pending = 0;
            self.loop_offset = 0;
            self.feed_pos_at_loop_start = 0;
            self.loop_pushed = 0;
        } else if !self.loop_active {
            self.loop_feed = LoopFeed::Idle;
            self.loop_buf.cursor = 0;
        }
        self.clear_cache();
        self.pos = frame as f64;
        self.pos_base = None; // 新 fed 坐标（P11.1：收尾圈锚点随 reset 作废）
        self.ctl.playhead.set(seconds);
        let engine_rate = self.engine_rate();
        // keylock 路径：reset + 重新锚定 + warm_start 预卷（spike 验证零欠载零 NaN）。
        // 读取线程从 read_frame（= target − preroll）重新解码。
        let read_frame = if let Some(kl) = self.keylocker.as_mut() {
            let preroll = if min_preroll {
                // P14 beatjump 最小预卷：priming 1 帧 → done_at=0 立即
                // 收尾（declick 淡入 64 帧）≈ 0 静音；代价 = settle
                // 冷启动的瞬态质量（先例：rebuild 路径 warm_start(1)）。
                // reader 正常 1-5ms 延迟无预卷掩蔽，首块可能短暂欠载，
                // 引擎欠载 declick 兜底。
                1
            } else {
                kl.warm_start_preroll_frames() as u64
            };
            let read_frame = frame.saturating_sub(preroll);
            kl.reset();
            kl.set_rate(engine_rate);
            kl.set_keylock(self.keylock_on);
            kl.set_track_position(read_frame);
            // 曲头 preroll 不足时传实际可用帧数（0 = 无预卷，release ramp 兜底）
            kl.warm_start((frame - read_frame) as u32);
            self.last_sent_rate = Some(engine_rate);
            self.keylock_sent = Some(self.keylock_on);
            self.feed_base = read_frame;
            self.feed_pos = read_frame;
            self.feed_chunk = None;
            self.eof_fed = false;
            self.eof_stall = 0;
            self.last_sp = 0.0;
            read_frame
        } else {
            frame
        };
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        // P22-C 智能排水：落点之前的陈旧前缀弹出；余环窗口含落点 →
        // 整环保留（resume = 窗口尾 end，reader 续推零欠载）。环空或
        // 落点超窗 → 全排 refill（旧行为）。保留仅按位置判定——环内
        // 新旧混排（seek 竞态残留）时 preserve 记尾块世代，过期残留
        // 由接受规则丢弃、位置规则消化。
        let resume = if let Some(rx) = self.chunk_rx.as_mut() {
            loop {
                let stale = rx
                    .first()
                    .is_some_and(|c| c.start_frame + CHUNK_FRAMES as u64 <= read_frame);
                if stale {
                    let _ = rx.try_pop();
                } else {
                    break;
                }
            }
            if rx.first().is_some_and(|c| c.start_frame <= read_frame) {
                let tail = rx.iter().last();
                self.preserve = tail.map(|c| c.epoch);
                tail.map(|c| c.start_frame + CHUNK_FRAMES as u64)
            } else {
                while rx.try_pop().is_some() {}
                self.preserve = None;
                None
            }
        } else {
            None
        };
        // 读取线程可能已退出（EOF 排空后 reader_main return）：
        // is_finished 预检 + send 失败双保险，任一命中 → 重生线程。
        // （is_finished 与 send 之间线程恰好死亡的理论窗口内 send 仍会
        // 成功、命令随 receiver 一起丢弃——该窗口为线程结束前最后数微秒，
        // 且下一块 seek 会再次触发重生。）
        let dead = self
            ._reader_handle
            .as_ref()
            .is_some_and(|h| h.is_finished());
        let sent = !dead
            && self.reader_cmd.as_ref().is_some_and(|tx| {
                tx.send(ReaderCmd::Seek {
                    epoch,
                    frame: read_frame,
                    resume,
                })
                .is_ok()
            });
        // reader_cmd 为 None = 从未有过读取线程（预填 ring 的测试 deck），无需重生
        if !sent && self.reader_cmd.is_some() {
            self.respawn_reader(epoch, read_frame);
        }
    }

    /// 清空 chunk 缓存（load/seek 后调用，旧数据世代已失效）。
    fn clear_cache(&mut self) {
        self.chunks = [
            (None, vec![0.0; 0].into_boxed_slice()),
            (None, vec![0.0; 0].into_boxed_slice()),
        ];
        self.chunk_slot = 0;
    }

    fn stop_reader(&mut self) {
        if let Some(tx) = self.reader_cmd.take() {
            let _ = tx.send(ReaderCmd::Shutdown);
        }
        self.chunk_rx = None;
        self.side_rx = None; // P22-B：侧环随 reader 停
        self.path = None;
        if let Some(h) = self._reader_handle.take() {
            let _ = h.join();
        }
        self.loaded = false;
        self.ctl.loaded.set(0.0);
        self.ctl.play.set(0.0);
        self.ctl.playhead.set(0.0);
        self.ctl.duration.set(0.0);
    }
}

// ---------- 测试夹具（deck 单测 + engine 级联动测试共用） ----------

/// 预填 n 个 chunk 的 440Hz 正弦（幅度 0.5，每块 CHUNK_FRAMES 帧）。
#[cfg(test)]
pub(crate) fn test_sine_chunks(n: usize) -> Vec<Chunk> {
    let mut v = Vec::new();
    for k in 0..n {
        let mut data = Vec::with_capacity(CHUNK_FRAMES * 2);
        for f in 0..CHUNK_FRAMES {
            let t = (k * CHUNK_FRAMES + f) as f32 / 48000.0;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            data.push(s);
            data.push(s);
        }
        v.push(Chunk {
            epoch: 1,
            start_frame: (k * CHUNK_FRAMES) as u64,
            data: data.into_boxed_slice(),
        });
    }
    v
}

/// 预填 ring 的测试 deck（keylock 引擎、无 reader 线程；测试 ring 容量
/// 256 > 生产 64）。返回 deck 与 prod（供 seek 后继续推 chunk 模拟 reader）。
#[cfg(test)]
pub(crate) fn test_deck_with_ring_and_prod(
    bus: &hypermixx_core::ControlBus,
    chunks: Vec<Chunk>,
    rate_pct: f64,
) -> (Deck, ringbuf::HeapProd<Chunk>) {
    let mut d = Deck::new(0, 48000, bus);
    let (mut prod, cons) = ringbuf::HeapRb::<Chunk>::new(256).split();
    for c in chunks {
        prod.try_push(c).ok().expect("ring 容量足够");
    }
    d.chunk_rx = Some(cons);
    // P22-B：侧环常驻（arm 时清残留；无 reader 线程时永远收不到数据）
    let (_, side_cons) =
        ringbuf::HeapRb::<Chunk>::new(crate::caching_reader::SIDE_RING_CAPACITY).split();
    d.side_rx = Some(side_cons);
    d.epoch = 1;
    d.loaded = true;
    d.ctl.play.set(1.0);
    d.ctl.volume.set(1.0);
    d.ctl.rate.set(rate_pct);
    d.ctl.keylock.set(1.0);
    d.keylocker = TimestretchLocker::build(48_000, false)
        .ok()
        .map(|k| Box::new(k) as Box<dyn Keylocker>);
    (d, prod)
}

/// P22-B：同 test_deck_with_ring_and_prod，另返回侧环 prod——回填测试
/// 直接推侧环模拟 reader 对 Backfill 命令的响应。
#[cfg(test)]
pub(crate) fn test_deck_with_rings_and_prods(
    bus: &hypermixx_core::ControlBus,
    chunks: Vec<Chunk>,
    rate_pct: f64,
) -> (
    Deck,
    ringbuf::HeapProd<Chunk>,
    ringbuf::HeapProd<Chunk>,
) {
    let (mut d, prod) = test_deck_with_ring_and_prod(bus, chunks, rate_pct);
    let (side_prod, side_cons) =
        ringbuf::HeapRb::<Chunk>::new(crate::caching_reader::SIDE_RING_CAPACITY).split();
    d.side_rx = Some(side_cons);
    (d, prod, side_prod)
}

/// 从任意帧开始的 440Hz 正弦 chunk（seek 后模拟 reader 推送）。
#[cfg(test)]
pub(crate) fn test_sine_chunk_at(start: u64, frames: usize, epoch: u32) -> Chunk {
    let mut data = Vec::with_capacity(frames * 2);
    for f in 0..frames {
        let t = (start + f as u64) as f32 / 48000.0;
        let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
        data.push(s);
        data.push(s);
    }
    Chunk {
        epoch,
        start_frame: start,
        data: data.into_boxed_slice(),
    }
}

/// seek 后补推新世代 chunk（engine 级联动测试用：测试 deck 无 reader
/// 线程，seek_internal 清空 ring 后不补推会永久欠载、播头冻结）。
/// 起推点超前 target 8192 帧 > 任何 preroll（管线延迟 + settle），
/// 喂入循环跳过重叠；64 chunk 覆盖 target 之后 2.7s，足够落点断言。
#[cfg(test)]
pub(crate) fn test_refill_after_seek(
    d: &mut Deck,
    prod: &mut ringbuf::HeapProd<Chunk>,
    target_secs: f64,
) {
    let start = ((target_secs * 48000.0) as u64).saturating_sub(8192);
    for k in 0..64 {
        prod.try_push(test_sine_chunk_at(
            start + k as u64 * CHUNK_FRAMES as u64,
            CHUNK_FRAMES,
            d.epoch,
        ))
        .ok()
        .expect("ring 容量足够");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::Producer;
    use super::test_deck_with_ring_and_prod as deck_with_ring_and_prod;
    use super::test_deck_with_rings_and_prods as deck_with_rings_and_prods;
    use super::test_sine_chunk_at as sine_chunk_at;
    use super::test_sine_chunks as sine_chunks;

    /// Keylock profile 引擎延迟（560 帧，spike 实测）折算秒。
    const KEYLOCK_LATENCY_S: f64 = 560.0 / 48000.0;

    /// 兜底 reset 后补推新世代 chunk：模拟真实 reader 对 Seek 命令的响应
    ///（测试 deck 无 reader 线程，seek 后 ring 里只剩旧世代 chunk，
    /// 不补推会永久欠载）。读起点 = loop_in − preroll，64 chunk 容量
    /// 足够任何测试环长（seek_internal 已清空 ring）。
    fn refill_ring_after_reset(d: &mut Deck, prod: &mut ringbuf::HeapProd<Chunk>, preroll: u64) {
        let read_frame = ((d.ctl.loop_in.get() * 48000.0) as u64).saturating_sub(preroll);
        for k in 0..64 {
            prod.try_push(sine_chunk_at(
                read_frame + k as u64 * CHUNK_FRAMES as u64,
                CHUNK_FRAMES,
                d.epoch,
            ))
            .ok()
            .expect("ring 容量足够");
        }
    }

    fn deck_with_ring(bus: &hypermixx_core::ControlBus, chunks: Vec<Chunk>, rate_pct: f64) -> Deck {
        let (d, _) = deck_with_ring_and_prod(bus, chunks, rate_pct);
        d
    }

    /// 处理 frames 帧，返回 (输出峰值, 播放头秒数)。
    fn run_frames(d: &mut Deck, frames: usize) -> (f32, f64) {
        let mut out = vec![0.0; 256 * 2];
        let mut peak = 0.0f32;
        let mut rem = frames;
        while rem > 0 {
            let n = rem.min(256);
            d.update_params();
            d.process(&mut out, n);
            for v in out.iter().take(n * 2) {
                peak = peak.max(v.abs());
            }
            rem -= n;
        }
        (peak, d.ctl.playhead.get())
    }

    /// 处理 seconds 秒并累计输出（交织立体声），供测频/NaN 检查。
    fn run_capture(d: &mut Deck, seconds: f64) -> Vec<f32> {
        let mut out = vec![0.0; 256 * 2];
        let mut rec = Vec::new();
        let blocks = (seconds * 48000.0 / 256.0).round() as usize;
        for _ in 0..blocks {
            d.update_params();
            d.process(&mut out, 256);
            rec.extend_from_slice(&out);
        }
        rec
    }

    /// 零交叉测频（左声道，[start_frame, end_frame) 窗口）。
    /// 只数上升沿：每个完整周期恰好一次 → crossings = 周期数。
    fn zero_crossing_freq(out: &[f32], start_frame: usize, end_frame: usize) -> f64 {
        let mut crossings = 0usize;
        let mut prev = out[start_frame * 2];
        for i in start_frame + 1..end_frame {
            let v = out[i * 2];
            if prev < 0.0 && v >= 0.0 {
                crossings += 1;
            }
            prev = v;
        }
        crossings as f64 * 48000.0 / (end_frame - start_frame) as f64
    }

    fn cents_off(measured: f64, reference: f64) -> f64 {
        1200.0 * (measured / reference).log2()
    }

    #[test]
    fn plays_audio_from_ring() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        let (peak, head) = run_frames(&mut d, 48000); // 1 秒
        assert!(peak > 0.4, "应输出正弦波，peak={peak}");
        // 引擎路径播头 = 墙钟 − 引擎延迟（560 帧延迟匹配）
        assert!(
            (head - (1.0 - KEYLOCK_LATENCY_S)).abs() < 0.01,
            "播放头应到 {}s，实际 {head}",
            1.0 - KEYLOCK_LATENCY_S
        );
    }

    #[test]
    fn underrun_holds_position_no_runaway() {
        let bus = hypermixx_core::ControlBus::default();
        // 16 chunk ≈ 0.68s 音频；处理 6 秒，读空后必须停在最后可读帧附近
        let mut d = deck_with_ring(&bus, sine_chunks(16), 0.0);
        let (peak, head) = run_frames(&mut d, 48000 * 6);
        assert!(peak > 0.4, "读空之前应出声，peak={peak}");
        assert!(
            head < 1.0,
            "欠载后播放头不得狂奔（旧实现会到 6.0s），实际 {head}"
        );
    }

    #[test]
    fn high_rate_stays_stable() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(16), 100.0); // +100% → 2.0×
        let (peak, head) = run_frames(&mut d, 48000 * 6);
        assert!(peak > 0.4, "2× 播放应出声，peak={peak}");
        assert!(
            head < 1.0,
            "高速欠载后播放头不得越过最后可读帧，实际 {head}"
        );
    }

    /// 回归：rate<1 时插值读跨 chunk 边界曾把仍在读的旧块挤掉，
    /// 导致"数据超前→欠载死锁"（播放头冻结在边界上）。-8% 连跨 ~43 个边界
    /// 不得冻结，播放头应与速率一致。
    #[test]
    fn slow_rate_crosses_boundaries_without_freezing() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), -8.0); // 0.92×
        let (peak, head) = run_frames(&mut d, 48000 * 2);
        assert!(peak > 0.4, "-8% 播放应出声，peak={peak}");
        assert!(
            (head - (2.0 * 0.92 - KEYLOCK_LATENCY_S * 0.92)).abs() < 0.01,
            "-8% 播放头应到 1.8293s 且不冻结，实际 {head}"
        );
    }

    /// 回归：速率中途从 +8% 变 -8%（模拟扫掠/拖拽滑杆），
    /// 跨越边界后仍须继续播放。
    #[test]
    fn rate_sweep_does_not_freeze() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 8.0); // 1.08×
        let mut out = vec![0.0; 256 * 2];
        let mut peak = 0.0f32;
        for _ in 0..(48000 / 256) {
            d.update_params();
            d.process(&mut out, 256);
        }
        d.ctl.rate.set(-8.0); // 1.08× → 0.92×
        for _ in 0..(48000 / 256) {
            d.update_params();
            d.process(&mut out, 256);
            for v in out.iter() {
                peak = peak.max(v.abs());
            }
        }
        let head = d.ctl.playhead.get();
        assert!(peak > 0.4, "变速后应出声，peak={peak}");
        assert!(
            (head - (1.08 + 0.92 - KEYLOCK_LATENCY_S * 1.08)).abs() < 0.01,
            "变速后播放头应到 1.9874s 且不冻结，实际 {head}"
        );
    }

    /// keylock 开：+8% 变速音高不变（440Hz ±10 cents）。
    #[test]
    fn keylock_holds_pitch_at_rate() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(200), 8.0); // 1.08×
        let rec = run_capture(&mut d, 4.0);
        assert!(rec.iter().all(|v| v.is_finite()), "输出出现 NaN/Inf");
        let freq = zero_crossing_freq(&rec, 48000, 3 * 48000); // 跳过热身期
        let cents = cents_off(freq, 440.0);
        assert!(
            cents.abs() <= 10.0,
            "keylock 开音高应保持 440Hz，实测 {freq:.2}Hz（{cents:.2} cents）"
        );
    }

    /// keylock 关：纯 varispeed，+8% → 音高 475.2Hz。
    #[test]
    fn keylock_off_follows_tempo() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(200), 8.0);
        d.ctl.keylock.set(0.0);
        let rec = run_capture(&mut d, 4.0);
        assert!(rec.iter().all(|v| v.is_finite()), "输出出现 NaN/Inf");
        let freq = zero_crossing_freq(&rec, 48000, 3 * 48000);
        let cents = cents_off(freq, 440.0 * 1.08);
        assert!(
            cents.abs() <= 10.0,
            "keylock 关音高应随变速到 475.2Hz，实测 {freq:.2}Hz（{cents:.2} cents）"
        );
    }

    /// key shift +3 半音：440 → 523.25Hz（keylock 开，速率不变）。
    #[test]
    fn pitch_shift_moves_key() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(200), 0.0);
        d.ctl.pitch.set(3.0);
        let rec = run_capture(&mut d, 4.0);
        assert!(rec.iter().all(|v| v.is_finite()), "输出出现 NaN/Inf");
        let freq = zero_crossing_freq(&rec, 48000, 3 * 48000);
        let cents = cents_off(freq, 523.25);
        assert!(
            cents.abs() <= 10.0,
            "+3 半音应到 523.25Hz，实测 {freq:.2}Hz（{cents:.2} cents）"
        );
    }

    /// keylock 播放中 seek：reset + 预卷 + 新世代数据，无 NaN 无冻结。
    #[test]
    fn seek_during_keylock() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, mut prod) = deck_with_ring_and_prod(&bus, sine_chunks(64), 0.0);
        run_frames(&mut d, 48000); // 播 1 秒
        let head_before = d.ctl.playhead.get();
        assert!(
            (head_before - (1.0 - KEYLOCK_LATENCY_S)).abs() < 0.01,
            "seek 前播头 {head_before}"
        );

        d.seek_seconds(1.0);
        // 模拟 reader 响应 seek：从 read_frame 推送新世代数据。
        // 1 秒内容 + preroll（1584 帧被 priming 丢弃）需要 ≈26 chunk，推 32 留余量。
        let read_frame = 48000 - d.keylocker.as_ref().unwrap().warm_start_preroll_frames() as u64;
        for k in 0..32 {
            prod.try_push(sine_chunk_at(read_frame + k as u64 * 2048, 2048, d.epoch))
                .ok()
                .expect("ring 容量足够");
        }
        let mut out = vec![0.0; 256 * 2];
        let mut peak = 0.0f32;
        for _ in 0..(48000 / 256) {
            d.update_params();
            d.process(&mut out, 256);
            for v in out.iter() {
                assert!(v.is_finite(), "seek 后输出出现 NaN/Inf");
                peak = peak.max(v.abs());
            }
        }
        let head = d.ctl.playhead.get();
        assert!(peak > 0.4, "seek 后应出声，peak={peak}");
        assert!(
            (head - (2.0 - KEYLOCK_LATENCY_S)).abs() < 0.02,
            "seek 后播头应 ≈1.988s 且不冻结，实际 {head}"
        );
    }

    /// keylock 播放中开关：交叉淡化切换，音高从 440 → 475.2Hz，无 NaN。
    #[test]
    fn keylock_toggle_midplay() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(200), 8.0);
        let rec = {
            let mut out = vec![0.0; 256 * 2];
            let mut rec = Vec::new();
            // 1 秒 keylock 开 + 3 秒 keylock 关
            for block in 0..(4.0 * 48000.0 / 256.0) as usize {
                if block == 48000 / 256 {
                    d.ctl.keylock.set(0.0);
                }
                d.update_params();
                d.process(&mut out, 256);
                rec.extend_from_slice(&out);
            }
            rec
        };
        assert!(rec.iter().all(|v| v.is_finite()), "输出出现 NaN/Inf");
        let freq = zero_crossing_freq(&rec, 2 * 48000, 4 * 48000);
        let cents = cents_off(freq, 440.0 * 1.08);
        assert!(
            cents.abs() <= 10.0,
            "keylock 关后音高应到 475.2Hz，实测 {freq:.2}Hz（{cents:.2} cents）"
        );
    }

    /// pitch 扫过 ±3 半音阈值：profile 重建（窄→宽→窄），全程无 NaN、
    /// 输出有界、播头持续推进；宽频段音高正确（+5 半音 ≈587.3Hz）。
    #[test]
    fn pitch_sweep_crosses_profile_switch() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(200), 0.0);
        let rec = {
            let mut out = vec![0.0; 256 * 2];
            let mut rec = Vec::new();
            let blocks = (4.0 * 48000.0 / 256.0) as usize;
            for block in 0..blocks {
                if block == 48000 / 256 {
                    d.ctl.pitch.set(5.0); // 跨阈值 → WideKeylock 重建
                } else if block == 3 * 48000 / 256 {
                    d.ctl.pitch.set(0.0); // 回到窄频 → 重建
                }
                d.update_params();
                d.process(&mut out, 256);
                rec.extend_from_slice(&out);
            }
            rec
        };
        assert!(rec.iter().all(|v| v.is_finite()), "输出出现 NaN/Inf");
        assert!(
            rec.iter().all(|v| v.abs() < 1.0),
            "profile 切换后输出不得失控"
        );
        // 宽频段（+5 半音）：587.33Hz
        let freq = zero_crossing_freq(&rec, 2 * 48000, 3 * 48000);
        let cents = cents_off(freq, 587.33);
        assert!(
            cents.abs() <= 15.0,
            "+5 半音应到 587.33Hz，实测 {freq:.2}Hz（{cents:.2} cents）"
        );
        assert!(
            d.ctl.playhead.get() > 3.4,
            "profile 切换后播头应持续推进，实际 {}",
            d.ctl.playhead.get()
        );
    }

    /// 曲尾：喂到 EOF → finish 冲刷 → 位置冻结 → 自动停止。
    #[test]
    fn eof_stops_playback() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(8), 0.0); // 8×2048 = 16384 帧
        d.track_frames.store(16384, Ordering::Relaxed);
        let (peak, head) = run_frames(&mut d, 48000 * 2);
        assert!(peak > 0.4, "曲中应出声，peak={peak}");
        assert!(
            d.ctl.play.get() < 0.5,
            "EOF 后应自动停止，play={}",
            d.ctl.play.get()
        );
        assert!(head < 0.4, "EOF 播头应停在曲尾附近，实际 {head}");
    }

    /// 引擎构建失败的回退路径：线性插值照常播放（无引擎延迟偏移）。
    #[test]
    fn legacy_fallback_plays() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        d.keylocker = None;
        let (peak, head) = run_frames(&mut d, 48000);
        assert!(peak > 0.4, "回退路径应输出正弦波，peak={peak}");
        assert!(
            (head - 1.0).abs() < 0.01,
            "回退路径播头应到 1.0s，实际 {head}"
        );
    }

    // ---------- P5 beat sync ----------

    /// 拍脉冲 chunk：440Hz 正弦底（幅度 0.05）+ 每 beat_period_frames 全局帧
    /// 一个 8ms 指数衰减脉冲（幅度 0.5）——输出包络可测节奏与相位。
    fn pulse_chunks(n: usize, beat_period_frames: usize) -> Vec<Chunk> {
        let mut v = Vec::new();
        for k in 0..n {
            let mut data = Vec::with_capacity(CHUNK_FRAMES * 2);
            for f in 0..CHUNK_FRAMES {
                let g = (k * CHUNK_FRAMES + f) as f64;
                let since = (g % beat_period_frames as f64) / 48000.0;
                let pulse = if since < 0.012 {
                    (-since / 0.004).exp() * 0.5
                } else {
                    0.0
                };
                let s = ((2.0 * std::f64::consts::PI * 440.0 * (g / 48000.0)).sin()
                    * (0.05 + pulse)) as f32;
                data.push(s);
                data.push(s);
            }
            v.push(Chunk {
                epoch: 1,
                start_frame: (k * CHUNK_FRAMES) as u64,
                data: data.into_boxed_slice(),
            });
        }
        v
    }

    /// 预填大 ring 的测试 deck（1024 chunk ≈ 43.7s，同步长测用）。
    fn deck_with_ring_big(bus: &hypermixx_core::ControlBus, chunks: Vec<Chunk>, rate_pct: f64) -> Deck {
        let mut d = Deck::new(0, 48000, bus);
        let (mut prod, cons) = ringbuf::HeapRb::<Chunk>::new(1024).split();
        for c in chunks {
            prod.try_push(c).ok().expect("ring 容量足够");
        }
        d.chunk_rx = Some(cons);
        d.epoch = 1;
        d.loaded = true;
        d.ctl.play.set(1.0);
        d.ctl.volume.set(1.0);
        d.ctl.rate.set(rate_pct);
        d.ctl.keylock.set(1.0);
        d.keylocker = TimestretchLocker::build(48_000, false)
            .ok()
            .map(|k| Box::new(k) as Box<dyn Keylocker>);
        d
    }

    /// 输出包络的拍脉冲时刻（秒）：64 帧 RMS 窗口（32 帧步进）→ 局部极大值
    /// → 抛物线插值。from/to 为窗口。
    fn envelope_beat_times(rec: &[f32], from_sec: f64, to_sec: f64) -> Vec<f64> {
        let win = 64usize;
        let sr = 48000.0;
        let step_s = (win / 2) as f64 / sr;
        let mut rms = Vec::with_capacity(rec.len() / 2 / (win / 2));
        let mut i = 0;
        while i + win <= rec.len() / 2 {
            let mut s = 0.0f64;
            for j in 0..win {
                let v = rec[(i + j) * 2] as f64;
                s += v * v;
            }
            rms.push((s / win as f64).sqrt());
            i += win / 2;
        }
        let f0 = (from_sec / step_s) as usize;
        let f1 = ((to_sec / step_s) as usize).min(rms.len());
        let mut times = Vec::new();
        let mut i = (f0 + 1).max(1);
        while i + 1 < f1 {
            if rms[i] > 0.1 && rms[i] >= rms[i - 1] && rms[i] > rms[i + 1] {
                // 抛物线插值峰位置
                let a = 0.5 * (rms[i + 1] + rms[i - 1]) - rms[i];
                let b = 0.5 * (rms[i + 1] - rms[i - 1]);
                let off = if a != 0.0 {
                    (-b / (2.0 * a)).clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                times.push((i as f64 + off) * step_s);
                i += win; // 跳过本脉冲（下一拍在 ~0.5s 外）
            } else {
                i += 1;
            }
        }
        times
    }

    fn median_spacing(times: &[f64]) -> f64 {
        let mut d: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
        d.sort_by(|a, b| a.partial_cmp(b).unwrap());
        d[d.len() / 2]
    }

    /// 平均间距：keylock 引擎在非整数速率比下拍包络峰位随 hop 相位
    /// 交替（相邻间距 ±3ms 摆动、均值 = 真实拍距），均值不受交替
    /// 相位/窗口内脉冲奇偶影响，测速率用它。
    fn mean_spacing(times: &[f64]) -> f64 {
        let d: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
        d.iter().sum::<f64>() / d.len() as f64
    }

    /// 假 leader：按真实时间推进位置（模拟另一 deck 的实际播放）。
    ///
    /// `pos` 语义 = 真实 deck 的播头（**出声位置** = 实时 − 管线延迟，
    /// 见 deck pos = feed_base + source_position 与 playhead 测试的
    /// −KEYLOCK_LATENCY_S 偏移）。故 pos 初始化为 −KEYLOCK_LATENCY_S：
    /// 与 follower 的延迟相消（生产里两 deck 都有延迟，同样相消），
    /// 否则测试架会引入恒定的 +11.7ms 相位偏置。
    struct FakeLeader {
        grid_bpm: f64,
        grid_offset: f64,
        tempo_rate: f64,
        pos: f64,
    }

    impl FakeLeader {
        fn snapshot(&self) -> SyncLeader {
            SyncLeader {
                loaded: true,
                playing: true,
                grid_bpm: self.grid_bpm,
                grid_offset: self.grid_offset,
                tempo_rate: self.tempo_rate,
                position_secs: self.pos,
            }
        }
        fn advance(&mut self) {
            self.pos += 256.0 / 48000.0 * self.tempo_rate;
        }
    }

    /// 同步驱动：每块 update_params → apply_sync → process，记录输出
    /// （复刻 engine.rs 的每块顺序）。leader 在 process 后推进：其位置
    /// 与 follower 播头（process 末尾更新）落在同一块边界——真实引擎里
    /// 双方播头都在各自的 process 末尾更新，采样时刻一致。
    fn run_sync(d: &mut Deck, leader: &mut FakeLeader, seconds: f64) -> Vec<f32> {
        let mut out = vec![0.0; 256 * 2];
        let mut rec = Vec::new();
        let blocks = (seconds * 48000.0 / 256.0).round() as usize;
        for _ in 0..blocks {
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(&mut out, 256);
            leader.advance();
            rec.extend_from_slice(&out);
        }
        rec
    }

    /// 120 vs 128：follower（滑杆 −8%）开 sync 后速率锁定到 128 BPM，
    /// 20s 窗口内间距 0.46875s、相位与 leader 拍一致（≤10ms）。
    #[test]
    fn sync_locks_follower_to_leader() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring_big(&bus, pulse_chunks(600, 24000), -8.0); // 120 BPM 脉冲
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        let mut leader = FakeLeader {
            grid_bpm: 128.0,
            grid_offset: 0.0,
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let rec = run_sync(&mut d, &mut leader, 20.0);
        let times = envelope_beat_times(&rec, 10.0, 20.0);
        assert!(times.len() >= 15, "窗口内应检测到足够脉冲，实得 {}", times.len());
        let sp = median_spacing(&times);
        assert!(
            (sp - 0.46875).abs() < 0.003,
            "同步后拍间距应 0.46875s（128 BPM），实得 {sp:.4}s"
        );
        // 相位：leader 拍 = k×0.46875 + 延迟常数；残差波动 ≤10ms
        let mut res: Vec<f64> = times
            .iter()
            .map(|&t| {
                let k = (t / 0.46875).round();
                t - k * 0.46875
            })
            .collect();
        let med = {
            let mut s = res.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            s[s.len() / 2]
        };
        res.retain(|r| (r - med).abs() < 0.02);
        assert!(
            res.iter().all(|r| (r - med).abs() < 0.010),
            "相位残差应稳定（≤10ms 抖动），中位 {med:.4}s，残差 {res:?}"
        );
    }

    /// 相位收敛（P10.1 PI 锁）：leader 网格领先 0.25 拍 → follower 连续
    /// 修正（τ ≈ 拍长/Kp ≈ 1.4s），4s 后拍脉冲落在 leader 拍
    /// （0.125 + k×0.5）上（残差 ≤10ms；死区 5ms 内锁死不再纹波）。
    #[test]
    fn sync_phase_correction_snaps() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring_big(&bus, pulse_chunks(600, 24000), 0.0); // 120 BPM 脉冲
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        let mut leader = FakeLeader {
            grid_bpm: 120.0,
            grid_offset: 0.125, // 0.25 拍领先
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let rec = run_sync(&mut d, &mut leader, 10.0);
        let times = envelope_beat_times(&rec, 4.0, 10.0);
        assert!(times.len() >= 10, "窗口内应检测到足够脉冲，实得 {}", times.len());
        for &t in &times {
            let k = ((t - 0.125) / 0.5).round();
            let err = t - (0.125 + k * 0.5) - KEYLOCK_LATENCY_S;
            assert!(
                err.abs() < 0.010,
                "拍 {k} 相位误差 {err:+.4}s（应 ≤10ms）"
            );
        }
    }

    /// leader 拉 tempo 推子 follower 立即跟随（P10.1 根因回归）：锁相
    /// 状态下 leader 1.0 → 1.04，follower 下一块重发基础速率，间距
    /// 0.5 → 0.4808s，相位不丢（残差围绕中位 ≤10ms）。
    #[test]
    fn sync_follows_leader_slider_drag() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring_big(&bus, pulse_chunks(600, 24000), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        let mut leader = FakeLeader {
            grid_bpm: 120.0,
            grid_offset: 0.0,
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let mut out = vec![0.0; 256 * 2];
        let mut rec = Vec::new();
        let blocks = (16.0 * 48000.0 / 256.0) as usize;
        for b in 0..blocks {
            if b == (8.0 * 48000.0 / 256.0) as usize {
                leader.tempo_rate = 1.04; // leader 拉 tempo 推子
            }
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(&mut out, 256);
            leader.advance();
            rec.extend_from_slice(&out);
        }
        let times = envelope_beat_times(&rec, 9.0, 16.0);
        let sp = median_spacing(&times);
        assert!(
            (sp - 0.5 / 1.04).abs() < 0.003,
            "leader 1.04× 后间距应 0.4808s，实得 {sp:.4}s"
        );
        // 相位：拖动后拍沿 8s 处连续延伸，残差围绕中位 ≤10ms
        let mut res: Vec<f64> = times
            .iter()
            .map(|&t| {
                let k = (t / (0.5 / 1.04)).round();
                t - k * (0.5 / 1.04)
            })
            .collect();
        let med = {
            let mut s = res.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            s[s.len() / 2]
        };
        res.retain(|r| (r - med).abs() < 0.02);
        assert!(
            res.iter().all(|r| (r - med).abs() < 0.010),
            "相位残差应 ≤10ms，中位 {med:.4}s，残差 {res:?}"
        );
    }

    /// 0.05% 网格失配（leader 120.06 vs follower 120）长跑 30s：
    /// target 精确补偿后双方相位零漂移（I 项把漂出死区的残差推回），
    /// 全程间距 = 60/120.06、残差 ≤10ms——用户"不失去同步"的稳定性验收。
    #[test]
    fn sync_integral_compensates_bpm_mismatch() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring_big(&bus, pulse_chunks(900, 24000), 0.0); // 120 BPM 脉冲
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        let mut leader = FakeLeader {
            grid_bpm: 120.06, // 0.05% 失配
            grid_offset: 0.0,
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let rec = run_sync(&mut d, &mut leader, 30.0);
        let times = envelope_beat_times(&rec, 15.0, 30.0);
        assert!(times.len() >= 25, "窗口内应检测到足够脉冲，实得 {}", times.len());
        let sp = median_spacing(&times);
        assert!(
            (sp - 60.0 / 120.06).abs() < 0.003,
            "同步后间距应 60/120.06 = {:.5}s，实得 {sp:.4}s",
            60.0 / 120.06
        );
        let mut res: Vec<f64> = times
            .iter()
            .map(|&t| {
                let k = (t / (60.0 / 120.06)).round();
                t - k * (60.0 / 120.06)
            })
            .collect();
        let med = {
            let mut s = res.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            s[s.len() / 2]
        };
        res.retain(|r| (r - med).abs() < 0.02);
        assert!(
            res.iter().all(|r| (r - med).abs() < 0.010),
            "30s 长跑相位残差应 ≤10ms，中位 {med:.4}s，残差 {res:?}"
        );
    }

    /// pitch 轴（P10.1 修复）：keylock 开 +6ST 时引擎速率 = r/p =
    /// 1/2^(6/12)——apply_sync 每块下发的必须是引擎轴速率而非源轴
    /// target（旧实现把 target 直接调度到引擎轴，音高开启时修正被
    /// r/p 放偏）。验证 last_sent_rate。
    #[test]
    fn sync_pitch_axis_rate_is_engine_rate() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring_big(&bus, pulse_chunks(600, 24000), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        d.ctl.pitch.set(6.0);
        let mut leader = FakeLeader {
            grid_bpm: 120.0,
            grid_offset: 0.0,
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let mut out = vec![0.0; 256 * 2];
        // 4s：覆盖 wide profile 重建（首块）与 PI 收敛瞬态
        for _ in 0..(4.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(&mut out, 256);
            leader.advance();
        }
        let expect = 1.0 / 2f64.powf(6.0 / 12.0);
        assert!(
            (d.last_sent_rate.unwrap() - expect).abs() < 1e-9,
            "引擎轴速率应 = r/p = {expect}，实得 {:?}",
            d.last_sent_rate
        );
    }

    /// wrap 语义文档化：leader 领先 1.25 拍 → wrap 到 0.25 拍收敛
    /// （整拍偏移对 PI 不可见；由 P10.2 网格锚点精度缓解）。follower
    /// 收敛到 leader 的最近等价相位（0.125 + k×0.5）。
    #[test]
    fn sync_whole_beat_phase_offset_stays_wrapped() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring_big(&bus, pulse_chunks(600, 24000), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        let mut leader = FakeLeader {
            grid_bpm: 120.0,
            grid_offset: 0.625, // 1.25 拍领先（整拍 + 0.25）
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let rec = run_sync(&mut d, &mut leader, 10.0);
        let times = envelope_beat_times(&rec, 4.0, 10.0);
        assert!(times.len() >= 10, "窗口内应检测到足够脉冲，实得 {}", times.len());
        for &t in &times {
            let k = ((t - 0.125) / 0.5).round();
            let err = t - (0.125 + k * 0.5) - KEYLOCK_LATENCY_S;
            assert!(
                err.abs() < 0.010,
                "应收敛到 0.25 拍等价相位（0.125 + k×0.5），err={err:+.4}s"
            );
        }
    }

    /// sync 忽略 follower 滑杆（含同步中途拖动）：滑杆 +8% 起步，
    /// 中途拖到 −8%，同步目标仍 1.0×（间距 0.5s 不变）。
    #[test]
    fn sync_ignores_follower_slider() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring_big(&bus, pulse_chunks(600, 24000), 8.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        let mut leader = FakeLeader {
            grid_bpm: 120.0,
            grid_offset: 0.0,
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let mut out = vec![0.0; 256 * 2];
        let mut rec = Vec::new();
        let blocks = (14.0 * 48000.0 / 256.0) as usize;
        for b in 0..blocks {
            if b == (7.0 * 48000.0 / 256.0) as usize {
                d.ctl.rate.set(-8.0); // 同步中途拖 follower 滑杆：应被忽略
            }
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(&mut out, 256);
            leader.advance();
            rec.extend_from_slice(&out);
        }
        for (from, to, label) in [(2.0, 7.0, "拖动前"), (8.0, 14.0, "拖动后")] {
            let times = envelope_beat_times(&rec, from, to);
            let sp = median_spacing(&times);
            assert!(
                (sp - 0.5).abs() < 0.003,
                "{label} sync 下速率应由 leader 决定（间距 0.5s），实得 {sp:.4}s"
            );
        }
    }

    /// 手动 BPM 编辑即时生效：follower 网格 120 → 130，目标速率
    /// 120/130 = 0.923，输出间距 0.5417s；随后 leader 拉推子 1.04×，
    /// follower 下一块跟随（间距 0.5417/1.04 = 0.5208s）。
    #[test]
    fn sync_follows_manual_bpm_edit() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring_big(&bus, pulse_chunks(900, 24000), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        let mut leader = FakeLeader {
            grid_bpm: 120.0,
            grid_offset: 0.0,
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let mut out = vec![0.0; 256 * 2];
        let mut rec = Vec::new();
        let blocks = (24.0 * 48000.0 / 256.0) as usize;
        for b in 0..blocks {
            if b == (6.0 * 48000.0 / 256.0) as usize {
                d.ctl.grid_bpm.set(130.0); // 模拟 nudge 编辑
            }
            if b == (16.0 * 48000.0 / 256.0) as usize {
                leader.tempo_rate = 1.04; // leader 拉 tempo 推子
            }
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(&mut out, 256);
            leader.advance();
            rec.extend_from_slice(&out);
        }
        // 编辑 6s 后（>3τ）收敛完：间距 0.5417s（速率 0.923×）。
        // 用平均间距：引擎非整数速率比下包络峰位按 hop 相位交替
        // （见 mean_spacing），均值才是真实拍距。
        let times = envelope_beat_times(&rec, 12.0, 16.0);
        let sp = mean_spacing(&times);
        assert!(
            (sp - 0.5 * 130.0 / 120.0).abs() < 0.003,
            "编辑后间距应 0.5417s（速率 0.923×），实得 {sp:.4}s"
        );
        let times = envelope_beat_times(&rec, 18.0, 24.0);
        let sp = mean_spacing(&times);
        assert!(
            (sp - 0.5 * 130.0 / 120.0 / 1.04).abs() < 0.003,
            "leader 1.04× 后间距应 0.5208s，实得 {sp:.4}s"
        );
    }

    /// P14+P15：取消 sync 时推子仅解除锁定——播放速率保持 sync 期间值，
    /// 不瞬跳回滑杆位置（旧行为每块无条件 rate = slider）；此后推子与
    /// 当前速率可能脱开，移动需先回位（进入当前速率 ±EPS 带）才恢复
    /// 直通（软接管，防触摸跳变直接拉速）。
    #[test]
    fn sync_disengage_soft_takeover_until_fader_returns() {
        let bus = hypermixx_core::ControlBus::default();
        // 滑杆 +8%（≠ 同步速率 1.0）：解锁时若跳回滑杆，bpm 会跳到 129.6
        let mut d = deck_with_ring_big(&bus, pulse_chunks(900, 24000), 8.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        let mut leader = FakeLeader {
            grid_bpm: 120.0,
            grid_offset: 0.0,
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let mut out = vec![0.0; 256 * 2];
        let blocks = |secs: f64| (secs * 48000.0 / 256.0) as usize;
        let run = |d: &mut Deck, leader: &mut FakeLeader, out: &mut [f32], secs: f64| {
            for _ in 0..blocks(secs) {
                d.update_params();
                d.apply_sync(&leader.snapshot());
                d.process(out, 256);
                leader.advance();
            }
        };
        run(&mut d, &mut leader, &mut out, 4.0);
        let bpm_synced = d.ctl.bpm.get(); // ≈ 120（sync 锁 leader，忽略滑杆 +8%）
        assert!((bpm_synced - 120.0).abs() < 1.0, "sync 后 bpm 应 ≈ 120：{bpm_synced}");

        // 取消 sync：速率保持（推子仅解锁），继续播放
        d.ctl.sync.set(0.0);
        let p0 = d.ctl.playhead.get();
        run(&mut d, &mut leader, &mut out, 2.0);
        let bpm_unlocked = d.ctl.bpm.get();
        assert!(
            (bpm_unlocked - bpm_synced).abs() < 0.01,
            "取消 sync 后 bpm 应保持（推子仅解锁，不跳回 +8% 滑杆）：{bpm_unlocked} vs {bpm_synced}"
        );
        let p1 = d.ctl.playhead.get();
        assert!(
            (p1 - p0 - 2.0).abs() < 0.02,
            "解锁后播放速率连续（≈1.0×）：{p0} → {p1}"
        );

        // P15 软接管：推子跳到 +4%（未回位：距当前速率 1.0 差 4% > EPS）
        // → 速率保持不动（触摸跳变不直接拉速）
        d.ctl.rate.set(4.0);
        run(&mut d, &mut leader, &mut out, 2.0);
        let bpm_held = d.ctl.bpm.get();
        assert!(
            (bpm_held - bpm_synced).abs() < 0.01,
            "推子 +4% 未回位：bpm 应保持 {bpm_synced}：{bpm_held}"
        );
        let p2 = d.ctl.playhead.get();
        assert!(
            (p2 - p1 - 2.0).abs() < 0.02,
            "未回位期间速率保持 1.0×：{p1} → {p2}"
        );

        // 回位（0% 进入当前速率带）→ 接管生效，恢复直通
        d.ctl.rate.set(0.0);
        run(&mut d, &mut leader, &mut out, 0.5);
        // 直通后滑杆 +4% 正常调速 → 124.8 BPM
        d.ctl.rate.set(4.0);
        run(&mut d, &mut leader, &mut out, 2.0);
        let bpm_slider = d.ctl.bpm.get();
        assert!(
            (bpm_slider - 124.8).abs() < 0.5,
            "回位后滑杆 +4% 应恢复直通（bpm ≈ 124.8）：{bpm_slider}"
        );
    }

    /// P15：sync 期间推子软接管——滑杆停在 +8%（≠ 锁速率 1.0），小步
    /// 拖过目标速率带（0% ±0.5）→ fader_armed 接管：rate = 推子（暂时
    /// 加减速），BPM 显示/波形窗口保持锁值 120（不缩放）；操作后不再
    /// 自动对拍（相位差保持，sync_align_done 不受复位）；拖回穿过带 →
    /// 重新锁定，相位差仍不追回。
    #[test]
    fn sync_fader_temporary_takeover_then_relock_without_realign() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring_big(&bus, pulse_chunks(900, 24000), 8.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        let mut leader = FakeLeader {
            grid_bpm: 120.0,
            grid_offset: 0.0,
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let grid = BeatGrid {
            bpm: 120.0,
            offset_secs: 0.0,
        };
        // 相位差（leader − follower，wrap ±0.5 拍，与 apply_sync 同语义）
        let phase_err = |d: &Deck, leader: &FakeLeader| {
            let lc = BeatClock::from_grid_at(&grid, leader.pos);
            let fc = BeatClock::from_grid_at(&grid, d.pos / d.sr);
            let mut e = lc.phase - fc.phase;
            if e > 0.5 {
                e -= 1.0;
            } else if e < -0.5 {
                e += 1.0;
            }
            e
        };
        let mut out = vec![0.0; 256 * 2];
        let blocks = |secs: f64| (secs * 48000.0 / 256.0) as usize;
        let step_block = |d: &mut Deck, leader: &mut FakeLeader, out: &mut [f32]| {
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(out, 256);
            leader.advance();
        };
        for _ in 0..blocks(4.0) {
            step_block(&mut d, &mut leader, &mut out);
        }
        let err0 = phase_err(&d, &leader);
        assert!(err0.abs() < 0.02, "预热后应已对齐：err={err0}");

        // 小步拖下：+8 → −8（每块 1.0% ≤ FADER_STEP_MAX），穿过 0% 带
        // → 接管；全程 BPM 显示锁 120（波形不缩放）。
        for k in (0..=16).rev() {
            d.ctl.rate.set(k as f64 - 8.0);
            step_block(&mut d, &mut leader, &mut out);
            assert!(
                (d.ctl.bpm.get() - 120.0).abs() < 1e-9,
                "拖动中 BPM 显示应锁 120：{}",
                d.ctl.bpm.get()
            );
        }
        assert!(d.fader_armed, "拖过目标带后应接管（armed）");
        assert!((d.rate - 0.92).abs() < 1e-9, "接管后 rate = 推子 0.92：{}", d.rate);

        // 暂时加减速 2s：播头按 0.92× 前进，相位差积累（不自动对拍），
        // BPM 显示仍 120
        let p0 = d.ctl.playhead.get();
        for _ in 0..blocks(2.0) {
            step_block(&mut d, &mut leader, &mut out);
        }
        let p1 = d.ctl.playhead.get();
        assert!(
            (p1 - p0 - 2.0 * 0.92).abs() < 0.05,
            "接管后按 0.92× 暂时减速：{p0} → {p1}"
        );
        assert!(
            (d.ctl.bpm.get() - 120.0).abs() < 1e-9,
            "暂时加减速中 BPM 显示仍锁 120：{}",
            d.ctl.bpm.get()
        );
        let err1 = phase_err(&d, &leader);
        assert!(
            (err1 - err0).abs() > 0.2,
            "暂时减速应积累相位差（不再自动对拍）：{err0} → {err1}"
        );

        // 小步拖回：−8 → +8，穿过 0% 带 → 重新锁定（rate = target），
        // 相位差保持不追回
        for k in (0..=16).rev() {
            d.ctl.rate.set(8.0 - k as f64);
            step_block(&mut d, &mut leader, &mut out);
        }
        assert!(!d.fader_armed, "拖回穿过带后应重新锁定");
        assert!((d.rate - 1.0).abs() < 1e-9, "锁定后 rate = target 1.0：{}", d.rate);
        let p2 = d.ctl.playhead.get();
        for _ in 0..blocks(2.0) {
            step_block(&mut d, &mut leader, &mut out);
        }
        let p3 = d.ctl.playhead.get();
        assert!(
            (p3 - p2 - 2.0).abs() < 0.02,
            "重新锁定后恢复 1.0×：{p2} → {p3}"
        );
        assert!(
            (d.ctl.bpm.get() - 120.0).abs() < 1e-9,
            "锁定后 BPM 显示 120：{}",
            d.ctl.bpm.get()
        );
        let err2 = phase_err(&d, &leader);
        assert!(
            (err2 - err1).abs() < 0.02,
            "锁定后不追回相位差（操作后 sync 不再自动对拍）：{err1} → {err2}"
        );
    }

    /// P15：sync 下 nudge 按住 = 暂时加减速——引擎轴 ×1.08（不并入
    /// self.rate），BPM 显示/波形窗口保持锁值 120（不缩放），相位差
    /// 积累（不自动对拍）；松开恢复锁速率、相位差保持。
    #[test]
    fn sync_nudge_bends_tempo_without_bpm_rescale() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring_big(&bus, pulse_chunks(900, 24000), 8.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        let mut leader = FakeLeader {
            grid_bpm: 120.0,
            grid_offset: 0.0,
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let grid = BeatGrid {
            bpm: 120.0,
            offset_secs: 0.0,
        };
        let phase_err = |d: &Deck, leader: &FakeLeader| {
            let lc = BeatClock::from_grid_at(&grid, leader.pos);
            let fc = BeatClock::from_grid_at(&grid, d.pos / d.sr);
            let mut e = lc.phase - fc.phase;
            if e > 0.5 {
                e -= 1.0;
            } else if e < -0.5 {
                e += 1.0;
            }
            e
        };
        let mut out = vec![0.0; 256 * 2];
        let blocks = |secs: f64| (secs * 48000.0 / 256.0) as usize;
        let step_block = |d: &mut Deck, leader: &mut FakeLeader, out: &mut [f32]| {
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(out, 256);
            leader.advance();
        };
        for _ in 0..blocks(4.0) {
            step_block(&mut d, &mut leader, &mut out);
        }
        let err0 = phase_err(&d, &leader);
        assert!(err0.abs() < 0.02, "预热后应已对齐：err={err0}");

        // nudge 按住 2s：暂时加速 ×1.08（keylock 关 = varispeed bend），
        // BPM 显示仍 120
        d.ctl.nudge.set(1.0);
        let p0 = d.ctl.playhead.get();
        for _ in 0..blocks(2.0) {
            step_block(&mut d, &mut leader, &mut out);
        }
        let p1 = d.ctl.playhead.get();
        assert!(
            (p1 - p0 - 2.0 * 1.08).abs() < 0.05,
            "nudge 按住应按 1.08× 暂时加速：{p0} → {p1}"
        );
        assert!(
            (d.ctl.bpm.get() - 120.0).abs() < 1e-9,
            "nudge 加速中 BPM 显示仍锁 120：{}",
            d.ctl.bpm.get()
        );
        let err1 = phase_err(&d, &leader);
        assert!(
            (err1 - err0).abs() > 0.2,
            "nudge 应积累相位差（不自动对拍）：{err0} → {err1}"
        );

        // 松开：恢复锁速率，相位差保持
        d.ctl.nudge.set(0.0);
        let p2 = d.ctl.playhead.get();
        for _ in 0..blocks(2.0) {
            step_block(&mut d, &mut leader, &mut out);
        }
        let p3 = d.ctl.playhead.get();
        assert!(
            (p3 - p2 - 2.0).abs() < 0.02,
            "松开后恢复 1.0×：{p2} → {p3}"
        );
        assert!(
            (d.ctl.bpm.get() - 120.0).abs() < 1e-9,
            "松开后 BPM 显示 120：{}",
            d.ctl.bpm.get()
        );
        let err2 = phase_err(&d, &leader);
        assert!(
            (err2 - err1).abs() < 0.02,
            "松开后不追回相位差：{err1} → {err2}"
        );
    }

    /// P14：对齐完成后 sync 下 seek（微调进度）不被拉回——旧连续 PI
    /// 每块追相位会把手动跳转抵消（"sync 下微调进度失败"）；一次性
    /// 对齐后仅速率锁，相位差保持 seek 引入的偏移。
    #[test]
    fn sync_seek_after_align_not_pulled_back() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, mut prod) = deck_with_ring_and_prod(&bus, sine_chunks(200), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.sync.set(1.0);
        let mut leader = FakeLeader {
            grid_bpm: 120.0,
            grid_offset: 0.0,
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let grid = BeatGrid {
            bpm: 120.0,
            offset_secs: 0.0,
        };
        // 相位差（leader − follower，wrap ±0.5 拍，与 apply_sync 同语义）
        let phase_err = |d: &Deck, leader: &FakeLeader| {
            let lc = BeatClock::from_grid_at(&grid, leader.pos);
            let fc = BeatClock::from_grid_at(&grid, d.pos / d.sr);
            let mut e = lc.phase - fc.phase;
            if e > 0.5 {
                e -= 1.0;
            } else if e < -0.5 {
                e += 1.0;
            }
            e
        };
        let mut out = vec![0.0; 256 * 2];
        let blocks = |secs: f64| (secs * 48000.0 / 256.0) as usize;
        for _ in 0..blocks(4.0) {
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(&mut out, 256);
            leader.advance();
        }
        assert!(
            phase_err(&d, &leader).abs() < 0.02,
            "预热后应已对齐：err={}",
            phase_err(&d, &leader)
        );

        // sync 下 seek：+0.37 拍（离拍微调）——不被拉回
        let target = d.ctl.playhead.get() + 0.37 * 0.5;
        d.seek_exact(target);
        // 模拟 reader 响应：128 chunk（5.5s）覆盖 4s 后断言窗口
        //（测试 deck 无 reader 线程，喂完即欠载、播头冻结）
        let start = ((target * 48000.0) as u64).saturating_sub(8192);
        for k in 0..128 {
            prod.try_push(sine_chunk_at(
                start + k as u64 * CHUNK_FRAMES as u64,
                CHUNK_FRAMES,
                d.epoch,
            ))
            .ok()
            .expect("ring 容量足够");
        }
        let seek_err = phase_err(&d, &leader); // ≈ −0.37 拍
        assert!(
            (seek_err + 0.37).abs() < 0.03,
            "seek 后相位差应 ≈ −0.37 拍：{seek_err}"
        );
        // 继续 sync 跑：相位差保持 seek 偏移（不被 PI 拉回），速率锁保持。
        // 先 0.5s 让 seek 瞬态（priming/引擎延迟重锚）过去，再对比：
        // 旧连续 PI 会把相位差朝 0 拉（4s 内 0.3 → 0.03 拍），新一次性
        // 对齐下应恒定。
        for _ in 0..blocks(0.5) {
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(&mut out, 256);
            leader.advance();
        }
        let err_settle = phase_err(&d, &leader);
        for _ in 0..blocks(3.5) {
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(&mut out, 256);
            leader.advance();
        }
        let err1 = phase_err(&d, &leader);
        assert!(
            (err1 - err_settle).abs() < 0.02,
            "相位偏移应保持恒定（不被拉回）：0.5s 后 {err_settle}，4s 后 {err1}"
        );
        assert!(err1.abs() > 0.1, "相位差应保持离拍（≈0.3 拍）：{err1}");
        assert!(
            (d.ctl.bpm.get() - 120.0).abs() < 1.0,
            "速率锁应保持（bpm ≈ 120）：{}",
            d.ctl.bpm.get()
        );
    }

    /// 无网格：follower 网格 0 或 leader 网格 0 → 不启用 sync，滑杆生效。
    #[test]
    fn sync_noop_without_grid() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring_big(&bus, pulse_chunks(600, 24000), 8.0);
        d.ctl.sync.set(1.0); // grid_bpm 保持 0
        let mut leader = FakeLeader {
            grid_bpm: 120.0,
            grid_offset: 0.0,
            tempo_rate: 1.0,
            pos: -KEYLOCK_LATENCY_S,
        };
        let rec = run_sync(&mut d, &mut leader, 8.0);
        let times = envelope_beat_times(&rec, 4.0, 8.0);
        let sp = median_spacing(&times);
        assert!(
            (sp - 0.5 / 1.08).abs() < 0.003,
            "follower 无网格应走滑杆 1.08×（间距 0.463s），实得 {sp:.4}s"
        );
        // leader 无网格：同样不启用
        d.ctl.grid_bpm.set(120.0);
        leader.grid_bpm = 0.0;
        let rec = run_sync(&mut d, &mut leader, 8.0);
        let times = envelope_beat_times(&rec, 4.0, 8.0);
        let sp = median_spacing(&times);
        assert!(
            (sp - 0.5 / 1.08).abs() < 0.003,
            "leader 无网格应走滑杆 1.08×，实得 {sp:.4}s"
        );
    }

    /// quantize seek：网格 120 BPM 时吸附到最近拍点；关闭/无网格不吸附。
    #[test]
    fn quantize_seek_snaps() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        // quantize 关：原样
        d.seek_seconds(0.76);
        assert!((d.ctl.playhead.get() - 0.76).abs() < 1e-9, "关闭时不吸附");
        // quantize 开：0.76 → 1.0；0.24 → 0.0
        d.ctl.quantize.set(1.0);
        d.seek_seconds(0.76);
        assert!(
            (d.ctl.playhead.get() - 1.0).abs() < 1e-9,
            "0.76 应吸附 1.0，实得 {}",
            d.ctl.playhead.get()
        );
        d.seek_seconds(0.24);
        assert!(
            (d.ctl.playhead.get() - 0.0).abs() < 1e-9,
            "0.24 应吸附 0.0，实得 {}",
            d.ctl.playhead.get()
        );
        // 无网格：不吸附
        d.ctl.grid_bpm.set(0.0);
        d.seek_seconds(0.76);
        assert!(
            (d.ctl.playhead.get() - 0.76).abs() < 1e-9,
            "无网格不吸附，实得 {}",
            d.ctl.playhead.get()
        );
    }

    /// 写 2s 440Hz 立体声 32-bit float WAV（真实解码路径用）。
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

    /// EOF 后 seek 必须重生读取线程（P2 计划移入 P6 的回归测试）。
    /// 场景：真实 reader 线程播完 2s 曲目自动停止（线程随 EOF 退出），
    /// 之后 seek 回 0.3s 再播——旧代码 Send 失败被静默吞掉 → 永久欠载。
    #[test]
    fn eof_seek_restarts_reader() {
        let bus = hypermixx_core::ControlBus::default();
        let path = std::env::temp_dir().join(format!("hypermixx_eof_seek_{}.wav", std::process::id()));
        write_sine_wav(&path, 2.0);

        let mut d = Deck::new(0, 48000, &bus);
        d.ctl.volume.set(1.0);
        d.load(path.clone());
        let mut out = vec![0.0f32; 256 * 2];
        // 播到 EOF：2s = 375 块 + 曲尾冲刷 + 8 块判停
        let mut blocks = 0;
        while d.ctl.play.get() > 0.5 && blocks < 500 {
            d.update_params();
            d.process(&mut out, 256);
            blocks += 1;
        }
        assert_eq!(d.ctl.play.get(), 0.0, "EOF 后应自动停止");
        assert!(blocks < 500, "2s 曲目应在 500 块内播完，实际 {blocks} 块");
        // 等读取线程完全退出（真机用户操作间隔远大于此；消除 is_finished
        // 竞态——并行测试负载下 20ms 固定 sleep 不够，轮询兜底）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !d._reader_handle.as_ref().unwrap().is_finished()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(d._reader_handle.as_ref().unwrap().is_finished());

        // EOF 后 seek：旧线程已死 → respawn_reader 重生 → 跳 0.3s → 重新播放
        d.seek_seconds(0.3);
        d.ctl.play.set(1.0);
        // 捕获 0.8s：reader 重生/预卷的启动瞬态在重载下可达数百 ms，
        // 测频窗放在捕获后段 [0.5s, 0.7s)，那时 reader 早已追平
        let rec = run_capture(&mut d, 0.8);
        // 出音且播头推进（未欠载冻结在 0.3s）
        let peak = rec.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.3, "seek 后应重新出音，peak={peak}");
        let head = d.ctl.playhead.get();
        assert!(head > 0.4, "播头应推进过 0.3s，实际 {head:.3}s");
        assert!(d.ctl.play.get() > 0.5, "重生后不应再次判停");
        // 频率仍是 440Hz（跳开 seek 缝与 reader 启动斜坡；参数为帧索引）
        let f = zero_crossing_freq(&rec, rec.len() / 2 - 14400, rec.len() / 2 - 4800);
        assert!(
            cents_off(f, 440.0).abs() < 10.0,
            "音高应保持 440Hz，实测 {f:.1}Hz"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// 对拍临时加减速：按住期间速率 ×1.08 / ×(1/1.08)，松手恢复。
    /// keylock 开：音高保持 440Hz（变速不变调）；关：纯 varispeed（±8% 变调）。
    #[test]
    fn nudge_bends_rate_temporarily() {
        let bus = hypermixx_core::ControlBus::default();
        // keylock 开 + nudge：音高不变
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        d.ctl.nudge.set(1.0);
        let _ = run_frames(&mut d, 4800); // 跨 set_rate 交叉淡化
        let rec = run_capture(&mut d, 0.5);
        let f = zero_crossing_freq(&rec, 2400, rec.len() / 2 - 2400);
        assert!(
            cents_off(f, 440.0).abs() < 10.0,
            "keylock 开 nudge 不应变调，实测 {f:.1}Hz"
        );
        // keylock 关 + nudge +：440 × 1.08 ≈ 475.2
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        d.ctl.keylock.set(0.0);
        d.ctl.nudge.set(1.0);
        let _ = run_frames(&mut d, 4800);
        let rec = run_capture(&mut d, 0.5);
        let f = zero_crossing_freq(&rec, 2400, rec.len() / 2 - 2400);
        assert!(
            cents_off(f, 440.0 * 1.08).abs() < 15.0,
            "keylock 关 nudge+ 应 ×1.08，实测 {f:.1}Hz"
        );
        // keylock 关 + nudge −：440 × (1/1.08) ≈ 407.4
        d.ctl.nudge.set(-1.0);
        let _ = run_frames(&mut d, 4800);
        let rec = run_capture(&mut d, 0.5);
        let f = zero_crossing_freq(&rec, 2400, rec.len() / 2 - 2400);
        assert!(
            cents_off(f, 440.0 / 1.08).abs() < 15.0,
            "keylock 关 nudge− 应 ×(1/1.08)，实测 {f:.1}Hz"
        );
        // 松手恢复 440
        d.ctl.nudge.set(0.0);
        let _ = run_frames(&mut d, 4800);
        let rec = run_capture(&mut d, 0.5);
        let f = zero_crossing_freq(&rec, 2400, rec.len() / 2 - 2400);
        assert!(
            cents_off(f, 440.0).abs() < 15.0,
            "松手应恢复原速率，实测 {f:.1}Hz"
        );
    }

    #[test]
    fn fx_distortion_clips_via_bus() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        // 纯总线驱动：drywet 1 + 换型失真
        bus.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(5.0);
        let _ = run_frames(&mut d, 256); // 触发换型
        // 换型写回：manifest 默认 drive=12dB；enable 不随换型置 1（P8 修复）
        assert!(
            (bus.control(&hypermixx_core::paths::deck_fx_p(0, 0, 0)).get() - 12.0).abs() < 1e-9,
            "换型应写回默认 drive=12dB"
        );
        assert!(
            bus.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).get() < 0.5,
            "换型不应强制 enable（只在 ON 时打开）"
        );
        bus.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(1.0);
        let _ = run_frames(&mut d, 256); // 淡入
        // 40dB drive：0.5 幅度正弦 ×100 → tanh 饱和
        bus.control(&hypermixx_core::paths::deck_fx_p(0, 0, 0)).set(40.0);
        let out = run_capture(&mut d, 1.0);
        let peak = out.iter().fold(0.0f32, |a, v| a.max(v.abs()));
        assert!(peak > 0.9, "40dB drive 应推到饱和区, peak={peak}");
        assert!(out.iter().all(|v| v.is_finite()), "无 NaN");
    }

    #[test]
    fn fx_unset_rack_is_passthrough() {
        // 空 rack 走整条链：输出与 FX 全关时一致（既有测试的逐位回归）
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        bus.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(0.0); // 显式空槽
        let out = run_capture(&mut d, 0.5);
        let f = zero_crossing_freq(&out, 2400, out.len() / 2 - 2400);
        assert!(
            cents_off(f, 440.0).abs() < 10.0,
            "空 rack 不影响信号，实测 {f:.1}Hz"
        );
    }

    /// 静音 chunk（模拟曲中/曲尾无声段，reader 持续供数据但内容为零）。
    fn silent_chunk_at(start: u64, epoch: u32) -> Chunk {
        Chunk {
            epoch,
            start_frame: start,
            data: vec![0.0; CHUNK_FRAMES * 2].into_boxed_slice(),
        }
    }

    /// 交织立体声 [lo_s, hi_s) 左声道峰值。
    fn window_peak_l(rec: &[f32], lo_s: f64, hi_s: f64) -> f32 {
        let lo = (lo_s * 48000.0) as usize;
        let hi = ((hi_s * 48000.0) as usize).min(rec.len() / 2);
        rec[lo * 2..hi * 2]
            .iter()
            .step_by(2)
            .fold(0.0f32, |m, v| m.max(v.abs()))
    }

    /// 交织立体声 [lo_s, hi_s) 左声道 RMS。
    fn window_rms_l(rec: &[f32], lo_s: f64, hi_s: f64) -> f32 {
        let lo = (lo_s * 48000.0) as usize;
        let hi = ((hi_s * 48000.0) as usize).min(rec.len() / 2);
        let sum: f64 = (lo..hi).map(|i| rec[i * 2] as f64 * rec[i * 2] as f64).sum();
        (sum / (hi - lo) as f64).sqrt() as f32
    }

    #[test]
    fn fx_echo_rings_after_input_stops() {
        let bus = hypermixx_core::ControlBus::default();
        // 8 chunk 正弦 + 24 chunk 静音：正弦结束（≈0.34s）后回声串继续衰减
        let mut chunks = sine_chunks(8);
        for k in 0..24 {
            chunks.push(silent_chunk_at((8 + k) as u64 * CHUNK_FRAMES as u64, 1));
        }
        let mut d = deck_with_ring(&bus, chunks, 0.0);
        bus.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(1.0); // echo
        let _ = run_frames(&mut d, 256); // 换型写回默认值
        bus.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(1.0);
        bus.control(&hypermixx_core::paths::deck_fx_p(0, 0, 0)).set(0.2); // time=0.2s
        bus.control(&hypermixx_core::paths::deck_fx_p(0, 0, 1)).set(0.8); // feedback=0.8
        let rec = run_capture(&mut d, 1.2);
        assert!(rec.iter().all(|v| v.is_finite()), "无 NaN");
        // 全湿输出：延迟线为空时（<0.2s）无输出
        let pre = window_peak_l(&rec, 0.02, 0.15);
        assert!(pre < 0.02, "首回声前应为静音, pre={pre}");
        // 输入停止后回声尾音可闻（末采样回声 ≈ 0.5·0.93 ≈ 0.46 @0.54s）
        let tail = window_peak_l(&rec, 0.5, 1.1);
        assert!(tail > 0.02, "输入停止后回声尾音应可闻, tail={tail}");
    }

    #[test]
    fn fx_gate_follows_beatgrid() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(200), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        bus.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(8.0); // gate
        bus.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(1.0);
        let rec = run_capture(&mut d, 2.0);
        assert!(rec.iter().all(|v| v.is_finite()), "无 NaN");
        // 120BPM、duty=0.5（默认）→ 每 0.25s 开关交替。0.1s 步进 × 0.2s 窗；
        // 窗口相位不锚定（keylock 延迟 560 帧），只断言交替模式：
        // 响窗（≥81% ON）rms≈0.34、静窗（≥92% OFF）rms<0.1、跨边界窗介于其间。
        let mut on = 0;
        let mut off_at = Vec::new();
        // 只扫完整窗（末窗 1.9–2.1 超出 2.0s 捕获被截断 → 排除）
        for k in 0..19 {
            let rms = window_rms_l(&rec, 0.1 * k as f64, 0.1 * k as f64 + 0.2);
            if rms > 0.25 {
                on += 1;
            } else if rms < 0.1 {
                off_at.push(k);
            } else {
                assert!(rms < 0.3, "跨边界窗应为中间值, k={k} rms={rms}");
            }
        }
        assert!(on >= 7, "响窗应 ≥7, on={on}");
        assert!(off_at.len() >= 3, "静窗应 ≥3, off={}", off_at.len());
        // 静窗间距 = 1 拍 = 0.5s（0.1s 步进 → 恰 5 步）：门控跟随 beatgrid 节拍
        for w in off_at.windows(2) {
            assert_eq!(w[1] - w[0], 5, "静窗间距应为 1 拍=0.5s, {w:?}");
        }
    }

    #[test]
    fn fx_type_change_writes_defaults_and_stays_clickfree() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        bus.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(1.0); // echo
        bus.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(1.0);
        let _ = run_frames(&mut d, 4800); // 0.1s：淡入完成
        bus.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(4.0); // → reverb
        let _ = run_frames(&mut d, 256); // 触发换型
        // 换型写回 manifest 默认值；enable 保持 1（不被换型重写，P8 修复）
        assert!(
            (bus.control(&hypermixx_core::paths::deck_fx_p(0, 0, 0)).get() - 0.5).abs() < 1e-9,
            "换型应写回默认 roomsize=0.5"
        );
        assert!(
            (bus.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).get() - 1.0).abs() < 1e-9,
            "换型不应改变 enable（用户置 ON 后保持）"
        );
        // 换型后逐采样 Δ 有界（10ms 淡入 + reverb 湿声小幅度）：无 click
        let rec = run_capture(&mut d, 0.5);
        assert!(rec.iter().all(|v| v.is_finite()), "无 NaN");
        let mut max_delta = 0.0f32;
        for i in 2..rec.len() {
            max_delta = max_delta.max((rec[i] - rec[i - 2]).abs());
        }
        assert!(max_delta < 0.1, "换型逐采样 Δ 过大: {max_delta}");
    }

    #[test]
    fn fx_enable_bypass_is_bitwise() {
        // 关闭 enable → 10ms 淡出 → settled 整槽跳过 DSP → 与无 FX 跑逐位一致
        let bus_a = hypermixx_core::ControlBus::default();
        let mut a = deck_with_ring(&bus_a, sine_chunks(64), 0.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(1.0); // echo
        bus_a.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(1.0);
        let _ = run_frames(&mut a, 4800); // 0.1s 回声活跃
        bus_a.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(0.0);
        let _ = run_frames(&mut a, 4800); // 淡出 10ms + settled
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_ring(&bus_b, sine_chunks(64), 0.0);
        let _ = run_frames(&mut b, 9600); // 相同块数推进到同位置
        let rec_b = run_capture(&mut b, 0.5);
        assert_eq!(rec_a, rec_b, "enable=0 settled 后应与无 FX 逐位一致");
    }

    #[test]
    fn fx_gate_bypass_without_grid() {
        // 无 beatgrid：gate 内部逐位直通 → 与无 FX 跑逐位一致
        let bus_a = hypermixx_core::ControlBus::default();
        let mut a = deck_with_ring(&bus_a, sine_chunks(64), 0.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(8.0); // gate
        bus_a.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(1.0);
        let _ = run_frames(&mut a, 4800); // 淡入完成（gate 直通 → 输出恒等干声）
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_ring(&bus_b, sine_chunks(64), 0.0);
        let _ = run_frames(&mut b, 4800);
        let rec_b = run_capture(&mut b, 0.5);
        assert_eq!(rec_a, rec_b, "无网格 gate 应与无 FX 逐位一致");
    }

    #[test]
    fn fx_survives_seek_and_eof() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, mut prod) = deck_with_ring_and_prod(&bus, sine_chunks(64), 0.0);
        bus.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(1.0); // echo
        bus.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(1.0);
        let _ = run_frames(&mut d, 48000); // 播 1 秒（回声稳定）
        let head_before = d.ctl.playhead.get();
        assert!(head_before > 0.9, "seek 前播头 {head_before}");

        // seek：与 seek_during_keylock 同款 reader 响应（新世代 chunk 推送）
        d.seek_seconds(1.0);
        let read_frame = 48000 - d.keylocker.as_ref().unwrap().warm_start_preroll_frames() as u64;
        for k in 0..32 {
            prod.try_push(sine_chunk_at(read_frame + k as u64 * 2048, 2048, d.epoch))
                .ok()
                .expect("ring 容量足够");
        }
        // 测试棚无 reader 线程：手动钉 track_frames，feed 到曲尾才会触发
        // finish() → EOF stall 判停（生产路径由 reader 的 metadata 设置）
        d.track_frames
            .store(read_frame + 32 * 2048, Ordering::Relaxed);
        let mut out = vec![0.0; 256 * 2];
        let mut peak = 0.0f32;
        for _ in 0..(48000 / 256) {
            d.update_params();
            d.process(&mut out, 256);
            for v in out.iter() {
                assert!(v.is_finite(), "seek 后输出出现 NaN/Inf");
                peak = peak.max(v.abs());
            }
        }
        assert!(peak > 0.3, "seek 后回声应继续, peak={peak}");

        // EOF：不再推 chunk → 读空 → finish 冲刷 → 判停。全程无 NaN 不 panic
        for _ in 0..(3.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            assert!(out.iter().all(|v| v.is_finite()), "EOF 过程出现 NaN/Inf");
        }
        assert!(d.ctl.play.get() < 0.5, "读空后应自动停止, play={}", d.ctl.play.get());
        assert!(d.ctl.playhead.get() < 3.0, "播头不得狂奔");
    }

    #[test]
    fn fx_echo_no_pitch_change() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(200), 0.0);
        bus.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(1.0); // echo
        let _ = run_frames(&mut d, 256); // 换型写回默认值
        bus.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(1.0);
        bus.control(&hypermixx_core::paths::deck_fx_p(0, 0, 0)).set(0.2); // time
        bus.control(&hypermixx_core::paths::deck_fx_p(0, 0, 1)).set(0.7); // feedback
        let rec = run_capture(&mut d, 1.0);
        // 稳态段：回声串 = 同频正弦延迟叠加 → 440Hz 不变
        let f = zero_crossing_freq(&rec, (0.6 * 48000.0) as usize, 48000);
        assert!(
            cents_off(f, 440.0).abs() < 10.0,
            "echo 不应改变音高, 实测 {f:.2}Hz"
        );
    }

    // -----------------------------------------------------------------------
    // P8 回归：fx enable 修复 / 8 槽扩展 / beat loop / beatjump
    // -----------------------------------------------------------------------

    #[test]
    fn fx_enable_off_after_type_change_is_passthrough() {
        // P8 修复回归：换型不强制 enable——enable 保持 0 时与无 FX 跑逐位一致
        let bus_a = hypermixx_core::ControlBus::default();
        let mut a = deck_with_ring(&bus_a, sine_chunks(64), 0.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(1.0); // echo
        let _ = run_frames(&mut a, 4800); // 换型 + 0.1s（enable 从未置 1）
        assert!(
            bus_a.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).get() < 0.5,
            "换型不应自动 enable（只在 ON 时打开）"
        );
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_ring(&bus_b, sine_chunks(64), 0.0);
        let _ = run_frames(&mut b, 4800);
        let rec_b = run_capture(&mut b, 0.5);
        assert_eq!(rec_a, rec_b, "enable=0 换型应与无 FX 逐位一致");
    }

    #[test]
    fn fx_slot_7_works() {
        // 8 槽扩展：第 8 槽（index 7）换型 + 开启 + 失真饱和
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        bus.control(&hypermixx_core::paths::deck_fx_drywet(0, 7)).set(1.0);
        bus.control(&hypermixx_core::paths::deck_fx_type(0, 7)).set(5.0);
        bus.control(&hypermixx_core::paths::deck_fx_enable(0, 7)).set(1.0);
        let _ = run_frames(&mut d, 256); // 触发换型（写回默认 drive=12）
        bus.control(&hypermixx_core::paths::deck_fx_p(0, 7, 0)).set(40.0);
        let out = run_capture(&mut d, 1.0);
        let peak = out.iter().fold(0.0f32, |a, v| a.max(v.abs()));
        assert!(peak > 0.9, "第 8 槽失真应推到饱和区, peak={peak}");
    }

    #[test]
    fn loop_activate_snaps_and_writes_buses() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256 * 90); // ≈0.48s（两拍之间）
        let pos = d.ctl.playhead.get();
        d.set_beat_loop(2.0);
        assert!(d.ctl.loop_active.get() > 0.5, "loop 应激活");
        let grid = hypermixx_core::BeatGrid {
            bpm: 120.0,
            offset_secs: 0.0,
        };
        let snap = grid.snap(pos);
        assert!((d.ctl.loop_in.get() - snap).abs() < 1e-9, "loop_in 应量化到拍");
        assert!(
            (d.ctl.loop_out.get() - (snap + 1.0)).abs() < 1e-9,
            "2 拍 @120BPM = 1s"
        );
        // 总线与字段一致（UI 读总线）
        assert!((bus.get(&hypermixx_core::paths::deck_loop_in(0)) - snap).abs() < 1e-9);
        assert!((bus.get(&hypermixx_core::paths::deck_loop_out(0)) - snap - 1.0).abs() < 1e-9);
        assert!(bus.get(&hypermixx_core::paths::deck_loop_active(0)) > 0.5);
    }

    #[test]
    fn loop_noop_without_grid_or_past_end_and_out_clamped() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        d.set_beat_loop(2.0); // 无网格 → no-op
        assert!(d.ctl.loop_active.get() < 0.5, "无网格不应激活");

        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.track_frames.store(48000, Ordering::Relaxed); // 1s 曲
        d.seek_exact(0.95);
        d.set_beat_loop(1.0); // snap(0.95)=1.0 ≥ duration → no-op
        assert!(d.ctl.loop_active.get() < 0.5, "起点越过曲尾不应激活");

        d.seek_exact(0.2);
        d.set_beat_loop(4.0); // snap(0.2)=0.0，4 拍 = 2s → 钳到 1.0s
        assert!(d.ctl.loop_active.get() > 0.5, "钳位后应激活");
        assert!((d.ctl.loop_in.get() - 0.0).abs() < 1e-9);
        assert!((d.ctl.loop_out.get() - 1.0).abs() < 1e-9, "loop_out 钳到曲尾");
    }

    #[test]
    fn loop_back_jumps_to_loop_in() {
        // P10.3 重写：切环走 deck 侧缓冲喂入（kl.set_track_position 重锚，
        // 无 reset、无 epoch 变化、无欠载）。P22-B：激活时已过 loop_in →
        // 部分回填布防，但本 rig 的侧环 prod 无人推数据（回填永不完成）
        // → 停滞兜底：feed 越过 out 一个 chunk 后 loop_wrap 悬挂 → 回退
        // 一次 reset 重捕（epoch+1，测试架补推新世代 chunk，模拟真实
        // reader 的 seek 响应）；之后每圈 FromBuffer 无缝环绕，不再动
        // 引擎/世代。此即"回填迟到/reader 死亡"安全网 = 今日行为。
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, mut prod) = deck_with_ring_and_prod(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256);
        d.set_beat_loop(1.0); // in=0，out=0.5s @120BPM
        assert!(d.ctl.loop_active.get() > 0.5);
        let loop_out = d.ctl.loop_out.get();
        let preroll = d.keylocker.as_ref().unwrap().warm_start_preroll_frames() as u64;
        let epoch_before = d.epoch;
        let mut last_epoch = d.epoch;
        let mut out = vec![0.0; 256 * 2];
        for _ in 0..(4.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            let head = d.ctl.playhead.get();
            assert!(
                head >= -0.2 && head < loop_out + 0.05,
                "播头不得逃出环, head={head}"
            );
            assert!(out.iter().all(|v| v.is_finite()));
            if d.epoch != last_epoch {
                // 首圈兜底 reset 发生（feed 内 epoch++）：补推新世代 chunk
                refill_ring_after_reset(&mut d, &mut prod, preroll);
                last_epoch = d.epoch;
            }
        }
        // 首圈不完整捕获回退一次 reset 后，后续环绕不再动引擎/世代
        assert_eq!(
            d.epoch,
            epoch_before + 1,
            "epoch 只应因首圈兜底 reset 增一次（之后无 reset 环绕）"
        );
        // 兜底 reset 恰在 loop_in=0：preroll 钳 0 → warm_start(0) 无 priming
        // → 该块 256 帧欠载静音（旧 reset 路径同款、P10.3 只付一次）。
        // 之后缓冲环绕不再产生任何欠载。
        assert!(
            d.keylocker.as_ref().unwrap().underrun_frames() <= 256,
            "环绕本身不应欠载（仅兜底 reset 块允许 1 块静音），实际 {}",
            d.keylocker.as_ref().unwrap().underrun_frames()
        );
        assert!(d.ctl.play.get() > 0.5, "循环播放不停止");
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "4s 后应处于缓冲喂入态，实际 {:?}",
            d.loop_feed
        );
    }

    /// P10.3：环绕无 reset（epoch 不变）+ 无欠载 + 播头折返。与
    /// loop_back_jumps_to_loop_in 的区别：激活在 feed 起点之前（pos=0、
    /// 未喂过任何帧），首圈捕获即完整 → 全程无任何 reset。
    #[test]
    fn loop_wrap_no_epoch_no_underrun() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, _prod) = deck_with_ring_and_prod(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        // 不做初始 run_frames：set_beat_loop 在喂入前激活（pos=0 → in=0）
        d.set_beat_loop(1.0); // in=0，out=0.5s @120BPM
        assert!(d.ctl.loop_active.get() > 0.5);
        let loop_out = d.ctl.loop_out.get();
        let epoch_before = d.epoch;
        let mut out = vec![0.0; 256 * 2];
        for _ in 0..(4.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            let head = d.ctl.playhead.get();
            assert!(head >= -0.2 && head < loop_out + 0.05, "播头不得逃出环, head={head}");
            assert!(out.iter().all(|v| v.is_finite()));
        }
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "4s 后应处于缓冲喂入态，实际 {:?}",
            d.loop_feed
        );
        assert!(
            d.epoch == epoch_before,
            "无 reset（首圈完整），epoch 不得变化：{epoch_before} → {}",
            d.epoch
        );
        assert!(
            d.keylocker.as_ref().unwrap().underrun_frames() == 0,
            "环绕不得欠载，实际 {}",
            d.keylocker.as_ref().unwrap().underrun_frames()
        );
    }

    /// P22-A：圈首接缝等功率交叉淡化——锯齿波 fixture（周期 = 环长，
    /// 圈界跳变 0.5）整拍 loop 跑 3 圈，逐采样 |Δ2| < 0.1（无 blend 时
    /// 接缝跳变 0.5 必爆；fx_type_change 同款 max_delta 手法）。blend
    /// 把接缝位移摊到 192 帧（Δ≈0.005），残余瞬态 ≤ bl·斜率（≈0.004）。
    #[test]
    fn loop_wrap_seam_blend_no_click() {
        let bus = hypermixx_core::ControlBus::default();
        let period = 24000usize; // 0.5s @120BPM = loop 长度（in=0, out=0.5s）
        let chunks: Vec<Chunk> = (0..64)
            .map(|k| {
                let start = k * CHUNK_FRAMES;
                let mut data = Vec::with_capacity(CHUNK_FRAMES * 2);
                for f in 0..CHUNK_FRAMES {
                    let phase = ((start + f) % period) as f32 / period as f32;
                    let s = phase * 0.5; // [0, 0.5)：圈界跳变恰 0.5
                    data.push(s);
                    data.push(s);
                }
                Chunk {
                    epoch: 1,
                    start_frame: start as u64,
                    data: data.into_boxed_slice(),
                }
            })
            .collect();
        let (mut d, _prod) = deck_with_ring_and_prod(&bus, chunks, 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.set_beat_loop(1.0); // 激活在喂入前 → 首圈捕获完整，无兜底 reset
        assert!(d.ctl.loop_active.get() > 0.5);
        let rec = run_capture(&mut d, 1.6); // 3.2 圈：覆盖 ≥2 次 wrap 接缝
        assert!(rec.iter().all(|v| v.is_finite()), "无 NaN");
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "应处于缓冲喂入态，实际 {:?}",
            d.loop_feed
        );
        assert!(
            d.keylocker.as_ref().unwrap().underrun_frames() == 0,
            "环绕不得欠载，实际 {}",
            d.keylocker.as_ref().unwrap().underrun_frames()
        );
        let mut max_delta = 0.0f32;
        for i in 2..rec.len() {
            max_delta = max_delta.max((rec[i] - rec[i - 2]).abs());
        }
        assert!(max_delta < 0.1, "圈首接缝逐采样 Δ 过大（blend 未生效）: {max_delta}");
    }

    /// P18：ManualLoop 控件经总线激活（loop_in/loop_out/loop_active 全
    /// 走 bus，零桥改动）→ 引擎边沿检测进入捕获。P22-B 重写：激活时
    /// feed 已过 loop_in（In 按下后播放头越过 in 是常态）→ 部分回填
    /// [li, feed_pos) 布防；侧环数据推入即排空 → 播到量化 Out 圈界无缝
    /// 切 FromBuffer——epoch 不变、underrun 0（首圈兜底 reset 33ms 静音
    /// 不再出现）。
    #[test]
    fn manual_loop_bus_activation_enters_capture() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, _prod, mut side_prod) =
            deck_with_rings_and_prods(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256);
        let epoch_before = d.epoch;
        // P18：全走总线（ManualLoop 的 In/Out/激活按钮即写这三路）
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 0.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 0.5);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        // 先跑一块让激活边沿布防（Backfill 命令发出），再模拟 reader 响应
        d.update_params();
        d.process(&mut vec![0.0; 512], 256);
        let backfill_to = d.feed_pos;
        side_prod
            .try_push(sine_chunk_at(0, CHUNK_FRAMES, d.epoch))
            .ok()
            .expect("侧环容量足够");
        let loop_out = d.ctl.loop_out.get();
        let mut out = vec![0.0; 256 * 2];
        for _ in 0..(4.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            let head = d.ctl.playhead.get();
            assert!(
                head >= -0.2 && head < loop_out + 0.05,
                "播头不得逃出环, head={head}"
            );
            assert!(out.iter().all(|v| v.is_finite()));
        }
        assert_eq!(
            d.epoch, epoch_before,
            "回填完成路径不得 reset（epoch 不变）"
        );
        assert!(
            d.keylocker.as_ref().unwrap().underrun_frames() == 0,
            "部分回填无缝入环不得欠载，实际 {}",
            d.keylocker.as_ref().unwrap().underrun_frames()
        );
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "4s 后应处于缓冲喂入态，实际 {:?}",
            d.loop_feed
        );
        // 播到量化 Out 才回绕：feed 停在 loop_out（非偏移入环不推进）
        assert!(
            backfill_to > 0 && backfill_to < 24000,
            "激活点应在环内（部分回填前置），实际 {backfill_to}"
        );
        assert_eq!(
            d.feed_pos,
            (d.loop_out * d.sr) as u64,
            "常规路径 feed_pos 应停在 loop_out，实际 {}",
            d.feed_pos
        );
    }

    /// P22-B：Out 已过量化点（激活时 feed > out）→ 全圈回填布防 +
    /// 排空即**偏移入环**——不等圈界、不跳回：epoch 不变、underrun 0、
    /// loop_offset == (feed_pos − out) mod len（feed_pos 停激活位 P，
    /// 每圈 +len）、播头映射入环相位（li + offset + folded ∈ [0.1, 0.6)）。
    #[test]
    fn manual_loop_out_past_enters_with_offset_immediately() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, _prod, mut side_prod) =
            deck_with_rings_and_prods(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256);
        // 播到 0.6s（feed 28800 > out 24000）
        let epoch_before = d.epoch;
        for _ in 0..(0.6 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut vec![0.0; 512], 256);
        }
        assert!(d.feed_pos > 24000, "feed 应已过 out，实际 {}", d.feed_pos);
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 0.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 0.5);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        // 先跑一块让激活边沿布防（Backfill 命令发出），再模拟 reader 响应
        d.update_params();
        d.process(&mut vec![0.0; 512], 256);
        // 模拟 reader 对 Backfill[0, 24000) 的响应：12 个整 chunk 覆盖全圈
        for k in 0..12 {
            side_prod
                .try_push(sine_chunk_at(k as u64 * CHUNK_FRAMES as u64, CHUNK_FRAMES, d.epoch))
                .ok()
                .expect("侧环容量足够");
        }
        let mut out = vec![0.0; 256 * 2];
        for _ in 0..(2.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            let head = d.ctl.playhead.get();
            assert!(
                (0.0..0.65).contains(&head),
                "偏移入环播头应映射入环相位 [offset, offset+len)，head={head}"
            );
            assert!(out.iter().all(|v| v.is_finite()));
        }
        assert_eq!(d.epoch, epoch_before, "全圈回填路径不得 reset");
        assert!(
            d.keylocker.as_ref().unwrap().underrun_frames() == 0,
            "偏移入环不得欠载，实际 {}",
            d.keylocker.as_ref().unwrap().underrun_frames()
        );
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "排空后应立即进入缓冲喂入态，实际 {:?}",
            d.loop_feed
        );
        // loop_offset = (feed−out) mod len；feed_pos 停激活位 P（每圈 +len），
        // P mod len == loop_offset 恒成立
        let n = 24000u64;
        assert!(
            d.loop_offset > 0 && d.feed_pos % n == d.loop_offset,
            "loop_offset = (feed−out) mod len（feed={}，offset={}）",
            d.feed_pos,
            d.loop_offset
        );
    }

    /// P22-B：偏移入环跑 ≥3 圈后释放 → 收尾圈退出，播头 = 正在出声的
    /// ring 内容位置（= 引擎标签 + Δ，Δ = feed_pos − feed_pos_at_loop_
    /// start − loop_pushed，常规环 Δ=−W×len、偏移入环 Δ=+d，同式覆盖，
    /// 延迟精确抵消——偏移入环时内容本身在退出处跳至续点 P+k×len，
    /// 播头必须随声音走，不能虚拟续进）；epoch 不变、underrun 不变、
    /// loop_offset 清 0。
    #[test]
    fn loop_offset_entry_exit_resumes_at_advanced_feed_pos() {
        let bus = hypermixx_core::ControlBus::default();
        // 140 chunk ≈ 5.97s：0.6 预播 + 1.5 环 + 2.0 退出线性续喂都在窗内
        let (mut d, _prod, mut side_prod) =
            deck_with_rings_and_prods(&bus, sine_chunks(140), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256);
        for _ in 0..(0.6 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut vec![0.0; 512], 256);
        }
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 0.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 0.5);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        // 先跑一块让激活边沿布防（Backfill 命令发出），再模拟 reader 响应
        d.update_params();
        d.process(&mut vec![0.0; 512], 256);
        // 侧环模拟全圈回填（12 chunk）
        for k in 0..12 {
            side_prod
                .try_push(sine_chunk_at(k as u64 * CHUNK_FRAMES as u64, CHUNK_FRAMES, d.epoch))
                .ok()
                .expect("侧环容量足够");
        }
        // 环内跑 1.5s（≥3 圈，FromBuffer 稳定）
        let mut out = vec![0.0; 256 * 2];
        let mut entry_feed = None;
        let mut entry_offset = 0u64;
        for _ in 0..(1.5 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            if entry_feed.is_none() && d.loop_feed == LoopFeed::FromBuffer {
                entry_feed = Some(d.feed_pos); // 入环位置 P（退出续点基准）
                entry_offset = d.loop_offset; // 退出锚点 Δ = +d（清 0 前捕获）
            }
        }
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "1.5s 后应在环绕态，实际 {:?}（feed={}，pending={}，frames={}）",
            d.loop_feed,
            d.feed_pos,
            d.loop_buf.backfill_pending,
            d.loop_buf.frames
        );
        assert!(d.loop_offset > 0, "应处于偏移入环，offset={}", d.loop_offset);
        let epoch_before = d.epoch;
        let underrun_before = d.keylocker.as_ref().unwrap().underrun_frames();
        let head_at_release = d.ctl.playhead.get();
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 0.0);
        // 退出契约（P22-B）：圈界退出后播头跟引擎标签（source_position =
        // 正在出声的 ring 内容位置 = 退出续点 P + k×len），不再从释放相位
        // 虚拟续进——音频内容本身在退出时跳至续点，播头必须随声音走。
        let mut exit_feed = None;
        let mut head_prev = head_at_release;
        for _ in 0..(2.0 * 48000.0 / 256.0) as usize {
            let was_from_buf = d.loop_feed == LoopFeed::FromBuffer;
            d.update_params();
            d.process(&mut out, 256);
            assert!(out.iter().all(|v| v.is_finite()));
            if was_from_buf && d.loop_feed != LoopFeed::FromBuffer {
                exit_feed = Some(d.feed_pos); // 圈界退出：续点 = P + k×len
                head_prev = d.ctl.playhead.get(); // 跳变块（播头随音频跳）跳过
            } else if d.loop_feed == LoopFeed::Idle && exit_feed.is_some() {
                // 退出后播头线性续进（跟标签；缓冲回收期标签可略快）
                let head_now = d.ctl.playhead.get();
                assert!(
                    head_now >= head_prev - 1.0 && head_now <= head_prev + 256.0 + 1000.0,
                    "退出后播头应线性续进：Δ={}",
                    head_now - head_prev
                );
                head_prev = head_now;
            }
        }
        let head = d.ctl.playhead.get();
        let sp = d.keylocker.as_ref().unwrap().source_position();
        // 播头（秒）应 = 引擎标签 + 冻结锚点（帧）：退出瞬间锚定
        // pos_base = Δ = feed_pos − 基准 − 环喂入（偏移入环 = +d），此后
        // 恒定 → 播头 = 正在出声的内容位置（退出后 feed_pos 继续推进，
        // 不能重算 Δ）。head 单位秒。
        let delta = d.pos_base.expect("退出应锚定 pos_base");
        assert_eq!(delta, entry_offset as f64, "偏移入环退出锚点 Δ = +d");
        assert!(
            (head * 48000.0 - (sp + delta)).abs() < 1.0,
            "播头应跟内容位置（标签+Δ），head={head} sp={sp} Δ={delta}"
        );
        // head 秒 → 帧：lag = feed_pos − 播头 = 引擎缓冲延迟（稳态 <12000）
        let lag = d.feed_pos as f64 - head * 48000.0;
        assert!(
            (0.0..12000.0).contains(&lag),
            "播头应贴近 ring 喂入位置（差=引擎缓冲延迟），feed={} head={head}",
            d.feed_pos
        );
        let ef = exit_feed.expect("退出应在 2s 内发生");
        let en = entry_feed.expect("应已入环");
        assert!(
            ef - en >= 3 * 24000,
            "退出续点应随圈推进（P + k×len），entry_feed={en} exit_feed={ef}"
        );
        assert_eq!(d.epoch, epoch_before, "退出不应触发任何 seek");
        assert_eq!(
            d.keylocker.as_ref().unwrap().underrun_frames(),
            underrun_before,
            "退出续喂不应产生欠载"
        );
        assert_eq!(d.loop_offset, 0, "退出后 loop_offset 应清 0");
    }

    /// P22-B 安全网：长偏移循环（20+ 圈）释放时 feed_pos 已超 ring 已解码
    /// 窗口（522240 = 预填 255 chunk 末端）→ 无缝续喂不可行 → seek_internal
    /// min-preroll 兜底（epoch+1）；测试补推新世代 chunk 后线性续进、
    /// 播头 ≈ 退出位置。
    #[test]
    fn loop_offset_exit_beyond_window_uses_min_preroll() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, mut prod, mut side_prod) =
            deck_with_rings_and_prods(&bus, sine_chunks(255), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256);
        for _ in 0..(0.6 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut vec![0.0; 512], 256);
        }
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 0.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 0.5);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        // 先跑一块让激活边沿布防（Backfill 命令发出），再模拟 reader 响应
        d.update_params();
        d.process(&mut vec![0.0; 512], 256);
        for k in 0..12 {
            side_prod
                .try_push(sine_chunk_at(k as u64 * CHUNK_FRAMES as u64, CHUNK_FRAMES, d.epoch))
                .ok()
                .expect("侧环容量足够");
        }
        // 环内跑直到 feed_pos > 528000（~10.4s，20+ 圈）：入环偏移沿 + 每圈
        // +len 推进 feed_pos，远超 ring 预填窗（255 chunk = 522240）
        let mut out = vec![0.0; 256 * 2];
        let mut blocks = 0;
        while d.feed_pos <= 528000 && blocks < 5000 {
            d.update_params();
            d.process(&mut out, 256);
            blocks += 1;
        }
        assert!(d.loop_feed == LoopFeed::FromBuffer, "长循环应稳定环绕");
        let epoch_before = d.epoch;
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 0.0);
        // 退出在下一圈界：feed = 28672 + 21×24000 + 24000 = 556672 > 522240
        // → 安全网 seek。epoch 变化后补推新世代 chunk。
        let mut blocks = 0;
        let mut last_epoch = d.epoch;
        while d.loop_feed != LoopFeed::Idle && blocks < 200 {
            d.update_params();
            d.process(&mut out, 256);
            blocks += 1;
            if d.epoch != last_epoch {
                let target = d.feed_pos as f64 / 48000.0;
                test_refill_after_seek(&mut d, &mut prod, target);
                last_epoch = d.epoch;
            }
        }
        assert!(d.loop_feed == LoopFeed::Idle, "应已退出循环");
        assert_eq!(d.epoch, epoch_before + 1, "超窗退出应走 min-preroll 兜底 seek");
        assert_eq!(d.loop_offset, 0, "退出后 loop_offset 应清 0");
        // 续播 0.3s：线性续进到 ≈ 退出位置 + 0.3
        let exit_pos = d.feed_pos as f64 / 48000.0;
        for _ in 0..(0.3 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
        }
        let head = d.ctl.playhead.get();
        assert!(
            (head - (exit_pos + 0.3)).abs() < 0.05,
            "兜底 seek 后应从退出位置线性续进（exit={exit_pos:.3}），head={head:.3}"
        );
    }

    /// P22-B 真 reader 路径：真实解码线程 + 部分回填（激活时 out 在前，
    /// feed=0.1s）→ 排空 + 捕获续写 → 播到量化 Out 圈界无缝入环。
    /// epoch 不变（无 33ms reset 兜底）、欠载 ≤ 1 chunk（启动瞬态容差）。
    #[test]
    fn manual_loop_backfill_with_real_reader() {
        let bus = hypermixx_core::ControlBus::default();
        let path = std::env::temp_dir()
            .join(format!("hypermixx_loop_backfill_{}.wav", std::process::id()));
        write_sine_wav(&path, 10.0);
        let mut d = Deck::new(0, 48000, &bus);
        d.ctl.volume.set(1.0);
        d.load(path.clone());
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let mut out = vec![0.0f32; 256 * 2];
        // 播到 0.1s（feed ≈4800 < out 24000）→ 部分回填布防。启动瞬态
        // 下 reader 追平前可能有短暂欠载（累积计数器），激活后才是断言区
        let mut blocks = 0;
        while d.feed_pos < 4800 && blocks < 600 {
            d.update_params();
            d.process(&mut out, 256);
            blocks += 1;
        }
        assert!(d.feed_pos >= 4800, "0.1s 内 reader 应追平，实际 {}", d.feed_pos);
        let epoch_before = d.epoch;
        let underrun_before = d.keylocker.as_ref().unwrap().underrun_frames();
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 0.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 0.5);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        // 等排空 + 捕获续写 + 圈界入环（0.1 激活 + 0.5 播到 out + 解码余量）
        let mut blocks = 0;
        while d.loop_feed != LoopFeed::FromBuffer && blocks < 600 {
            d.update_params();
            d.process(&mut out, 256);
            blocks += 1;
        }
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "真实回填路径应无缝入环，实际 {:?}",
            d.loop_feed
        );
        assert_eq!(d.epoch, epoch_before, "真实回填路径不得 reset");
        assert_eq!(d.feed_pos, 24000, "常规路径 feed_pos 停量化 Out");
        assert!(
            d.keylocker.as_ref().unwrap().underrun_frames() - underrun_before <= 2048,
            "激活后欠载应 ≤ 1 chunk（仅启动瞬态在前），实际 {}",
            d.keylocker.as_ref().unwrap().underrun_frames() - underrun_before
        );
        let _ = std::fs::remove_file(&path);
    }

    /// P22-B 真 reader 路径 + 全圈回填：激活时 feed 已过 out（0.6s）→
    /// Backfill[0, 24000) 真实解码排空 → 偏移入环。epoch 不变、loop_offset
    /// 与 feed_pos 同余、播头映射入环相位。
    #[test]
    fn manual_loop_out_past_with_real_reader() {
        let bus = hypermixx_core::ControlBus::default();
        let path = std::env::temp_dir()
            .join(format!("hypermixx_loop_out_past_{}.wav", std::process::id()));
        write_sine_wav(&path, 10.0);
        let mut d = Deck::new(0, 48000, &bus);
        d.ctl.volume.set(1.0);
        d.load(path.clone());
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let mut out = vec![0.0f32; 256 * 2];
        // 播过 0.5s（feed > out 24000）。启动瞬态下 reader 追平前欠载，
        // feed 推进会慢——按位置等待而非固定块数
        let mut blocks = 0;
        while d.feed_pos <= 24000 && blocks < 600 {
            d.update_params();
            d.process(&mut out, 256);
            blocks += 1;
        }
        assert!(d.feed_pos > 24000, "feed 应已过 out，实际 {}", d.feed_pos);
        let epoch_before = d.epoch;
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 0.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 0.5);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        // 等全圈回填排空 → 偏移入环
        let mut blocks = 0;
        while d.loop_feed != LoopFeed::FromBuffer && blocks < 600 {
            d.update_params();
            d.process(&mut out, 256);
            blocks += 1;
        }
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "全圈回填排空后应偏移入环，实际 {:?}",
            d.loop_feed
        );
        assert_eq!(d.epoch, epoch_before, "真实回填路径不得 reset");
        let n = 24000u64;
        assert_eq!(
            d.feed_pos % n,
            d.loop_offset,
            "loop_offset 与 feed_pos 同余（feed={}，offset={}）",
            d.feed_pos,
            d.loop_offset
        );
        // 入环后播头映射入环相位（li + offset + folded ∈ [0.1, 0.6)）
        for _ in 0..(0.5 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            let head = d.ctl.playhead.get();
            assert!(
                (0.0..0.65).contains(&head),
                "偏移入环播头应映射入环相位，head={head}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    /// P18：激活中改边界（ManualLoop Out/×2/÷2 写 loop_out bus）→
    /// 边沿检测重捕，新环生效（播头不逃出新 out）。
    #[test]
    fn manual_loop_boundary_change_recaptures() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, mut prod) = deck_with_ring_and_prod(&bus, sine_chunks(128), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256);
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 0.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 1.0);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        let preroll = d.keylocker.as_ref().unwrap().warm_start_preroll_frames() as u64;
        let mut last_epoch = d.epoch;
        // 播一段进入 [0,1) 环
        for _ in 0..(0.3 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut vec![0.0; 512], 256);
            if d.epoch != last_epoch {
                refill_ring_after_reset(&mut d, &mut prod, preroll);
                last_epoch = d.epoch;
            }
        }
        // ManualLoop 边界变化：out 1.0 → 0.5（×2/÷2 或 Out 按钮）
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 0.5);
        // 继续播 2s：新 out=0.5 生效，播头不得逃出
        for _ in 0..(2.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut vec![0.0; 512], 256);
            let head = d.ctl.playhead.get();
            assert!(
                (-0.2..0.5 + 0.05).contains(&head),
                "改边界后播头不得逃出新环 out=0.5, head={head}"
            );
            if d.epoch != last_epoch {
                refill_ring_after_reset(&mut d, &mut prod, preroll);
                last_epoch = d.epoch;
            }
        }
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "边界变化后应重新缓冲喂入，实际 {:?}",
            d.loop_feed
        );
    }

    /// P11.1：释放 loop 后收尾圈 + 圈界重锚——播头从释放位置无缝续进
    ///（旧 bug：pos = feed_base + sp 叠加 N×环长，释放瞬间跳变；
    /// "释放即重锚"变体在释放块跳 len−lap_offset，本断言同样失败）。
    /// 无 epoch 变化、无欠载（feed_chunk 游标停在 loop_out 续喂）。
    #[test]
    fn loop_exit_resumes_linear_without_gap() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, _prod) = deck_with_ring_and_prod(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.set_beat_loop(1.0); // in=0，out=0.5s @120BPM
        // 环内跑 2s（≥2 圈，FromBuffer 稳定）
        let mut out = vec![0.0; 256 * 2];
        for _ in 0..(2.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
        }
        assert!(d.loop_feed == LoopFeed::FromBuffer, "2s 后应在环绕态");
        let epoch_before = d.epoch;
        let underrun_before = d.keylocker.as_ref().unwrap().underrun_frames();
        // 关 loop（UI 写总线）→ 收尾圈后线性继续，播头 = 释放位置 + 已播时长
        let head_at_release = d.ctl.playhead.get();
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 0.0);
        for _ in 0..(2.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            assert!(out.iter().all(|v| v.is_finite()));
        }
        let head = d.ctl.playhead.get();
        assert!(
            (head - (head_at_release + 2.0)).abs() < 0.02,
            "释放后播头应从释放位置线性续进（释放={head_at_release}），head={head}"
        );
        assert_eq!(d.epoch, epoch_before, "退出不应触发任何 seek");
        assert_eq!(
            d.keylocker.as_ref().unwrap().underrun_frames(),
            underrun_before,
            "退出续喂不应产生欠载"
        );
    }

    /// P11.1：释放后逐块播头增量钉死"显示与音频同速、无跳变"——
    /// 旧 bug 在释放块跳 N×环长；"冻结 k 圈数转 feed_base"变体在圈界块
    /// 跳 (k+1)×len−occ；两者都会被本断言抓住。同步 leader 相位同单调。
    #[test]
    fn loop_exit_no_per_block_jump() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, _prod) = deck_with_ring_and_prod(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.set_beat_loop(1.0); // in=0，out=0.5s @120BPM
        let mut out = vec![0.0; 256 * 2];
        for _ in 0..(2.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
        }
        assert!(d.loop_feed == LoopFeed::FromBuffer, "2s 后应在环绕态");
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 0.0);
        // 每块增量 = rate×256/sr（rate=1 恒 256/48000；允许欠载冻结 Δ=0，
        // 不允许跳变）
        let block_delta = 256.0 / 48000.0;
        let mut prev_head = d.ctl.playhead.get();
        let mut prev_leader = d.sync_leader_snapshot().position_secs;
        for _ in 0..(2.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            assert!(out.iter().all(|v| v.is_finite()));
            let head = d.ctl.playhead.get();
            let delta = head - prev_head;
            assert!(
                delta >= 0.0 && delta <= block_delta + 0.01,
                "释放后播头增量应 ≈ 一块（{block_delta:.5}），Δ={delta}，head={head}"
            );
            let leader = d.sync_leader_snapshot().position_secs;
            assert!(
                leader >= prev_leader && leader - prev_leader <= block_delta + 0.01,
                "sync leader 位置应同单调，Δ={}",
                leader - prev_leader
            );
            prev_head = head;
            prev_leader = leader;
        }
    }

    /// P11.1：释放收尾圈后播头正确 → 再次 set_beat_loop 量化落点正确
    ///（旧 bug：pos ≈ loop_out + N×环长，落点飞到后面）。释放时机选在
    /// 环内已知相位，保证二次激活时 feed 尚未越过落点 → 捕获完整、
    /// 无 reset 兜底（确定性）。
    #[test]
    fn loop_reactivate_after_release_lands_on_quantized_pos() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, mut prod) = deck_with_ring_and_prod(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.set_beat_loop(1.0); // in=0，out=0.5s @120BPM
        let mut out = vec![0.0; 256 * 2];
        // 环内跑 2s（≥2 圈，FromBuffer 稳定）
        for _ in 0..(2.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
        }
        assert!(d.loop_feed == LoopFeed::FromBuffer, "2s 后应在环绕态");
        // 等到播头进入 [0.25, 0.3) 再释放（每圈 0.5s，窗口 50ms ≈ 10 块）
        let mut guard = 0;
        while !(0.25..0.3).contains(&d.ctl.playhead.get()) {
            assert!(guard < 200, "0.5s 环内 1s 内必进窗口");
            guard += 1;
            d.update_params();
            d.process(&mut out, 256);
        }
        let head_at_release = d.ctl.playhead.get();
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 0.0);
        // 线性跑 1.0s：播头 ∈ [1.25, 1.3)
        for _ in 0..(1.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
        }
        let head = d.ctl.playhead.get();
        assert!(
            (1.25..1.3).contains(&head),
            "释放后 1s 播头应 ≈ 释放位置+1.0 ∈ [1.25, 1.3)，head={head}（释放={head_at_release}）"
        );
        // 再次激活 1 拍环：量化落点 = snap(head) = 1.5（120BPM 网格）
        //（bug 实现 head≈2.5+ → 落点 2.5，本断言失败）
        let epoch_before = d.epoch;
        d.set_beat_loop(1.0);
        let loop_in = d.ctl.loop_in.get();
        let loop_out = d.ctl.loop_out.get();
        assert!(
            loop_in == 1.5 && loop_out == 2.0,
            "二次激活落点应量化到 1.5s 网格拍，loop_in={loop_in}（head={head}）"
        );
        // feed 尚未越过 1.5 → 捕获完整、切环无 reset；ring 内容 2.73s 足够
        // 捕获到 2.0。捕获期（feed 1.279→2.0，约 135 块）播头沿线性
        // 继续 [1.257, 2.0)，切环后折返进 [1.5, 2.0)。再跑 1s 分两段断言：
        let total_blocks = (1.0 * 48000.0 / 256.0) as usize; // 188
        for i in 0..total_blocks {
            d.update_params();
            d.process(&mut out, 256);
            let h = d.ctl.playhead.get();
            if i < total_blocks - 40 {
                // 捕获线性段 + 切环瞬间：允许宽区间（不得后退/逃出环区）
                assert!(
                    (1.2..2.05).contains(&h),
                    "捕获期播头应在线性 [1.2, 2.05) 内，h={h}"
                );
            } else {
                // 切环已完成（≤150 块内）：环绕 [loop_in, loop_out)
                assert!(
                    h >= loop_in - 0.05 && h < loop_out + 0.05,
                    "二次环绕播头应在环内 [{}，{}), h={h}",
                    loop_in,
                    loop_out
                );
            }
            assert!(out.iter().all(|v| v.is_finite()));
        }
        assert_eq!(d.epoch, epoch_before, "捕获完整 → 二次环绕不应 seek");
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "二次环绕应回到缓冲喂入态，实际 {:?}",
            d.loop_feed
        );
        let _ = &mut prod; // 无 reset 路径不需要补 ring（保留引用防误用）
    }

    /// P10.3 回归：FromBuffer 环绕中 beatjump 出环 → 缓冲态必须退出
    ///（seek_internal 置 Idle），喂入回到 ring 的新位置而非继续喂旧环。
    #[test]
    fn loop_beatjump_out_while_from_buffer_resumes_ring() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, mut prod) = deck_with_ring_and_prod(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.set_beat_loop(1.0); // in=0，out=0.5s
        let mut out = vec![0.0; 256 * 2];
        for _ in 0..(0.6 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
        }
        assert!(d.loop_feed == LoopFeed::FromBuffer, "0.6s 后应在环绕态");
        // beatjump 4 拍（2s @120BPM）→ 落点 2.0s，出环 → 取消 loop
        d.beatjump(4.0);
        assert!(d.ctl.loop_active.get() < 0.5, "出环 beatjump 应取消 loop");
        assert!(
            d.loop_feed == LoopFeed::Idle,
            "出环 seek 后应退出缓冲喂入，实际 {:?}",
            d.loop_feed
        );
        // 模拟 reader 对 Seek 的响应（测试 deck 无 reader 线程）。
        // P14 beatjump 最小预卷：read_frame = 落点 − 1；喂入起点只需
        // 覆盖 feed_pos（提前 1 帧），起推点从落点即可。
        let read_frame = (2.0 * 48000.0) as u64 - 1;
        for k in 0..64 {
            prod.try_push(sine_chunk_at(
                read_frame + k as u64 * CHUNK_FRAMES as u64,
                CHUNK_FRAMES,
                d.epoch,
            ))
            .ok()
            .expect("ring 容量足够");
        }
        let epoch_after_jump = d.epoch;
        for _ in 0..(1.5 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            assert!(out.iter().all(|v| v.is_finite()));
        }
        let head = d.ctl.playhead.get();
        assert!(
            head > 2.5,
            "跳转后应线性播放（2.0s 落点 + 1.5s ≈ 3.5s），head={head}"
        );
        assert_eq!(d.epoch, epoch_after_jump, "跳转后线性播放不再 seek");
        assert!(
            d.loop_feed == LoopFeed::Idle,
            "跳转后保持 Idle，实际 {:?}",
            d.loop_feed
        );
    }

    /// P10.3：环长超上限（min(64 拍, 30s)）→ 不缓冲，切环回退
    /// reset 路径（旧语义：每圈 seek_internal）。2400BPM 网格下 65 拍
    /// = 1.625s > 64 拍上限、< 30s——环长刻意跨过拍数上限。
    #[test]
    fn loop_longer_than_buffer_cap_falls_back_to_reset() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, mut prod) = deck_with_ring_and_prod(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(2400.0);
        d.ctl.grid_offset.set(0.0);
        d.set_beat_loop(65.0); // 65 拍 @2400BPM = 1.625s > 64 拍上限
        assert!(d.ctl.loop_active.get() > 0.5);
        assert!(
            d.loop_feed == LoopFeed::Idle,
            "超限环不应缓冲，实际 {:?}",
            d.loop_feed
        );
        let loop_out = d.ctl.loop_out.get();
        assert!(loop_out < 30.0, "测试环长应在 30s 秒数上限内（拍数超限）");
        let preroll = d.keylocker.as_ref().unwrap().warm_start_preroll_frames() as u64;
        let epoch_start = d.epoch;
        let mut last_epoch = d.epoch;
        let mut out = vec![0.0; 256 * 2];
        for _ in 0..(3.3 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            let head = d.ctl.playhead.get();
            assert!(
                head >= -0.2 && head < loop_out + 0.05,
                "播头不得逃出环, head={head}"
            );
            assert!(out.iter().all(|v| v.is_finite()));
            if d.epoch != last_epoch {
                // 每圈 reset 回跳：补推新世代 chunk（模拟 reader seek 响应）
                refill_ring_after_reset(&mut d, &mut prod, preroll);
                last_epoch = d.epoch;
            }
        }
        assert!(
            d.epoch > epoch_start + 1,
            "超限环每圈走 reset 回跳（epoch 多次递增），before={epoch_start} after={}",
            d.epoch
        );
        assert!(d.ctl.play.get() > 0.5, "循环播放不停止");
    }

    /// P10.3：同尺寸二次激活复用缓冲（跳过捕获，切环直接 FromBuffer
    /// 无 reset）。
    #[test]
    fn loop_redeactivate_reuse_buffer() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, _prod) = deck_with_ring_and_prod(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.set_beat_loop(1.0); // in=0，out=0.5s
        let mut out = vec![0.0; 256 * 2];
        // 0.6s：首切环在 ~0.47s 发生，去激活时播头 ≈ 0.1s < 0.25 → 二次
        // 激活 snap 回 0.0（同 [0, 0.5) 环 → 复用分支，确定性）
        for _ in 0..(0.6 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
        }
        assert!(d.loop_feed == LoopFeed::FromBuffer, "0.6s 后应在环绕态");
        // 关 → 开（同尺寸）：P11.1 释放后收尾圈（FromBuffer 保持到圈界），
        // 收尾圈内重激活同尺寸环 → 复用缓冲直接继续环绕
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 0.0);
        d.update_params();
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "释放后收尾圈应保持 FromBuffer，实际 {:?}",
            d.loop_feed
        );
        d.set_beat_loop(1.0); // 同尺寸重激活（pos ≈ 0.1s → snap 回 0）
        let epoch_before = d.epoch;
        assert!(d.loop_buf.frames > 0, "缓冲应保留复用");
        for _ in 0..(1.5 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            assert!(out.iter().all(|v| v.is_finite()));
        }
        assert!(
            d.loop_feed == LoopFeed::FromBuffer,
            "复用缓冲后应回到环绕态"
        );
        assert_eq!(
            d.epoch, epoch_before,
            "缓冲复用：二次激活无需 reset 兜底（epoch 不变）"
        );
        assert_eq!(
            d.keylocker.as_ref().unwrap().underrun_frames(),
            0,
            "复用环绕不应欠载"
        );
    }

    #[test]
    fn loop_external_seek_outside_deactivates() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.set_beat_loop(2.0); // in=0，out=1.0
        assert!(d.ctl.loop_active.get() > 0.5);
        d.seek_seconds(0.4); // 环内 → 保持
        assert!(d.ctl.loop_active.get() > 0.5, "环内 seek 不取消");
        d.seek_seconds(1.5); // 环外 → 取消
        assert!(d.ctl.loop_active.get() < 0.5, "环外 seek 应取消 loop");
        assert!(
            bus.get(&hypermixx_core::paths::deck_loop_active(0)) < 0.5,
            "总线应同步清零"
        );
        // seek_exact 走同样的取消逻辑
        d.set_beat_loop(2.0); // pos=1.5 → snap=1.5，out=2.5
        assert!(d.ctl.loop_active.get() > 0.5);
        d.seek_exact(0.6); // [1.5, 2.5) 之外 → 取消
        assert!(d.ctl.loop_active.get() < 0.5, "seek_exact 环外同样取消");
    }

    #[test]
    fn loop_deactivate_via_bus_lets_playhead_pass() {
        // 总线关 loop_active 后播头越过 loop_out 不回跳
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.set_beat_loop(1.0); // in=0，out=0.5
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 0.0);
        let _ = run_frames(&mut d, 256 * 110); // ≈0.59s > 0.5s
        assert!(
            d.ctl.playhead.get() > 0.5,
            "关闭后播头应越过 loop_out, head={}",
            d.ctl.playhead.get()
        );
        assert!(d.ctl.loop_active.get() < 0.5);
    }

    /// P10.1 源拍域：跳 N 拍 = N×60/grid_bpm（与播放速率无关），
    /// 落点 snap 到网格拍。rate=1 与 rate=2 跳距相同（旧实现 rate=2
    /// 只跳 1s——两轨同速时相对相位会散）。
    #[test]
    fn beatjump_source_beat_domain_exact_distance() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(160), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256 * 10);
        let p0 = d.ctl.playhead.get();
        // P16（P17 回滚）：跳距精确 N 拍（4 拍 @120 = 2s 源拍域），
        // 不吸附网格（落点 snap 使离拍起跳跳距缩短——用户否决）
        d.beatjump(4.0);
        assert!(
            (d.ctl.playhead.get() - p0 - 2.0).abs() < 1e-6,
            "跳距应精确 4 拍 = 2s 源拍, head={}, p0={p0}",
            d.ctl.playhead.get()
        );
        d.ctl.rate.set(100.0); // +100%：拍长不变（源拍域）
        d.update_params();
        let p1 = d.ctl.playhead.get();
        d.beatjump(4.0);
        let head2 = d.ctl.playhead.get();
        assert!(
            (head2 - p1 - 2.0).abs() < 1e-6,
            "rate=2 跳距仍 2s 源拍（旧实现会跳 1s）, head={head2}, p1={p1}"
        );
        assert!(
            (head2 - p0 - 4.0).abs() < 1e-9,
            "两次跳距均 = 2s 源拍（与速率无关），差 {}",
            head2 - p0
        );
    }

    /// P16（P17 回滚）：跳距精确 N 拍（离拍位置也整拍——落点 snap 会
    /// 缩短跳距）；无网格 no-op。
    #[test]
    fn beatjump_exact_distance_and_noop_without_grid() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        let p0 = d.ctl.playhead.get();
        d.beatjump(4.0); // 无网格 → no-op
        assert!(
            (d.ctl.playhead.get() - p0).abs() < 1e-12,
            "无网格 beatjump 不动"
        );
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.seek_exact(1.3); // 不在拍点上（相位 0.6）
        d.beatjump(1.0);
        assert!(
            (d.ctl.playhead.get() - 1.8).abs() < 1e-6,
            "1.3 + 1 拍 = 1.8（跳距精确 1 拍，不吸附到 2.0）, head={}",
            d.ctl.playhead.get()
        );
    }

    /// P16（P17 回滚）：sync 关、任意速率下跳距恒 = N 拍整（速率只
    /// 影响墙钟时长，源拍域跳距不变）。
    #[test]
    fn beatjump_exact_distance_at_any_rate_sync_off() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(160), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        for rate_pct in [0.0, 30.0, -8.0] {
            d.ctl.rate.set(rate_pct);
            d.update_params();
            d.seek_exact(1.3); // 离拍
            let p = d.ctl.playhead.get();
            d.beatjump(3.0);
            let head = d.ctl.playhead.get();
            assert!(
                (head - p - 3.0 * 0.5).abs() < 1e-9,
                "rate={rate_pct}% 时跳距应精确 3 拍 = 1.5s：{p} → {head}"
            );
        }
    }

    /// 目标 ② 的引擎侧保证：从拍点出发跳 N 拍距离恒为 N×60/grid_bpm
    /// 且落点仍在拍点上——两轨同速时相对相位不变（不开 sync 也不
    /// 失去同步），与速率无关。
    #[test]
    fn beatjump_preserves_beat_relationship_from_grid_point() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(160), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.ctl.rate.set(30.0); // 1.3×：任意非 1 速率
        d.update_params();
        d.seek_exact(1.5); // 拍点（第 3 拍）
        d.beatjump(4.0);
        let head = d.ctl.playhead.get();
        assert!(
            (head - 3.5).abs() < 1e-6,
            "从拍点跳 4 拍 = 精确 2s 源拍（相对相位不变）, head={head}"
        );
    }

    #[test]
    fn beatjump_outside_loop_deactivates() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.set_beat_loop(1.0); // in=0，out=0.5
        d.beatjump(4.0); // 2s → 出环
        assert!(d.ctl.loop_active.get() < 0.5, "beatjump 出环应取消 loop");
    }

    /// P22-C：落点命中 ring 已解码窗口 → seek 保留窗口（零拷贝零分配），
    /// 不补喂也零欠载——跳后直接从窗口续喂，无 refill 停滞。
    #[test]
    fn beatjump_lands_in_preserved_window_no_underrun() {
        let bus = hypermixx_core::ControlBus::default();
        // 128 chunk = 5.46s 预解码窗口
        let mut d = deck_with_ring(&bus, sine_chunks(128), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256 * 100); // 播 ~0.53s，窗口余 [0.53, 5.46]
        let p0 = d.ctl.playhead.get();
        let underrun_before = d.keylocker.as_ref().unwrap().underrun_frames();
        d.beatjump(4.0); // +2s → ~2.5s，落在余窗内
        let target = p0 + 2.0;
        assert!(
            d.preserve.is_some(),
            "落点在窗内应保留窗口（preserve={:?}）",
            d.preserve
        );
        let (_, q0) = run_frames(&mut d, 256 * 30);
        assert_eq!(
            d.keylocker.as_ref().unwrap().underrun_frames(),
            underrun_before,
            "保留窗口跳后应零欠载（无 refill）"
        );
        // 连续喂入：播头 = 落点 + 推进 − 引擎延迟（延迟补偿动态契约）
        let lag = (target + 30.0 * 256.0 / 48000.0) - q0;
        assert!(
            lag > 0.0 && lag <= 0.03,
            "播头应跟内容位置：q0={q0} target={target} lag={lag}"
        );
    }

    /// P22-C 负控：落点超窗（64 chunk = 2.73s，跳 5 拍 → ~3.0s 出窗）→
    /// 全排 refill 旧行为——无保留、无补喂时 feed 冻结（引擎停在预卷
    /// 静音态，欠载计数器不涨）、补喂后从落点续进。
    #[test]
    fn beatjump_beyond_window_still_requires_refill() {
        let bus = hypermixx_core::ControlBus::default();
        let (mut d, mut prod) = deck_with_ring_and_prod(&bus, sine_chunks(64), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256 * 100); // 播 ~0.53s，余窗 [0.53, 2.73]
        let p0 = d.ctl.playhead.get();
        d.beatjump(5.0); // +2.5s → ~3.0s 超窗
        assert!(d.preserve.is_none(), "超窗不得保留窗口");
        let feed_frozen = d.feed_pos;
        let _ = run_frames(&mut d, 256 * 8); // 不补喂：无数据可喂
        assert_eq!(
            d.feed_pos, feed_frozen,
            "超窗无 refill feed 不得推进（旧行为依赖 refill）"
        );
        // 补喂后恢复：播头从落点续进（旧行为完整路径）
        test_refill_after_seek(&mut d, &mut prod, p0 + 2.5);
        let (_, q) = run_frames(&mut d, 256 * 8);
        let lag = (p0 + 2.5 + 8.0 * 256.0 / 48000.0) - q;
        assert!(
            lag > 0.0 && lag <= 0.03,
            "refill 后播头应续进：q={q} p0={p0} lag={lag}"
        );
    }

    /// P22-C 真 reader：落点命中预解码窗口 → Seek{resume} 送达活 reader，
    /// 保留窗口喂完后无缝续推（窗口尾 → reader 续推），全程零欠载。
    #[test]
    fn beatjump_preserve_with_real_reader() {
        let bus = hypermixx_core::ControlBus::default();
        let path = std::env::temp_dir()
            .join(format!("hypermixx_beatjump_preserve_{}.wav", std::process::id()));
        write_sine_wav(&path, 30.0);
        let mut d = Deck::new(0, 48000, &bus);
        d.ctl.volume.set(1.0);
        d.load(path.clone());
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let mut out = vec![0.0f32; 256 * 2];
        // 播到 ~1s（reader 预解码窗口建立：ring 256 chunk ≈ 10.9s）
        let mut blocks = 0;
        while d.ctl.playhead.get() < 1.0 && blocks < 600 {
            d.update_params();
            d.process(&mut out, 256);
            blocks += 1;
        }
        let p0 = d.ctl.playhead.get();
        let underrun_before = d.keylocker.as_ref().unwrap().underrun_frames();
        d.beatjump(4.0); // +2s → ~3s，落在预解码窗口内
        assert!(d.preserve.is_some(), "真 reader 落点窗内应保留窗口");
        // 推进越过窗口尾（~11.9s）→ reader 续推接管
        let past = ((p0 + 11.0) * 48000.0) as u64;
        let mut blocks = 0;
        while d.feed_pos < past && blocks < 3000 {
            d.update_params();
            d.process(&mut out, 256);
            blocks += 1;
        }
        assert!(
            d.feed_pos >= past,
            "feed 应推进过保留窗口并续推，实际 {}",
            d.feed_pos
        );
        assert_eq!(
            d.keylocker.as_ref().unwrap().underrun_frames(),
            underrun_before,
            "跳后全程零欠载（保留窗口 + reader 续推）"
        );
        let _ = std::fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // P9：通道增益（dB）/ deck 滤波 / 交叉推子
    // -----------------------------------------------------------------------

    /// 左声道 RMS（[start_frame, end_frame)）。
    fn rms_left(rec: &[f32], start_frame: usize, end_frame: usize) -> f32 {
        let n = end_frame - start_frame;
        let mut sum = 0.0f32;
        for i in start_frame..end_frame {
            sum += rec[i * 2] * rec[i * 2];
        }
        (sum / n as f32).sqrt()
    }

    #[test]
    fn gain_default_is_unity() {
        // Deck1.gain 显式写 0.0（= 0dB）→ 与未触碰的 deck 逐位一致
        let bus_a = hypermixx_core::ControlBus::default();
        let mut a = deck_with_ring(&bus_a, sine_chunks(64), 0.0);
        bus_a.control(&hypermixx_core::paths::deck_gain(0)).set(0.0);
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_ring(&bus_b, sine_chunks(64), 0.0);
        let rec_b = run_capture(&mut b, 0.5);
        assert_eq!(rec_a, rec_b, "gain=0dB 应与默认逐位一致");
    }

    #[test]
    fn gain_trim_boosts_and_cuts() {
        // +12dB ≈ ×3.98、-12dB ≈ ×0.25（0.2s 平滑稳定后测稳态 RMS）
        let bus_a = hypermixx_core::ControlBus::default();
        let mut a = deck_with_ring(&bus_a, sine_chunks(64), 0.0);
        bus_a.control(&hypermixx_core::paths::deck_gain(0)).set(12.0);
        let _ = run_frames(&mut a, 9600); // 0.2s 稳定
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_ring(&bus_b, sine_chunks(64), 0.0);
        bus_b.control(&hypermixx_core::paths::deck_gain(0)).set(-12.0);
        let _ = run_frames(&mut b, 9600);
        let rec_b = run_capture(&mut b, 0.5);

        let bus_c = hypermixx_core::ControlBus::default();
        let mut c = deck_with_ring(&bus_c, sine_chunks(64), 0.0);
        let _ = run_frames(&mut c, 9600);
        let rec_c = run_capture(&mut c, 0.5);

        let (ra, rb, rc) = (
            rms_left(&rec_a, 0, 24000) as f64,
            rms_left(&rec_b, 0, 24000) as f64,
            rms_left(&rec_c, 0, 24000) as f64,
        );
        assert!(
            (ra / rc - 3.981).abs() < 0.2,
            "+12dB 应 ≈×3.98: ra={ra} rc={rc} 比值={}",
            ra / rc
        );
        assert!(
            (rb / rc - 0.2512).abs() < 0.02,
            "-12dB 应 ≈×0.25: rb={rb} rc={rc} 比值={}",
            rb / rc
        );
    }

    #[test]
    fn deck_filter_passthrough_at_zero() {
        // 滤波旋钮 0（曾开过）→ settled 整体旁路 → 与未触碰 deck 逐位一致。
        // 注意：
        // - cutoff log2 平滑器从满扫回极端需 ~140ms 才精确 settled（1e-5 阈值），
        //   回落期要跑够 0.3s；
        // - 两 deck 的 run_frames 分块必须镜像（尾块丢弃量一致，否则
        //   pitch shifter 相位错位 → 捕获相位不同，非滤波差异）。
        let bus_a = hypermixx_core::ControlBus::default();
        let mut a = deck_with_ring(&bus_a, sine_chunks(64), 0.0);
        bus_a.control(&hypermixx_core::paths::deck_filter(0)).set(0.8);
        let _ = run_frames(&mut a, 4800); // 0.1s LP 活跃
        bus_a.control(&hypermixx_core::paths::deck_filter(0)).set(0.0);
        let _ = run_frames(&mut a, 14400); // 0.3s 回落 + settled
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_ring(&bus_b, sine_chunks(64), 0.0);
        let _ = run_frames(&mut b, 4800);
        let _ = run_frames(&mut b, 14400);
        let rec_b = run_capture(&mut b, 0.5);
        assert_eq!(rec_a, rec_b, "滤波旋钮 0 且稳定应与默认逐位一致");
    }

    #[test]
    fn deck_filter_kills_highs_at_lp() {
        // 旋钮 +1 → LP@20Hz：440Hz 正弦 RMS 崩塌（对比旁路参照 deck）
        let bus_a = hypermixx_core::ControlBus::default();
        let mut a = deck_with_ring(&bus_a, sine_chunks(64), 0.0);
        bus_a.control(&hypermixx_core::paths::deck_filter(0)).set(1.0);
        let _ = run_frames(&mut a, 9600); // 0.2s 稳定
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_ring(&bus_b, sine_chunks(64), 0.0);
        let _ = run_frames(&mut b, 9600);
        let rec_b = run_capture(&mut b, 0.5);

        let (ra, rb) = (rms_left(&rec_a, 0, 24000), rms_left(&rec_b, 0, 24000));
        assert!(rb > 0.3, "参照 deck 应输出正弦: rb={rb}");
        assert!(ra < rb * 0.05, "LP 全开应杀 440Hz: ra={ra} rb={rb}");
    }

    /// P11.4 回归防护（bug："滚动波形到结束时仍右移"的引擎侧）：
    /// 播到 EOF 自动停止，播头冻结在曲尾（不越过、不继续推进）。
    #[test]
    fn eof_playhead_freezes_at_track_end() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_ring(&bus, sine_chunks(64), 0.0); // 131072 帧 ≈ 2.73s
        let n = 64 * CHUNK_FRAMES as u64;
        d.track_frames.store(n, Ordering::Relaxed);
        let mut out = vec![0.0f32; 256 * 2];
        let mut max_pos = 0.0f64;
        for _ in 0..4000 {
            d.update_params();
            d.process(&mut out, 256);
            max_pos = max_pos.max(d.pos);
        }
        assert!(!d.playing, "EOF 后应自动停止");
        assert!(
            max_pos <= n as f64 + 200.0,
            "pos 不应明显越过曲尾: max_pos={max_pos} n={n}"
        );
        // 停止后继续跑若干块：播头必须冻结（bug 现象 = 仍右移）
        let frozen = d.pos;
        for _ in 0..64 {
            d.update_params();
            d.process(&mut out, 256);
            assert_eq!(d.pos, frozen, "EOF 后播头应冻结: {} vs {frozen}", d.pos);
        }
    }
}
