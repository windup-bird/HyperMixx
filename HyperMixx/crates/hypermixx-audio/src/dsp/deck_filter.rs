//! deck 滤波旋钮（Pioneer 式单旋钮 LP↔HP）。
//!
//! 旋钮 −1..+1：正 = 低通（20kHz→20Hz），负 = 高通（20Hz→20kHz），0 = 旁路。
//! 三路混合架构：输出 = 干声×(1−ml−mh) + LP×ml + HP×mh（ml=max(knob,0)、
//! mh=max(−knob,0)，同一时刻只有一条滤波臂非零）。干声分量保证：
//! - 旋钮过零、任意跳变（MIDI 绝对 CC 遥控）都连续——两臂权重走 10ms
//!   Smoother，永远不会瞬时交换滤波臂；
//! - 居中时输出严格等于输入（干声 ×1，两臂 ×0）。
//!
//! 单 biquad 换型 / 双 biquad 交叉淡化在"臂交换"瞬时都有 |lp−hp| 量级的
//! 阶跃，干路无 rack mix 斜坡掩护时会 click，故弃用。
//!
//! 截止频率在 log2 域逐采样平滑（对数轴听感均匀），变化超过 0.02 倍频程
//! 才重算系数（仿 filter_linear.rs / EQ 的 retune 规则）。
//! 旋钮 |knob|<ε 且各平滑器均已稳定 → 整个旁路跳过（逐位直通，
//! 保住 deck 级 bitwise passthrough 测试，未动旋钮时输出零变化）。

use crate::dsp::biquad::{Biquad, BiquadKind};
use crate::dsp::smoother::Smoother;

const KNOB_EPS: f32 = 1e-6;
const SMOOTH_TAU_S: f32 = 0.010;
/// 平滑值变化超过此阈值（倍频程）才重算系数。
const RETUNE_EPS_OCT: f32 = 0.02;
/// 截止频率范围（Hz，与 filter-linear FX 一致）。
const LOG2_MIN: f32 = 4.321_928; // log2(20)
const LOG2_MAX: f32 = 14.287_713; // log2(20000)
const RANGE: f32 = LOG2_MAX - LOG2_MIN;

pub struct DeckFilter {
    /// LP 臂截止（log2）：knob>0 时 20kHz→20Hz，knob≤0 时停在 20kHz。
    lp_log2: Smoother,
    lp_applied: f32,
    /// HP 臂截止（log2）：knob<0 时 20Hz→20kHz，knob≥0 时停在 20Hz。
    hp_log2: Smoother,
    hp_applied: f32,
    /// LP/HP 混合系数（max(±knob, 0)），平滑；干声 = 1 − ml − mh ≥ 0。
    mix_lp: Smoother,
    mix_hp: Smoother,
    lp_l: Biquad,
    lp_r: Biquad,
    hp_l: Biquad,
    hp_r: Biquad,
    /// |knob| > KNOB_EPS；旁路条件的一部分。
    engaged: bool,
}

impl DeckFilter {
    pub fn new(sr: f32) -> Self {
        let coeff = 1.0 - (-1.0 / (SMOOTH_TAU_S * sr)).exp();
        Self {
            lp_log2: Smoother::new(LOG2_MAX, coeff),
            lp_applied: LOG2_MAX,
            hp_log2: Smoother::new(LOG2_MIN, coeff),
            hp_applied: LOG2_MIN,
            mix_lp: Smoother::new(0.0, coeff),
            mix_hp: Smoother::new(0.0, coeff),
            lp_l: Biquad::new(BiquadKind::LowPass, sr, 20000.0, 0.707, 0.0),
            lp_r: Biquad::new(BiquadKind::LowPass, sr, 20000.0, 0.707, 0.0),
            hp_l: Biquad::new(BiquadKind::HighPass, sr, 20.0, 0.707, 0.0),
            hp_r: Biquad::new(BiquadKind::HighPass, sr, 20.0, 0.707, 0.0),
            engaged: false,
        }
    }

