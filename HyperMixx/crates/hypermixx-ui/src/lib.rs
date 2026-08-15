//! HyperMixx UI：Slint 窗口 + 60Hz 控制轮询 + 波形纹理。
//! 引擎通信只走两路：ControlBus（参数/状态）与 EngineHandle（加载/跳转）。

slint::include_modules!();

mod waveform_texture;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use hypermixx_analysis::{AnalysisEvent, SEG_FRAMES};
use hypermixx_audio::EngineHandle;
use hypermixx_core::{ControlBus, paths};
use slint::Image;
use waveform_texture::WaveState;

const WINDOW_DUR_SECS: f64 = 60.0;

pub struct Ui {
    pub window: MainWindow,
    state: Arc<Mutex<UiState>>,
    // drop 会取消 slint::Timer，必须持有
    _timer: slint::Timer,
}

/// 每 deck 的分析元数据（BPM/key/beatgrid），UI 显示用。
/// 字符串不落 ControlBus（bus 仅 f64），BPM/offset 同步到 grid_bpm/grid_offset。
struct TrackMeta {
    bpm: f64,
    /// 秒坐标拍点（升序），beatgrid 线渲染用。
    beats: Vec<f64>,
    downbeats: Vec<f64>,
    key: String,
}

impl Default for TrackMeta {
    fn default() -> Self {
        Self {
            bpm: 0.0,
            beats: Vec::new(),
            downbeats: Vec::new(),
            key: String::new(),
        }
    }
}

struct UiState {
    controls: ControlBus,
    engine: EngineHandle,
    waves: [WaveState; 2],
    meta: [TrackMeta; 2],
    /// 上次写入 UI 文本的 (bpm, key)，避免每 tick 生成字符串。
    meta_shown: [(f64, String); 2],
    win_start: [f64; 2],
    win_dur: [f64; 2],
    /// 统一缩放（两 deck 共用一根滑杆）。
    zoom: f64,
    // 上次渲染的 (窗口起点, 缩放, 波形数据版本)，避免每 tick 重复上传
    rendered: [(f64, f64, u64); 2],
    /// 波形数据版本：每个分析事件 +1，驱动纹理重绘
    wave_rev: [u64; 2],
    /// 每 deck 分析事件通道（load 时重建）
    wave_rx: [Option<mpsc::Receiver<AnalysisEvent>>; 2],
    /// 每 deck 代际：重载后递增，旧分析线程的事件被丢弃
    wave_generation: [u64; 2],
    /// 分析优先级（播放头所在段），由 tick 持续更新
    wave_priority: [Arc<AtomicU64>; 2],
    /// 旧分析线程的退出标志（重载时置位）
    wave_shutdown: [Option<Arc<AtomicBool>>; 2],
    /// 首次检测到非零 master VU 时打印一次（信号链验证用）
    vu_reported: bool,
    /// 每 deck 首次纹理渲染后打印一次亮像素统计（波形验证用）
    tex_reported: [bool; 2],
}

impl UiState {
    fn load_track(&mut self, deck: usize, path: PathBuf) {
        self.engine.load(deck, path.clone());
        // 停掉旧分析线程；代际 +1 使旧线程的事件全部失效
        if let Some(f) = self.wave_shutdown[deck].take() {
            f.store(true, Ordering::Relaxed);
        }
        self.wave_generation[deck] += 1;
        let generation = self.wave_generation[deck];
        self.waves[deck] = WaveState::None;
        self.meta[deck] = TrackMeta::default();
        self.meta_shown[deck] = (0.0, String::new());
        // 旧曲的网格失效（引擎 bpm 显示回 0，P5 sync 不误用）
        self.controls.set(&paths::deck_grid_bpm(deck), 0.0);
        self.controls.set(&paths::deck_grid_offset(deck), 0.0);
        self.wave_rev[deck] = 0;
        self.win_start[deck] = 0.0;
        self.rendered[deck] = (f64::NAN, f64::NAN, u64::MAX);
        let (tx, rx) = mpsc::channel();
        self.wave_rx[deck] = Some(rx);
        let priority = self.wave_priority[deck].clone();
        priority.store(0, Ordering::Relaxed);
        let shutdown = Arc::new(AtomicBool::new(false));
        self.wave_shutdown[deck] = Some(shutdown.clone());
        hypermixx_analysis::start_analysis(path, priority, shutdown, generation, tx);
    }

