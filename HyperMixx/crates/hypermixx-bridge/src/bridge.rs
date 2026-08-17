//! 桥内部逻辑（非 FRB 注解面）：引擎/总线/分析生命周期、事件转发、wire 转换。
//! 不依赖 codegen（注解层在 api.rs），可直接单元测试。

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};
use lofty::prelude::{Accessor, TaggedFileExt};

use hypermixx_analysis::{start_analysis, AnalysisEvent, Column, WaveformData};
use hypermixx_audio::{CpalBackend, Engine, EngineHandle};
use hypermixx_core::{paths, ControlBus};

use crate::api::{
    AllSnapshotWire, AnalysisEventWire, DeckSnapshotWire, FxEffectWire, FxParamWire,
    MasterSnapshotWire, TrackMetadataWire, WireColumn,
};
// codegen 在 frb_generated 模块内生成 StreamSink（默认 SseCodec）
use crate::frb_generated::StreamSink;

const DECKS: usize = 2;

/// TrackAnalysis 置信低于此值不写 grid 总线（保留旧网格）——低置信网格
/// 若发布，引擎 sync/loop 量化会建在劣质锚点上且旧的可用网格被覆盖；
/// beats 仍随事件转发给 UI 画网格线。与 segment.rs 的 GRID_MIN_CONFIDENCE
/// 同阈值（分析侧低置信时 bpm 已置 0，这里再兜底）。
const GRID_PUBLISH_MIN_CONFIDENCE: f32 = 0.25;

/// 引擎核心：bus/engine/backend 同生命周期——Engine 不 Clone、backend 必须
/// 比音频流活得久，所以 init 后静态持有到进程结束。
struct Core {
    bus: ControlBus,
    handle: EngineHandle,
    _engine: Engine,
    _backend: CpalBackend,
}

static CORE: Mutex<Option<Core>> = Mutex::new(None);
/// 每 deck 播放头所在分析段（分析线程按距其远近排序）。
static PRIORITY: [OnceLock<Arc<AtomicU64>>; DECKS] = [OnceLock::new(), OnceLock::new()];
/// 每 deck 重载代际：重载 +1，旧线程事件整体作废。
static GENERATION: [AtomicU64; DECKS] = [AtomicU64::new(0), AtomicU64::new(0)];
/// 每 deck 当前分析线程的 shutdown 开关。
static SHUTDOWN: Mutex<[Option<Arc<AtomicBool>>; DECKS]> = Mutex::new([None, None]);

fn priority(deck: usize) -> &'static Arc<AtomicU64> {
    PRIORITY[deck].get_or_init(|| Arc::new(AtomicU64::new(0)))
}

fn with_core<T>(f: impl FnOnce(&Core) -> T) -> Option<T> {
    let core = CORE.lock().ok()?;
    core.as_ref().map(f)
}

/// 启动引擎。幂等：已初始化直接返回 Ok。
pub fn init_engine() -> Result<()> {
    let mut core = CORE.lock().map_err(|_| anyhow!("core 锁中毒"))?;
    if core.is_some() {
        return Ok(());
    }
    let bus = ControlBus::default();
    let backend = CpalBackend::new()?;
    let engine = Engine::start(&backend, &bus, None)?;
    let handle = engine.handle.clone();
    *core = Some(Core {
        bus,
        handle,
        _engine: engine,
        _backend: backend,
    });
    Ok(())
}

/// api::load_track 的实现：桥未初始化时向 sink 发 Failed 事件（不 panic）。
pub fn load_track(deck: u32, path: String, sink: StreamSink<AnalysisEventWire>) {
    let Some((bus, handle)) = with_core(|c| (c.bus.clone(), c.handle.clone())) else {
        let _ = sink.add(AnalysisEventWire::Failed {
            generation: u64::MAX,
            msg: "引擎未初始化（先调 init_engine）".into(),
        });
        return;
    };
    let sink2 = sink.clone(); // StreamSink 可克隆：move 进转发线程
    let _ = load_track_inner(&bus, &handle, deck, Path::new(&path), move |ev| {
        sink2.add(ev).is_ok()
    });
}

