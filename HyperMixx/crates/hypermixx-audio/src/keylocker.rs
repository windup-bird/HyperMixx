//! Keylocker：变速不变调引擎的 trait 缝（P2）。
//!
//! 唯一实时实现 `TimestretchLocker` 包装 timestretch-rs 引擎（P1 spike 定案：
//! 音高保持 p95=0.22 cents、CPU p99=0.065、恒定延迟 560 帧@48k，详见
//! `crates/hypermixx-bench/src/bin/keylock_spike.rs` 与实现方案.md P1 决策表）。
//! Deck 只依赖 trait——未来换引擎或加回退实现无需改动 deck。
//!
//! # 引擎协议要点（spike 实证）
//! - seek：`reset()` → `set_track_position(frame)` → `warm_start(preroll)` →
//!   喂 preroll 帧；priming 输出被丢弃，收尾带 declick 淡入。
//! - `source_position()` 是 reset 后喂入坐标（不含音轨锚点）：播头 =
//!   feed_base + source_position()；欠载时冻结（EOF/欠载判停依据）。
//! - `finish()` 推 82 帧 padding 冲刷 resampler lookahead；ring 满时返回
//!   false，下一块重试。
//! - 引擎以 256 帧整块粒度消费 ring；`output_frames()`（delivered）含欠载
//!   静音，不能用于 EOF 判定。
//! - 速率 clamp 到 [0.25, 4.0]（MIN/MAX_TEMPO_RATE）。

use anyhow::Result;
use timestretch::engine::{Engine, EngineConfig, EngineHandles, EngineProfile};

/// 变速不变调引擎（Deck 视角的实时接口）。所有方法 infallible、零分配。
/// Send：deck 随引擎闭包跨线程传递（音频回调需要 Send）。
pub trait Keylocker: Send {
    /// 渲染一块交织立体声（每声道 out.len()/2 帧）。
    fn process(&mut self, out: &mut [f32]);
    /// 喂入交织立体声源数据，返回实际接受的帧数。
    fn push(&mut self, interleaved: &[f32]) -> usize;
    fn occupied_frames(&self) -> usize;
    /// 输出 out_frames 帧所需的源帧下界（含 resampler lookahead 余量）。
    fn demand_hint(&self, out_frames: usize, max_rate: f64) -> usize;
    /// 目标速率（下一块边界生效）。
    fn set_rate(&mut self, rate: f64);
    /// 在精确输出帧调度变速（P5 同步相位校正用）。
    fn set_rate_at(&mut self, rate: f64, at_output_frame: u64);
    /// keylock 开关（内建 512 帧交叉淡化；关闭 = 延迟匹配的纯 varispeed）。
    fn set_keylock(&mut self, on: bool);
    /// seek：有界排空 ring + 清管线（零分配，保留 release ramp 防 click）。
    fn reset(&mut self);
    /// 锚定喂入坐标到音轨帧（pre_analysis 工件映射用）。
    fn set_track_position(&mut self, track_frame: u64);
    /// 预卷：接下来 preroll 源帧跑图但不输出，结束带 declick 淡入。
    fn warm_start(&mut self, preroll_frames: u32);
    /// 推荐的 warm_start 预卷帧数（管线延迟 + 收敛余量）。
    fn warm_start_preroll_frames(&self) -> usize;
    fn pipeline_latency_frames(&self) -> usize;
    /// 欠载静音帧累计。
    fn underrun_frames(&self) -> u64;
    /// 引擎交付的输出帧累计（含欠载静音，不可用于 EOF 判定）。
    fn output_frames(&self) -> u64;
    /// 延迟补偿的当前出声源帧（喂入坐标）；欠载时冻结。
    fn source_position(&self) -> f64;
    /// EOF 冲刷；ring 满返回 false，下一块重试。
    fn finish(&mut self) -> bool;
    /// 当前是否宽频 profile（WideKeylock）。
    fn is_wide(&self) -> bool;
}

/// timestretch-rs 实时引擎薄包装：controller（控制）+ processor（音频
/// 线程）+ source（喂入）三句柄聚合，全部操作零分配。
pub struct TimestretchLocker {
    controller: timestretch::engine::EngineController,
    processor: timestretch::engine::EngineProcessor,
    source: timestretch::engine::SourceProducer,
    wide: bool,
}

impl TimestretchLocker {
    /// 构建引擎。
    ///
    /// `wide` 选 profile：key shift 生效时引擎速率 = r/p 可达 0.46–2.16，
    /// 超出 Keylock profile ±20.5% 的全 keylock 带 → WideKeylock（0.25–2.0，
    /// CPU 约为窄频 3.3×）；无 shift 用 Keylock（RK3399 预算内）。
    /// 512 帧（10.7ms）环形源容量 8192 帧 ≈ 171ms。
    pub fn build(sr: u32, wide: bool) -> Result<Self> {
        let config = EngineConfig {
            sample_rate: sr,
            channels: 2,
            profile: if wide {
                EngineProfile::WideKeylock
            } else {
                EngineProfile::Keylock
            },
            initial_tempo_rate: 1.0,
            max_block_frames: 256,
            source_capacity_frames: 8192,
            pre_analysis: None,
        };
        let EngineHandles {
            controller,
            processor,
            source,
        } = Engine::build(config)?;
        Ok(Self {
            controller,
            processor,
            source,
            wide,
        })
    }
}

impl Keylocker for TimestretchLocker {
    fn process(&mut self, out: &mut [f32]) {
        self.processor.process(out);
    }

    fn push(&mut self, interleaved: &[f32]) -> usize {
        self.source.push(interleaved)
    }

    fn occupied_frames(&self) -> usize {
        self.source.occupied_frames()
    }

    fn demand_hint(&self, out_frames: usize, max_rate: f64) -> usize {
        self.source.demand_hint(out_frames, max_rate)
    }

    fn set_rate(&mut self, rate: f64) {
        self.controller.set_tempo_rate(rate);
    }

    fn set_rate_at(&mut self, rate: f64, at_output_frame: u64) {
        self.controller.set_tempo_rate_at(rate, at_output_frame);
    }

    fn set_keylock(&mut self, on: bool) {
        self.controller.set_keylock(on);
    }

    fn reset(&mut self) {
        self.processor.reset();
    }

    fn set_track_position(&mut self, track_frame: u64) {
        self.source.set_track_position(track_frame);
    }

    fn warm_start(&mut self, preroll_frames: u32) {
        self.controller.warm_start(preroll_frames);
    }

    fn warm_start_preroll_frames(&self) -> usize {
        self.processor.warm_start_preroll_frames()
    }

    fn pipeline_latency_frames(&self) -> usize {
        self.processor.pipeline_latency_frames()
    }

    fn underrun_frames(&self) -> u64 {
        self.controller.underrun_frames()
    }

    fn output_frames(&self) -> u64 {
        self.controller.delivered_frames()
    }

    fn source_position(&self) -> f64 {
        self.controller.source_position()
    }

    fn finish(&mut self) -> bool {
        self.source.finish()
    }

    fn is_wide(&self) -> bool {
        self.wide
    }
}
