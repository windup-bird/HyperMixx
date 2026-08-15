//! FX rack：每 deck 8 槽串行（上一槽输出 = 下一槽输入），统一干湿混。
//!
//! 热路径零分配；槽位换型 = 唯一允许分配的操作（用户发起）。
//! 干湿混经 10ms Smoother 斜坡：
//! - 换型/开启：mix 从 0 爬升 → 淡入无 click（新效果从静默起）；
//! - 关闭：淡出期间持续处理（新鲜湿内容混出，无 DC 冻结伪影），
//!   settled 后整槽跳过 DSP（状态冻结免 CPU；重开时冻结尾在斜坡下
//!   淡入，click-free，即"旁路冻结"惯例）。gate 不受冻结相位影响
//!   ——每块从 ctx.beats_total 重锚。

use super::{EffectId, EffectProcessor, FxContext, instantiate};
use crate::dsp::smoother::Smoother;

/// 干湿混平滑时间常数（秒）。
const MIX_TAU_S: f32 = 0.010;

struct FxSlot {
    processor: Option<Box<dyn EffectProcessor>>,
    enabled: bool,
    drywet: f32,
    params: [f32; 4],
    /// current = 实际混音比例，target = enabled × drywet。
    mix: Smoother,
}

impl FxSlot {
    fn new(sr: f32) -> Self {
        let coeff = 1.0 - (-1.0 / (MIX_TAU_S * sr)).exp();
        Self {
            processor: None,
            enabled: false,
            drywet: 0.0,
            params: [0.0; 4],
            mix: Smoother::new(0.0, coeff),
        }
    }
}

pub struct FxRack {
    sr: f32,
    slots: [FxSlot; 8],
    /// 干声暂存（每块 256 帧 = 512 f32；防御性 resize 只发生一次）。
    scratch: Vec<f32>,
}

impl FxRack {
    pub fn new(sr: f32) -> Self {
        Self {
            sr,
            slots: std::array::from_fn(|_| FxSlot::new(sr)),
            scratch: vec![0.0; 512],
        }
    }

    /// 槽位换型（用户操作；音频线程唯一允许分配的点）。
    /// 新实例从静默起 + mix 归零重新爬升 → 10ms 淡入无 click。
    pub fn set_slot_type(&mut self, slot: usize, id: Option<EffectId>) {
        let s = &mut self.slots[slot];
        s.processor = id.and_then(|id| instantiate(id, self.sr));
        s.mix.set_immediate(0.0);
        if let Some(p) = s.processor.as_mut() {
            p.reset();
        }
    }

    /// 每块参数快照（update_params 调用；内部即时 clamp，廉价幂等）。
    pub fn set_slot_params(&mut self, slot: usize, enabled: bool, drywet: f32, params: [f32; 4]) {
        let s = &mut self.slots[slot];
        s.enabled = enabled;
        s.drywet = drywet.clamp(0.0, 1.0);
        s.params = params;
        if let Some(p) = s.processor.as_mut() {
            p.set_params(&s.params);
        }
    }

    /// 原位串行处理（EQ 之后、gain 之前）。
    pub fn process(&mut self, out: &mut [f32], frames: usize, ctx: &FxContext) {
        let n = frames * 2;
        if self.scratch.len() < n {
            self.scratch.resize(n, 0.0);
        }
        for slot in 0..self.slots.len() {
            let s = &mut self.slots[slot];
            let target = if s.enabled { s.drywet } else { 0.0 };
            s.mix.set_target(target);
            // 完全旁路（settled 于 0）：跳过 DSP，状态冻结
            if s.mix.settled() && target == 0.0 {
                continue;
            }
            let Some(p) = s.processor.as_mut() else {
                continue;
            };
            self.scratch[..n].copy_from_slice(&out[..n]);
            p.process(&mut out[..n], frames, ctx);
            for (i, v) in out.iter_mut().enumerate().take(n) {
                let m = s.mix.step();
                *v = self.scratch[i] * (1.0 - m) + *v * m;
            }
        }
    }