    /// 每块一次：读旋钮（-1..+1）更新平滑目标。
    pub fn set_knob(&mut self, knob: f32) {
        let knob = knob.clamp(-1.0, 1.0);
        let engaged = knob.abs() > KNOB_EPS;
        // 活动臂目标：LP 20kHz→20Hz / HP 20Hz→20kHz；非活动臂停在本方向极端。
        let lp_target = if knob > 0.0 {
            LOG2_MAX - knob * RANGE
        } else {
            LOG2_MAX
        };
        let hp_target = if knob < 0.0 {
            LOG2_MIN - knob * RANGE
        } else {
            LOG2_MIN
        };
        if engaged && !self.engaged {
            // 重新切入：清空四臂陈旧 z1/z2（首样本 = b0·x，且干声权重 ≈1，无 click）。
            self.lp_l.reset();
            self.lp_r.reset();
            self.hp_l.reset();
            self.hp_r.reset();
        }
        self.engaged = engaged;
        self.lp_log2.set_target(lp_target);
        self.hp_log2.set_target(hp_target);
        self.mix_lp.set_target(knob.max(0.0));
        self.mix_hp.set_target((-knob).max(0.0));
    }

    /// 载入新音轨时调用：清状态、目标回默认（自然滑落，不用 set_immediate）。
    pub fn reset(&mut self) {
        self.lp_l.reset();
        self.lp_r.reset();
        self.hp_l.reset();
        self.hp_r.reset();
        self.lp_log2.set_target(LOG2_MAX);
        self.hp_log2.set_target(LOG2_MIN);
        self.mix_lp.set_target(0.0);
        self.mix_hp.set_target(0.0);
    }

    fn retune_lp(&mut self, log2_fc: f32) {
        let fc = 2f32.powf(log2_fc);
        self.lp_l.set_params(fc, 0.707, 0.0);
        self.lp_r.set_params(fc, 0.707, 0.0);
        self.lp_applied = log2_fc;
    }

    fn retune_hp(&mut self, log2_fc: f32) {
        let fc = 2f32.powf(log2_fc);
        self.hp_l.set_params(fc, 0.707, 0.0);
        self.hp_r.set_params(fc, 0.707, 0.0);
        self.hp_applied = log2_fc;
    }

    /// 逐采样：cutoff 平滑值越过阈值才重算系数（未稳定时每 0.02 倍频程一次）。
    #[inline]
    fn maybe_retune(&mut self, cur_lp: f32, cur_hp: f32) {
        if (cur_lp - self.lp_applied).abs() > RETUNE_EPS_OCT
            || (self.lp_log2.settled() && cur_lp != self.lp_applied)
        {
            self.retune_lp(cur_lp);
        }
        if (cur_hp - self.hp_applied).abs() > RETUNE_EPS_OCT
            || (self.hp_log2.settled() && cur_hp != self.hp_applied)
        {
            self.retune_hp(cur_hp);
        }
    }