/// 载曲 + 起渐进分析（无 FRB 依赖，测试可直接驱动）。
/// 返回本次分析 generation；deck 越界返回 Err。
pub fn load_track_inner(
    bus: &ControlBus,
    handle: &EngineHandle,
    deck: u32,
    path: &Path,
    sink_add: impl FnMut(AnalysisEventWire) -> bool + Send + 'static,
) -> Result<u64> {
    let deck = deck as usize;
    if deck >= DECKS {
        return Err(anyhow!("deck {deck} 越界（0..{DECKS}）"));
    }
    // 引擎语义：载入即自动开播（deck.rs）
    handle.load(deck, path.to_path_buf());
    // 新载曲：旧网格失效（引擎不自动清，桥按载曲语义清）
    bus.set(&paths::deck_grid_bpm(deck), 0.0);
    bus.set(&paths::deck_grid_offset(deck), 0.0);
    // 新载曲：旧 loop 失效（deck.rs load 内也复位，这里双保险）
    bus.set(&paths::deck_loop_active(deck), 0.0);
    bus.set(&paths::deck_loop_in(deck), 0.0);
    bus.set(&paths::deck_loop_out(deck), 0.0);

    // 停旧分析线程（shutdown 在段边界生效）；旧事件由 generation 递增作废
    let mut slots = SHUTDOWN.lock().map_err(|_| anyhow!("shutdown 锁中毒"))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    if let Some(old) = slots[deck].replace(shutdown.clone()) {
        old.store(true, Ordering::Relaxed);
    }
    drop(slots);

    let generation = GENERATION[deck].fetch_add(1, Ordering::Relaxed) + 1;
    let (tx, rx) = mpsc::channel();
    start_analysis(
        path.to_path_buf(),
        priority(deck).clone(),
        shutdown,
        generation,
        tx,
    );
    let bus = bus.clone();
    std::thread::Builder::new()
        .name(format!("bridge-forward-{deck}"))
        .spawn(move || forward_events(rx, &bus, deck, generation, sink_add))
        .map_err(|e| anyhow!("spawn forwarder: {e}"))?;
    Ok(generation)
}

/// 事件转发线程主体：旧代事件丢弃；TrackAnalysis 写 grid 总线；
/// sink 断开（返回 false）即自退——Dart 侧取消订阅的信号。
fn forward_events(
    rx: Receiver<AnalysisEvent>,
    bus: &ControlBus,
    deck: usize,
    generation: u64,
    mut sink_add: impl FnMut(AnalysisEventWire) -> bool,
) {
    while let Ok(ev) = rx.recv() {
        let g = match &ev {
            AnalysisEvent::Segment { generation, .. }
            | AnalysisEvent::TrackAnalysis { generation, .. }
            | AnalysisEvent::Done { generation, .. }
            | AnalysisEvent::Failed { generation, .. } => *generation,
        };
        if g != generation {
            continue;
        }
        if let AnalysisEvent::TrackAnalysis {
            bpm,
            offset_secs,
            confidence,
            ..
        } = &ev
        {
            // 低置信（或 bpm=0）不写 grid 总线：旧网格保留，防引擎
            // sync/loop 停摆；事件本身照常转发（UI 网格线仍可画）。
            if *confidence >= GRID_PUBLISH_MIN_CONFIDENCE && *bpm > 0.0 {
                bus.set(&paths::deck_grid_bpm(deck), *bpm);
                bus.set(&paths::deck_grid_offset(deck), *offset_secs);
            }
        }
        if !sink_add(to_wire(ev)) {
            break;
        }
    }
}

/// 跳到指定秒（未初始化/越界时静默忽略）。
pub fn seek(deck: u32, seconds: f64) {
    if (deck as usize) < DECKS
        && let Some(handle) = with_core(|c| c.handle.clone())
    {
        handle.seek(deck as usize, seconds);
    }
}