    pub fn reset(&mut self) {
        for s in self.slots.iter_mut() {
            if let Some(p) = s.processor.as_mut() {
                p.reset();
            }
            s.mix.set_immediate(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    /// 测试效果：乘性增益（p1 = 增益，内部 clamp 0..10）。
    struct GainFx {
        g: f32,
    }
    impl EffectProcessor for GainFx {
        fn process(&mut self, out: &mut [f32], _frames: usize, _ctx: &FxContext) {
            for v in out.iter_mut() {
                *v *= self.g;
            }
        }
        fn set_params(&mut self, params: &[f32; 4]) {
            self.g = params[0].clamp(0.0, 10.0);
        }
        fn reset(&mut self) {}
    }

    fn set_test_processor(rack: &mut FxRack, slot: usize, g: f32) {
        let mut p = Box::new(GainFx { g });
        p.set_params(&[g, 0.0, 0.0, 0.0]);
        rack.slots[slot].processor = Some(p);
    }

    fn sine_blocks(n: usize) -> Vec<f32> {
        (0..n * 512)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * (i / 2) as f32 / SR).sin() * 0.5)
            .collect()
    }

    fn run(rack: &mut FxRack, input: &[f32], frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        for chunk in input.chunks(frames * 2) {
            out.copy_from_slice(chunk);
            rack.process(&mut out, frames, &FxContext::default());
        }
        out
    }

    #[test]
    fn empty_rack_is_bitwise_passthrough() {
        let mut rack = FxRack::new(SR);
        let input = sine_blocks(16);
        let out = run(&mut rack, &input, 256);
        assert_eq!(out, input[input.len() - 512..], "空 rack 逐位直通");
    }

    #[test]
    fn drywet_zero_is_bitwise_passthrough() {
        let mut rack = FxRack::new(SR);
        set_test_processor(&mut rack, 0, 3.0);
        rack.set_slot_params(0, true, 0.0, [3.0, 0.0, 0.0, 0.0]);
        let input = sine_blocks(16);
        let out = run(&mut rack, &input, 256);
        assert_eq!(out, input[input.len() - 512..], "干湿 0 逐位直通");
    }

    #[test]
    fn full_wet_scales_after_fade() {
        let mut rack = FxRack::new(SR);
        set_test_processor(&mut rack, 0, 3.0);
        rack.set_slot_params(0, true, 1.0, [3.0, 0.0, 0.0, 0.0]);
        let input = sine_blocks(64); // ~0.34s：淡入完成
        let mut out = vec![0.0; 512];
        for chunk in input.chunks(512) {
            out.copy_from_slice(chunk);
            rack.process(&mut out, 256, &FxContext::default());
        }
        let expect: Vec<f32> = input[input.len() - 512..].iter().map(|v| v * 3.0).collect();
        let err: f32 = out
            .iter()
            .zip(expect.iter())
            .map(|(a, b)| (a - b).abs())
            .sum::<f32>()
            / 512.0;
        assert!(err < 1e-3, "稳态输出 ≈ 3× 输入, err={err}");
    }

    #[test]
    fn disable_returns_to_dry_bitwise() {
        let mut rack = FxRack::new(SR);
        set_test_processor(&mut rack, 0, 3.0);
        rack.set_slot_params(0, true, 1.0, [3.0, 0.0, 0.0, 0.0]);
        let input = sine_blocks(160);
        let mut out = vec![0.0; 512];
        // 前 64 块：淡入到满湿
        for chunk in input[..64 * 512].chunks(512) {
            out.copy_from_slice(chunk);
            rack.process(&mut out, 256, &FxContext::default());
        }
        assert_ne!(out[..], input[..512], "满湿态应与干声不同");

        // 关闭后 0.5s（96 块 > 10ms 淡出）：应逐位回到干声
        rack.set_slot_params(0, false, 1.0, [3.0, 0.0, 0.0, 0.0]);
        for chunk in input[64 * 512..].chunks(512) {
            out.copy_from_slice(chunk);
            rack.process(&mut out, 256, &FxContext::default());
        }
        assert_eq!(out[..], input[input.len() - 512..], "淡出结束逐位等于输入");
    }

    #[test]
    fn fade_is_clickfree() {
        let mut rack = FxRack::new(SR);
        set_test_processor(&mut rack, 0, 3.0);
        rack.set_slot_params(0, true, 1.0, [3.0, 0.0, 0.0, 0.0]);
        // 100Hz 正弦（信号本身斜率小，Δ 预算留给 mix 斜坡）
        let input: Vec<f32> = (0..512)
            .map(|i| (2.0 * std::f32::consts::PI * 100.0 * (i / 2) as f32 / SR).sin() * 0.5)
            .collect();
        let mut out = input.clone();
        rack.process(&mut out, 256, &FxContext::default());
        let mut max_delta = 0.0f32;
        for i in 1..out.len() {
            max_delta = max_delta.max((out[i] - out[i - 1]).abs());
        }
        assert!(max_delta < 0.05, "淡入逐采样 Δ 过大: {max_delta}");
    }

    #[test]
    fn type_change_fades_from_silence() {
        let mut rack = FxRack::new(SR);
        set_test_processor(&mut rack, 0, 3.0);
        rack.set_slot_params(0, true, 1.0, [3.0, 0.0, 0.0, 0.0]);
        let input = sine_blocks(64);
        let out = run(&mut rack, &input, 256); // 满湿稳态

        // 换型（模拟 set_slot_type：换处理器 + mix 归零）
        rack.set_slot_type(0, None);
        set_test_processor(&mut rack, 0, 2.0);
        rack.set_slot_params(0, true, 1.0, [2.0, 0.0, 0.0, 0.0]);
        let mut first = input[..512].to_vec();
        rack.process(&mut first, 256, &FxContext::default());
        // 从干声淡入：前半块对干声的偏差应小于后半块（mix 单调爬升）
        let dev: Vec<f32> = first
            .iter()
            .zip(input[..512].iter())
            .map(|(a, b)| (a - b).abs())
            .collect();
        let early: f32 = dev[..256].iter().sum::<f32>() / 256.0;
        let late: f32 = dev[256..].iter().sum::<f32>() / 256.0;
        assert!(early < late, "换型应从干声淡入: early={early} late={late}");
        // 逐采样 Δ 有界（淡入无 click）
        let mut max_delta = 0.0f32;
        for i in 1..first.len() {
            max_delta = max_delta.max((first[i] - first[i - 1]).abs());
        }
        assert!(max_delta < 0.1, "换型淡入 Δ 过大: {max_delta}");
        assert!(first.iter().all(|v| v.is_finite()), "无 NaN");
        let _ = out; // 换型前满湿稳态（长度已在 run 内保证）
    }
}
