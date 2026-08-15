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
- RGB / 3-bands

开发中

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

## 许可

MIT