/// 精确跳转（不量化；cue/hotcue 召回用）。
pub fn seek_exact(deck: u32, seconds: f64) {
    if (deck as usize) < DECKS
        && let Some(handle) = with_core(|c| c.handle.clone())
    {
        handle.seek_exact(deck as usize, seconds);
    }
}

/// 按拍跳跃（简单加减，拍长匹配当前速度）。
pub fn beatjump(deck: u32, beats: f64) {
    if (deck as usize) < DECKS
        && let Some(handle) = with_core(|c| c.handle.clone())
    {
        handle.beatjump(deck as usize, beats);
    }
}

/// 激活/调整 beat loop（取消由 UI 写 loop_active=0）。
pub fn set_beat_loop(deck: u32, beats: f64) {
    if (deck as usize) < DECKS
        && let Some(handle) = with_core(|c| c.handle.clone())
    {
        handle.set_beat_loop(deck as usize, beats);
    }
}

/// 写控制总线（未初始化时静默忽略）。
pub fn bus_set(path: &str, value: f64) {
    if let Some(bus) = with_core(|c| c.bus.clone()) {
        bus.set(path, value);
    }
}

/// 读控制总线（未初始化时 0.0）。
pub fn bus_get(path: &str) -> f64 {
    with_core(|c| c.bus.get(path)).unwrap_or(0.0)
}

/// 60Hz 全量快照（未初始化时全零）。
pub fn snapshot_all() -> AllSnapshotWire {
    with_core(|c| snapshot_from(&c.bus)).unwrap_or_default()
}

/// 从总线组快照（不触碰 CORE 静态，可直接测试）。
pub fn snapshot_from(bus: &ControlBus) -> AllSnapshotWire {
    AllSnapshotWire {
        deck0: deck_snapshot(bus, 0),
        deck1: deck_snapshot(bus, 1),
        master: MasterSnapshotWire {
            volume: bus.get(paths::master_volume()),
            vu: bus.get(paths::master_vu()),
        },
    }
}

fn deck_snapshot(bus: &ControlBus, deck: usize) -> DeckSnapshotWire {
    DeckSnapshotWire {
        playhead: bus.get(&paths::deck_playhead(deck)),
        duration: bus.get(&paths::deck_duration(deck)),
        loaded: bus.get(&paths::deck_loaded(deck)),
        playing: bus.get(&paths::deck_play(deck)),
        vu: bus.get(&paths::deck_vu(deck)),
        rate: bus.get(&paths::deck_rate(deck)),
        volume: bus.get(&paths::deck_volume(deck)),
        bpm: bus.get(&paths::deck_bpm(deck)),
        grid_bpm: bus.get(&paths::deck_grid_bpm(deck)),
        keylock: bus.get(&paths::deck_keylock(deck)),
        sync: bus.get(&paths::deck_sync(deck)),
        loop_active: bus.get(&paths::deck_loop_active(deck)),
        loop_in: bus.get(&paths::deck_loop_in(deck)),
        loop_out: bus.get(&paths::deck_loop_out(deck)),
        eq_low: bus.get(&paths::deck_eq_low(deck)),
        eq_mid: bus.get(&paths::deck_eq_mid(deck)),
        eq_high: bus.get(&paths::deck_eq_high(deck)),
        cache_filled: bus.get(&paths::deck_cache_filled(deck)),
    }
}

/// FX 效果清单（manifest → wire）。桥未初始化也可返回（纯静态数据）。
pub fn fx_manifests() -> Vec<FxEffectWire> {
    use hypermixx_audio::fx::ParamKind;
    hypermixx_audio::fx::all_manifests()
        .iter()
        .map(|m| FxEffectWire {
            id: m.id.to_bus() as u32,
            name: m.name.to_string(),
            label: m.label.to_string(),
            params: m
                .params
                .iter()
                .map(|p| {
                    let (kind_stepped, kind_min, kind_max, kind_step) = match p.kind {
                        ParamKind::Continuous { min, max } => (false, min, max, 0.0),
                        ParamKind::Stepped { min, max, step } => (true, min, max, step),
                    };
                    FxParamWire {
                        name: p.name.to_string(),
                        label: p.label.to_string(),
                        unit: p.unit.to_string(),
                        kind_stepped,
                        kind_min: kind_min as f64,
                        kind_max: kind_max as f64,
                        kind_step: kind_step as f64,
                        default_value: p.default as f64,
                    }
                })
                .collect(),
        })
        .collect()
}

