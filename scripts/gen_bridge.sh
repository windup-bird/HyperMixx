#!/usr/bin/env bash
# 重新生成 FRB 桥接代码（crates/hypermixx-bridge/src/api.rs 变更后运行）。
# 依赖：flutter_rust_bridge_codegen（cargo install --locked --version 2.12.0）
set -euo pipefail

cd "$(dirname "$0")/../flutter"

~/.cargo/bin/flutter_rust_bridge_codegen generate
dart run build_runner build

echo "gen_bridge 完成"
