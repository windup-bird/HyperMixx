# 提示词：Hypermixx 架构重构

## 任务

把当前 workspace 从「audio + analysis + cli + vendored」重构为五层「core / media / audio / library / cli」。目标：**audio 和 library 完全解耦，PCM 归 media，协议归 core，分析归 library，引擎归 audio。**

## 最终目录

```
hypermixx/
├── Cargo.toml
├── scripts/phase_probe.sh
└── crates/
    ├── hypermixx-core/
    │   └── src/
    │       ├── lib.rs
    │       ├── beatgrid.rs       # BeatGrid
    │       ├── key.rs            # Key / KeyMode / KeyFormat
    │       ├── analysis.rs       # TrackAnalysis
    │       ├── deck.rs           # DeckId / DeckState
    │       └── command.rs        # Command / CommandResponse / Backend
    │
    ├── hypermixx-media/
    │   └── src/
    │       ├── lib.rs
    │       ├── source.rs         # Source trait
    │       ├── pool.rs           # PcmPool
    │       └── decoder.rs        # symphonia 解码
    │
    ├── hypermixx-audio/
    │   └── src/
    │       ├── lib.rs
    │       ├── ringbuf.rs
    │       ├── pipeline.rs
    │       ├── deck/
    │       │   ├── mod.rs
    │       │   ├── deck.rs
    │       │   ├── jump.rs        # Seek::resolve
    │       │   ├── loop.rs        # LoopState 占位
    │       │   └── flowshift.rs   # 原 timeshift
    │       ├── flow/
    │       │   ├── mod.rs
    │       │   ├── flow.rs
    │       │   └── pitchshift.rs
    │       └── source/            # 删除
    │
    ├── hypermixx-library/
    │   └── src/
    │       ├── lib.rs
    │       ├── track.rs           # TrackId / TrackInfo / Metadata
    │       ├── beat_spec.rs       # BeatSpec / Segment
    │       ├── grid_compiler.rs   # GridCompiler（BeatSpec → BeatGrid）
    │       ├── waveform.rs
    │       └── analyser/
    │           ├── mod.rs         # RawAnalysis / Backend / analyze()
    │           ├── stratum.rs
    │           ├── timestretch.rs
    │           └── refine.rs      # 最简刚性拟合 → BeatSpec
    │
    └── hypermixx-cli/
        └── src/main.rs
```

删掉：`crates/hypermixx-analysis`、`crates/hypermixx-audio/src/beatgrid.rs`、`crates/hypermixx-audio/src/source/`、`crates/hypermixx-audio/src/deck/timeshift.rs`、`crates/hypermixx-audio/src/command.rs`。

## 依赖方向（严格单向）

```
core   ←  media
core   ←  audio
core   ←  library
core   ←  cli
media  ←  audio
media  ←  library
audio  ←  cli
library ← cli
```

- `audio` 不依赖 `library`
- `library` 不依赖 `audio`
- `media` 只依赖 `core`
- `cli` 依赖所有

## 步骤

### 1. 建 `hypermixx-core`

从当前 `hypermixx-audio/src/beatgrid.rs` 搬类型：

- **`BeatGrid`**：编译后的网格。字段 `beat_frames: Vec<u64>` + `sample_rate: u32`。
  - 保留：`from_constant_bpm` / `from_frames` / `empty` / `is_empty` / `frame_at_beat` / `floor_beat` / `phase` / `beat_width` / `average_bpm`
  - **删掉**：`nearest_beat` / `current_beat_frame` / `next_beat_frame` / `bpm_at_beat` / `beat_count` / `beatjump_target` / `from_seconds`
- **`Key`**（原名 `KeyReport`）：`{ pc: u8, mode: KeyMode, confidence: f32 }`，加 `traditional()` / `camelot()` / `format(KeyFormat)` / `both()`
- **`KeyMode`**：`Major | Minor`
- **`KeyFormat`**：`Traditional | Camelot`
- **`TrackAnalysis`**：`{ beatgrid: BeatGrid, key: Option<Key>, bpm: Option<f32> }`
- **`DeckId` / `DeckState`**
- **`Command` / `CommandResponse`**：不含 `Analyse`
- **`Backend`**：`{ Auto, Stratum, Timestretch }`

`core` 只依赖 `serde`。

### 2. 建 `hypermixx-media`

从 `hypermixx-audio/src/source/` 整体搬过来：

- `Source` trait
- `PcmPool`
- `decoder::decode_file`（symphonia）

`media` 依赖 `symphonia`。

### 3. 重排 `hypermixx-audio`