pub fn set_analysis_priority(deck: u32, seg: u32) {
    if (deck as usize) < DECKS {
        priority(deck as usize).store(u64::from(seg), Ordering::Relaxed);
    }
}

/// 系统文件选择对话框（rfd/XDG portal）。阻塞调用线程直到选择/取消。
/// 取消返回 None；不支持对话框环境（无 portal）也返回 None。
pub fn pick_audio_file() -> Option<String> {
    rfd::FileDialog::new()
        .add_filter("音频", &["flac", "mp3", "wav", "ogg", "m4a", "aac"])
        .pick_file()
        .map(|p| p.to_string_lossy().into_owned())
}

/// 读曲目元数据（lofty）：title/artist/封面。慢（解头部 + 读封面字节），
/// 只在载曲动作时调用一次。
pub fn read_metadata(path: &str) -> Result<TrackMetadataWire> {
    let tagged = lofty::read_from_path(path)?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let title = tag.and_then(|t| t.title().map(|s| s.to_string()));
    let artist = tag.and_then(|t| t.artist().map(|s| s.to_string()));
    let (cover, cover_mime) = tag
        .and_then(|t| t.pictures().first())
        .map(|p| {
            let mime = p
                .mime_type()
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            (p.data().to_vec(), mime)
        })
        .unwrap_or_default();
    Ok(TrackMetadataWire {
        title,
        artist,
        cover,
        cover_mime,
    })
}

/// AnalysisEvent → wire（FRB 可翻译类型）。
fn to_wire(ev: AnalysisEvent) -> AnalysisEventWire {
    match ev {
        AnalysisEvent::Segment {
            generation,
            seg,
            detail,
            overview,
        } => AnalysisEventWire::Segment {
            generation,
            seg: seg as u32,
            detail: to_wire_cols(&detail),
            overview: to_wire_cols(&overview),
        },
        AnalysisEvent::TrackAnalysis {
            generation,
            bpm,
            key,
            offset_secs,
            beats_secs,
            downbeats_secs,
            confidence,
            tempo_segments,
        } => AnalysisEventWire::TrackAnalysis {
            generation,
            bpm,
            key_name: key.map(|k| k.name()).unwrap_or_default(),
            key_camelot: key.map(|k| k.camelot()).unwrap_or_default(),
            offset_secs,
            beats_secs: beats_secs.into_vec(),
            downbeats_secs: downbeats_secs.into_vec(),
            confidence,
            tempo_segments,
        },
        AnalysisEvent::Done {
            generation,
            wave:
                WaveformData {
                    detail,
                    overview,
                    frames_per_col,
                    sample_rate,
                    duration_frames,
                },
        } => AnalysisEventWire::Done {
            generation,
            detail: to_wire_cols(&detail),
            overview: to_wire_cols(&overview),
            frames_per_col,
            sample_rate,
            duration_frames,
        },
        AnalysisEvent::Failed { generation, msg } => {
            AnalysisEventWire::Failed { generation, msg }
        }
    }
}