    /// 应用一个分析事件；旧代际的事件直接丢弃。
    fn apply_wave_event(&mut self, deck: usize, ev: AnalysisEvent) {
        let cur = self.wave_generation[deck];
        match ev {
            AnalysisEvent::Segment {
                generation,
                seg,
                detail,
                ..
            } => {
                if generation != cur {
                    return;
                }
                if !matches!(self.waves[deck], WaveState::Partial { .. }) {
                    self.waves[deck] = WaveState::Partial { segs: Vec::new() };
                }
                if let WaveState::Partial { segs } = &mut self.waves[deck] {
                    while segs.len() <= seg {
                        segs.push(None);
                    }
                    segs[seg] = Some(detail);
                }
                self.wave_rev[deck] += 1;
            }
            AnalysisEvent::TrackAnalysis {
                generation,
                bpm,
                key,
                offset_secs,
                beats_secs,
                downbeats_secs,
                confidence,
            } => {
                if generation != cur {
                    return;
                }
                log::info!(
                    "deck {} 单遍分析：BPM {bpm:.1}（置信 {confidence:.2}），key {}，{} 拍",
                    deck + 1,
                    key.as_ref().map(|k| k.name()).unwrap_or_default(),
                    beats_secs.len()
                );
                self.meta[deck] = TrackMeta {
                    bpm,
                    beats: beats_secs.into_vec(),
                    downbeats: downbeats_secs.into_vec(),
                    key: key.as_ref().map(|k| k.camelot()).unwrap_or_default(),
                };
                // 引擎侧（bpm 显示、P5 sync）读 grid_bpm/grid_offset
                self.controls.set(&paths::deck_grid_bpm(deck), bpm);
                self.controls
                    .set(&paths::deck_grid_offset(deck), offset_secs);
                self.wave_rev[deck] += 1; // 网格线驱动重绘
            }
            AnalysisEvent::Done { generation, wave } => {
                if generation != cur {
                    return;
                }
                log::info!("deck {} 波形就绪：{} 列", deck + 1, wave.detail.len());
                self.waves[deck] = WaveState::Full(wave);
                self.wave_rev[deck] += 1;
            }
            AnalysisEvent::Failed { generation, msg } => {
                if generation != cur {
                    return;
                }
                log::error!("deck {} 波形分析失败: {msg}", deck + 1);
                self.waves[deck] = WaveState::None;
                self.wave_rev[deck] += 1;
            }
        }
    }

    fn tick(&mut self, ui: &MainWindow) {
        // 0. UI 控件 → 控制总线
        poll_controls(&self.controls, ui);
        // 统一缩放：一根滑杆驱动两个 deck 的窗口时长
        let z = ui.get_master_zoom() as f64;
        if (z - self.zoom).abs() > 1e-6 {
            self.zoom = z;
            let wd = WINDOW_DUR_SECS / z.max(0.25);
            self.win_dur = [wd, wd];
        }

        // 1. 波形分析事件（渐进分段；take/put 避免与 apply_wave_event 冲突借用）
        for deck in 0..2 {
            let rx = self.wave_rx[deck].take();
            if let Some(rx) = rx {
                while let Ok(ev) = rx.try_recv() {
                    self.apply_wave_event(deck, ev);
                }
                self.wave_rx[deck] = Some(rx);
            }
        }

        // 2. 每 deck 状态
        for deck in 0..2 {
            let ph = self.controls.get(&paths::deck_playhead(deck));
            let dur = self.controls.get(&paths::deck_duration(deck));
            let loaded = self.controls.get(&paths::deck_loaded(deck)) > 0.5;
            let playing = self.controls.get(&paths::deck_play(deck)) > 0.5;
            let vu = self.controls.get(&paths::deck_vu(deck)).min(1.0);
            let wd = self.win_dur[deck];

            // 分析优先级：播放头所在段（seek 后自动重排，就近填充）
            let p = (ph * 48_000.0 / SEG_FRAMES as f64) as u64;
            if self.wave_priority[deck].load(Ordering::Relaxed) != p {
                self.wave_priority[deck].store(p, Ordering::Relaxed);
            }

            // 窗口滚动：播放头恒固定在窗口中央（含未播放/曲头——窗口起点
            // 可为负，纹理渲染曲头前透明留白）；仅尾端受钳制时指针向右偏移
            // （曲尾之后无内容可补，DJ 软件标准行为）。
            let start = (ph - 0.5 * wd).min((dur - wd).max(0.0));
            self.win_start[deck] = start;

            // 纹理重绘：窗口移动 / 缩放变化 / 波形数据更新（rev 变化）
            let (r_start, r_zoom, r_rev) = self.rendered[deck];
            let zoom = self.zoom;
            if loaded
                && self.waves[deck].cols_total() > 0
                && (r_start != start || r_zoom != zoom || r_rev != self.wave_rev[deck])
                && let Some(img) = self.render_texture(deck)
            {
                set_deck_wave(ui, deck, img);
                self.rendered[deck] = (start, zoom, self.wave_rev[deck]);
            }

            // BPM/key 文本（值变化才写 UI，避免每 tick 生成字符串）
            let m = &self.meta[deck];
            let shown = (m.bpm, m.key.as_str());
            if (self.meta_shown[deck].0, self.meta_shown[deck].1.as_str()) != shown {
                let text = if shown.0 > 0.0 {
                    format!(
                        "{:.2} BPM · {}",
                        shown.0,
                        if shown.1.is_empty() { "—" } else { shown.1 }
                    )
                } else {
                    String::new()
                };
                set_deck_info(ui, deck, text);
                self.meta_shown[deck] = (m.bpm, m.key.clone());
            }

            // playhead 位置（窗口内比例；居中时恒为 0.5）
            let frac = if wd > 0.0 {
                ((ph - start) / wd).clamp(0.0, 1.0)
            } else {
                0.0
            };
            set_deck_state(ui, deck, playing, vu, frac);
        }

        // 3. master VU
        let mv = self.controls.get(paths::master_vu()).min(1.0);
        if !self.vu_reported && mv > 0.05 {
            log::info!("信号链正常：master VU = {mv:.3}");
            self.vu_reported = true;
        }
        ui.set_master_vu(mv as f32);
    }

