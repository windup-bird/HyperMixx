//! hypermixx-bridge：Flutter 前端与 HyperMixx 引擎之间的进程内桥。
//!
//! 结构：`api` = FRB 注解的对外接口（codegen 输入）；`bridge` = 非注解的
//! 内部逻辑（引擎/总线/分析生命周期），可脱离 codegen 做单元测试。
//!
//! 线程模型：引擎/解码/分析全在 Rust 线程；Dart 侧 60Hz 经 `#[frb(sync)]`
//! 单调用取快照（微秒级，无分配），分析事件经 StreamSink 推流。

mod frb_generated;

pub mod api;
pub mod bridge;
