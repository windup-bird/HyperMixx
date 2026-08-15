//! FX 引擎：不可变 manifest → 可实例化处理器（实现方案.md 决策 #4）。
//!
//! 协议（全部方法 infallible、热路径零分配，仿 Keylocker 的 trait 缝）：
//! - `EffectManifest` 静态声明参数（自然单位）；`instantiate(id, sr)` 产出
//!   `Box<dyn EffectProcessor>`。自研算法只会 OOM 失败 → infallible。
//! - 音频线程每块：rack 先 `set_slot_params`（快照 + 内部 clamp）再 `process`。
//!   热路径禁止分配与逐采样 libm 调用（允许 set_params 每块算系数）。
//! - 效果只输出湿声；干湿混合由 rack 统一（10ms 平滑防爆音）。
//! - 槽位换型是唯一允许在音频线程分配的操作（用户发起、低频；
//!   deck 侧按 fx_type 快照去抖，仿 rebuild_pending）。
//! - 无网格（grid_bpm ≤ 0）时 `FxContext::default()`：gate 逐位直通、
//!   echo sync 回落自由时值。

mod delay;
mod distortion;
mod echo;
mod filter_linear;
mod filter_moog;
mod flanger;
mod gate;
mod lfo;
mod manifest;
mod phaser;
mod rack;
mod reverb;

pub use manifest::{all_manifests, manifest, EffectManifest, ParamKind, ParamSpec};
pub use rack::FxRack;

/// 块首拍上下文（deck 在 process 内计算——sync 可能在 update_params 与
/// process 之间改 rate，process 里拿到的才是本块真实出声节奏）。
#[derive(Clone, Copy, Debug)]
pub struct FxContext {
    /// 块首绝对拍（拍序号 + 拍内相位）；gate 周期定位用。无网格 0。
    pub beats_total: f64,
    /// 当前拍内相位 0..1。
    pub beat_phase_01: f32,
    /// 出声拍周期（秒）= 60/(grid_bpm × 实际速率)；无网格 ∞。
    pub beat_period_secs: f64,
    pub grid_valid: bool,
}

impl Default for FxContext {
    fn default() -> Self {
        Self {
            beats_total: 0.0,
            beat_phase_01: 0.0,
            beat_period_secs: f64::INFINITY,
            grid_valid: false,
        }
    }
}

/// 效果 ID（判别值 = ControlBus 上的 fx_type 值，勿改）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum EffectId {
    None = 0,
    Echo = 1,
    Flanger = 2,
    Phaser = 3,
    Reverb = 4,
    Distortion = 5,
    FilterLinear = 6,
    FilterMoog = 7,
    Gate = 8,
}

impl EffectId {
    /// 总线值 → 效果（0 与越界都表示空槽）。
    pub fn from_bus(v: f64) -> Option<EffectId> {
        match v as i64 {
            1 => Some(EffectId::Echo),
            2 => Some(EffectId::Flanger),
            3 => Some(EffectId::Phaser),
            4 => Some(EffectId::Reverb),
            5 => Some(EffectId::Distortion),
            6 => Some(EffectId::FilterLinear),
            7 => Some(EffectId::FilterMoog),
            8 => Some(EffectId::Gate),
            _ => None,
        }
    }

    pub fn to_bus(self) -> f64 {
        self as u8 as f64
    }
}

/// 实时效果处理器（Send：音频线程可能迁入后端回调，同 Keylocker）。
pub trait EffectProcessor: Send {
    /// 原位处理 frames 帧交织立体声（out.len() == frames*2），只输出湿声。
    fn process(&mut self, out: &mut [f32], frames: usize, ctx: &FxContext);
    /// 每块参数快照（自然单位，按位对应 manifest params）。
    /// 须内部 clamp、幂等、廉价（无分配、无逐采样 libm）。
    fn set_params(&mut self, params: &[f32; 4]);
    /// 清全部内部状态（load / 换型时调用）。
    fn reset(&mut self);
}

/// 按 id 实例化效果（槽位换型时调用，允许分配）。
pub fn instantiate(id: EffectId, sr: f32) -> Option<Box<dyn EffectProcessor>> {
    match id {
        EffectId::None => None,
        EffectId::Echo => Some(Box::new(echo::Echo::new(sr))),
        EffectId::Flanger => Some(Box::new(flanger::Flanger::new(sr))),
        EffectId::Phaser => Some(Box::new(phaser::Phaser::new(sr))),
        EffectId::Reverb => Some(Box::new(reverb::Reverb::new(sr))),
        EffectId::Distortion => Some(Box::new(distortion::Distortion::new(sr))),
        EffectId::FilterLinear => Some(Box::new(filter_linear::FilterLinear::new(sr))),
        EffectId::FilterMoog => Some(Box::new(filter_moog::FilterMoog::new(sr))),
        EffectId::Gate => Some(Box::new(gate::Gate::new(sr))),
    }
}