    fn render_texture(&mut self, deck: usize) -> Option<Image> {
        let data = &self.waves[deck];
        let m = &self.meta[deck];
        let beats = (!m.beats.is_empty()).then_some(m.beats.as_slice());
        let downbeats = (!m.downbeats.is_empty()).then_some(m.downbeats.as_slice());
        let buf = waveform_texture::render(
            data,
            self.win_start[deck],
            self.win_dur[deck],
            beats,
            downbeats,
        );
        if !self.tex_reported[deck] {
            // 一次性诊断：确认纹理内容真的画出来了
            let lit = buf.as_bytes().chunks(4).filter(|p| p[3] > 0).count();
            log::info!(
                "deck {} 波形纹理已渲染：{}×{}，亮像素 {lit}",
                deck + 1,
                buf.width(),
                buf.height()
            );
            self.tex_reported[deck] = true;
        }
        Some(Image::from_rgba8(buf))
    }
}

/// UI 控件当前值 → 控制总线（30Hz 轮询；set 对未变更值是无操作）。
/// slint 1.17 不再为属性生成 `on_xxx_changed` 回调，轮询是统一通道。
fn poll_controls(controls: &ControlBus, ui: &MainWindow) {
    controls.set(&paths::deck_rate(0), ui.get_deck1_rate() as f64);
    controls.set(&paths::deck_eq_low(0), ui.get_deck1_eq_low() as f64);
    controls.set(&paths::deck_eq_mid(0), ui.get_deck1_eq_mid() as f64);
    controls.set(&paths::deck_eq_high(0), ui.get_deck1_eq_high() as f64);
    controls.set(&paths::deck_volume(0), ui.get_deck1_volume() as f64);
    controls.set(&paths::deck_pitch(0), ui.get_deck1_pitch() as f64);
    controls.set(&paths::deck_keylock(0), if ui.get_deck1_keylock() { 1.0 } else { 0.0 });
    controls.set(&paths::deck_sync(0), if ui.get_deck1_sync() { 1.0 } else { 0.0 });
    controls.set(
        &paths::deck_quantize(0),
        if ui.get_deck1_quantize() { 1.0 } else { 0.0 },
    );
    controls.set(&paths::deck_nudge(0), ui.get_deck1_nudge() as f64);
    controls.set(&paths::deck_rate(1), ui.get_deck2_rate() as f64);
    controls.set(&paths::deck_eq_low(1), ui.get_deck2_eq_low() as f64);
    controls.set(&paths::deck_eq_mid(1), ui.get_deck2_eq_mid() as f64);
    controls.set(&paths::deck_eq_high(1), ui.get_deck2_eq_high() as f64);
    controls.set(&paths::deck_volume(1), ui.get_deck2_volume() as f64);
    controls.set(&paths::deck_pitch(1), ui.get_deck2_pitch() as f64);
    controls.set(&paths::deck_keylock(1), if ui.get_deck2_keylock() { 1.0 } else { 0.0 });
    controls.set(&paths::deck_sync(1), if ui.get_deck2_sync() { 1.0 } else { 0.0 });
    controls.set(
        &paths::deck_quantize(1),
        if ui.get_deck2_quantize() { 1.0 } else { 0.0 },
    );
    controls.set(&paths::deck_nudge(1), ui.get_deck2_nudge() as f64);
    controls.set(paths::master_volume(), ui.get_master_volume() as f64);
}

