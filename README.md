# HyperMixx

轻量、性能优先的跨平台 DJ 混音软件，Rust 音频引擎 + Flutter 桌面 UI。

## 功能

已实现：

- 双 deck 载入播放、任意位置无延迟跳转（全曲预解码缓存 + 最小预卷）
- keylock 变调不变速、pitch 移调
- BPM / 调性分析、beatgrid（渐进分析，播放头优先）
- beat sync（一次性对齐 + 速率锁 + 推子/nudge 软接管）
- beat loop / ManualLoop 手动定界（起点终点量化对齐 beatgrid）、beatjump（精确整拍跳距）
- cue / hotcue 召回
- deck FX（8 槽：回声/混响/滤波器等）、每 deck 三带 EQ + 滤波器 + 增益
- tempo 推子、交叉推子、master 音量
- RGB（mixxx 风格）/ 三色（bands）滚动波形、overview、loop/cue 标记
- 曲目元数据（标题/艺人/封面，lofty 提取）

待完善：beat grid edit、更多 FX/色彩 FX、曲库管理、MIDI 控制、midi guide、从 rkb/serato/vdj 导入。

## 项目结构

```
.
├── HyperMixx/                 # Rust workspace：引擎 + 分析 + FRB 桥
│   ├── crates/
│   │   ├── hypermixx-core     # 控制总线（ControlBus，Mixxx 模式）、拍钟、beatgrid、路径常量
│   │   ├── hypermixx-audio    # 音频后端、全曲预解码缓存（TrackCache）、deck 实时处理、DSP/FX/keylock
│   │   ├── hypermixx-analysis # 渐进波形分析、BPM/调性检测、能量信封（energy_envelope）
│   │   └── hypermixx-bridge   # flutter_rust_bridge 桥（cdylib → libhypermixx_bridge.so）
│   └── Cargo.toml             # workspace 清单（5 个成员已裁为 4 crate + 无旧 UI）
├── flutter/                   # Flutter 桌面界面（当前唯一 UI）
│   ├── lib/engine/            # 控制器：总线快照轮询、事件流、波形模型
│   ├── lib/painters/          # 波形/overview/播放头渲染
│   ├── lib/widgets/           # deck 面板、transport、pads、FX、tempo 等组件
│   ├── test/                  # widget 测试（注入假动作，不碰桥）
│   └── integration_test/      # 真引擎集成测试（见下方测试指南）
├── scripts/
│   ├── run_flutter.sh         # 构建 release 桥 + 设环境变量 + flutter run
│   └── gen_bridge.sh          # 改桥接口后重新生成 FRB 绑定
└── 实现方案.md                # 设计决策 + P0–P23 开发日记（gitignored）
```

数据流：`load → TrackCache 全曲预解码（filler 线程渐进填充）→ Deck 直读缓存喂 keylock 引擎 → 混音 → master`；分析线程独立解码 → `AnalysisEvent` 事件流 → 桥转发 → Flutter 画波形/网格。UI 与引擎通过**控制总线**通信（seqlock + watch），60Hz 快照驱动界面；解码永不在实时线程。

## 上手开发

依赖：Rust（edition 2024）、Flutter（Linux 桌面）、CMake、ALSA 等音频依赖库。

```bash
# 一键运行（自动构建 release 桥 + 环境变量 + flutter run）
scripts/run_flutter.sh [曲目路径]

# 或手动分步
cargo build --release -p hypermixx-bridge --manifest-path HyperMixx/Cargo.toml
export HYPERMIXX_BRIDGE_LIB="$(pwd)/HyperMixx/target/release/libhypermixx_bridge.so"
cd flutter && flutter run -d linux
```

改桥接口（`HyperMixx/crates/hypermixx-bridge/src/api.rs`）后重新生成绑定：

```bash
# 依赖 flutter_rust_bridge_codegen（cargo install --locked --version 2.12.0）
scripts/gen_bridge.sh
```

改完 Rust 侧（除 api.rs 注释外）通常不需要 regen——只有 `api.rs` 的 `#[frb]` 注解面变化才影响 Dart 绑定。

## 测试指南

| 层级 | 命令 | 说明 |
|---|---|---|
| Rust 单元 | `cd HyperMixx && cargo test --all-targets` | ≈1 分钟，约 200 个测试，无需音频设备 |
| Rust lint | `cargo clippy --all-targets -- -D warnings` | 必须零告警 |
| Flutter widget | `cd flutter && flutter analyze && flutter test` | ≈20 秒，99 个测试，注入假动作不碰桥 |
| 集成测试 | 见下 | 真引擎 + 真音频设备，必须逐个串行 |

集成测试（`integration_test/`，3 个文件，每个都起真引擎实时播放，**必须依次单独运行、不可并行**，否则各进程争音频设备互相干扰）：

```bash
cd flutter
export HYPERMIXX_BRIDGE_LIB="$(pwd)/../HyperMixx/target/release/libhypermixx_bridge.so"  # 先构建 release 桥

flutter test integration_test/deck_pads_test.dart -d linux
flutter test integration_test/fx_panel_test.dart -d linux
flutter test integration_test/eof_playhead_test.dart -d linux
```

测试环境变量：

| 变量 | 用途 |
|---|---|
| `HYPERMIXX_BRIDGE_LIB` | 桥 .so 路径（`flutter run` 与集成测试都需要；`run_flutter.sh` 自动设） |
| `HYPERMIXX_TRACK` | `run_flutter.sh` 初始载入的曲目路径 |
| `HYPERMIXX_TEST_TRACK` | bridge 元数据测试的真实曲目路径（默认路径不存在时该测试自动跳过） |

## 跨平台编译（Windows / macOS）

仓库目前只带 Linux runner，其他平台需在目标平台上生成工程（Rust 与 Flutter 工具链也需在对应平台安装）：

```bash
cd flutter
flutter create --platforms=windows .   # 一次性；新建 windows/ 目录
flutter create --platforms=macos .     # 一次性；新建 macos/ 目录

# 构建 Rust 桥
cargo build --release -p hypermixx-bridge --manifest-path ../HyperMixx/Cargo.toml

# 运行（桥库用 HYPERMIXX_BRIDGE_LIB 指定——自动查找只认 .so）
# Windows（PowerShell）：
$env:HYPERMIXX_BRIDGE_LIB = "$(pwd)\..\HyperMixx\target\release\hypermixx_bridge.dll"
flutter run -d windows
# macOS：
export HYPERMIXX_BRIDGE_LIB="$(pwd)/../HyperMixx/target/release/libhypermixx_bridge.dylib"
flutter run -d macos
```

注意：

- 窗口标题等平台配置在各平台 runner 里（Linux 在 `linux/runner/my_application.cc`）
- cpal 音频后端两平台均支持，但驱动与文件对话框行为**目前只在 Linux 实机验证过**，首次移植需实机测试
- Android 同理（`flutter create --platforms=android .`），另需 Rust NDK 交叉编译工具链，未实测

## 许可

MIT OR Apache-2.0