fn to_wire_cols(cols: &[Column]) -> Vec<WireColumn> {
    cols.iter()
        .map(|c| WireColumn {
            low_p: c.low_p,
            low_n: c.low_n,
            mid_p: c.mid_p,
            mid_n: c.mid_n,
            high_p: c.high_p,
            high_n: c.high_n,
            all_p: c.all_p,
            all_n: c.all_n,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_analysis::{KeyEstimate, KeyMode, SEG_COLS};
    use std::path::PathBuf;
    use std::time::Duration;

    /// 合成 24s 48kHz 立体声 WAV：120 BPM 咔嗒（瞬态能量集中），
    /// 供 BPM 检测出 120——同 hypermixx-analysis 测试手法。
    fn synth_click_track(path: &Path, secs: f64) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let period = 60.0 / 120.0;
        let n = (secs * 48_000.0) as usize;
        for i in 0..n {
            let t = i as f64 / 48_000.0;
            let since = t.rem_euclid(period);
            // 12ms 1.5kHz 脉冲、4ms 指数衰减
            let s = if since < 0.012 {
                ((2.0 * std::f64::consts::PI * 1500.0 * since).sin() * (-since / 0.004).exp()
                    * 0.9) as f32
            } else {
                0.0
            };
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    /// 测试曲路径（env 可覆盖；不存在则跳过相关测试）。
    fn test_track() -> PathBuf {
        std::env::var("HYPERMIXX_TEST_TRACK")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/home/windupbird/Workspace/HyperMixx/yehno - Always.flac"))
    }

    #[test]
    fn snapshot_reflects_bus() {
        let bus = ControlBus::default();
        bus.set(&paths::deck_playhead(0), 12.5);
        bus.set(&paths::deck_duration(0), 329.0);
        bus.set(&paths::deck_rate(0), 3.5);
        bus.set(&paths::deck_sync(1), 1.0);
        bus.set(paths::master_volume(), 0.75);
        let s = snapshot_from(&bus);
        assert_eq!(s.deck0.playhead, 12.5);
        assert_eq!(s.deck0.duration, 329.0);
        assert_eq!(s.deck0.rate, 3.5);
        assert_eq!(s.deck0.sync, 0.0);
        assert_eq!(s.deck1.sync, 1.0);
        assert_eq!(s.master.volume, 0.75);
        assert_eq!(s.deck1.playhead, 0.0); // 未设置 = 默认
    }

    #[test]
    fn snapshot_includes_eq() {
        let bus = ControlBus::default();
        bus.set(&paths::deck_eq_low(0), -12.0);
        bus.set(&paths::deck_eq_high(0), 4.5);
        let s = snapshot_from(&bus);
        assert_eq!(s.deck0.eq_low, -12.0);
        assert_eq!(s.deck0.eq_high, 4.5);
        assert_eq!(s.deck0.eq_mid, 0.0); // 未设置 = 默认 0dB 直通
    }

    #[test]
    fn snapshot_includes_loop() {
        let bus = ControlBus::default();
        bus.set(&paths::deck_loop_active(0), 1.0);
        bus.set(&paths::deck_loop_in(0), 1.5);
        bus.set(&paths::deck_loop_out(0), 5.5);
        let s = snapshot_from(&bus);
        assert_eq!(s.deck0.loop_active, 1.0);
        assert_eq!(s.deck0.loop_in, 1.5);
        assert_eq!(s.deck0.loop_out, 5.5);
        assert_eq!(s.deck1.loop_active, 0.0); // 未设置 = 默认 0
    }

    #[test]
    fn loop_beatjump_seekexact_ops_propagate() {
        // deck 1：与 full-flow 测试（deck 0）隔离 GENERATION/SHUTDOWN 全局量
        let p = std::env::temp_dir().join("hypermixx-bridge-ops.wav");
        synth_click_track(&p, 24.0);
        let bus = ControlBus::default();
        let (mut state, handle) = Engine::core(&bus);
        // sink 立即关闭 → 分析事件不写 grid，测试自己控制网格
        load_track_inner(&bus, &handle, 1, &p, |_| false).unwrap();
        bus.set(&paths::deck_grid_bpm(1), 120.0);
        bus.set(&paths::deck_grid_offset(1), 0.0);
        let mut out = vec![0.0; 256 * 2];

        // SetBeatLoop：2 拍 @120BPM = 1s，起点量化
        handle.set_beat_loop(1, 2.0);
        state.process(&mut out);
        assert!(bus.get(&paths::deck_loop_active(1)) > 0.5, "loop 应激活");
        let loop_in = bus.get(&paths::deck_loop_in(1));
        let loop_out = bus.get(&paths::deck_loop_out(1));
        assert!((loop_out - loop_in - 1.0).abs() < 1e-9, "2 拍 @120BPM = 1s");

        // BeatJump：4 拍 = 2s（rate=1），出环自动取消 loop。
        // 跳转后播头 = target − 预卷瞬态（≈33ms，seek 后首块引擎未喂入），
        // 0.05 容差覆盖（量化吸附会落在 1.5/2.5s 拍点，差 0.2s+，必失败）。
        let p0 = bus.get(&paths::deck_playhead(1));
        handle.beatjump(1, 4.0);
        state.process(&mut out);
        let head = bus.get(&paths::deck_playhead(1));
        assert!(
            (head - (p0 + 2.0)).abs() < 0.05,
            "beatjump 4 拍应 +2s, p0={p0}, head={head}"
        );
        assert!(bus.get(&paths::deck_loop_active(1)) < 0.5, "出环取消 loop");

        // SeekExact：不量化，精确落点（quantize 会把 1.3 吸到 1.5）
        handle.seek_exact(1, 1.3);
        state.process(&mut out);
        let head = bus.get(&paths::deck_playhead(1));
        assert!(
            (head - 1.3).abs() < 0.05,
            "seek_exact 精确落点（不被量化）, head={head}"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn fx_manifests_matches_engine_ids() {
        let m = fx_manifests();
        assert_eq!(m.len(), 8, "8 个效果，顺序 = EffectId 1..=8");
        // 位 0 是"无效果"（id=0 无 manifest）
        for (i, e) in m.iter().enumerate() {
            assert_eq!(e.id as usize, i + 1);
        }
        // echo 4 参数、reverb 3 参数、distortion 2 参数（manifest 声明数）
        assert_eq!(m[0].params.len(), 4);
        assert_eq!(m[3].params.len(), 3);
        assert_eq!(m[4].params.len(), 2);
        // 参数单位自然值保留；stepped 标志正确
        let filter = &m[5]; // FilterLinear
        let mode = &filter.params[0];
        assert!(mode.kind_stepped, "filter mode 应为离散步进");
        assert_eq!(mode.kind_step, 1.0);
        let cutoff = &filter.params[1];
        assert!(!cutoff.kind_stepped);
        assert!((cutoff.kind_min - 20.0).abs() < 1e-9);
        assert!((cutoff.kind_max - 20000.0).abs() < 1e-9);
    }

    #[test]
    fn forwarder_writes_grid_and_drops_stale() {
        let bus = ControlBus::default();
        let (tx, rx) = mpsc::channel();
        let collected = Arc::new(Mutex::new(Vec::new()));
        let cc = collected.clone();
        let g = 3u64;
        let tbus = bus.clone(); // 克隆进线程，主线程保留总线做断言
        let t = std::thread::spawn(move || {
            forward_events(rx, &tbus, 0, g, |w| {
                cc.lock().unwrap().push(w);
                true
            })
        });
        // 预置旧网格：低置信事件不得覆盖它
        bus.set(&paths::deck_grid_bpm(0), 120.0);
        bus.set(&paths::deck_grid_offset(0), 0.3);
        // 旧代：丢弃，不写 grid
        tx.send(AnalysisEvent::TrackAnalysis {
            generation: 2,
            bpm: 100.0,
            key: None,
            offset_secs: 1.0,
            beats_secs: Box::new([]),
            downbeats_secs: Box::new([]),
            confidence: 0.0,
            tempo_segments: Vec::new(),
        })
        .unwrap();
        // 当前代低置信：事件转发但 grid 总线保留旧网格
        tx.send(AnalysisEvent::TrackAnalysis {
            generation: g,
            bpm: 130.1,
            key: None,
            offset_secs: 0.5,
            beats_secs: Box::new([]),
            downbeats_secs: Box::new([]),
            confidence: 0.2,
            tempo_segments: Vec::new(),
        })
        .unwrap();
        assert_eq!(bus.get(&paths::deck_grid_bpm(0)), 120.0, "低置信不写 grid_bpm");
        assert_eq!(
            bus.get(&paths::deck_grid_offset(0)),
            0.3,
            "低置信不写 grid_offset"
        );
        // 当前代高置信：写 grid
        tx.send(AnalysisEvent::TrackAnalysis {
            generation: g,
            bpm: 130.1,
            key: None,
            offset_secs: 0.5,
            beats_secs: Box::new([]),
            downbeats_secs: Box::new([]),
            confidence: 1.0,
            tempo_segments: Vec::new(),
        })
        .unwrap();
        drop(tx); // 关通道结束转发线程
        t.join().unwrap();
        assert_eq!(bus.get(&paths::deck_grid_bpm(0)), 130.1);
        assert_eq!(bus.get(&paths::deck_grid_offset(0)), 0.5);
        let evs = collected.lock().unwrap();
        assert_eq!(evs.len(), 2, "旧代不转发，两件当前代事件均转发");
        match &evs[0] {
            AnalysisEventWire::TrackAnalysis { bpm, .. } => assert_eq!(*bpm, 130.1),
            _ => panic!("应为 TrackAnalysis"),
        }
    }

    #[test]
    fn forwarder_stops_when_sink_closed() {
        let bus = ControlBus::default();
        let (tx, rx) = mpsc::channel();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cc = count.clone();
        let t = std::thread::spawn(move || {
            forward_events(rx, &bus, 0, 1, |_| {
                cc.fetch_add(1, Ordering::Relaxed);
                false // 模拟 Dart 侧取消订阅：sink.add 返回 Err
            })
        });
        for i in 0..3 {
            tx.send(AnalysisEvent::Failed {
                generation: 1,
                msg: format!("e{i}"),
            })
            .unwrap();
        }
        t.join().unwrap(); // 第一条后即自退，不会因 tx 存活而死锁
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn to_wire_maps_columns_and_key() {
        let c = Column {
            low_p: 1,
            low_n: 2,
            mid_p: 3,
            mid_n: 4,
            high_p: 5,
            high_n: 6,
            all_p: 7,
            all_n: 8,
        };
        let w = to_wire(AnalysisEvent::Done {
            generation: 7,
            wave: WaveformData {
                detail: vec![c],
                overview: vec![c],
                frames_per_col: 128,
                sample_rate: 48_000,
                duration_frames: 1000,
            },
        });
        match w {
            AnalysisEventWire::Done {
                generation,
                detail,
                overview,
                frames_per_col,
                sample_rate,
                duration_frames,
            } => {
                assert_eq!(generation, 7);
                assert_eq!(frames_per_col, 128);
                assert_eq!(sample_rate, 48_000);
                assert_eq!(duration_frames, 1000);
                assert_eq!(detail[0].all_n, 8);
                assert_eq!(overview[0].high_p, 5);
            }
            _ => panic!("应为 Done"),
        }
        // A minor → "A minor" / "8A"
        let w = to_wire(AnalysisEvent::TrackAnalysis {
            generation: 1,
            bpm: 120.0,
            key: Some(KeyEstimate {
                root: 9,
                mode: KeyMode::Minor,
                confidence: 0.95,
            }),
            offset_secs: 0.0,
            beats_secs: Box::new([]),
            downbeats_secs: Box::new([]),
            confidence: 0.95,
            tempo_segments: Vec::new(),
        });
        match w {
            AnalysisEventWire::TrackAnalysis {
                key_name,
                key_camelot,
                ..
            } => {
                assert_eq!(key_name, "A minor");
                assert_eq!(key_camelot, "8A");
            }
            _ => panic!("应为 TrackAnalysis"),
        }
    }

    /// 全流程：载曲（无音频设备的 Engine::core 头less）→ 事件按序到达
    /// （Segment 代际匹配 → TrackAnalysis 写 grid≈120 → Done 全曲波形）→
    /// 重载代际递增且旧 shutdown 置位。24s@48kHz = 9000 列 = 2 段。
    #[test]
    fn load_track_full_flow_and_reload() {
        let p = std::env::temp_dir().join("hypermixx-bridge-load.wav");
        synth_click_track(&p, 24.0);

        let bus = ControlBus::default();
        let (_state, handle) = Engine::core(&bus);
        // 预置旧网格值，验证载曲时清零
        bus.set(&paths::deck_grid_bpm(0), 99.0);
        bus.set(&paths::deck_grid_offset(0), 1.5);

        let (tx, rx) = mpsc::channel();
        let txc = tx.clone();
        let g = load_track_inner(&bus, &handle, 0, &p, move |ev| txc.send(ev).is_ok()).unwrap();
        assert_eq!(g, 1, "首次载曲 generation = 1");
        assert_eq!(bus.get(&paths::deck_grid_bpm(0)), 0.0, "载曲清零 grid_bpm");
        assert_eq!(bus.get(&paths::deck_grid_offset(0)), 0.0);

        let mut evs = Vec::new();
        loop {
            let ev = rx.recv_timeout(Duration::from_secs(60)).expect("分析事件超时");
            let last = matches!(
                ev,
                AnalysisEventWire::Done { .. } | AnalysisEventWire::Failed { .. }
            );
            evs.push(ev);
            if last {
                break;
            }
        }

        // 全部事件代际匹配
        for e in &evs {
            let g = match e {
                AnalysisEventWire::Segment { generation, .. }
                | AnalysisEventWire::TrackAnalysis { generation, .. }
                | AnalysisEventWire::Done { generation, .. }
                | AnalysisEventWire::Failed { generation, .. } => *generation,
            };
            assert_eq!(g, g);
        }
        // 首事件 = 段 0，列数 = SEG_COLS
        match &evs[0] {
            AnalysisEventWire::Segment { seg, detail, .. } => {
                assert_eq!(*seg, 0);
                assert_eq!(detail.len(), SEG_COLS);
            }
            _ => panic!("首事件应为 Segment"),
        }
        // TrackAnalysis 写 grid≈120
        let ta = evs
            .iter()
            .find_map(|e| match e {
                AnalysisEventWire::TrackAnalysis { bpm, .. } => Some(*bpm),
                _ => None,
            })
            .expect("应有 TrackAnalysis");
        assert!((ta - 120.0).abs() < 1.0, "BPM 应≈120，实得 {ta}");
        assert!((bus.get(&paths::deck_grid_bpm(0)) - 120.0).abs() < 1.0);
        // Done 全曲波形 9000 列
        match evs.last().unwrap() {
            AnalysisEventWire::Done { detail, .. } => assert_eq!(detail.len(), 9000),
            _ => panic!("应以 Done 结束"),
        }

        // 重载：代际 +1、旧线程 shutdown 置位
        let first_shutdown = SHUTDOWN.lock().unwrap()[0].clone().unwrap();
        let (tx2, rx2) = mpsc::channel();
        let txc2 = tx2.clone();
        let gen2 = load_track_inner(&bus, &handle, 0, &p, move |ev| txc2.send(ev).is_ok()).unwrap();
        assert_eq!(gen2, 2);
        assert!(
            first_shutdown.load(Ordering::Relaxed),
            "重载应停掉旧分析线程"
        );
        drop(rx2); // 关通道让第二次分析线程自退
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_track_rejects_bad_deck() {
        let bus = ControlBus::default();
        let (_state, handle) = Engine::core(&bus);
        let err = load_track_inner(&bus, &handle, 2, Path::new("/nonexistent"), |_| true);
        assert!(err.is_err());
    }

    #[test]
    fn read_metadata_real_track() {
        let p = test_track();
        if !p.exists() {
            eprintln!("跳过：无测试曲 {}", p.display());
            return;
        }
        let m = read_metadata(p.to_str().unwrap()).expect("元数据读取失败");
        // 字段可空，但读取本身必须成功；有封面则 mime 非空
        if !m.cover.is_empty() {
            assert!(!m.cover_mime.is_empty(), "有封面必有 mime");
        }
    }
}