- `lib.rs` 只留常量（`SAMPLE_RATE` / `CHANNELS` / `BLOCK_SIZE` / `DECK_COUNT` / `DECK_MIX_GAIN`）
- `ringbuf.rs` / `pipeline.rs` 保留
- `deck/timeshift.rs` → `deck/flowshift.rs`，`TimeShift` → `FlowShift`，`Deck.timeshift` → `Deck.flowshift`
- `deck/jump.rs`：定义 `Seek` enum（`Frames` / `Beats` / `Beat` / `Quantized`），`resolve` 和 `phase_preserving` 函数
- `deck/loop.rs`：`LoopState` 占位，暂不实现环绕
- `deck/deck.rs`：`beat_target_frame` 改调 `jump::resolve`，`use hypermixx_core::BeatGrid`
- `flow/flow.rs`：加空的 `set_loop` 占位
- 删 `source/` 目录、`command.rs`、`beatgrid.rs`
- `Cargo.toml` 依赖 `core` + `media` + `timestretch`

**硬约束**：`audio` 不能 `use hypermixx_library`。

### 4. 建 `hypermixx-library`

删 `hypermixx-analysis`，新建：

**`beat_spec.rs`**：
```rust
pub struct Segment {
    pub bpm: f64,
    pub start_frame: u64,
    pub beats: Option<u64>,   // None = 到下一个 segment 或曲末
}

pub struct BeatSpec {
    pub segments: Vec<Segment>,
}
```

**`grid_compiler.rs`**：
```rust
pub struct GridCompiler {
    pub sample_rate: u32,
    pub total_frames: u64,
    pub dedup_ratio: f64,   // 默认 0.9
}

impl GridCompiler {
    pub fn new(sample_rate: u32, total_frames: u64) -> Self;
    pub fn compile(&self, spec: &BeatSpec) -> BeatGrid;   // 返回 core::BeatGrid
}
```

编译逻辑（三段）：
1. **打起点**：第一个 segment 从 `start_frame` 相位对齐向前推到帧 0
2. **等间距生成**：每个 segment 在 `[start_frame, next.start_frame)` 内均匀展开 `beats` 拍
3. **边界去重 + 外推**：`gap < min(fpb_prev, fpb_curr) * dedup_ratio` 时删前一拍；最后一个 segment 外推至 `total_frames`

**`analyser/mod.rs`**：
```rust
pub struct RawAnalysis {
    pub beats_sec: Vec<f64>,
    pub key: Option<Key>,
    pub bpm_hint: Option<f32>,
    pub duration_sec: f64,
}
pub use hypermixx_core::command::Backend;

pub fn analyze(
    source: Arc<dyn Source>,
    sample_rate: u32,
    total_frames: u64,
    backend: Backend,
) -> Result<TrackAnalysis, AnalyserError>;

fn downmix_to_mono(source: &dyn Source) -> Vec<f32>;
```

`dispatch(backend, mono, sr) -> Result<RawAnalysis, AnalyserError>`：
- `Stratum` → 只调 stratum
- `Timestretch` → 只调 timestretch
- `Auto` → 先 stratum，失败 fallback timestretch

**`analyser/stratum.rs`**：
```rust
pub fn analyze(mono: &[f32], sr: u32) -> Result<RawAnalysis, AnalyserError>;
// catch_unwind + stratum_dsp::analyze_audio
// beats_sec = r.beat_grid.beats
// key = convert_key(r.key)
// bpm_hint = Some(r.bpm as f32)
```

**`analyser/timestretch.rs`**：
```rust
pub fn analyze(mono: &[f32], sr: u32) -> Result<RawAnalysis, AnalyserError>;
// 若 vendored 未暴露 pre_analysis，返回 Err(NotCompiled("timestretch"))
```

**`analyser/refine.rs`**：最简刚性拟合

```rust
pub struct RefineConfig {
    pub min_bpm: f64,       // 60
    pub max_bpm: f64,       // 200
    pub inlier_ratio: f64,  // 0.03
    pub min_beats: usize,   // 16
}

pub fn fit_rigid(
    raw: &RawAnalysis,
    sample_rate: u32,
    total_frames: u64,
    cfg: &RefineConfig,
) -> Result<BeatSpec, AnalyserError>;
```

算法：
1. 秒 → 帧
2. 拍数 < `min_beats` 报错
3. 间隔中位数 = period 初值
4. 倍率消歧：把 period 拉进 `[min_bpm, max_bpm]`（×2 / ÷2）
5. 分配拍号 `k = round((t - t0) / period)`，剔 `|t - (t0 + k*period)| > period * inlier_ratio` 的异常
6. 最小二乘解 `t = t0 + k * period`
7. 出单段 `BeatSpec`

**`track.rs`**：
```rust
pub struct TrackId(pub u64);

pub struct TrackInfo {
    pub id: TrackId,
    pub path: PathBuf,
    pub metadata: Metadata,
    pub spec: Mutex<BeatSpec>,          // 可编辑
    pub key: Option<Key>,
    analysis: OnceLock<Arc<TrackAnalysis>>,
    waveform: OnceLock<Arc<Waveform>>,
}

pub struct Metadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub year: Option<u32>,
    pub duration_frames: u64,
    pub sample_rate: u32,
}

impl TrackInfo {
    pub fn open(path: PathBuf) -> Result<Self, LibError>;
    pub fn analysis(&self) -> Arc<TrackAnalysis>;   // 惰性，编译 spec
    pub fn invalidate_analysis(&mut self);
}
```

