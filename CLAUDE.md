# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目

HyperMixx：轻量、性能优先的跨平台 DJ 混音软件。Rust 音频引擎（workspace，4 个 crate）+ Flutter 桌面 UI，通过 flutter_rust_bridge (FRB 2.12.0) 进程内桥接。目前只在x86 linux运行。

## 常用命令

```bash
# 一键运行（构建 release 桥 + 设环境变量 + flutter run）
scripts/run_flutter.sh [曲目路径]

# 手动分步
cargo build --release -p hypermixx-bridge --manifest-path HyperMixx/Cargo.toml
export HYPERMIXX_BRIDGE_LIB="$(pwd)/HyperMixx/target/release/libhypermixx_bridge.so"
cd flutter && flutter run -d linux
```

改了桥接口（`HyperMixx/crates/hypermixx-bridge/src/api.rs`）之后要重新生成 Dart 绑定：

```bash
scripts/gen_bridge.sh   # 依赖 flutter_rust_bridge_codegen 2.12.0（cargo install --locked --version 2.12.0）
```

改 Rust 侧其他文件通常不需要 regen——只有 `api.rs` 的 `#[frb]` 注解面变化才影响 Dart 绑定。

### 测试

| 层级 | 命令 | 说明 |
|---|---|---|
| Rust 单元 | `cd HyperMixx && cargo test --all-targets` | ≈1 分钟，约 200 个测试，无需音频设备 |
| Rust lint | `cd HyperMixx && cargo clippy --all-targets -- -D warnings` | 必须零告警 |
| Flutter widget | `cd flutter && flutter analyze && flutter test` | ≈20 秒，注入假动作，不碰真桥 |
| 集成测试 | 见下 | 真引擎 + 真音频设备 |

运行单个 Rust 测试：`cargo test -p hypermixx-audio test_name --manifest-path HyperMixx/Cargo.toml`。

集成测试（`flutter/integration_test/`，3 个文件，每个都起真引擎实时播放，**必须依次单独运行、不可并行**，否则多进程抢音频设备互相干扰）：

```bash
cd flutter
export HYPERMIXX_BRIDGE_LIB="$(pwd)/../HyperMixx/target/release/libhypermixx_bridge.so"  # 先构建 release 桥

flutter test integration_test/deck_pads_test.dart -d linux
flutter test integration_test/fx_panel_test.dart -d linux
flutter test integration_test/eof_playhead_test.dart -d linux
```

环境变量：

| 变量 | 用途 |
|---|---|
| `HYPERMIXX_BRIDGE_LIB` | 桥 .so 路径；`flutter run` 与集成测试都需要（`run_flutter.sh` 自动设） |
| `HYPERMIXX_TRACK` | `run_flutter.sh` / 引擎启动时初始载入的曲目路径 |
| `HYPERMIXX_TEST_TRACK` | bridge 元数据测试用的真实曲目路径（不存在时该测试自动跳过） |

## 架构

### Workspace 结构

```
HyperMixx/crates/
├── hypermixx-core      # 控制总线（ControlBus）、BeatGrid/BeatClock 纯数学、控制点路径常量
├── hypermixx-audio     # 音频后端（cpal）、全曲预解码缓存（TrackCache）、deck 实时处理、DSP/FX/keylock
├── hypermixx-analysis  # 渐进波形分析、BPM/调性检测（timestretch crate）、energy_envelope、全局刚性网格拟合
└── hypermixx-bridge    # flutter_rust_bridge 桥，编译为 cdylib（libhypermixx_bridge.so）+ rlib（供 cargo test）
```

依赖方向严格自上而下：`core` 无 IO/无实时依赖，是全项目最小公共层；`audio`/`analysis` 依赖 `core`；`bridge` 依赖前三者并对外暴露 FRB 接口。

`flutter/lib/`：
- `engine/` — `EngineController`（单例，60Hz tick）+ `DeckController`（每 deck 细粒度 notifier），FRB 生成绑定在 `lib/src/rust/`
- `painters/` — 波形/overview/播放头渲染
- `widgets/` — deck 面板、transport、pads、FX、tempo 等组件

### 三条数据流

1. **播放路径（实时线程，永不阻塞）**：`load → TrackCache 全曲预解码（filler 线程渐进填充）→ Deck 直读缓存喂 keylock 引擎 → 混音 → master`。解码/IO 绝不在音频实时回调里发生。
2. **控制路径**：UI 与引擎之间唯一的通信面是 `ControlBus`（`hypermixx-core/src/control.rs`）——Mixxx `ControlObject` 思想的 Rust 版。每个控制点是 `path -> SeqLock<f64>`（如 `"Deck1.play"`、`"Master.crossfader"`，常量见 `hypermixx-core::paths`）。读侧无锁（seqlock），写侧罕见（UI/MIDI 事件率）。UI 侧 `EngineController` 60Hz 轮询 `snapshotAll()` 拿全量快照分发给两个 `DeckController`；写操作走 `busSet(path, value)`。新增控制点：先在 `hypermixx-core::paths` 加常量，音频侧读取，Flutter 侧通过 `busSet`/快照读写——不要写裸字符串。
3. **分析路径**：分析线程独立解码 → `AnalysisEvent` 事件流（`hypermixx-analysis::segment`）→ 桥转发为 `AnalysisEventWire`（Segment → TrackAnalysis → Done/Failed，按 `generation` 计数区分新旧载曲，防止陈旧事件覆盖）→ StreamSink 推流给 Flutter 画波形/网格。

### 桥（hypermixx-bridge）

- `api.rs`：FRB 注解的对外接口，是 codegen 的唯一输入。60Hz 热路径全部 `#[frb(sync)]`（同步调用、零分配）；慢操作（载曲、读元数据）是普通 `async fn`；分析事件走 `StreamSink` 参数。所有导出函数不能 panic——panic 跨 FFI 会崩掉整个 Flutter 进程，桥未初始化时统一返回默认值/空事件。
- `bridge.rs`：不带 FRB 注解的内部逻辑（引擎/总线/分析生命周期），可脱离 codegen 单独做单元测试。
- wire 类型约定：热路径字段一律 `u32`/`f64`/字符串，不用 `usize`（会变成 Dart BigInt）。

### BeatGrid

`hypermixx-core::beatgrid`：刚性节拍网格 = 恒定 BPM + 首拍秒偏移（offset_secs），拍点公式 `offset + k·period`（k 可为负，外推到曲首之前）。P4（网格编辑）与 P5（同步相位）共用同一套数学。`hypermixx-analysis::global_grid` 是全曲拟合刚性网格的分析算法，与分段网格（tempo_segments，自研算法参考用）分开。

### 依赖注意

- `timestretch` + `signalsmith-stretch`：keylock 变调不变速引擎 + BPM/调性检测，钉版本 `0.11`（README 注明曾用过本地 fork 修 DSP/分析问题，提交仓库时切回 crates.io 版本，注意行为差异）。
- `flutter_rust_bridge` 两侧必须钉同一版本（Cargo.toml workspace 依赖与 `flutter/pubspec.yaml` 都是 `2.12.0`），改动一侧要同步另一侧并跑 `gen_bridge.sh`。
</content>
