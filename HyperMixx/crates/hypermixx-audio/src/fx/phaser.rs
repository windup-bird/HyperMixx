//! phaser：6 级一阶全通链 + LFO + 反馈。
//!
//! 每级全通 `y = −a·x + x1 + a·y1`，`a = (1−g)/(1+g)`，`g = tan(πfc/sr)`
//! （经典移相器形式；|a|<1 恒稳定，|H|≡1 严格单位幅度——由测试钉死）。
//! fc = base × 2^(2·depth·lfo)：depth=0 静态中心，lfo ∈ [−1,1] 扫
//! ±2·depth 倍频程；与干声混后凹口出现在全通总相位 = −180° 处
//! （6 级：约 fc/3.7、fc、3.7·fc 三处）。
//! 反馈 u = x + fb·y6（上一样本的末级输出；|fb|<1 → 环路增益 <1 有界）。
//! LFO 波表逐采样推进，g/a 逐采样重算（fc 调制本身平滑，无需 Smoother）。

use super::{EffectProcessor, FxContext};
use crate::fx::lfo::SineLfo;

/// 每声道全通级数。
const STAGES: usize = 6;

/// 一阶全通：y = −a·x + x1 + a·y1（严格单位幅度，H(z) = (−a+z⁻¹)/(1−a·z⁻¹)）。
struct Allpass1 {
    x1: f32,
    y1: f32,
}

impl Allpass1 {
    #[inline]
    fn process(&mut self, x: f32, a: f32) -> f32 {
        let y = -a * x + self.x1 + a * self.y1;
        self.x1 = x;
        self.y1 = y;
        // 状态防 denormal
        if self.x1.abs() < 1e-30 {
            self.x1 = 0.0;
        }
        if self.y1.abs() < 1e-30 {
            self.y1 = 0.0;
        }
        y
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.y1 = 0.0;
    }
}

pub struct Phaser {
    sr: f32,
    rate: f32,
    /// 中心频率（Hz）。
    base: f32,
    /// 调制深度（倍频程系数，0..1 → ±0..±2 倍频程）。
    depth: f32,
    feedback: f32,
    lfo: SineLfo,
    /// 每声道 6 级全通状态。
    stages: [[Allpass1; STAGES]; 2],
    /// 反馈抽头：上一采样末级输出（每声道）。
    fb_state: [f32; 2],
}

impl Phaser {
    pub fn new(sr: f32) -> Self {
        let mut fx = Self {
            sr,
            rate: 0.5,
            base: 800.0,
            depth: 0.5,
            feedback: 0.3,
            lfo: SineLfo::new(sr),
            stages: std::array::from_fn(|_| std::array::from_fn(|_| Allpass1 { x1: 0.0, y1: 0.0 })),
            fb_state: [0.0; 2],
        };
        fx.lfo.set_rate(fx.rate, fx.sr);
        fx
    }
}

impl EffectProcessor for Phaser {
    fn process(&mut self, out: &mut [f32], _frames: usize, _ctx: &FxContext) {
        let fb = self.feedback;
        let base = self.base;
        let depth = self.depth;
        for i in 0..out.len() / 2 {
            let lfo_v = self.lfo.next();
            let fc = base * (2.0 * depth * lfo_v).exp2();
            let g = (std::f32::consts::PI * fc / self.sr).tan();
            let a = (1.0 - g) / (1.0 + g);
            for c in 0..2 {
                let x = out[i * 2 + c];
                let u = x + self.fb_state[c] * fb;
                let mut y = u;
                for stage in self.stages[c].iter_mut() {
                    y = stage.process(y, a);
                }
                self.fb_state[c] = y;
                out[i * 2 + c] = y;
            }
        }
    }

    fn set_params(&mut self, params: &[f32; 4]) {
        self.rate = params[0].clamp(0.05, 5.0);
        self.base = params[1].clamp(100.0, 4000.0);
        self.depth = params[2].clamp(0.0, 1.0);
        self.feedback = params[3].clamp(-0.9, 0.9);
        self.lfo.set_rate(self.rate, self.sr);
    }