`analysis()` 内部：读 `spec` 锁 → `GridCompiler::compile` → 组装 `TrackAnalysis`。**不调 analyser**，analyser 只用于首次生成 spec。

**`waveform.rs`**：
```rust
pub struct Waveform {
    pub peaks: Vec<(f32, f32)>,   // (min, max)
    pub bucket_frames: u64,
}
pub fn peaks(source: &dyn Source, bucket_frames: u64) -> Waveform;
```

**`Cargo.toml`**：依赖 `core` + `media` + `stratum-dsp`。

### 5. 重写 `hypermixx-cli`

- `Cargo.toml` 依赖 `core` + `media` + `audio` + `library`
- 启动时 `--backend` 参数（`auto` / `stratum` / `timestretch`）
- 分发规则：
  - `load` 命令：CLI 先 `media::decoder::decode_file` 拿 `Arc<PcmPool>` → 发 `Command::Load { deck_id, source }`
  - 用户显式 `analyse <deck>` 命令：CLI 直接调 `library::analyser::analyze`（后台线程），完成后发 `Command::SetAnalysis`。**无 `Command::Analyse`**
  - 其他命令原样转发 producer
- 无 `spawn_analysis` 函数；分析调用就是一行 `analyser::analyze(...)`

### 6. 删 vendored（可选）

- `crates/stratum-dsp`、`crates/timestretch` 若已有 git tag / 远端，改 cargo `git` 依赖
- 若仍是本地 fork，保留 vendored，用 `path = "../../stratum-dsp"` 引用
- 不确定就保留，不影响重构

### 7. 验证

- `cargo build --workspace` 通过
- `cargo test --workspace` 通过，含：
  - `core::BeatGrid`：`frame_at_beat` 外推、`floor_beat` 边界、`phase` 精度
  - `core::Key`：`traditional()` 对 C / Am / F#；`camelot()` 对 C=8B / Am=8A / G=9B / Em=9A
  - `audio::deck::jump`：相位保持、负跳钳制、`Seek::Quantized`
  - `library::grid_compiler`：刚性单段、多段、动态（`beats=Some(1)`）、混合、边界去重
  - `library::analyser::refine`：合成 128 BPM + 20ms 噪声 + 10% 漏拍 → BPM 误差 < 0.01；64 BPM 输入消歧到 128
- `scripts/phase_probe.sh` 通过

## 硬约束

1. **`audio` 的 `Cargo.toml` 不能出现 `hypermixx-library`。**
2. **`library` 的 `Cargo.toml` 不能出现 `hypermixx-audio`。**
3. **`core` 只能依赖 `serde`。**
4. **`media` 只依赖 `core` + `symphonia`。**
5. **`Command` 不含 `Analyse`。** producer 收到的所有命令都能执行。
6. **`BeatGrid` 只在 core 定义一次**，其他 crate `use hypermixx_core::BeatGrid`。
7. **`TimeShift` / `timeshift` 全部改名 `FlowShift` / `flowshift`。**
8. **`KeyReport` 全部改名 `Key`。**
9. **`GridSpec` 全部改名 `BeatSpec`。**

## 不做的事

- 不引入 SQLite
- 不实现 `loop` 环绕（只留 `LoopState` 占位）
- 不实现 `RANSAC`（`refine` 只用中位数 + LS）
- 不实现 `stems`（`media` 里留目录位置即可）
- 不实现流媒体（`Source` trait 已就绪，实现留后）
- 不改 vendored crate 内部

## 验收清单

- [ ] `cargo build --workspace` 通过
- [ ] `cargo test --workspace` 全绿
- [ ] `cargo tree -p hypermixx-audio` 不含 `hypermixx-library`
- [ ] `cargo tree -p hypermixx-library` 不含 `hypermixx-audio`
- [ ] `crates/hypermixx-audio/src/beatgrid.rs` 不存在
- [ ] `crates/hypermixx-audio/src/source/` 不存在
- [ ] `crates/hypermixx-audio/src/deck/timeshift.rs` 不存在
- [ ] `crates/hypermixx-analysis/` 不存在
- [ ] `grep -r "KeyReport" crates/` 无结果
- [ ] `grep -r "TimeShift" crates/` 无结果（除注释）
- [ ] `grep -r "GridSpec" crates/` 无结果
- [ ] `grep -r "Command::Analyse" crates/` 无结果
- [ ] `scripts/phase_probe.sh` 通过

## 一句话

**建 core / media / library 三个新 crate，把类型 / PCM / 分析各归其位；删 `beatgrid.rs`、`source/`、`timeshift.rs`、`hypermixx-analysis`；`audio` 和 `library` 零交叉依赖；`Command` 不含 `Analyse`，CLI 直接调 `analyser`；`KeyReport → Key`，`TimeShift → FlowShift`，`GridSpec → BeatSpec`。**