fn set_deck_state(ui: &MainWindow, deck: usize, playing: bool, vu: f64, playhead_frac: f64) {
    match deck {
        0 => {
            ui.set_deck1_playing(playing);
            ui.set_deck1_vu(vu as f32);
            ui.set_deck1_playhead(playhead_frac as f32);
        }
        _ => {
            ui.set_deck2_playing(playing);
            ui.set_deck2_vu(vu as f32);
            ui.set_deck2_playhead(playhead_frac as f32);
        }
    }
}

fn set_deck_wave(ui: &MainWindow, deck: usize, img: Image) {
    match deck {
        0 => ui.set_deck1_wave(img),
        _ => ui.set_deck2_wave(img),
    }
}

fn set_deck_info(ui: &MainWindow, deck: usize, text: String) {
    match deck {
        0 => ui.set_deck1_info(text.into()),
        _ => ui.set_deck2_info(text.into()),
    }
}

impl Ui {
    pub fn new(controls: ControlBus, engine: EngineHandle) -> anyhow::Result<Self> {
        let ui = MainWindow::new()?;

        // 控件初始值 → 控制总线（引擎侧默认是 0，避免一启动就静音）
        let state = Arc::new(Mutex::new(UiState {
            controls: controls.clone(),
            engine,
            waves: [WaveState::None, WaveState::None],
            meta: std::array::from_fn(|_| TrackMeta::default()),
            meta_shown: [(0.0, String::new()), (0.0, String::new())],
            win_start: [0.0, 0.0],
            win_dur: [WINDOW_DUR_SECS, WINDOW_DUR_SECS],
            zoom: 1.0,
            rendered: [(f64::NAN, f64::NAN, u64::MAX); 2],
            wave_rev: [0, 0],
            wave_rx: [None, None],
            wave_generation: [0, 0],
            wave_priority: std::array::from_fn(|_| Arc::new(AtomicU64::new(0))),
            wave_shutdown: [None, None],
            vu_reported: false,
            tex_reported: [false; 2],
        }));

        // 启动参数从 UI 默认值同步到控制总线（之后由 30Hz tick 持续轮询）
        poll_controls(&controls, &ui);

        // 播放/暂停
        for deck in 0..2 {
            let st = Arc::clone(&state);
            let play_path = paths::deck_play(deck);
            let toggle = move || {
                let st = st.lock().unwrap();
                let cur = st.controls.get(&play_path);
                st.controls.set(&play_path, 1.0 - cur);
            };
            match deck {
                0 => ui.on_deck1_play_toggle(toggle),
                _ => ui.on_deck2_play_toggle(toggle),
            }
        }

        // 加载：文件对话框 + 引擎 + 后台分析
        for deck in 0..2 {
            let st = Arc::clone(&state);
            let load = move || {
                let mut st = st.lock().unwrap();
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("音频", &["flac", "mp3", "wav", "ogg", "m4a", "aac"])
                    .pick_file()
                {
                    log::info!("deck {} 选择文件: {}", deck + 1, p.display());
                    st.load_track(deck, p);
                }
            };
            match deck {
                0 => ui.on_deck1_load(load),
                _ => ui.on_deck2_load(load),
            }
        }

        // 波形点击跳转
        for deck in 0..2 {
            let st = Arc::clone(&state);
            let seek = move |f: f32| {
                let st = st.lock().unwrap();
                // 窗口起点可为负（曲头前留白），跳转位置钳到曲首
                let secs = (st.win_start[deck] + f as f64 * st.win_dur[deck]).max(0.0);
                st.engine.seek(deck, secs);
            };
            match deck {
                0 => ui.on_deck1_seek_frac(seek),
                _ => ui.on_deck2_seek_frac(seek),
            }
        }

        // 缩放由 tick 轮询（slint 1.17 无属性变更回调）

        // 60Hz 轮询（滚动波形 + 控制轮询共用；滚动顺滑度需要高于 30Hz）
        let weak = ui.as_weak();
        let timer = slint::Timer::default();
        let timer_state = Arc::clone(&state);
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                let state = Arc::clone(&timer_state);
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    let mut st = state.lock().unwrap();
                    st.tick(&ui);
                });
            },
        );

        Ok(Ui {
            window: ui,
            state,
            _timer: timer,
        })
    }

    pub fn run(&self) -> anyhow::Result<()> {
        self.window.run()?;
        Ok(())
    }

    /// 加载曲目到 deck（引擎 + 后台波形分析），供 CLI 参数与 UI 共用。
    pub fn load_track(&self, deck: usize, path: PathBuf) {
        let mut st = self.state.lock().unwrap();
        st.load_track(deck, path);
    }
}
