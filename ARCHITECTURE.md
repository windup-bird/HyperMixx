# Hypermixx 架构

Rust workspace,五个 crate 单向分层:**core(类型) ← media(PCM) ← audio(引擎)**;
library(分析)与 audio 零交叉依赖,只被 cli 调用。引擎域 **44.1 kHz 立体声**(与目标设备
ALSA default 的原生时钟一致,cpal 回调直通,零转换)。

```
cli ──→ core / media / audio / library
audio ──→ core, media          (+ timestretch, vendored)
library ──→ core, media        (+ stratum-dsp, vendored)
media ──→ core                 (+ symphonia)
core ──→ serde
```

```
crates/
├── hypermixx-core/       # 协议与类型(仅依赖 serde)
│   └── src/
│       ├── beatgrid.rs   # BeatGrid:绝对帧拍网格 + 相位/拍宽查询
│       ├── key.rs        # Key / KeyMode / KeyFormat(传统 + Camelot)
│       ├── analysis.rs   # TrackAnalysis { beatgrid, key, bpm }
│       ├── deck.rs       # DeckId = u8 / DeckState 快照
│       ├── command.rs    # Command / CommandResponse / Backend
│       └── source.rs     # Source trait + Shared = Arc<dyn Source>
│
├── hypermixx-media/      # PCM:解码 + 内存池(core + symphonia)
│   └── src/
│       ├── decoder.rs    # decode_file:symphonia → 44.1kHz 立体声 f32
│       └── pool.rs       # PcmPool:不可变 Arc 数据的 Source 实现
│
├── hypermixx-audio/      # 实时引擎(core + media + timestretch)
│   ├── src/
│   │   ├── ringbuf.rs    # rtrb SPSC 封装 + 自由函数
│   │   ├── pipeline.rs   # AudioPipeline:producer 线程 + cpal 输出
│   │   ├── deck/
│   │   │   ├── deck.rs      # Deck:流状态机 + cued_from 跳转补偿
│   │   │   ├── jump.rs      # Seek{Frames,Beats,Beat,Quantized} + phase_preserving
│   │   │   ├── flowshift.rs # FlowShift(原 TimeShift):后台流预热
│   │   │   └── loop_.rs     # LoopState 占位(尚未生效)
│   │   └── flow/
│   │       ├── flow.rs      # Flow:单次播放单元(状态机)
│   │       └── pitchshift.rs # PitchShiftEngine:timestretch 引擎包装
│   └── tests/            # engine / beatlock / decode / tone_faithful
│
├── hypermixx-library/    # 分析与曲库(core + media + stratum-dsp)
│   └── src/
│       ├── beat_spec.rs     # BeatSpec/Segment:可编辑网格真源
│       ├── grid_compiler.rs # GridCompiler:前填/等距/接缝去重/尾外推 → BeatGrid
│       ├── track.rs         # TrackInfo:持 BeatSpec,惰性缓存编译结果
│       ├── waveform.rs      # 峰值概览
│       └── analyser/
│           ├── mod.rs         # analyze(source) = backend → refine → compile
│           ├── stratum.rs     # stratum-dsp 适配(兜 panic,关静音裁剪)
│           ├── timestretch.rs # 占位:vendored 未暴露离线分析,返回 Unsupported
│           └── refine.rs      # fit_rigid:中位数周期 + 最小二乘 + 倍频归位
│
├── hypermixx-cli/        # 前端(二进制,依赖全部四层)
│   └── src/main.rs       # 行解析 / 后台 decode+analyse / 响应打印线程
│
├── stratum-dsp/          # vendored:节拍/调性分析(仅用公开入口)
└── timestretch/          # vendored:实时时间拉伸引擎(仅用公开入口)
```

---

## hypermixx-core

只放类型与协议。`Source` 定义在这里是 `Command::Load` 能携带 `Arc<dyn Source>` 的前提
(若在 media,core→media 成环);所有 crate 通过 `use hypermixx_core::…` 共享。

### `BeatGrid`
绝对帧位置 `Vec<u64>`(严格递增),BPM 永远派生、不存储。
- `from_constant_bpm(bpm, first, total, sr)` — 恒速网格,无累积误差
- `from_frames(beats, sr)` / `empty(sr)` / `is_empty()`
- `frame_at_beat(beat)` — 存储范围内直接索引,越界按末拍间隔外推
- `floor_beat(frame)` / `phase(frame)` / `beat_width(beat)` / `average_bpm()`

