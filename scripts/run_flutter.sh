#!/usr/bin/env bash
# 构建桥 crate 并以真引擎启动 Flutter（Linux 桌面）。
# 用法: scripts/run_flutter.sh [可选: 启动自动载入的曲目路径]
# 本脚本注入 HYPERMIXX_BRIDGE_LIB；参数 1 作为 HYPERMIXX_TRACK 自动载曲。
set -euo pipefail

cd "$(dirname "$0")/.."

cargo build --release -p hypermixx-bridge --manifest-path HyperMixx/Cargo.toml

export HYPERMIXX_BRIDGE_LIB="$(pwd)/HyperMixx/target/release/libhypermixx_bridge.so"
if [[ -n "${1:-}" ]]; then
  export HYPERMIXX_TRACK="$1"
fi

cd flutter
exec flutter run -d linux
