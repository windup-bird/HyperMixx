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
  [deck] load <path> [bpm]     decode a file (deck defaults to the focused one)
  [deck] analyse               run the analyser and publish its grid
  [deck] play | pause          transport
  [deck] jump <frame>          seek to a frame (1 second = 44100 frames)
  [deck] beatjump <beats>      seek by whole beats, keeping the phase
  [deck] rate <ratio>          set tempo rate (1.0 = unity, 0.5 = half speed)
  [deck] profile <name>        tape / keylock / wide (default: tape)
  [deck|master] fx ...         effects on a chain (see `fx help`)
  state                        show every deck
  zoom in|out|fit              waveform zoom (UI only)
  quit                         exit
```

命令是 target-first：行首可写 `deck0` / `0` / `master` 覆盖默认目标，省略时作用于 REPL 的 deck0（TUI 里是焦点 deck，`Tab` 切换）。

### 示例会话

```bash
$ cargo run -p hypermixx-cli
hypermixx 0.1.0 — 2 decks, 44100Hz stereo, backend Auto. `help` for commands, `quit` to exit.

hypermixx> load test.mp3 122
deck0 loaded: 18462369 frames (7:00.648)

hypermixx> play
hypermixx> state
deck0  playing 0:00.123 / 7:00.648  [5403/18462369]  122.0 BPM  --

hypermixx> beatjump 16
hypermixx> state
deck0  playing 0:04.567 / 7:00.648  [203456/18462369]  122.0 BPM  --

hypermixx> fx add filter
hypermixx> fx list
deck0 fx[0] filter [on]
    value = 0.0000
    resonance = 0.2000

hypermixx> fx set filter value -0.3     # 槽位可用名称,不必记序号
hypermixx> fx set filter resonance 0.6 # -1..+1:负=低通、正=高通、0=全开;共振=峰值增益
hypermixx> pause
hypermixx> quit
```

### 分析后端选择

```bash
cargo run -p hypermixx-cli -- --backend auto       # 默认: stratum 优先, 失败回退 timestretch
cargo run -p hypermixx-cli -- --backend stratum    # 仅 stratum-dsp
cargo run -p hypermixx-cli -- --backend timestretch # 仅 timestretch(当前返回 Unsupported)
```

### 终端 UI(`--tui`)

```bash
cargo run -p hypermixx-cli -- --tui
```

自上而下:全局 TOP 栏 / 每个 deck 一块（文件名 / 三频段波形 / 传输信息，多 deck 纵向排列）/ response 日志 / command 输入框。

- 波形为 ratatui `Canvas` + Braille:playhead 固定在面板水平中心,视口每帧按当前播放帧重算,波形右→左滚动;两 deck 共用同一缩放。三个频段**在同一基线上叠加**（不是各占一行），每个边界是该频段真实幅度；绘制顺序 low→mid→high 使颜色自下而上为 白=高频、绿=中频、蓝=低频。beatgrid 暗色叠加。
- TOP 栏:采样率、后端、deck 数、UI 帧耗时、引擎响应延迟、输出环标称延迟、master/cue 电平与丢样本数。
- command 是一个常驻边框输入框，**直接打字即可**（无需 `:` 前缀）。行首显示当前 deck (`deck0 ▸`)，省略 deck 的命令就作用于它；`Tab` / `Shift+Tab` 切换焦点 deck（输入框前缀与 deck 边框高亮同步）。
- 编辑：`Enter` 执行（当前词有补全且候选与已输入不同时，先应用补全，再按一次才提交）、`Esc` 清空、`Tab` 切换 deck（有补全弹窗时接受补全）、`↑/↓` 选补全（无补全时翻命令历史）、`←/→/Home/End/Backspace/Delete` 编辑、`PgUp/PgDn` 滚 response、`Ctrl+C` 退出。除此之外没有单键快捷键，所有引擎操作都通过命令完成。
- 补全：命令、路径、`fx` 子命令/效果名/参数；fx 槽位既可用**效果名**也可用序号，候选里名字带 `slot N` 提示、序号带名字提示。启动时会**静默预取**各链槽位并在本地跟踪 add/remove，所以直接用效果名即可，**不需要先 `fx list`**（预取不会写进 response）。
- `zoom in|out|fit` 控制波形缩放。

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
