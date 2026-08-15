//! FRB 注解的对外接口——flutter_rust_bridge_codegen 的输入模块。
//!
//! 规则：
//! - 60Hz 热路径全部 `#[frb(sync)]`（Dart 侧同步调用，微秒级、零分配）；
//! - 慢动作（载曲、读元数据）保持普通 async fn（Dart 侧 Future，FRB 线程池执行）；
//! - 分析事件走参数位置 StreamSink（Dart 侧生成 `Stream`，不 await）；
//! - 所有函数不 panic（panic 跨 FFI 会崩掉整个 Flutter 进程）：
//!   桥未初始化时返回默认值/空事件。

use flutter_rust_bridge::frb;

use crate::bridge;
// codegen 在 frb_generated 模块内生成 StreamSink（默认 SseCodec）
use crate::frb_generated::StreamSink;

/// 冒烟回路：桥库已加载的证明。
#[frb(sync)]
pub fn ping() -> u64 {
    42
}

/// 启动引擎（CpalBackend::new + Engine::start）。幂等。
/// 失败（无音频设备等）返回错误串，Flutter 顶部红条显示；之后才可载曲/取快照。
#[frb(sync)]
pub fn init_engine() -> Result<(), String> {
    bridge::init_engine().map_err(|e| format!("{e:#}"))
}

/// 载曲到 deck 并启动渐进分析，事件推入 `sink`
/// （Segment → TrackAnalysis → Done/Failed，每条带 generation）。
/// 重复调用 = 重载：旧分析线程停掉、旧事件按代际作废。
/// 引擎语义：载入即自动开播。
pub fn load_track(deck: u32, path: String, sink: StreamSink<AnalysisEventWire>) {
    bridge::load_track(deck, path, sink);
}

/// 跳到指定秒。
#[frb(sync)]
pub fn seek(deck: u32, seconds: f64) {
    bridge::seek(deck, seconds);
}

/// 精确跳转（不量化；cue/hotcue 召回用）。
#[frb(sync)]
pub fn seek_exact(deck: u32, seconds: f64) {
    bridge::seek_exact(deck, seconds);
}

/// 按拍跳跃（简单加减，拍长匹配当前速度）。
#[frb(sync)]
pub fn beatjump(deck: u32, beats: f64) {
    bridge::beatjump(deck, beats);
}

/// 激活/调整 beat loop（量化起止；取消由 UI 写 loop_active=0）。
#[frb(sync)]
pub fn set_beat_loop(deck: u32, beats: f64) {
    bridge::set_beat_loop(deck, beats);
}

/// 写控制总线（音量 / rate / sync / keylock / zoom 等任意 paths）。
#[frb(sync)]
pub fn bus_set(path: String, value: f64) {
    bridge::bus_set(&path, value);
}

#[frb(sync)]
pub fn bus_get(path: String) -> f64 {
    bridge::bus_get(&path)
}

/// 60Hz 全量快照：全 f64、零字符串、零分配，Dart 侧单次同步调用。
/// 桥未初始化时返回全零结构。
#[frb(sync)]
pub fn snapshot_all() -> AllSnapshotWire {
    bridge::snapshot_all()
}

/// FX 效果静态清单（manifest 是 Flutter 生成 FX 面板参数的唯一入口）。
/// 启动时调用一次并缓存；效果/参数顺序与 EffectId 判别值一致。
#[frb(sync)]
pub fn fx_manifests() -> Vec<FxEffectWire> {
    bridge::fx_manifests()
}

/// 播放头所在分析段（UI 每 tick 更新，分析线程按距其远近排序）。
#[frb(sync)]
pub fn set_analysis_priority(deck: u32, priority: u32) {
    bridge::set_analysis_priority(deck, priority);
}

/// 读曲目元数据（title/artist/封面，lofty）。几十 ms，载曲动作用。
pub fn read_metadata(path: String) -> Result<TrackMetadataWire, String> {
    bridge::read_metadata(&path).map_err(|e| format!("{e:#}"))
}

/// 打开系统文件选择对话框（rfd/XDG portal，复刻 Slint 加载按钮）。
/// 阻塞 UI 线程直到选择/取消；取消返回 None。
#[frb(sync)]
pub fn pick_audio_file() -> Option<String> {
    bridge::pick_audio_file()
}

