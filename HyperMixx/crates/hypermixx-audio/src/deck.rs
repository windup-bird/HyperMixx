//! Deck：单个播放通道的实时状态（只被音频线程 + 引擎操作段触碰）。
//!
//! 播放链（keylock 引擎可用时）：
//! 缓存读取器 → 喂入(keylocker) → 引擎整块渲染（变速不变调）→ pitch（key shift）
//! → EQ → deck 滤波（旋钮 LP/HP）→ FX rack（8 槽）→ gain（音量 × 通道增益 dB）。
//! 引擎构建失败时回退线性插值路径（read_stereo + pos += rate）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::dsp::deck_filter::DeckFilter;
use crate::dsp::eq::ThreeBandEq;
use crate::dsp::pitch::PitchShifter;
use crate::dsp::smoother::Smoother;
use crate::fx::{EffectId, FxContext, FxRack, manifest};
use crate::keylocker::{Keylocker, TimestretchLocker};
use crate::track_cache::{CHUNK_FRAMES, TrackCache};
use hypermixx_core::{BeatClock, BeatGrid, ControlHandle};

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

// ---- P23 Phase B loop 缓存窗口循环喂入 ----
/// 接缝交叉淡化长度（帧）：4ms@48k。圈首 blend = 尾(lo−bl..lo) 淡出 ×
/// 头(li..li+bl) 淡入（等功率）；偏移入环 entry blend = 刚喂内容淡出 ×
/// 入环位置淡入。固定数组预计算，音频线程零分配。
const LOOP_BLEND_FRAMES: usize = 192;
/// sync leader 跳变判定（拍）：连续两块 leader 快照位置差超此值 = 跳转
///（beatjump/seek/loop 回绕——正常推进每块 <0.02 拍 @200bpm 极限），
/// follower 重新对齐（P14「操作后不再自动对拍」仅限 follower 自身操作）。
const SYNC_LEADER_JUMP_BEATS: f64 = 0.5;


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

    // 数据源（P23：全曲预解码缓存直读，无 ring/读取线程）
    loaded: bool,
    epoch: u32,
    cache: Option<Arc<TrackCache>>,
    /// 缓存直读暂存（copy_ready 目标，load 时预分配，音频线程零分配）。
    feed_scratch: Vec<f32>,

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
    /// P11.1 收尾圈完成后的线性显示锚点：pos = pos_base + source_position()。
    /// seek/load/引擎重建清空（坐标系重建）。
    pos_base: Option<f64>,

    // ---- P23 Phase B loop 环状态（缓存窗口循环喂入，无捕获无 reset）----
    /// 环喂入已初始化（feed 已到 li 且激活沿已处理；此后 feed 走环分支）。
    loop_ring: bool,
    /// 关环沿（loop_active true→false）：等当前圈喂完（cursor 回绕）退出。
    loop_exiting: bool,
    /// 环长（帧）：(loop_out − loop_in) × sr，激活时冻结（激活中改参数 =
    /// 退出重建，loop_len 随即更新）。
    loop_len: u64,
    /// 入环偏移（帧）：d = (feed_pos − li) mod len——入环瞬间的环内相位。
    loop_offset: u64,
    /// 入环判别：true = 偏移入环（feed ≥ lo：内容跳变 + reset 重锚），
    /// false = 连续入环（feed ∈ [li, lo)：不 reset，折返从入环相位起）。
    /// finish 退出公式与 wrap 记账在 true 上分派——不能拿 loop_offset > 0
    /// 判别（连续入环也有 d = 入环相位 > 0，会误入偏移公式）。
    loop_offset_engage: bool,
    /// 环内喂入游标（0..len，圈界回绕）。
    loop_cursor: u64,
    /// 退出锚点基准：feed_pos 停驻值（常规 = loop_out；偏移 = 入环位置 P）。
    loop_feed_base: u64,
    /// 环期间累计喂入帧（偏移路径退出锚点 Δ = feed_pos − base − pushed）。
    loop_pushed: u64,
    /// 折返/退出锚点基准：入环瞬间（或 reset 后）source_position。
    loop_sp_anchor: f64,
    /// 圈首 blend 段起点（0 = 标准圈首；entry blend 用入环偏移 d）。
    /// u64::MAX = 当前无 entry 段（wrap blend 每圈从 0 起，无需标记）。
    loop_entry_at: u64,
    /// entry blend 长度（帧；0 = 无 entry blend——连续入环不需要）。
    loop_entry_len: usize,
    /// 圈首 wrap blend（尾×头等功率，192 帧，入环时预计算全程复用）。
    loop_wrap_blend: [f32; LOOP_BLEND_FRAMES * 2],
    loop_wrap_blend_len: usize,
    /// 环尚未完成首次回绕（入环相位 d < blend 长度时，首圈 cursor<bl
    /// 区必须喂原始内容——wrap blend 起点接续的是 lo−1 的已听内容，
    /// 中途入环时接不上，喂 blend 会 click）。
    loop_first_circle: bool,
    /// 偏移入环 entry blend（刚喂内容淡出 × 入环位置淡入，一次性）。
    loop_entry_blend: [f32; LOOP_BLEND_FRAMES * 2],
    /// P23-B 量化边沿：上次处理的 loop_in/loop_out 总线值（None = 未初始）
    /// ——写入时 snap 到 beatgrid 拍线并写回总线（起点终点全部对齐）。
    loop_in_sent: Option<f64>,
    loop_out_sent: Option<f64>,
    /// P23-B sync：上一块 leader 快照位置（秒）——跳变检测重新对齐。
    last_leader_pos: Option<f64>,

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
            cache: None,
            feed_scratch: vec![0.0; CHUNK_FRAMES * 2],
            keylocker: None,
            pitch_shifter: PitchShifter::new(),
            engine_scratch: vec![0.0; (ENGINE_BLOCK * 2 + 2) * 2],
            shifter_frac: 0.0,
            feed_pos: 0,
            feed_base: 0,
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
            pos_base: None,
            loop_ring: false,
            loop_exiting: false,
            loop_first_circle: false,
            loop_len: 0,
            loop_offset: 0,
            loop_offset_engage: false,
            loop_cursor: 0,
            loop_feed_base: 0,
            loop_pushed: 0,
            loop_sp_anchor: 0.0,
            loop_entry_at: u64::MAX,
            loop_entry_len: 0,
            loop_wrap_blend: [0.0; LOOP_BLEND_FRAMES * 2],
            loop_wrap_blend_len: 0,
            loop_entry_blend: [0.0; LOOP_BLEND_FRAMES * 2],
            loop_in_sent: None,
            loop_out_sent: None,
            last_leader_pos: None,
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
        // P23-B loop 量化（ManualLoop In/Out 起点终点全部对齐 beatgrid）：
        // 总线 loop_in/out 写入时（值变化边沿）snap 到拍线并写回（UI 快照
        // 读回一致；引擎自身回写已 snap，幂等）。无网格（bpm≤0）保持原始
        // 值不量化。引擎写回发生在 update_params 内，快照线程读到的就是
        // 对齐后的值，Flutter 侧只需传原始 playhead。
        self.snap_loop_bounds();
        self.handle_loop_edge();
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

    }

    /// P23-B loop 量化边沿：loop_in/out 总线写入（值变化）→ snap 到
    /// beatgrid 拍线（无网格不量化），写回总线保持 UI 一致。起点 = 最近
    /// 拍线（grid.snap）；终点 = 最近拍线，且距起点不足半拍时补足 1 拍
    /// （保底整拍，P21 语义保留）；起点无效（0 或 ≥ 终点）时回拉起点 =
    /// 终点 − 4 拍（P21 默认拍数）。全部用 grid 拍长（60/grid_bpm 源拍域
    /// ——旧 ManualLoop 用输出拍长 60/(grid×rate)，rate≠1 时终点错位）。
    fn snap_loop_bounds(&mut self) {
        let grid = BeatGrid {
            bpm: self.ctl.grid_bpm.get(),
            offset_secs: self.ctl.grid_offset.get(),
        };
        let raw_in = self.loop_in;
        let raw_out = self.loop_out;
        if self.loop_in_sent != Some(raw_in) {
            self.loop_in_sent = Some(raw_in);
            if raw_in > 0.0 && grid.is_valid() {
                let snapped = grid.snap(raw_in).max(0.0);
                if (snapped - raw_in).abs() > 1e-9 {
                    self.loop_in = snapped;
                    self.ctl.loop_in.set(snapped);
                }
            }
        }
        if self.loop_out_sent != Some(raw_out) {
            self.loop_out_sent = Some(raw_out);
            if raw_out > 0.0 && grid.is_valid() {
                // 终点 snap 到拍线；起点已 snap（同一块先处理 in 边沿，
                // 拍线 + 整拍 → 终点必在拍线上）。保底判断用原始 out 距
                // 起点的拍距（P21 语义：Out 与 In 同拍 → 不足半拍 → 补足
                // 1 拍——snap 后的 out 距可能已缩到 0，误判无效起点回拉）。
                let period = grid.period_secs();
                let mut snapped = grid.snap(raw_out).max(0.0);
                let in_valid = self.loop_in > 0.0 && self.loop_in < raw_out - 1e-9;
                if in_valid && raw_out - self.loop_in < 0.5 * period {
                    snapped = self.loop_in + period; // 保底 1 拍（P21 语义）
                } else if !in_valid {
                    // 无有效起点（未设 or 起点 ≥ 终点）：回拉起点 = 终点 −
                    // 4 拍（P21 默认拍数），snap 到拍线
                    let in_pull = grid.snap(snapped - 4.0 * period).max(0.0);
                    self.loop_in = in_pull;
                    self.ctl.loop_in.set(in_pull);
                }
                if (snapped - raw_out).abs() > 1e-9 {
                    self.loop_out = snapped;
                    self.ctl.loop_out.set(snapped);
                }
            }
        }
    }

    // ---- P23 Phase B：环状态机（激活即入，无捕获无 reset）----
    // 状态：loop_ring（环喂入中）/ loop_exiting（关环沿，收尾圈后退出）。
    // 事件：激活沿 → init_loop_ring（feed≥li 立即入环，d=(feed−li) mod len；
    //   feed<li 保持线性喂，feed 推进到 li 后下块 init）；关环沿 → exiting
    //   （wrap 处 finish）；激活中 in/out 变化（len 变化）→ 立即退出重建。

    /// 激活沿/参数变化处理（update_params 每块调用）。
    fn handle_loop_edge(&mut self) {
        let active = self.loop_active && self.loop_out > self.loop_in;
        if active && self.loop_exiting {
            self.loop_exiting = false; // 收尾圈中重新激活：环继续
        }
        if active && self.loop_ring && !self.loop_exiting {
            // 激活中 in/out 参数变化（len 变）→ 内容跳变：min-preroll seek
            // 到当前显示位置折叠进新环（declick 兜底 + 环相位重建；先重算
            // wrap blend——旧环长下预计算的 blend 对新边界已失效）。
            let new_len = ((self.loop_out - self.loop_in) * self.sr).max(0.0) as u64;
            if new_len > 0 && new_len != self.loop_len {
                let li = (self.loop_in * self.sr) as u64;
                self.build_wrap_blend(li, li + new_len, new_len);
                let rel = (self.pos as i64 - li as i64).rem_euclid(new_len as i64) as u64;
                self.seek_internal((li as f64 + rel as f64) / self.sr, true);
                return;
            }
        }
        if active && !self.loop_ring {
            self.init_loop_ring();
        } else if !active && self.loop_ring && !self.loop_exiting {
            self.loop_exiting = true; // 关环沿：喂完当前圈退出
        }
    }

    /// 入环初始化：d = (feed_pos − li) mod len；feed 在 [li, lo) = 连续入环
    ///（不 reset，折返从入环相位起，退出续点 = lo）；feed ≥ lo = 偏移入环
    ///（内容跳变：entry blend + reset 重锚到 li+d，退出续点 = P + k×len）。
    /// feed < li = pending（保持线性喂，feed 推进后下块重试）。
    fn init_loop_ring(&mut self) {
        let li = (self.loop_in * self.sr) as u64;
        let lo = (self.loop_out * self.sr) as u64;
        if lo <= li {
            return;
        }
        if self.feed_pos < li {
            return; // pending
        }
        let len = lo - li;
        self.loop_len = len;
        self.loop_offset = (self.feed_pos - li) % len;
        self.loop_cursor = self.loop_offset;
        self.loop_pushed = 0;
        self.loop_entry_at = u64::MAX;
        self.loop_entry_len = 0;
        self.build_wrap_blend(li, lo, len);
        if self.feed_pos >= lo {
            // 偏移入环：内容跳变 → entry blend + keylocker 重锚
            self.loop_offset_engage = true;
            self.loop_first_circle = false; // entry blend 处理 [d, d+bl)
            self.build_entry_blend(li, len);
            self.loop_feed_base = self.feed_pos; // 退出续点基准 P
            // feed_pos 保持 P（每圈 +len 记账，见 loop_wrap_check wrap）
            if let Some(kl) = self.keylocker.as_mut() {
                kl.set_track_position(li + self.loop_offset);
                self.loop_sp_anchor = kl.source_position();
            }
        } else {
            // 连续入环：feed_pos 置 lo（退出续点）。
            // 首圈 cursor<bl 区喂原始内容（中途入环时 wrap blend 起点
            // 接续的是 lo−1，接不上入环相位——首圈禁用，wrap 后启用）。
            self.loop_offset_engage = false;
            self.loop_first_circle = true;
            self.loop_feed_base = lo;
            self.feed_pos = lo;
        }
        self.loop_ring = true;
    }

    /// 圈首 wrap blend：尾(lo−bl..lo) 淡出 × 头(li..li+bl) 淡入（等功率
    /// cos/sin，192 帧封顶）。内容恒定 → 入环时预计算，全程复用。
    fn build_wrap_blend(&mut self, li: u64, lo: u64, len: u64) {
        self.loop_wrap_blend_len = 0;
        // 钳半（旧 P22-A 语义）：blend 区 [0, bl) 与尾区 [len−bl, len) 不得
        // 重叠（重叠 = 同一帧既作淡出又作淡入，短环取错样本）。
        let bl = LOOP_BLEND_FRAMES.min(len as usize / 2);
        if bl == 0 {
            return;
        }
        let Some(cache) = self.cache.as_ref() else { return };
        let mut tail = [0.0f32; LOOP_BLEND_FRAMES * 2];
        let mut head = [0.0f32; LOOP_BLEND_FRAMES * 2];
        let got_t = cache.copy_ready(&mut tail[..bl * 2], lo - bl as u64, bl);
        let got_h = cache.copy_ready(&mut head[..bl * 2], li, bl);
        if got_t == 0 || got_h == 0 {
            return; // 未填区欠载：无 blend（seek/欠载路径已有 declick 兜底）
        }
        let use_len = got_t.min(got_h);
        for i in 0..use_len {
            let t = ((i as f32 + 0.5) / use_len as f32) * (std::f32::consts::PI / 2.0);
            let (g_out, g_in) = (t.cos(), t.sin());
            for ch in 0..2 {
                self.loop_wrap_blend[i * 2 + ch] =
                    tail[i * 2 + ch] * g_out + head[i * 2 + ch] * g_in;
            }
        }
        self.loop_wrap_blend_len = use_len;
    }

    /// 偏移入环 entry blend：刚喂内容（缓存[P−bl..P)，P = 入环 feed 位置）
    /// 淡出 × 入环位置（缓存[li+d..li+d+bl)）淡入——旧 P22-B feed_tail
    /// 重建的缓存直读等价（feed 尾帧 = 缓存[P−bl..P)，全曲预解码免费）。
    fn build_entry_blend(&mut self, li: u64, len: u64) {
        self.loop_entry_len = 0;
        self.loop_entry_at = u64::MAX;
        let d = self.loop_offset;
        let bl = LOOP_BLEND_FRAMES.min(len.saturating_sub(d) as usize);
        if bl == 0 {
            return;
        }
        let Some(cache) = self.cache.as_ref() else { return };
        let p = self.loop_feed_base;
        let fade_out_start = p.saturating_sub(bl as u64);
        let mut tail = [0.0f32; LOOP_BLEND_FRAMES * 2];
        let mut head = [0.0f32; LOOP_BLEND_FRAMES * 2];
        let got_t = cache.copy_ready(&mut tail[..bl * 2], fade_out_start, bl);
        let got_h = cache.copy_ready(&mut head[..bl * 2], li + d, bl);
        if got_t == 0 || got_h == 0 {
            return;
        }
        let use_len = got_t.min(got_h);
        for i in 0..use_len {
            let t = ((i as f32 + 0.5) / use_len as f32) * (std::f32::consts::PI / 2.0);
            let (g_out, g_in) = (t.cos(), t.sin());
            for ch in 0..2 {
                self.loop_entry_blend[i * 2 + ch] =
                    tail[i * 2 + ch] * g_out + head[i * 2 + ch] * g_in;
            }
        }
        self.loop_entry_len = use_len;
        self.loop_entry_at = d;
    }

    /// 环退出：pos_base 重锚 + 清环态（feed_pos 已是线性续点）。
    /// `immediate`：激活中参数变化（内容已跳，播头锚定当前显示位置）；
    /// false：收尾圈完成（常规 = 折返公式，偏移 = feed 记账）。
    fn finish_loop_ring(&mut self, immediate: bool) {
        if immediate {
            if let Some(kl) = self.keylocker.as_ref() {
                self.pos_base = Some(self.pos - kl.source_position());
            }
        } else if self.loop_offset_engage {
            // 偏移路径：音频切回线性续点 P+k×len（feed_pos 已累计），
            // 播头随声音跳（有符号中间量：偏移路径 Δ = k×len − pushed）
            self.pos_base = Some(
                (self.feed_pos as i64 - self.loop_feed_base as i64 - self.loop_pushed as i64)
                    as f64,
            );
        } else if let Some(kl) = self.keylocker.as_ref() {
            // 常规路径：音频 = 环相位连续续进（[li,lo) 接 lo 无缝），
            // 播头 = 可闻位置（位置模反解当前标签：显示 = sp 恒等）。
            // 不能用 (sp−anchor) 旧锚折返——引擎管线深度随喂入节奏
            // 漂移（冷启动预填 3376 → 环中 3250），旧锚在退出时产生
            // stale 结果；位置模 (sp−li−d) 与环显示同式，退出无缝。
            // 收尾圈完成处 sp−li−d = len−in-flight < len（无回绕）→
            // pos_base = 0，显示 = sp = 线性契约。
            let sp = kl.source_position();
            let li = (self.loop_in * self.sr) as u64;
            let d = self.loop_offset;
            let folded = (sp - (li + d) as f64).rem_euclid(self.loop_len as f64);
            self.pos_base = Some((li + d) as f64 + folded - sp);
        }
        self.loop_ring = false;
        self.loop_exiting = false;
        self.loop_first_circle = false;
        self.loop_len = 0;
        self.loop_offset = 0;
        self.loop_cursor = 0;
        self.loop_pushed = 0;
        self.loop_entry_at = u64::MAX;
        self.loop_entry_len = 0;
        self.loop_wrap_blend_len = 0;
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

        // P23-B：leader 位置跳变（beatjump/seek/loop 回绕——正常推进每块
        // <0.02 拍 @200bpm 极限，阈值 0.5 拍）→ 重新对齐。follower 自身
        // 的微调/seek 不改变 leader 位置 → 不触发（P14「操作后不再自动
        // 对拍」保持，只对 follower 侧成立）。
        if let Some(last) = self.last_leader_pos
            && (leader.position_secs - last).abs() > SYNC_LEADER_JUMP_BEATS * fgrid.period_secs()
        {
            self.sync_align_done = false;
        }
        self.last_leader_pos = Some(leader.position_secs);

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
        // P23 Phase B：环期间折返映射回 [loop_in, loop_out)（source_position
        // 是累计帧，锚点不归零）；收尾圈退出后 pos_base 重锚 → 线性续进。
        let sp = kl.source_position();
        self.pos = if self.loop_ring {
            let len = self.loop_len;
            let li = (self.loop_in * self.sr) as u64;
            let d = self.loop_offset;
            if self.loop_offset_engage {
                // 偏移路径：advance-mod（入环瞬间锚定，旧 P22-B 显示语义——
                // engage 立即折返 li+d，长跑不出 [li+d, li+d+len)）
                li as f64 + d as f64 + (sp - self.loop_sp_anchor).rem_euclid(len as f64)
            } else {
                // 连续路径：位置模 = 可闻位置相位（标签 = 内容坐标，
                // sp−li−d ∈ [−L, len−L)，负值折叠回环；与 finish 同式
                // 退出无缝，管线深度变化不产生 stale 折叠）
                li as f64 + d as f64 + (sp - (li + d) as f64).rem_euclid(len as f64)
            }
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
                // P23 Phase B：legacy 环 = 逐帧回绕（fallback 无 blend，
                // 旧实现仅曲尾钳制回跳，现统一任意环越界回绕）
                if self.loop_active && self.pos >= self.loop_out * self.sr {
                    self.pos = self.loop_in * self.sr;
                    match self.read_stereo(self.pos) {
                        Some((l, r)) => {
                            self.pos += self.rate;
                            (l, r)
                        }
                        None => (0.0, 0.0),
                    }
                } else if n > 0 && self.pos >= n as f64 {
                    // 播放到结尾：停
                    self.playing = false;
                    self.ctl.play.set(0.0);
                    (0.0, 0.0)
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

    /// 喂入引擎：按 demand_hint（基于本块渲染帧数）补源帧；曲尾 finish() 冲刷。
    ///
    /// P23 Phase A：数据源 = TrackCache 直读（copy_ready → 预分配
    /// feed_scratch，音频线程零分配）。未填块 = 欠载等待（request_priority
    /// 注册跳填，filler 赶来补块后下块续喂），播头冻结——与旧 ring 空
    /// 同构。loop 捕获/回喂机制已删（Phase B 重做）。
    fn feed_keylocker(&mut self, engine_frames: usize) {
        if self.keylocker.is_none() {
            return;
        }
        let n = self.track_frames.load(Ordering::Relaxed);
        let want = self
            .keylocker
            .as_ref()
            .unwrap()
            .demand_hint(engine_frames, MAX_ENGINE_RATE);
        while self.keylocker.as_ref().unwrap().occupied_frames() < want && !self.eof_fed {
            // P23 Phase B：环喂入（缓存窗口循环，无捕获无 reset；EOF 不
            // 涉及——环内容恒在缓存内，未填块 = 欠载 break）
            if self.loop_ring {
                if !self.feed_loop_segment() {
                    break; // 引擎 ring 满 或 欠载
                }
                continue;
            }
            // 曲尾：冲刷 resampler lookahead（finish 失败下一块重试）。
            if n > 0 && self.feed_pos >= n {
                self.eof_fed = self.keylocker.as_mut().unwrap().finish();
                break;
            }
            let Some(cache) = &self.cache else {
                break;
            };
            // 缓存直读：未填块（copy_ready == 0）= 欠载等待，播头冻结——
            // request_priority 注册跳填，filler 赶来补块后下块续喂。
            // 尾块拷贝量钳到 n−feed_pos：EOF 补零写在块内，越 total 的
            // 补零帧不喂（旧 reader 精确停 n 的语义）。n=0（测试棚未
            // 设 track_frames）时不做钳制。
            let want_frames = if n > 0 {
                (n - self.feed_pos).min(CHUNK_FRAMES as u64) as usize
            } else {
                CHUNK_FRAMES
            };
            let got = cache.copy_ready(&mut self.feed_scratch, self.feed_pos, want_frames);
            if got == 0 {
                cache.request_priority(self.feed_pos);
                break;
            }
            let accepted = self
                .keylocker
                .as_mut()
                .unwrap()
                .push(&self.feed_scratch[..got * 2])
                .min(got);
            if accepted == 0 {
                break; // 引擎 ring 满
            }
            self.feed_pos += accepted as u64;
        }
    }

    /// 环喂入：从缓存循环读 [li, li+len)（cursor 0..len 回绕），段界 =
    /// entry blend 段 / 圈首 wrap blend 段 / 整圈（普通段）。返回 false =
    /// 引擎 ring 满 或 缓存欠载（播头冻结，request_priority 跳填）。
    /// 圈界回绕时：偏移路径 feed_pos += len（退出续点记账）；loop_exiting
    /// 时收尾圈完成 → finish_loop_ring（退出锚点 + 切线性续喂）。
    fn feed_loop_segment(&mut self) -> bool {
        let li = (self.loop_in * self.sr) as u64;
        let len = self.loop_len;
        let cache = match &self.cache {
            Some(c) => c,
            None => return true,
        };
        // entry 段已喂完 → 失效（后续圈首走 wrap blend 段）
        if self.loop_entry_at != u64::MAX
            && self.loop_cursor >= self.loop_entry_at + self.loop_entry_len as u64
        {
            self.loop_entry_at = u64::MAX;
        }
        // 段界：entry 段 / 圈首 wrap blend 段 / 整圈
        let in_entry = self.loop_entry_at != u64::MAX
            && self.loop_cursor >= self.loop_entry_at;
        let (src, base, seg_end): (&[f32], u64, u64) = if in_entry {
            (
                &self.loop_entry_blend,
                self.loop_entry_at,
                self.loop_entry_at + self.loop_entry_len as u64,
            )
        } else if !self.loop_first_circle && self.loop_cursor < self.loop_wrap_blend_len as u64 {
            (&self.loop_wrap_blend, 0, self.loop_wrap_blend_len as u64)
        } else {
            // 普通段：缓存直拷（分段到 CHUNK_FRAMES；段界 = 圈界 → 不跨圈）
            let seg_end = len;
            let start = li + self.loop_cursor;
            let n = (seg_end - self.loop_cursor).min(CHUNK_FRAMES as u64) as usize;
            let got = cache.copy_ready(&mut self.feed_scratch, start, n);
            if got == 0 {
                cache.request_priority(start);
                return false; // 欠载：播头冻结（等 filler）
            }
            let accepted = self
                .keylocker
                .as_mut()
                .unwrap()
                .push(&self.feed_scratch[..got * 2])
                .min(got);
            self.loop_cursor += accepted as u64;
            self.loop_pushed += accepted as u64;
            if accepted == 0 {
                return false; // 引擎 ring 满
            }
            return self.loop_wrap_check(li, len);
        };
        // blend 段：喂 blend 切片（部分接受时停在段内续推）
        let start_idx = (self.loop_cursor - base) as usize;
        let n = (seg_end - self.loop_cursor) as usize;
        let accepted = self
            .keylocker
            .as_mut()
            .unwrap()
            .push(&src[start_idx * 2..(start_idx + n) * 2])
            .min(n);
        self.loop_cursor += accepted as u64;
        self.loop_pushed += accepted as u64;
        if accepted == 0 {
            return false;
        }
        self.loop_wrap_check(li, len)
    }

    /// 圈界回绕：cursor 回 0；偏移路径 feed_pos += len；exiting → 收尾圈
    /// 完成退出。返回 true（喂入继续）。
    fn loop_wrap_check(&mut self, _li: u64, len: u64) -> bool {
        if self.loop_cursor < len {
            return true;
        }
        self.loop_cursor -= len; // accepted ≤ 段长 ≤ len → 至多跨一圈
        self.loop_first_circle = false; // 已回绕：wrap blend 区启用
        if self.loop_offset_engage {
            self.feed_pos += len; // 偏移路径退出续点记账（P + k×len）
        }
        if self.loop_exiting {
            self.finish_loop_ring(false); // 收尾圈完成：退出锚点 + 切线性
        }
        true
    }

    /// 读取指定帧（48kHz 时间轴）的线性插值立体声采样（缓存直读）。
    fn read_stereo(&mut self, pos: f64) -> Option<(f32, f32)> {
        let i0 = pos as usize;
        let frac = (pos - i0 as f64) as f32;
        let (l0, r0) = self.frame_at(i0)?;
        let (l1, r1) = self.frame_at(i0 + 1)?;
        Some((l0 + (l1 - l0) * frac, r0 + (r1 - r0) * frac))
    }

    /// 取单帧（缓存直读；未填块返回 None = 欠载，保持位置等待）。
    /// EOF 尾块补零由 deck 端 n 检查保护（feed_keylocker/process_legacy）。
    fn frame_at(&mut self, idx: usize) -> Option<(f32, f32)> {
        let cache = self.cache.as_ref()?;
        let mut pair = [0.0f32; 2];
        if cache.copy_ready(&mut pair, idx as u64, 1) == 1 {
            Some((pair[0], pair[1]))
        } else {
            None
        }
    }

    /// 引擎操作：加载音轨（TrackCache::open：同步解码首块 + filler 线程
    /// 续填；全曲预解码 → 加载完成后拔 U 盘不中断播放）。
    pub fn load(&mut self, path: std::path::PathBuf) {
        // 停旧缓存（filler 线程退出、旧世代作废）
        if let Some(cache) = self.cache.take() {
            cache.stop();
        }

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

        // 开新缓存：失败 → 保持未加载（loaded=false、play 停、静音）
        let cache = match TrackCache::open(&path, self.sr as u32) {
            Ok(c) => c,
            Err(e) => {
                log::error!("加载失败（{path:?}）：{e:#}");
                self.loaded = false;
                self.ctl.loaded.set(0.0);
                self.ctl.play.set(0.0);
                self.ctl.playhead.set(0.0);
                return;
            }
        };
        self.cache = Some(cache.clone());
        self.epoch = self.epoch.wrapping_add(1);
        self.track_frames = cache.total_frames.clone();
        self.loaded = true;
        self.pos = 0.0;
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
        self.ctl.loop_active.set(0.0);
        self.ctl.loop_in.set(0.0);
        self.ctl.loop_out.set(0.0);
        self.loop_ring = false;
        self.loop_exiting = false;
        self.loop_len = 0;
        self.loop_offset = 0;
        self.loop_offset_engage = false;
        self.loop_cursor = 0;
        self.loop_feed_base = 0;
        self.loop_pushed = 0;
        self.loop_sp_anchor = 0.0;
        self.loop_entry_at = u64::MAX;
        self.loop_entry_len = 0;
        self.loop_wrap_blend_len = 0;
        self.loop_in_sent = None; // 新曲网格未就绪，量化边沿重新跟踪
        self.loop_out_sent = None;
        self.last_leader_pos = None; // 换曲：leader 位置坐标重建，跳变基准清零
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
        // P23 Phase A：捕获准备已删——越界统一走块首回跳临时降级
        //（Phase B 重做缓存窗口循环喂入，环长上限届时一并删除）。
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
    ///
    /// P23：数据源 = 缓存直读，无排水/命令/respawn——只剩 reset 重锚 +
    /// request_priority（未填区由 filler 按 priority 跳填，音频线程零
    /// 阻塞 store）。捕获状态机已删（Phase B 重做）。
    fn seek_internal(&mut self, seconds: f64, min_preroll: bool) {
        let frame = (seconds * self.sr) as u64;
        self.pos = frame as f64;
        self.pos_base = None; // 新 fed 坐标（P11.1：收尾圈锚点随 reset 作废）
        self.ctl.playhead.set(seconds);
        let engine_rate = self.engine_rate();
        // keylock 路径：reset + 重新锚定 + warm_start 预卷（spike 验证零欠载零 NaN）。
        // 缓存直读从 read_frame（= target − preroll）起喂。
        let read_frame = if let Some(kl) = self.keylocker.as_mut() {
            let preroll = if min_preroll {
                // P14 beatjump 最小预卷：priming 1 帧 → done_at=0 立即
                // 收尾（declick 淡入 64 帧）≈ 0 静音；代价 = settle
                // 冷启动的瞬态质量（先例：rebuild 路径 warm_start(1)）。
                // 未填区首次访问 = priority-fill 时间（数十 ms 级），
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
            self.eof_fed = false;
            self.eof_stall = 0;
            self.last_sp = 0.0;
            read_frame
        } else {
            frame
        };
        // P23 Phase B：环与 seek 交互——
        // - 落点在环内（调用方已保 active）：重建环相位（内容从落点续喂，
        //   seek 的 reset+declick 已防 click；统一常规语义，退出续点 = lo）；
        // - 落点在环外（调用方 deactivate 已清 active）：清环态（seek 后
        //   线性喂从 read_frame 起，旧环 feed 记账作废）。
        // 判别用 loop_ring || loop_active（不能只看 active）：beatjump/
        // seek 出环路径先 deactivate_loop_if_outside 清 active 再进这里，
        // active 已 false 时残留 loop_ring 必须照样清除——否则显示与
        // 喂入仍走环分支（播头钳回环内、内容回跳环头）。
        if (self.loop_ring || self.loop_active)
            && self.loop_out > self.loop_in
            && self.keylocker.is_some()
        {
            let li = (self.loop_in * self.sr) as u64;
            let len = ((self.loop_out - self.loop_in) * self.sr) as u64;
            if len > 0 && read_frame >= li && read_frame < li + len {
                self.loop_len = len;
                self.loop_offset = read_frame - li;
                self.loop_cursor = read_frame - li;
                // 偏移记账（与激活即入的偏移路径同构）：退出续点 =
                // read_frame + k×len（seek 后内容从落点续喂，退出续点
                // 必须跟随落点——旧"常规续点 = lo"会让退出跳回 lo）。
                self.loop_offset_engage = true;
                self.loop_feed_base = read_frame;
                self.feed_pos = read_frame;
                self.loop_pushed = 0;
                self.loop_entry_at = u64::MAX;
                self.loop_entry_len = 0;
                self.loop_exiting = false;
                self.loop_ring = true;
                // 首圈禁用 wrap blend（中途入环接不上 lo−1）；从未建过
                // blend（激活即 pending、先 seek 后入环）时补建。
                self.loop_first_circle = true;
                if self.loop_wrap_blend_len == 0 {
                    self.build_wrap_blend(li, li + len, len);
                }
                if let Some(kl) = self.keylocker.as_ref() {
                    self.loop_sp_anchor = kl.source_position();
                }
            } else {
                self.loop_ring = false;
                self.loop_exiting = false;
                self.loop_first_circle = false;
                self.loop_len = 0;
                self.loop_offset = 0;
                self.loop_offset_engage = false;
                self.loop_cursor = 0;
                self.loop_pushed = 0;
                self.loop_entry_at = u64::MAX;
                self.loop_entry_len = 0;
                self.loop_wrap_blend_len = 0;
            }
        }
        self.epoch = self.epoch.wrapping_add(1);
        // 未填区由 filler 按 priority 跳填（seek 落点在已填区时无害 no-op）
        if let Some(cache) = &self.cache {
            cache.request_priority(read_frame);
        }
    }

}

// ---------- 测试夹具（deck 单测 + engine 级联动测试共用） ----------

/// 440Hz 正弦（幅度 0.5）全填缓存（test seam 直接置块：无线程无 I/O）。
/// 尾块填满整块（含越 total 的补零区）——deck 端 EOF 检查（feed_pos >= n）
/// 先行保护，不喂补零区。
#[cfg(test)]
pub(crate) fn test_filled_cache(secs: f64) -> Arc<TrackCache> {
    let sr = 48_000u32;
    let cache = TrackCache::test_new_empty(sr);
    let n = (secs * sr as f64) as u64;
    cache.test_set_total(n);
    let chunks = n.div_ceil(CHUNK_FRAMES as u64) as usize;
    for k in 0..chunks {
        let mut data = Vec::with_capacity(CHUNK_FRAMES * 2);
        for f in 0..CHUNK_FRAMES {
            let t = (k * CHUNK_FRAMES + f) as f32 / sr as f32;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            data.push(s);
            data.push(s);
        }
        cache.test_set_chunk(k, data.into_boxed_slice());
    }
    cache
}

/// 预填缓存的测试 deck（keylock 引擎、无 reader/filler 线程）。
/// secs 秒 440Hz 正弦内容（全块 Ready，任意 seek 即时可读）。
#[cfg(test)]
pub(crate) fn test_deck_with_cache(
    bus: &hypermixx_core::ControlBus,
    secs: f64,
    rate_pct: f64,
) -> Deck {
    let mut d = Deck::new(0, 48000, bus);
    let cache = test_filled_cache(secs);
    d.cache = Some(cache.clone());
    d.track_frames = cache.total_frames.clone(); // 与 load 同源（EOF 判停依赖）
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

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_deck_with_cache as deck_with_cache;

    /// Keylock profile 引擎延迟（560 帧，spike 实测）折算秒。
    const KEYLOCK_LATENCY_S: f64 = 560.0 / 48000.0;

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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
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
        let mut d = deck_with_cache(&bus, 0.68, 0.0);
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
        let mut d = deck_with_cache(&bus, 0.68, 100.0); // +100% → 2.0×
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
        let mut d = deck_with_cache(&bus, 2.73, -8.0); // 0.92×
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
        let mut d = deck_with_cache(&bus, 2.73, 8.0); // 1.08×
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
        let mut d = deck_with_cache(&bus, 8.53, 8.0); // 1.08×
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
        let mut d = deck_with_cache(&bus, 8.53, 8.0);
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
        let mut d = deck_with_cache(&bus, 8.53, 0.0);
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
        run_frames(&mut d, 48000); // 播 1 秒
        let head_before = d.ctl.playhead.get();
        assert!(
            (head_before - (1.0 - KEYLOCK_LATENCY_S)).abs() < 0.01,
            "seek 前播头 {head_before}"
        );

        // 缓存全填：seek 后数据即时可用，无 reader 响应延迟。
        d.seek_seconds(1.0);
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
        let mut d = deck_with_cache(&bus, 8.53, 8.0);
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
        let mut d = deck_with_cache(&bus, 8.53, 0.0);
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
        let mut d = deck_with_cache(&bus, 0.34, 0.0); // 8×2048 = 16384 帧
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
        d.keylocker = None;
        let (peak, head) = run_frames(&mut d, 48000);
        assert!(peak > 0.4, "回退路径应输出正弦波，peak={peak}");
        assert!(
            (head - 1.0).abs() < 0.01,
            "回退路径播头应到 1.0s，实际 {head}"
        );
    }

    // ---------- P5 beat sync ----------

    /// 拍脉冲缓存（440Hz 正弦底 0.05 + 每 beat_period_frames 全局帧一个
    /// 8ms 指数衰减脉冲 0.5）——输出包络可测节奏与相位。secs 秒全填。
    fn pulse_cache(secs: f64, beat_period_frames: u64) -> Arc<TrackCache> {
        let cache = TrackCache::test_new_empty(48_000);
        let n = (secs * 48000.0) as u64;
        cache.test_set_total(n);
        let chunks = n.div_ceil(CHUNK_FRAMES as u64) as usize;
        for k in 0..chunks {
            let mut data = Vec::with_capacity(CHUNK_FRAMES * 2);
            for f in 0..CHUNK_FRAMES {
                let g = (k * CHUNK_FRAMES + f) as u64;
                let since = (g % beat_period_frames) as f64 / 48000.0;
                let pulse = if since < 0.012 {
                    (-since / 0.004).exp() * 0.5
                } else {
                    0.0
                };
                let s = ((2.0 * std::f64::consts::PI * 440.0 * (g as f64 / 48000.0)).sin()
                    * (0.05 + pulse)) as f32;
                data.push(s);
                data.push(s);
            }
            cache.test_set_chunk(k, data.into_boxed_slice());
        }
        cache
    }

    /// 预填脉冲缓存的测试 deck（同步长测用）。
    fn deck_with_cache_big(
        bus: &hypermixx_core::ControlBus,
        cache: Arc<TrackCache>,
        rate_pct: f64,
    ) -> Deck {
        let mut d = Deck::new(0, 48000, bus);
        d.cache = Some(cache.clone());
        d.track_frames = cache.total_frames.clone(); // 与 load 同源
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(25.6, 24000), -8.0); // 120 BPM 脉冲
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(25.6, 24000), 0.0); // 120 BPM 脉冲
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(25.6, 24000), 0.0);
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(38.4, 24000), 0.0); // 120 BPM 脉冲
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(25.6, 24000), 0.0);
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(25.6, 24000), 0.0);
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(25.6, 24000), 8.0);
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(38.4, 24000), 0.0);
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(38.4, 24000), 8.0);
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(38.4, 24000), 8.0);
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(38.4, 24000), 8.0);
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
        let mut d = deck_with_cache(&bus, 8.53, 0.0);
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

        // sync 下 seek：+0.37 拍（离拍微调）——不被拉回。缓存全填，
        // seek 后数据即时可用（无 reader 响应模拟）。
        let target = d.ctl.playhead.get() + 0.37 * 0.5;
        d.seek_exact(target);
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
        let mut d = deck_with_cache_big(&bus, pulse_cache(25.6, 24000), 8.0);
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
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

    /// 写 440Hz 立体声 32-bit float WAV（真实解码路径用），采样率可指定。
    fn write_sine_wav_sr(path: &std::path::Path, secs: f64, sr: u32) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: sr,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let n = (secs * sr as f64) as usize;
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    fn write_sine_wav(path: &std::path::Path, secs: f64) {
        write_sine_wav_sr(path, secs, 48_000);
    }

    /// EOF 后 seek 回已填区直接出声（Phase A 语义：全曲缓存驻留 RAM，
    /// 无 reader 重生/重填等待）。场景：真实 filler 线程播完 2s 曲目
    /// 自动停止，之后 seek 回 0.3s 再播——数据早已在缓存，min-preroll
    /// 立即出声、零欠载。
    #[test]
    fn eof_then_seek_back_plays_instantly() {
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
        // 等 filler 填满全曲（2s 曲 ≈5-20× 实时，几百 ms 内完成；轮询
        // 兜底并行测试负载）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !d.cache.as_ref().unwrap().fill_done()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(d.cache.as_ref().unwrap().fill_done(), "filler 应填满 2s 曲");

        // EOF 后 seek：数据已在缓存，min-preroll 立即出声（无 respawn）。
        // seek 前欠载计数含 EOF-stall 窗口的合法静音（finish 后冲刷），
        // 断言 seek 后不再新增（数据即时可得）。
        let underrun_before = d.keylocker.as_ref().unwrap().underrun_frames();
        d.seek_seconds(0.3);
        d.ctl.play.set(1.0);
        let rec = run_capture(&mut d, 0.8);
        let peak = rec.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.3, "seek 后应重新出音，peak={peak}");
        let head = d.ctl.playhead.get();
        assert!(head > 0.4, "播头应推进过 0.3s，实际 {head:.3}s");
        assert!(d.ctl.play.get() > 0.5, "seek 后不应再次判停");
        // 频率仍是 440Hz（参数为帧索引）
        let f = zero_crossing_freq(&rec, rec.len() / 2 - 14400, rec.len() / 2 - 4800);
        assert!(
            cents_off(f, 440.0).abs() < 10.0,
            "音高应保持 440Hz，实测 {f:.1}Hz"
        );
        assert_eq!(
            d.keylocker.as_ref().unwrap().underrun_frames(),
            underrun_before,
            "EOF 后 seek 数据即时可得，不得新增欠载"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// 回归：44.1kHz 源文件（非 48k）加载即出声。历史 bug：fill_block
    /// 把升采样 pending 空无条件当 EOF → total=0、首块 0 帧、加载即无声
    ///（beatjump 仍"可用"——seek 路径独立不受影响）。修复后首块同步
    /// 填好、filler 续填全曲、音高保持 440Hz。
    #[test]
    fn loads_441k_plays_immediately() {
        let bus = hypermixx_core::ControlBus::default();
        let path =
            std::env::temp_dir().join(format!("hypermixx_441k_{}.wav", std::process::id()));
        write_sine_wav_sr(&path, 2.0, 44_100);

        let mut d = Deck::new(0, 48000, &bus);
        d.ctl.volume.set(1.0);
        d.load(path.clone());

        // 核心回归点：total=0 则此断言即失败（bug 症状）
        let cache = d.cache.as_ref().expect("load 应建立缓存");
        assert!(
            cache.total_frames() > 0,
            "44.1k 升采样 total_frames 不得为 0（fill_block EOF 误判回归）"
        );
        assert!(
            cache.filled_frames() >= 2048,
            "open 应同步填好首块，实际 {}",
            cache.filled_frames()
        );

        // 等 filler 填满全曲（2s 曲 ≈5-20× 实时，轮询兜底并行负载）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !cache.fill_done() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(cache.fill_done(), "44.1k filler 应填满 2s 曲");

        // seek 过首块边界（0.043s）进入 filler 填的区，验证整曲可播
        d.seek_seconds(0.3);
        d.ctl.play.set(1.0);
        let rec = run_capture(&mut d, 0.5);
        let peak = rec.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.3, "44.1k 升采样应出声，peak={peak}");
        let head = d.ctl.playhead.get();
        assert!(head > 0.4, "播头应推进过 0.3s，实际 {head:.3}s");
        // 频率仍是 440Hz（升采样保音高）
        let f = zero_crossing_freq(&rec, rec.len() / 2 - 14400, rec.len() / 2 - 4800);
        assert!(
            cents_off(f, 440.0).abs() < 10.0,
            "44.1k 升采样音高应保持 440Hz，实测 {f:.1}Hz"
        );

        let _ = std::fs::remove_file(&path);
    }
    /// keylock 开：音高保持 440Hz（变速不变调）；关：纯 varispeed（±8% 变调）。
    #[test]
    fn nudge_bends_rate_temporarily() {
        let bus = hypermixx_core::ControlBus::default();
        // keylock 开 + nudge：音高不变
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
        d.ctl.nudge.set(1.0);
        let _ = run_frames(&mut d, 4800); // 跨 set_rate 交叉淡化
        let rec = run_capture(&mut d, 0.5);
        let f = zero_crossing_freq(&rec, 2400, rec.len() / 2 - 2400);
        assert!(
            cents_off(f, 440.0).abs() < 10.0,
            "keylock 开 nudge 不应变调，实测 {f:.1}Hz"
        );
        // keylock 关 + nudge +：440 × 1.08 ≈ 475.2
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
        bus.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(0.0); // 显式空槽
        let out = run_capture(&mut d, 0.5);
        let f = zero_crossing_freq(&out, 2400, out.len() / 2 - 2400);
        assert!(
            cents_off(f, 440.0).abs() < 10.0,
            "空 rack 不影响信号，实测 {f:.1}Hz"
        );
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
        // 缓存全填正弦但曲长钳到 0.34s（8 chunk）：EOF 判停后输入停止，
        // 回声串继续衰减（原 rig 用正弦+静音 chunk 模拟同一场景）
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
        d.track_frames.store((0.34 * 48000.0) as u64, Ordering::Relaxed);
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
        let mut d = deck_with_cache(&bus, 8.53, 0.0);
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
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
        let mut a = deck_with_cache(&bus_a, 2.73, 0.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(1.0); // echo
        bus_a.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(1.0);
        let _ = run_frames(&mut a, 4800); // 0.1s 回声活跃
        bus_a.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(0.0);
        let _ = run_frames(&mut a, 4800); // 淡出 10ms + settled
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_cache(&bus_b, 2.73, 0.0);
        let _ = run_frames(&mut b, 9600); // 相同块数推进到同位置
        let rec_b = run_capture(&mut b, 0.5);
        assert_eq!(rec_a, rec_b, "enable=0 settled 后应与无 FX 逐位一致");
    }

    #[test]
    fn fx_gate_bypass_without_grid() {
        // 无 beatgrid：gate 内部逐位直通 → 与无 FX 跑逐位一致
        let bus_a = hypermixx_core::ControlBus::default();
        let mut a = deck_with_cache(&bus_a, 2.73, 0.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(8.0); // gate
        bus_a.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(1.0);
        let _ = run_frames(&mut a, 4800); // 淡入完成（gate 直通 → 输出恒等干声）
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_cache(&bus_b, 2.73, 0.0);
        let _ = run_frames(&mut b, 4800);
        let rec_b = run_capture(&mut b, 0.5);
        assert_eq!(rec_a, rec_b, "无网格 gate 应与无 FX 逐位一致");
    }

    #[test]
    fn fx_survives_seek_and_eof() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
        bus.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(1.0); // echo
        bus.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).set(1.0);
        let _ = run_frames(&mut d, 48000); // 播 1 秒（回声稳定）
        let head_before = d.ctl.playhead.get();
        assert!(head_before > 0.9, "seek 前播头 {head_before}");

        // 缓存全填：seek 后数据即时可用（无 reader 响应模拟）。
        d.seek_seconds(1.0);
        // 曲尾 = 缓存 total_frames（deck_with_cache 已设 2.73s）：
        // feed 到曲尾触发 finish() → EOF stall 判停。
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
        let mut d = deck_with_cache(&bus, 8.53, 0.0);
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
        let mut a = deck_with_cache(&bus_a, 2.73, 0.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_drywet(0, 0)).set(1.0);
        bus_a.control(&hypermixx_core::paths::deck_fx_type(0, 0)).set(1.0); // echo
        let _ = run_frames(&mut a, 4800); // 换型 + 0.1s（enable 从未置 1）
        assert!(
            bus_a.control(&hypermixx_core::paths::deck_fx_enable(0, 0)).get() < 0.5,
            "换型不应自动 enable（只在 ON 时打开）"
        );
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_cache(&bus_b, 2.73, 0.0);
        let _ = run_frames(&mut b, 4800);
        let rec_b = run_capture(&mut b, 0.5);
        assert_eq!(rec_a, rec_b, "enable=0 换型应与无 FX 逐位一致");
    }

    #[test]
    fn fx_slot_7_works() {
        // 8 槽扩展：第 8 槽（index 7）换型 + 开启 + 失真饱和
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
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
    fn loop_external_seek_outside_deactivates() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
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
        let mut d = deck_with_cache(&bus, 6.83, 0.0);
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
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
        let mut d = deck_with_cache(&bus, 6.83, 0.0);
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
        let mut d = deck_with_cache(&bus, 6.83, 0.0);
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        d.set_beat_loop(1.0); // in=0，out=0.5
        d.beatjump(4.0); // 2s → 出环
        assert!(d.ctl.loop_active.get() < 0.5, "beatjump 出环应取消 loop");
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
        let mut a = deck_with_cache(&bus_a, 2.73, 0.0);
        bus_a.control(&hypermixx_core::paths::deck_gain(0)).set(0.0);
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_cache(&bus_b, 2.73, 0.0);
        let rec_b = run_capture(&mut b, 0.5);
        assert_eq!(rec_a, rec_b, "gain=0dB 应与默认逐位一致");
    }

    #[test]
    fn gain_trim_boosts_and_cuts() {
        // +12dB ≈ ×3.98、-12dB ≈ ×0.25（0.2s 平滑稳定后测稳态 RMS）
        let bus_a = hypermixx_core::ControlBus::default();
        let mut a = deck_with_cache(&bus_a, 2.73, 0.0);
        bus_a.control(&hypermixx_core::paths::deck_gain(0)).set(12.0);
        let _ = run_frames(&mut a, 9600); // 0.2s 稳定
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_cache(&bus_b, 2.73, 0.0);
        bus_b.control(&hypermixx_core::paths::deck_gain(0)).set(-12.0);
        let _ = run_frames(&mut b, 9600);
        let rec_b = run_capture(&mut b, 0.5);

        let bus_c = hypermixx_core::ControlBus::default();
        let mut c = deck_with_cache(&bus_c, 2.73, 0.0);
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
        let mut a = deck_with_cache(&bus_a, 2.73, 0.0);
        bus_a.control(&hypermixx_core::paths::deck_filter(0)).set(0.8);
        let _ = run_frames(&mut a, 4800); // 0.1s LP 活跃
        bus_a.control(&hypermixx_core::paths::deck_filter(0)).set(0.0);
        let _ = run_frames(&mut a, 14400); // 0.3s 回落 + settled
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_cache(&bus_b, 2.73, 0.0);
        let _ = run_frames(&mut b, 4800);
        let _ = run_frames(&mut b, 14400);
        let rec_b = run_capture(&mut b, 0.5);
        assert_eq!(rec_a, rec_b, "滤波旋钮 0 且稳定应与默认逐位一致");
    }

    #[test]
    fn deck_filter_kills_highs_at_lp() {
        // 旋钮 +1 → LP@20Hz：440Hz 正弦 RMS 崩塌（对比旁路参照 deck）
        let bus_a = hypermixx_core::ControlBus::default();
        let mut a = deck_with_cache(&bus_a, 2.73, 0.0);
        bus_a.control(&hypermixx_core::paths::deck_filter(0)).set(1.0);
        let _ = run_frames(&mut a, 9600); // 0.2s 稳定
        let rec_a = run_capture(&mut a, 0.5);

        let bus_b = hypermixx_core::ControlBus::default();
        let mut b = deck_with_cache(&bus_b, 2.73, 0.0);
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
        let mut d = deck_with_cache(&bus, 2.73, 0.0); // 131072 帧 ≈ 2.73s
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

    // ---------- P23 Phase B：环状态机（缓存窗口循环喂入） ----------

    /// 锯齿缓存（周期 = period 帧，值 = 相位×0.5）：圈界跳变 0.5 的
    /// 接缝 fixture（同旧 loop_wrap_seam_blend_no_click）。本地内容
    /// 平滑（192 帧斜率 ≈0.005）→ 无 blend 时的圈界跳变必爆 Δ 断言。
    fn saw_cache(secs: f64, period: usize) -> Arc<TrackCache> {
        let cache = TrackCache::test_new_empty(48_000);
        let n = (secs * 48000.0) as u64;
        cache.test_set_total(n);
        let chunks = n.div_ceil(CHUNK_FRAMES as u64) as usize;
        for k in 0..chunks {
            let mut data = Vec::with_capacity(CHUNK_FRAMES * 2);
            for f in 0..CHUNK_FRAMES {
                let phase = ((k * CHUNK_FRAMES + f) % period) as f32 / period as f32;
                let s = phase * 0.5;
                data.push(s);
                data.push(s);
            }
            cache.test_set_chunk(k, data.into_boxed_slice());
        }
        cache
    }

    /// 最大 2 帧步进增量（左声道）：click = 大步跳变（无 blend 的圈界
    /// 跳变 0.5 必爆；blend 摊到 192 帧后 Δ≈0.005）。
    fn max_delta2(rec: &[f32]) -> f32 {
        let mut max_delta = 0.0f32;
        for i in 2..rec.len() {
            max_delta = max_delta.max((rec[i] - rec[i - 2]).abs());
        }
        max_delta
    }

    /// P23-B：bus 激活时 feed < li（pending）→ 线性续喂推进到 li 后
    /// 连续入环——当块生效、播头折返 [li, lo)、无欠载、6s 不出环。
    #[test]
    fn loop_pending_then_engages_at_loop_in_no_stall() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 8.0, 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256 * 60); // ≈0.32s：feed < li=1.0
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 1.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 3.0);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        // 激活后第一块（feed 仍 < li：pending，保持线性）
        let (peak, _head) = run_frames(&mut d, 256);
        assert!(peak > 0.4, "pending 期间应继续出声, peak={peak}");
        // 推进到 feed ≥ li → 连续入环（d ≈ 0..256 帧）
        let (peak, head) = run_frames(&mut d, 256 * 130); // ≈0.69s：跨过 1.0s
        assert!(peak > 0.4, "入环后应继续出声（激活即入，无 33ms 静音）, peak={peak}");
        assert!(head >= 1.0, "播头应折返进 [1,3), head={head}");
        // 6s：跨 ≥2 次圈首（wrap blend + 首圈禁用后启用）
        let (peak, head2) = run_frames(&mut d, 48000 * 6);
        assert!(peak > 0.4, "长环绕应持续出声, peak={peak}");
        assert!(
            (1.0..3.15).contains(&head2),
            "6s 后播头应在环内（首圈偏移 < 0.01s）, head={head2}"
        );
        assert!(
            d.keylocker.as_ref().unwrap().underrun_frames() == 0,
            "环绕不得欠载（缓存全覆盖）, underrun={}",
            d.keylocker.as_ref().unwrap().underrun_frames()
        );
        assert!(d.loop_ring, "应处于环绕态");
    }

    /// P23-B：feed ≥ lo 偏移入环——播头立即折返 li+d（内容跳变 +
    /// entry blend），长跑不出 [li+d, li+d+len)（旧 P22-B 显示语义）。
    #[test]
    fn loop_offset_engage_folds_playhead_no_stall() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 8.0, 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256 * 680); // ≈3.63s：feed ≥ lo=3.0
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 1.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 3.0);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        let (peak, head) = run_frames(&mut d, 256);
        assert!(peak > 0.4, "偏移入环当块应出声, peak={peak}");
        let d_frames = (d.feed_pos - 48000) % 96000; // d = (feed−li) mod len
        let expect = 1.0 + d_frames as f64 / 48000.0;
        assert!(
            (head - expect).abs() < 0.02,
            "播头应折返到 li+d={expect:.3}, head={head}"
        );
        // 长跑：显示不出 [li+d, li+d+len)（旧 P22-B 折返公式语义）
        let (peak, head2) = run_frames(&mut d, 48000 * 4);
        assert!(peak > 0.4, "长环绕应持续出声, peak={peak}");
        assert!(
            head2 >= expect - 0.05 && head2 < expect + 2.05,
            "播头应在环相位窗口内, head={head2}"
        );
        assert!(d.keylocker.as_ref().unwrap().underrun_frames() == 0);
        assert!(d.loop_ring);
    }

    /// P23-B 偏移退出续点：feed 记账 P + k×len——3.52s（660 块）偏移
    /// 入环（P = 3.515s）、2 个整圈后关环，退出线性续喂从 P+2×len。
    /// 播头终值 = 墙钟 − 引擎延迟（内容在缓存 12s 内，P 在差中消去）。
    #[test]
    fn loop_offset_exit_resumes_at_advanced_feed_pos() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 12.0, 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256 * 660); // ≈3.52s：feed ≥ lo → 偏移入环
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 1.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 3.0);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        let _ = run_frames(&mut d, 256 * 470); // ≈2.507s：圈 1 + 圈 2 中途
        assert!(d.loop_ring);
        // 偏移会计不变量：退出公式 pos_base = d（feed 记账，见
        // finish_loop_ring），显示 = pos_base + sp = d + sp。d 此刻捕获
        //（feed_pos = P + 2×len，(feed_pos−li) mod len 不随圈数变化）。
        let d_frames = (d.feed_pos - 48000) % 96000;
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 0.0);
        // 收尾圈（折叠 0.512 → 2.0，≈1.49s）→ 退出，feed 续点 = P+2×len
        let (peak, head) = run_frames(&mut d, (48000.0 * 4.5) as usize);
        assert!(peak > 0.4, "退出后应继续出声, peak={peak}");
        assert!(!d.loop_ring, "收尾圈完成后应已退出");
        // 退出后线性：播头 = d + sp（sp = 可闻标签，标签随续点跳；
        // 断言自洽——只验 pos_base = d 不变量，不依赖 sp 与 feed 的
        // 具体关系，喂入节奏变化不破坏）
        let sp = d.keylocker.as_ref().unwrap().source_position();
        assert!(
            (head * 48000.0 - (sp + d_frames as f64)).abs() < 960.0,
            "偏移退出后播头应 = 续点标签 + d, head={head}, sp={sp}, d={d_frames}"
        );
        assert!(d.keylocker.as_ref().unwrap().underrun_frames() == 0);
    }

    /// P23-B 常规退出（连续入环）：释放后播头从释放位置线性续进无跳变
    /// （旧 loop_exit_resumes_linear_without_gap 锚点；收尾圈 + 退出
    /// 全程每块增量 ≈ 256/48000）。
    #[test]
    fn loop_exit_resumes_linear_without_gap() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 8.0, 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256 * 60); // feed < li → pending
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 1.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 3.0);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        let _ = run_frames(&mut d, 256 * 470); // ≈2.5s：入环 + 跑 >1 圈
        assert!(d.loop_ring);
        let head_at_release = d.ctl.playhead.get();
        let underrun_before = d.keylocker.as_ref().unwrap().underrun_frames();
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 0.0);
        // 收尾圈（折叠 1.824 → 2.0）+ 线性 4.0s；播头每块增量 ≈ 256/48000
        //（显示 = 可闻位置，全程无缝；唯一异常 = 退出块音频 d-回跳
        //（环内容 [li+d, lo+d) 回 lo 接续：−d ≈ −0.024 ± 喂入节奏抖动
        // ≤0.006）——允许 −0.035 下界）
        let mut out = vec![0.0; 256 * 2];
        let mut prev = d.ctl.playhead.get();
        for _ in 0..(4.0 * 48000.0 / 256.0) as usize {
            d.update_params();
            d.process(&mut out, 256);
            assert!(out.iter().all(|v| v.is_finite()), "无 NaN");
            let h = d.ctl.playhead.get();
            let delta = h - prev;
            assert!(
                (-0.035..=256.0 / 48000.0 + 0.005).contains(&delta),
                "退出过程播头增量应 ≈ 一块（不跳变）, Δ={delta}, head={h}"
            );
            prev = h;
        }
        let head = d.ctl.playhead.get();
        // 终值 = 释放位置 + 4.0 − d 回跳（≈0.024）± 喂入节奏抖动（≤0.013）
        assert!(
            (head - (head_at_release + 4.0)).abs() < 0.05,
            "释放后播头应从释放位置线性续进（释放={head_at_release:.3}）, head={head}"
        );
        assert!(!d.loop_ring, "应已退出");
        assert_eq!(
            d.keylocker.as_ref().unwrap().underrun_frames(),
            underrun_before,
            "退出续喂不应产生欠载"
        );
    }

    /// P23-B：超旧上限（30s）的长环零欠载——31s 环（缓存 33s）跑
    /// 32.5s：内容恒在缓存内，无需任何捕获/回填/reset。
    #[test]
    fn loop_arbitrary_length_no_reset_path() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 33.0, 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256 * 100); // ≈0.53s
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 0.5);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 31.5);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        let (peak, head) = run_frames(&mut d, 48000 * 32); // ≈32s：跨 1 个整圈
        assert!(peak > 0.4, "31s 长环应持续出声, peak={peak}");
        assert!(
            (0.45..31.65).contains(&head),
            "长环播头应折返 [0.5, 31.5)（偏移 <0.01s）, head={head}"
        );
        assert!(
            d.keylocker.as_ref().unwrap().underrun_frames() == 0,
            "长环不得欠载（旧 30s 上限已删，缓存恒覆盖）"
        );
        assert!(d.loop_ring);
    }

    /// P23-B：圈首 wrap blend 无 click（旧锚点改写）——锯齿周期 = 环长
    /// 的 fixture，圈界跳变 0.5；无 blend 时逐采样 Δ2 必爆 0.1。
    #[test]
    fn loop_wrap_seam_blend_no_click() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache_big(&bus, saw_cache(8.0, 24000), 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        // 环 [0, 0.5)：feed=0 时激活 → 连续入环（d=0，首圈原始内容，
        // 第 2 圈起 wrap blend）
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 0.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 0.5);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        let rec = run_capture(&mut d, 1.6); // 3.2 圈：覆盖 ≥2 次 wrap 接缝
        assert!(rec.iter().all(|v| v.is_finite()), "无 NaN");
        let max_delta = max_delta2(&rec);
        assert!(
            max_delta < 0.1,
            "圈首接缝逐采样 Δ 过大（blend 未生效）: {max_delta}"
        );
        assert!(d.loop_ring);
        assert!(d.keylocker.as_ref().unwrap().underrun_frames() == 0);
    }

    /// P23-B（mixi 盲区专项）：变速下圈首接缝同样无 click——rate=0.5
    /// 时 blend 输出时长 = 192/0.5 = 384 帧，接缝内容连续性不变。
    #[test]
    fn loop_wrap_seam_no_click_at_rate_half() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache_big(&bus, saw_cache(8.0, 24000), -50.0); // rate 0.5
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 0.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 0.5);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        // rate 0.5：内容消费减半 → 4s 输出 = 2s 内容 = 4 圈
        let rec = run_capture(&mut d, 4.0);
        assert!(rec.iter().all(|v| v.is_finite()), "无 NaN");
        let max_delta = max_delta2(&rec);
        assert!(
            max_delta < 0.1,
            "rate=0.5 圈首接缝 Δ 过大（blend 未生效）: {max_delta}"
        );
        assert!(d.keylocker.as_ref().unwrap().underrun_frames() == 0);
    }

    /// P23-B：激活中 in/out 参数变化（len 变）→ min-preroll seek 重建
    /// （declick + 新环相位）——播头连续（无 NaN、不出新环、无欠载）。
    #[test]
    fn loop_len_change_rebuilds_seamless() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 8.0, 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256 * 60);
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 1.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 3.0);
        bus.set(&hypermixx_core::paths::deck_loop_active(0), 1.0);
        let _ = run_frames(&mut d, 256 * 200); // 入环 + 跑 ~1s
        assert!(d.loop_ring);
        // Out 重按 → 3.0 → 4.0（len 2s → 3s）：立即重建（新环 [1,4)）
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 4.0);
        // 2 块：min-preroll 重建块（可能含 warm priming/淡入）+ 收敛块
        let (peak, head) = run_frames(&mut d, 512);
        assert!(peak > 0.4, "重建后应出声（无静音）, peak={peak}");
        assert!((1.0..4.1).contains(&head), "重建后播头应折返进新环, head={head}");
        let (peak, head2) = run_frames(&mut d, 48000 * 4);
        assert!(peak > 0.4, "新环应持续出声, peak={peak}");
        assert!(
            (1.0..4.1).contains(&head2),
            "4s 后播头应仍在 [1,4) 内, head={head2}"
        );
        assert!(d.keylocker.as_ref().unwrap().underrun_frames() == 0);
        assert!(d.loop_ring);
    }

    // ---------- P23-B loop 量化（起点终点全部对齐 beatgrid） ----------

    /// ManualLoop 总线写入离拍 In/Out → 字段与总线都 snap 到拍线
    ///（120BPM 拍线 = 0.5s 整数倍），幂等。
    #[test]
    fn manual_loop_bus_in_out_quantized_to_grid() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 8.0, 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        let _ = run_frames(&mut d, 256);
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 1.03);
        d.update_params();
        assert!(
            (d.ctl.loop_in.get() - 1.0).abs() < 1e-9,
            "loop_in 应 snap 到 1.0, got {}",
            d.ctl.loop_in.get()
        );
        assert!(
            (bus.get(&hypermixx_core::paths::deck_loop_in(0)) - 1.0).abs() < 1e-9,
            "总线应写回对齐值"
        );
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 3.02);
        d.update_params();
        assert!(
            (d.ctl.loop_out.get() - 3.0).abs() < 1e-9,
            "loop_out 应 snap 到 3.0, got {}",
            d.ctl.loop_out.get()
        );
        assert!((bus.get(&hypermixx_core::paths::deck_loop_out(0)) - 3.0).abs() < 1e-9);
        // 幂等：已对齐值再跑一块不漂移
        let _ = run_frames(&mut d, 256);
        assert!((d.ctl.loop_in.get() - 1.0).abs() < 1e-9);
        assert!((d.ctl.loop_out.get() - 3.0).abs() < 1e-9);
    }

    /// 终点距起点不足半拍 → 保底 1 拍（P21 语义；Out 与 In 同拍时
    /// snap 后的距离会缩到 0——用原始 out 判断）。
    #[test]
    fn loop_quantize_out_half_beat_floor_one_beat() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 8.0, 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 1.0);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 1.12); // 0.12s < 半拍 0.25
        d.update_params();
        assert!(
            (d.ctl.loop_out.get() - 1.5).abs() < 1e-9,
            "不足半拍应保底 1 拍 = 1.5, got {}",
            d.ctl.loop_out.get()
        );
        assert!((d.ctl.loop_in.get() - 1.0).abs() < 1e-9, "起点应保持 1.0");
    }

    /// 无有效起点（未设 or 起点 ≥ 终点）→ 起点回拉 = 终点 − 4 拍
    ///（P21 默认拍数），snap 到拍线。
    #[test]
    fn loop_quantize_invalid_in_pull_back_4_beats() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 8.0, 0.0);
        d.ctl.grid_bpm.set(120.0);
        d.ctl.grid_offset.set(0.0);
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 3.0); // ≥ 终点 → 无效
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 2.0);
        d.update_params();
        assert!(
            (d.ctl.loop_in.get() - 0.0).abs() < 1e-9,
            "起点应回拉 = 2.0 − 4×0.5 = 0.0, got {}",
            d.ctl.loop_in.get()
        );
        assert!((d.ctl.loop_out.get() - 2.0).abs() < 1e-9, "终点应保持 2.0");
    }

    /// 无网格（bpm ≤ 0）→ 不量化，保持原始值。
    #[test]
    fn loop_quantize_no_grid_keeps_raw() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache(&bus, 8.0, 0.0);
        bus.set(&hypermixx_core::paths::deck_loop_in(0), 1.03);
        bus.set(&hypermixx_core::paths::deck_loop_out(0), 3.02);
        d.update_params();
        assert!(
            (d.ctl.loop_in.get() - 1.03).abs() < 1e-12,
            "无网格不应量化, got {}",
            d.ctl.loop_in.get()
        );
        assert!((d.ctl.loop_out.get() - 3.02).abs() < 1e-12);
    }

    // ---------- P23-B sync leader 跳变重对齐 ----------

    /// 对齐收敛后 leader 跳 2.5 拍（非整拍 → 相位 +0.5）→ follower
    /// 重新对齐（脉冲相位回到 leader 新相位；无修复时相位差 0.5 拍
    /// 永久残留）。
    #[test]
    fn sync_realigns_after_leader_jump() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache_big(&bus, pulse_cache(25.6, 24000), 0.0); // 120 BPM 脉冲
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
        let blocks = (20.0 * 48000.0 / 256.0) as usize; // 20s 全程
        let jump_at = (8.0 * 48000.0 / 256.0) as usize;
        for (b, _) in (0..blocks).enumerate() {
            if b == jump_at {
                leader.pos += 1.25; // 跳 2.5 拍 @120BPM（相位 +0.5）
            }
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(&mut out, 256);
            leader.advance();
            rec.extend_from_slice(&out);
        }
        // 跳前窗口 [4,7] 与 跳后窗口 [15,18]（跳后 7s，收敛 + 稳定）：
        // 脉冲相位 = (时间 mod 0.5s) 的中位残差
        let res_pre = pulse_residuals(&rec, 4.0, 7.0);
        let res_post = pulse_residuals(&rec, 15.0, 18.0);
        let r_pre = median(&res_pre);
        let r_post = median(&res_post);
        let shift = (r_post - r_pre).rem_euclid(0.5);
        assert!(
            (shift - 0.25).abs() < 0.015,
            "leader 跳 2.5 拍后 follower 相位应平移 0.25s（0.5 拍），实得 {shift:.4}s（pre={r_pre:.4} post={r_post:.4}）"
        );
        // 速率锁保持：间距仍 0.5s
        let times = envelope_beat_times(&rec, 15.0, 18.0);
        let sp = median_spacing(&times);
        assert!(
            (sp - 0.5).abs() < 0.003,
            "重对齐后拍间距应仍 0.5s（120 BPM），实得 {sp:.4}s"
        );
    }

    /// P14 语义保持：follower 自身操作（跳 0.5 拍 → 自身相位偏移）不
    /// 触发重对齐（leader 位置无跳变）——相位偏移永久残留。
    #[test]
    fn sync_follower_own_jump_no_realign() {
        let bus = hypermixx_core::ControlBus::default();
        let mut d = deck_with_cache_big(&bus, pulse_cache(25.6, 24000), 0.0);
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
        let blocks = (18.0 * 48000.0 / 256.0) as usize;
        let jump_at = (8.0 * 48000.0 / 256.0) as usize;
        for (b, _) in (0..blocks).enumerate() {
            if b == jump_at {
                d.beatjump(0.5); // follower 自跳 0.5 拍 = 0.25s（相位 +0.5）
            }
            d.update_params();
            d.apply_sync(&leader.snapshot());
            d.process(&mut out, 256);
            leader.advance();
            rec.extend_from_slice(&out);
        }
        let r_pre = median(&pulse_residuals(&rec, 4.0, 7.0));
        let r_post = median(&pulse_residuals(&rec, 14.0, 17.0));
        let shift = (r_post - r_pre).rem_euclid(0.5);
        assert!(
            (shift - 0.25).abs() < 0.015,
            "follower 自跳后相位偏移应保留（leader 无跳变 → 不重对齐），实得 {shift:.4}s（pre={r_pre:.4} post={r_post:.4}）"
        );
    }

    /// 脉冲时刻 mod 0.5s 的中位残差（相位测量；latency 常数在差中消去）。
    fn pulse_residuals(rec: &[f32], from: f64, to: f64) -> Vec<f64> {
        envelope_beat_times(rec, from, to)
            .iter()
            .map(|&t| t.rem_euclid(0.5))
            .collect()
    }

    fn median(v: &[f64]) -> f64 {
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s[s.len() / 2]
    }
}