    /// 逐采样处理（交错立体声）。
    #[inline]
    pub fn process(&mut self, out: &mut [f32], frames: usize) {
        // 旁路：旋钮 0 且全部平滑器稳定 → 不碰采样（逐位直通）。
        if !self.engaged
            && self.mix_lp.settled()
            && self.mix_hp.settled()
            && self.lp_log2.settled()
            && self.hp_log2.settled()
        {
            return;
        }
        for i in 0..frames {
            let cur_lp = self.lp_log2.step();
            let cur_hp = self.hp_log2.step();
            let ml = self.mix_lp.step();
            let mh = self.mix_hp.step();
            self.maybe_retune(cur_lp, cur_hp);
            let dry = 1.0 - ml - mh; // ≥ 0：两权重一阶单调、目标和 ≤ 1
            let (xl, xr) = (out[i * 2], out[i * 2 + 1]);
            out[i * 2] = dry * xl + ml * self.lp_l.process(xl) + mh * self.hp_l.process(xl);
            out[i * 2 + 1] = dry * xr + ml * self.lp_r.process(xr) + mh * self.hp_r.process(xr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    /// 单声道正弦 RMS（绕过交错约定；L/R 同幅同相）。
    fn rms(fx: &mut DeckFilter, freq: f32, secs: f32) -> f32 {
        let n = (secs * SR) as usize;
        let mut sum = 0.0f32;
        for i in 0..n {
            let x = (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin() * 0.5;
            let mut out = [x, x];
            fx.process(&mut out, 1);
            sum += out[0] * out[0];
        }
        (sum / n as f32).sqrt()
    }

    /// 设定旋钮并让平滑器稳定到目标（0.5s 远超 10ms τ）。
    fn set(fx: &mut DeckFilter, knob: f32) {
        fx.set_knob(knob);
        let mut out = [0.0; 2];
        for _ in 0..(0.5 * SR) as usize {
            fx.process(&mut out, 1);
        }
    }

    #[test]
    fn default_zero_knob_is_bitwise_passthrough() {
        let mut fx = DeckFilter::new(SR);
        let mut out: Vec<f32> = (0..512).map(|i| ((i * 7919) % 1000) as f32 / 1000.0 - 0.5).collect();
        let orig = out.clone();
        fx.process(&mut out, 256);
        assert_eq!(out, orig, "旋钮 0 且稳定时必须逐位直通");
    }

    #[test]
    fn lp_full_knob_kills_highs() {
        let mut fx = DeckFilter::new(SR);
        let bypass = rms(&mut fx, 440.0, 0.5); // 未动旋钮：旁路
        set(&mut fx, 1.0); // LP@20Hz
        let cut = rms(&mut fx, 440.0, 0.5);
        assert!(cut < bypass * 0.01, "LP 全开应杀 440Hz: bypass={bypass} cut={cut}");
    }

    #[test]
    fn hp_half_knob_passes_highs_stops_lows() {
        let mut fx = DeckFilter::new(SR);
        set(&mut fx, -0.5); // HP@~631Hz，湿重 0.5
        let low = rms(&mut fx, 100.0, 0.5);
        let high = rms(&mut fx, 8000.0, 0.5);
        assert!(high > low * 1.5, "HP 半开应偏高频: low={low} high={high}");
    }

    #[test]
    fn hp_full_knob_kills_lows() {
        let mut fx = DeckFilter::new(SR);
        set(&mut fx, -1.0); // HP@20kHz
        let low = rms(&mut fx, 100.0, 0.5);
        assert!(low < 0.005, "HP 全开应杀 100Hz: low={low}");
    }

    #[test]
    fn engage_disengage_is_clickfree() {
        let mut fx = DeckFilter::new(SR);
        // 1kHz 正弦连续跨块：0 → +1（LP 全关）→ 0，全程逐采样 Δ 有界。
        let blocks = 30;
        let input: Vec<f32> = (0..blocks * 512)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * (i / 2) as f32 / SR).sin() * 0.5)
            .collect();
        let mut out = vec![0.0; 512];
        let mut prev = 0.0f32;
        let mut max_delta = 0.0f32;
        for b in 0..blocks {
            let knob = match b {
                0..=3 => 0.0,
                4..=13 => 1.0,
                _ => 0.0,
            };
            fx.set_knob(knob);
            out.copy_from_slice(&input[b * 512..(b + 1) * 512]);
            fx.process(&mut out, 256);
            for i in 0..256 {
                max_delta = max_delta.max((out[i * 2] - prev).abs());
                prev = out[i * 2];
            }
        }
        assert!(max_delta < 0.2, "切入/切出逐采样 Δ 过大: {max_delta}");
    }

    #[test]
    fn sign_flip_is_clickfree() {
        let mut fx = DeckFilter::new(SR);
        // 直接 LP↔HP 翻越（不走 0 停留，模拟 MIDI 绝对 CC 跳变）：
        // -0.8 → +0.8，逐采样 Δ 有界（三路混合权重全部一阶平滑）。
        let blocks = 30;
        let input: Vec<f32> = (0..blocks * 512)
            .map(|i| (2.0 * std::f32::consts::PI * 3000.0 * (i / 2) as f32 / SR).sin() * 0.5)
            .collect();
        let mut out = vec![0.0; 512];
        let mut prev = 0.0f32;
        let mut max_delta = 0.0f32;
        for b in 0..blocks {
            let knob = if b < 10 { -0.8 } else { 0.8 };
            fx.set_knob(knob);
            out.copy_from_slice(&input[b * 512..(b + 1) * 512]);
            fx.process(&mut out, 256);
            for i in 0..256 {
                max_delta = max_delta.max((out[i * 2] - prev).abs());
                prev = out[i * 2];
            }
        }
        assert!(max_delta < 0.2, "过零翻转逐采样 Δ 过大: {max_delta}");
    }

    #[test]
    fn full_knob_sweep_no_nan() {
        let mut fx = DeckFilter::new(SR);
        let input: Vec<f32> = (0..512)
            .map(|i| (2.0 * std::f32::consts::PI * 997.0 * (i / 2) as f32 / SR).sin() * 0.8)
            .collect();
        for step in 0..61 {
            let knob = -1.0 + 2.0 * step as f32 / 60.0;
            fx.set_knob(knob);
            let mut out = input.clone();
            fx.process(&mut out, 256);
            assert!(out.iter().all(|v| v.is_finite()), "step {step} NaN");
        }
    }
}
