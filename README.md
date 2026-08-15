# HyperMixx

轻量、性能优先的跨平台DJ混音软件，使用Rust+Flutter构建。

## 功能

已实现

- 2 deck load and play
- keylock
- BPM/key analysis
- beat sync
- loop / beatjump
- cue / hotcue
- deck fx / filter
- RGB / 3-bands waveform

待完善
- beat sync 逻辑完善
- loop / beatjump 微小偏移
- beat grid analyze 偏差
- RGB / 3-bands waveform 可视性

开发中
- beat grid edit
- more fx / color fx
- library management
- midi control
- import from rkb/serato/vdj


## 架构

```
HyperMixx/                # Rust workspace：音频引擎 + FRB 桥
  crates/
    hypermixx-core        # 控制总线（ControlBus）、拍钟、网格、共享类型
    hypermixx-audio       # 输出后端、解码/缓存读取、deck 实时处理、DSP
    hypermixx-analysis    # BPM/调性/波形分析
    hypermixx-bridge      # flutter_rust_bridge 桥（cdylib → libhypermixx_bridge.so）
    hypermixx-ui / -app   # Slint 旧版 UI（开发遗留）
flutter/                  # Flutter 桌面界面（当前主 UI）
```

- UI 与引擎通过**控制总线**通信（Mixxx ControlObject 模式，seqlock + watch），60Hz 快照驱动界面
- 解码永不在实时线程（CachingReader + SPSC chunk ring）
- FX 用不可变 manifest → processor 结构

## 构建与运行

依赖：Rust（edition 2024）、Flutter（Linux 桌面）、CMake、systemd 依赖库。

```bash
# 一键构建桥并启动(Linux)
scripts/run_flutter.sh

# 或手动分步
cargo build --release -p hypermixx-bridge --manifest-path HyperMixx/Cargo.toml
export HYPERMIXX_BRIDGE_LIB="$(pwd)/HyperMixx/target/release/libhypermixx_bridge.so"
cd flutter && flutter run -d linux
```

## 测试

```bash
# Rust（引擎 + 桥）
cd HyperMixx && cargo test --all-targets && cargo clippy --all-targets -- -D warnings

# Flutter widget 测试
cd flutter && flutter analyze && flutter test

# 集成测试（真引擎 + 真桥，必须依次单独运行）
HYPERMIXX_BRIDGE_LIB="$(pwd)/HyperMixx/target/release/libhypermixx_bridge.so" \
  flutter test integration_test/deck_pads_test.dart -d linux
HYPERMIXX_BRIDGE_LIB="$(pwd)/HyperMixx/target/release/libhypermixx_bridge.so" \
  flutter test integration_test/fx_panel_test.dart -d linux
HYPERMIXX_BRIDGE_LIB="$(pwd)/HyperMixx/target/release/libhypermixx_bridge.so" \
  flutter test integration_test/eof_playhead_test.dart -d linux
```

## 跨平台编译（Windows / macOS）

现状：仓库目前只带 Linux 桌面 runner（`flutter/linux/`）。Dart UI 与 Rust 引擎本身跨平台，但 Flutter 不支持从 Linux 交叉编译 Windows/macOS 目标——**需在对应主机上操作**：

```bash
cd flutter

# 1. 生成平台 runner（一次性；新建 windows/ 或 macos/ 目录，可提交）
flutter create --platforms=windows .   # Windows
flutter create --platforms=macos .     # macOS

# 2. 构建 Rust 桥（本机工具链；产物在 HyperMixx/target/release/）
cargo build --release -p hypermixx-bridge --manifest-path ../HyperMixx/Cargo.toml

# 3. 运行（桥库必须用 HYPERMIXX_BRIDGE_LIB 指定——自动查找只认 .so）
# Windows（PowerShell）：
$env:HYPERMIXX_BRIDGE_LIB = "$(pwd)\..\HyperMixx\target\release\hypermixx_bridge.dll"
flutter run -d windows
# macOS：
export HYPERMIXX_BRIDGE_LIB="$(pwd)/../HyperMixx/target/release/libhypermixx_bridge.dylib"
flutter run -d macos
```

注意事项：

- 平台窗口标题等配置在各自 runner 里（Linux 在 `linux/runner/my_application.cc`；Windows/macOS 生成后按需改）
- 音频后端 cpal 两平台均支持，但驱动与文件对话框行为**目前只在 Linux 实机验证过**，首次移植需实机测试
- Android 同理（`flutter create --platforms=android .`），另需 Rust NDK 交叉编译工具链，未实测

## 许可

MIT
