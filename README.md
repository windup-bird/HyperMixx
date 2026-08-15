# HyperMixx

开源 DJ 混音软件 —— Rust 实时音频引擎 + Flutter 桌面界面。

> 目标平台：香橙派 4 一体机（RK3399 / aarch64）与 Linux PC。轻量、性能优先，全依赖宽松许可（MIT OR Apache-2.0）。

## 功能

- 双 deck 播放（keylock 变调不变速 / 变速不变调）
- BPM 检测 + 调性检测 + beatgrid 自动网格
- beat sync 同步（leader 判定、临时加减速、推子软接管）
- 手动 loop（In/Out 手动定界 + 整拍量化）、beatloop、beatjump
- 主 CUE + 16 槽 hotcue（落点 / 试听 / 召回）
- 每 deck 主 FX 通道（echo / gate 等，旋钮 + 开关 + 选型菜单）
- 三段 EQ / 滤波 / 增益 / 交叉推子混音台
- RGB 与 3-bands 双模式滚动波形 + 全区预览（loop / cue 标记、拍轴网格）
- 曲库（进行中）、MIDI（进行中）

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
# 一键构建桥并启动 Flutter（真引擎）
scripts/run_flutter.sh [可选曲目路径]

# 或手动分步
cargo build --release -p hypermixx-bridge --manifest-path HyperMixx/Cargo.toml
export HYPERMIXX_BRIDGE_LIB="$(pwd)/HyperMixx/target/release/libhypermixx_bridge.so"
cd flutter && flutter run -d linux
```

`HYPERMIXX_TRACK=路径` 可指定启动自动载入的曲目。

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

MIT OR Apache-2.0（自研代码；依赖栈全宽松许可，零 GPL）。
