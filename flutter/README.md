# HyperMixx Flutter UI

Flutter 客户端（UI 层）。引擎、分析、桥接代码都在 Rust workspace（`../HyperMixx/`）。

- 项目总览、构建、测试指南：见仓库根 `../README.md`
- 修改桥接口（`HyperMixx/crates/hypermixx-bridge/src/api.rs`）后需重生成绑定：`../scripts/gen_bridge.sh`
