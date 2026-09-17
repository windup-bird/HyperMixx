# Hypermixx

实时音频播放引擎 —— Rust workspace，五层单向分层架构。

双 Deck 播放、节拍网格、按拍跳转（相位保持）、时间拉伸（Tape / Keylock / WideKeylock）、
命令行界面。引擎域固定 44.1 kHz 立体声，与目标设备（ALSA `default`）原生时钟对齐，
cpal 回调直通，零采样率转换。

## 架构

```
cli ──→ core / media / audio / library
audio ──→ core, media          (+ timestretch, git 依赖 rev 锁定)
library ──→ core, media        (+ stratum-dsp, git 依赖 rev 锁定)
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
│           ├── timestretch.rs # 占位:上游未暴露离线分析,返回 Unsupported
│           └── refine.rs      # fit_rigid:中位数周期 + 最小二乘 + 倍频归位
│
└── hypermixx-cli/        # 前端(二进制,依赖全部四层)
    └── src/main.rs       # 行解析 / 后台 decode+analyse / 响应打印线程
```

### 外部依赖(git, rev 锁定)

| crate | 来源 | 锁定 |
|---|---|---|
| `stratum-dsp` | `github.com/HLLMR/stratum-dsp`(第三方库) | `rev = 758e0b6` |
| `timestretch` | `github.com/robmorgan/timestretch-rs`(经 gh-proxy 镜像地址) | `rev = 2628090` |

不随本仓库分发;升级 = 改 rev + `cargo update -p <crate>`。

## 功能特性

- **双 Deck 独立播放**：各自 transport、独立时钟、独立跳转
- **非阻塞跳转**：`jump()` 创建新 Flow → 后台预热线程 → 下个块切换；
  `cued_from` 补偿预热期间已播放的帧量，保证跨 Deck 相位锁定
- **按拍跳转（相位保持）**：`beatjump ±N` 从拍内任意位置跳 N 拍，
  落点保持相同相位，兼容非均匀网格
- **时间拉伸三 Profile**：
  - `Tape`：零延迟直通，ratio=1.0 时 bit-exact 透传
  - `Keylock`：SOLA 算法，音高锁定
  - `WideKeylock`：相位声码器，全频谱音高锁定
- **节拍网格**：`BeatGrid` 绝对帧位置，BPM 派生不存储，无累积误差
- **分析管线**：`Source → downmix_to_mono → stratum-dsp → fit_rigid → BeatSpec → GridCompiler → TrackAnalysis`
- **可编辑网格真源**：`BeatSpec`（段落化 BPM + 起点 + 拍数），`GridCompiler` 三段编译
- **惰性缓存**：`TrackInfo` 持 `BeatSpec`，改 spec 即失效编译缓存

## 构建与运行

### 前置条件

- Rust 2021 edition（建议 1.70+）
- 音频输出设备（ALSA / CoreAudio / WASAPI）
- 系统依赖：`libasound2-dev`（Linux ALSA）

### 构建

```bash
cargo build --workspace
```

### 运行 CLI

```bash
cargo run -p hypermixx-cli
```

启动后进入交互式命令行：

```
hypermixx> help
commands (2 decks, ids 0..1):
  load <deck> <path> [bpm]   decode a file; a given bpm builds a fixed grid, skipping analysis
  analyse <deck>             run the analyser on the deck's track and publish its grid
  play <deck>                start that deck
  pause <deck>               stop it, keeping the position
  jump <deck> <frame>        seek to a frame (1 second = 44100 frames)
  beatjump <deck> <beats>    seek by whole beats, keeping the phase
  rate <deck> <ratio>        set tempo rate (1.0 = unity, 0.5 = half speed)
  profile <deck> <name>      tape / keylock / wide (default: tape)
  state                      show every deck
  quit                       exit
```

### 示例会话

```bash
$ cargo run -p hypermixx-cli
hypermixx 0.1.0 — 2 decks, 44100Hz stereo, backend Auto. `help` for commands, `quit` to exit.

hypermixx> load 0 test.mp3 122
deck0 loaded: 18462369 frames (7:00.648)

hypermixx> play 0
hypermixx> state
deck0  playing 0:00.123 / 7:00.648  [5403/18462369]  122.0 BPM  --

hypermixx> beatjump 0 16
hypermixx> state
deck0  playing 0:04.567 / 7:00.648  [203456/18462369]  122.0 BPM  --

hypermixx> pause 0
hypermixx> quit
```

### 分析后端选择

```bash
cargo run -p hypermixx-cli -- --backend auto       # 默认: stratum 优先, 失败回退 timestretch
cargo run -p hypermixx-cli -- --backend stratum    # 仅 stratum-dsp
cargo run -p hypermixx-cli -- --backend timestretch # 仅 timestretch(当前返回 Unsupported)
```

## 测试

```bash
# 全量测试
cargo test --workspace

# 仅引擎测试
cargo test -p hypermixx-audio

# 仅分析测试
cargo test -p hypermixx-library

# 相位锁定回归(需要 test.mp3)
cargo test -p hypermixx-audio --test beatlock

# 设备信息(诊断播放速度)
cargo run -p hypermixx-audio --example device_info
```

### 相位锁定验证

```bash
./scripts/phase_probe.sh
```

双 Deck 从 frame 0 同点起播，每隔 N 拍给 deck1 `beatjump +N`，
验证相位差增量恒等于 N 拍的网格距离。

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

## 采样率约定

引擎域固定 44.1 kHz：目标设备(ALSA `default`)原生 44.1 kHz，`preferred_config` 按引擎率
请求即直通。历史教训——引擎 48k 而设备 44.1k 时，ALSA 静默接受 48k 请求却按 44.1k 时钟
消费 → 慢放 8.8% + 音调低 8.8%。多设备自适应(设备率≠引擎率时在回调内重采样)是后续工作。

## 常量

| 常量 | 值 | 说明 |
|---|---|---|
| `SAMPLE_RATE` | 44_100 | 引擎域采样率 |
| `CHANNELS` | 2 | 立体声 |
| `BLOCK_SIZE` | 256 | 每块帧数 |
| `OUTPUT_RING_CAPACITY` | 4096 | 输出环形缓冲容量(帧) |
| `PREFILL_FRAMES` | 2048 | 启动前预填充静音帧数 |
| `DECK_COUNT` | 2 | Deck 数量 |
| `DECK_MIX_GAIN` | 0.5 | 双 Deck 混音增益 |

## 许可证

MIT