### `Command` / `CommandResponse`
- `Load { deck_id, source, analysis }` — source 已解码,analysis 可选(常网格快路径)
- `Play` / `Pause` / `Jump { target_frame }` / `BeatJump { beats }`
- `SetRate` / `SetProfile` / `SetAnalysis` — 分析结果从这里进入 deck,**没有 `Analyse` 命令**
- `GetState` / `GetAllStates`(同块原子快照)/ `Quit`
- `Backend { Auto, Stratum, Timestretch }` — 分析后端选择

---

## hypermixx-media

- `decode_file(path)` — symphonia 解码 → 重采样到引擎率(44.1 kHz)→ 立体声交错 f32。
  非 44.1k 素材在加载时一次性转换,引擎内不再有采样率概念。
- `PcmPool` — `Arc<Vec<f32>>` 的 `Source` 实现,克隆即 Arc bump;deck 与分析器共享同一条数据。

---

## hypermixx-audio

### `pipeline.rs`
拓扑:`CLI → [producer: deck0 + deck1 → 混音 ×0.5] → ring → [cpal 回调] → 声卡`
- 按设备**原生速率**打开输出(本机 44100 = 引擎率,直通);速率不符时打印警告
- 回调零锁零分配,只读 ring;producer 以 ring 余量配速

### `deck/`
- `Deck` — 单 source、单活跃流;`jump()` 记 `cued_from`,`switch_to()` 把新流落点前移
  预热期间已播放的帧量,保证跨 deck 相位锁定(`phase_probe.sh` 的 `err` 恒 +0)
- `jump::phase_preserving` — 目标帧 = 目标拍位 + `phase(当前) × 目标拍宽`,兼容非均匀网格
- `FlowShift` — 后台预热线程:submit Flow → `prepare()` → 入库 → 通报 id,deck 下个块切流

### `flow/`
- `Flow` — `Preparing → Ready → Active → Retired`;活跃时经引擎采样,暂停/耗尽补静音
- `PitchShiftEngine` — timestretch-rs 三 Profile:**Tape**(零延迟直通)/ **Keylock** /
  **WideKeylock**(预热走 `reset → set_track_position → warm_start`)

---

## hypermixx-library

分析管线:`Source → downmix_to_mono → backend → RawAnalysis → fit_rigid → BeatSpec
→ GridCompiler → TrackAnalysis → SetAnalysis`

- `BeatSpec` — 可编辑真源(段落化 BPM + 起点 + 拍数);刚性网格 = 单段
- `GridCompiler` — 三段编译:起点前填至 frame 0 / 段内等距 / 接缝去重 + 末段外推到轨尾
- `fit_rigid` — 中位数拍距 → 倍频归位(拉进 [60, 200] BPM)→ 整数拍号分配 + 最小二乘
- `TrackInfo` — 持 `BeatSpec` 与 key;`Mutex<Option<TrackAnalysis>>` 惰性编译,改 spec 即失效

---

## hypermixx-cli

- `load <deck> <path> [bpm]` — 后台解码;给 bpm 则建常网格跳过分析
- `analyse <deck>` — 后台跑 `library::analyser::analyze`,完成后 `SetAnalysis`
- `--backend auto|stratum|timestretch`;响应由独立打印线程渲染,输入永不阻塞

---

## 采样率约定

引擎域固定 44.1 kHz:目标设备(ALSA `default`)原生 44.1 kHz,`preferred_config` 按引擎率
请求即直通。历史教训——引擎 48k 而设备 44.1k 时,ALSA 静默接受 48k 请求却按 44.1k 时钟
消费 → 慢放 8.8% + 音调低 8.8%。多设备自适应(设备率≠引擎率时在回调内重采样)是后续工作。

---

## 数据流

```
CLI  stdin
  │ spawn_load: decode → PcmPool ── Command::Load{source, analysis?} ──┐
  │ spawn_analyse: analyser::analyze → Command::SetAnalysis ───────────┤ crossbeam
  ▼                                                                    ▼
AudioPipeline ──producer thread──────────────────────────────────────────┐
  │ Jump/BeatJump: jump::resolve 算目标 → 记 cued_from → FlowShift       │
  │ FlowShift worker: Flow.prepare() → 入库 → 通报                       │
  │ 每 tick: poll_ready → switch_to(补偿) → active.process_block → mix   │
  └─ f32 ──→ ring ──→ cpal 回调(44.1k 直通)──→ 声卡
```