// ---------------------------------------------------------------------------
// wire 类型（FRB 直译：纯数据、无引用、无 usize——u64 会变 Dart BigInt，
// 热路径字段一律 u32/f64/字符串）
// ---------------------------------------------------------------------------

/// 单 deck 快照（60Hz）。字段与总线 paths 一一对应。
#[derive(Clone, Copy, Debug, Default)]
pub struct DeckSnapshotWire {
    pub playhead: f64,
    pub duration: f64,
    pub loaded: f64,
    pub playing: f64,
    pub vu: f64,
    pub rate: f64,
    pub volume: f64,
    pub bpm: f64,
    pub grid_bpm: f64,
    pub keylock: f64,
    pub sync: f64,
    /// beat loop 状态（active 0/1；in/out 秒，未激活时为 0）。
    pub loop_active: f64,
    pub loop_in: f64,
    pub loop_out: f64,
    /// EQ 三带增益（dB，-40..+6，0 = 直通）。
    pub eq_low: f64,
    pub eq_mid: f64,
    pub eq_high: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MasterSnapshotWire {
    pub volume: f64,
    pub vu: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AllSnapshotWire {
    pub deck0: DeckSnapshotWire,
    pub deck1: DeckSnapshotWire,
    pub master: MasterSnapshotWire,
}

/// 波形单列 8 值：`*_p` = 正峰、`*_n` = 负半折叠幅度（|min|），全部 ≥0。
/// 半波预览 = p 全 α 矩形 + n α0.55 矩形（折叠整流）；对称波形 = max(p, n)。
#[derive(Clone, Copy, Debug, Default)]
pub struct WireColumn {
    pub low_p: u8,
    pub low_n: u8,
    pub mid_p: u8,
    pub mid_n: u8,
    pub high_p: u8,
    pub high_n: u8,
    pub all_p: u8,
    pub all_n: u8,
}

/// 渐进分析事件流（与分析 crate 的 AnalysisEvent 一一对应）。
#[derive(Debug, Clone)]
pub enum AnalysisEventWire {
    /// 一段分析完成（满刻度 √ 压缩显示值；全曲完成后由 Done 替换）。
    Segment {
        generation: u64,
        seg: u32,
        detail: Vec<WireColumn>,
        overview: Vec<WireColumn>,
    },
    /// 单遍 BPM/调性/beatgrid 结果，Done 之前发出。bpm=0 = 未检测到。
    TrackAnalysis {
        generation: u64,
        bpm: f64,
        key_name: String,
        key_camelot: String,
        /// 首拍秒偏移（grid 为空时为 0）。
        offset_secs: f64,
        beats_secs: Vec<f64>,
        downbeats_secs: Vec<f64>,
        confidence: f32,
    },
    /// 全曲分析完成：全局归一化数据（渐进分段由它整体替换）。
    Done {
        generation: u64,
        detail: Vec<WireColumn>,
        overview: Vec<WireColumn>,
        frames_per_col: u32,
        sample_rate: u32,
        duration_frames: u64,
    },
    Failed {
        generation: u64,
        msg: String,
    },
}

/// 曲目元数据。封面原始字节（FLAC 内嵌图由 lofty 解出）。
#[derive(Debug, Clone, Default)]
pub struct TrackMetadataWire {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub cover: Vec<u8>,
    /// 如 `image/jpeg`；无封面时为空串。
    pub cover_mime: String,
}

/// FX 单参数规格（manifest 参数，自然单位）。
/// `kind_stepped=false` → 连续滑杆；`true` → 离散步进（Slider divisions）。
/// 字段名避开 Dart 关键字 `default`（FRB 会把 `default` 生成冲突的 static 方法）。
#[derive(Debug, Clone, Default)]
pub struct FxParamWire {
    pub name: String,
    pub label: String,
    pub unit: String,
    pub kind_stepped: bool,
    pub kind_min: f64,
    pub kind_max: f64,
    pub kind_step: f64,
    pub default_value: f64,
}

/// 单效果清单（id = EffectId 判别值 1..=8；params 位对应总线 fxN_p1..p4）。
#[derive(Debug, Clone, Default)]
pub struct FxEffectWire {
    pub id: u32,
    pub name: String,
    pub label: String,
    pub params: Vec<FxParamWire>,
}