    fn reset(&mut self) {
        for c in 0..2 {
            for stage in self.stages[c].iter_mut() {
                stage.reset();
            }
            self.fb_state[c] = 0.0;
        }
        self.lfo.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    /// 左声道正弦 RMS（响应测量；绕过 EffectProcessor 的交织约定）。
    fn rms(fx: &mut Phaser, freq: f32, secs: f32) -> f32 {
        let n = (secs * SR) as usize;
        let mut sum = 0.0f32;
        for i in 0..n {
            let x = (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin() * 0.5;
            let mut out = [x, x];
            fx.process(&mut out, 1, &FxContext::default());
            sum += out[0] * out[0];
        }
        (sum / n as f32).sqrt()
    }

    /// 干湿 1:1 混合后的 RMS（相位抵消测量用：out + in）。
    fn mixed_rms(fx: &mut Phaser, freq: f32, secs: f32) -> f32 {
        let n = (secs * SR) as usize;
        let mut sum = 0.0f32;
        for i in 0..n {
            let x = (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin() * 0.5;
            let mut out = [x, x];
            fx.process(&mut out, 1, &FxContext::default());
            sum += (out[0] + x) * (out[0] + x);
        }
        (sum / n as f32).sqrt()
    }

    fn set(fx: &mut Phaser, rate: f32, base: f32, depth: f32, feedback: f32) {
        fx.set_params(&[rate, base, depth, feedback]);
        let mut out = [0.0; 2];
        for _ in 0..(0.2 * SR) as usize {
            fx.process(&mut out, 1, &FxContext::default());
        }
    }

    #[test]
    fn allpass_stage_has_unit_gain() {
        // 孤立单级：任意 a（此处 0.3）对任意频率严格单位幅度
        let mut ap = Allpass1 { x1: 0.0, y1: 0.0 };
        for &freq in &[100.0, 997.0, 8000.0] {
            let n = 0.5 * SR;
            let mut sum_in = 0.0f32;
            let mut sum_out = 0.0f32;
            for i in 0..n as usize {
                let x = (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
                let y = ap.process(x, 0.3);
                sum_in += x * x;
                sum_out += y * y;
            }
            let ratio = (sum_out / sum_in).sqrt();
            assert!((ratio - 1.0).abs() < 1e-2, "{freq}Hz 全通增益应 =1, ratio={ratio}");
        }
    }

    #[test]
    fn feedback_zero_chain_is_unit_gain() {
        // fb=0：6 级全通级联 |H|≡1（LFO 调制 fc 不影响幅度）
        let mut fx = Phaser::new(SR);
        set(&mut fx, 0.5, 800.0, 0.5, 0.0);
        for &freq in &[100.0, 2000.0, 8000.0] {
            let out = rms(&mut fx, freq, 0.5);
            let din = 0.5 / std::f32::consts::SQRT_2;
            let ratio = out / din;
            assert!((ratio - 1.0).abs() < 1e-2, "{freq}Hz 级联增益应 =1, ratio={ratio}");
        }
    }

    #[test]
    fn cascade_creates_notches() {
        // fb=0、depth=0（fc 静态 = base = 800Hz）：与干声 1:1 混合后，
        // 6 级总相位 −540° ≡ 180° 处完全抵消 → 凹口 ≈ 800Hz（另两处
        // ≈ 214Hz / 2954Hz，均为总相位 −180° 奇数倍）
        let mut fx = Phaser::new(SR);
        set(&mut fx, 0.05, 800.0, 0.0, 0.0);
        let in_rms = 0.5 / std::f32::consts::SQRT_2;
        let mut min_ratio = f32::INFINITY;
        let mut min_freq = 0.0;
        for hz in (100..=4000).step_by(25) {
            let r = mixed_rms(&mut fx, hz as f32, 0.2);
            let ratio = r / in_rms;
            if ratio < min_ratio {
                min_ratio = ratio;
                min_freq = hz as f32;
            }
        }
        assert!(min_ratio < 0.3, "应有深凹口, min={min_ratio} @ {min_freq}Hz");
        // 三个凹口（≈214Hz / 800Hz / 2954Hz）同等深度，min 落在哪取决于
        // 扫描网格；fc 处凹口必须存在——这才是"级联移相"的关键钉
        let at_fc = mixed_rms(&mut fx, 800.0, 0.2) / in_rms;
        assert!(at_fc < 0.3, "fc=800Hz 处应抵消, ratio={at_fc}");
        // 远离凹口处（5kHz，最高凹口 2954Hz 之上）应不抵消
        let high = mixed_rms(&mut fx, 5000.0, 0.2) / in_rms;
        assert!(high > 0.5, "5kHz 不应抵消, ratio={high}");
    }

    #[test]
    fn depth_modulation_moves_notch() {
        // 800Hz 处：depth=0 凹口钉死（ratio≈0）；depth=1 时 fc 扫
        // 200..3200Hz，凹口周期性掠过 800Hz → 平均 ratio 显著抬升
        let in_rms = 0.5 / std::f32::consts::SQRT_2;
        let mut fx = Phaser::new(SR);
        set(&mut fx, 0.05, 800.0, 0.0, 0.0);
        let static_ratio = mixed_rms(&mut fx, 800.0, 0.4) / in_rms;
        let mut fx = Phaser::new(SR);
        set(&mut fx, 0.05, 800.0, 1.0, 0.0);
        let swept_ratio = mixed_rms(&mut fx, 800.0, 0.4) / in_rms;
        assert!(
            swept_ratio > static_ratio + 0.5,
            "LFO 调制应移动凹口: static={static_ratio} swept={swept_ratio}"
        );
    }

    #[test]
    fn feedback_nine_tenths_bounded() {
        // 环路增益 = 0.9·|H6| = 0.9 < 1：任意参数稳态有界
        let mut fx = Phaser::new(SR);
        for step in 0..20 {
            let t = step as f32 / 19.0;
            fx.set_params(&[0.05 + 4.95 * t, 100.0 + 3900.0 * t, t, 0.9]);
            let mut peak = 0.0f32;
            for i in 0..(0.3 * SR) as usize {
                let x = (2.0 * std::f32::consts::PI * 997.0 * i as f32 / SR).sin() * 0.5;
                let mut out = [x, x];
                fx.process(&mut out, 1, &FxContext::default());
                peak = peak.max(out[0].abs());
            }
            assert!(peak < 6.0, "step {step} 峰值越界: {peak}");
        }
    }

    #[test]
    fn full_param_sweep_no_nan() {
        let mut fx = Phaser::new(SR);
        for step in 0..40 {
            let t = step as f32 / 39.0;
            fx.set_params(&[0.05 + 4.95 * t, 100.0 + 3900.0 * t, t, -0.9 + 1.8 * t]);
            for i in 0..(0.2 * SR) as usize {
                let x = (2.0 * std::f32::consts::PI * 997.0 * i as f32 / SR).sin() * 0.5;
                let mut out = [x, x];
                fx.process(&mut out, 1, &FxContext::default());
                assert!(out.iter().all(|v| v.is_finite()), "step {step} NaN");
            }
        }
    }
}
