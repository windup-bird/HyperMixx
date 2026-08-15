//! filter-moog：4 级一阶低通级联 + 全局反馈 + tanh 非线性。
//!
//! 结构 = 指数映射级联（非梯形 TPT）：
//! `u = 8·tanh(0.125·(x·drive − 4·res·s4))`，逐级 `y = g·(u − s) + s; s = y`。
//! 增益分级 8×（小信号严格等价于计划方程的 u = tanh(...)）：tanh 钳位 ±1
//! 会把共振峰压死（res→0.95 时 −180° 交叉点环路增益 ≈0.91，tanh 自变量
//! 被放大 ~10× 饱和）；线性区扩 8 倍后共振峰恢复 >1.5×，res=1 自激
//! 仍为有界极限环（幅值 ≈0.55×8×F，|y| ≲ 1.4）。
//! 系数取 g = 1 − exp(−2πfc/sr)（每级在 fc 恰 −3dB，极点恒在单位圆内，
//! 20kHz 截止也稳定）——计划草案的 tan(πfc/sr) 系数属 TPT 约定，
//! 用在指数映射形式上极点 1−g 于 fc ≳ 17kHz 失稳，故不采用。
//! cutoff 走 log2 域 10ms Smoother + 逐采样阈值重算（同 filter_linear）；
//! res 变化即时重算；drive 经 10ms Smoother。行为由测试钉死：
//! DC 增益 ≈ 1/(1+4res)、截止处 ≈ −12dB（res≈0）、共振峰 > 1.5×。

use super::{EffectProcessor, FxContext};
use crate::dsp::smoother::Smoother;

const SMOOTH_TAU_S: f32 = 0.010;
/// 平滑值变化超过此阈值（倍频程）才重算系数。
const RETUNE_EPS_OCT: f32 = 0.02;

pub struct FilterMoog {
    sr: f32,
    /// log2(cutoff)，平滑。
    cutoff_log2: Smoother,
    res: f32,
    /// 线性 drive 增益（10^(dB/20)），平滑。
    drive: Smoother,
    applied_log2: f32,
    applied_res: f32,
    /// 一阶低通系数 g = 1 − exp(−2πfc/sr)。
    g: f32,
    /// 4 级 × 2 声道级联状态（各级输出）。
    s: [[f32; 4]; 2],
}

impl FilterMoog {
    pub fn new(sr: f32) -> Self {
        let coeff = 1.0 - (-1.0 / (SMOOTH_TAU_S * sr)).exp();
        let mut fx = Self {
            sr,
            cutoff_log2: Smoother::new(2000.0f32.log2(), coeff),
            res: 0.2,
            drive: Smoother::new(1.0, coeff),
            applied_log2: 2000.0f32.log2(),
            applied_res: 0.2,
            g: 0.0,
            s: [[0.0; 4]; 2],
        };
        fx.recompute_g();
        fx
    }

    fn recompute_g(&mut self) {
        let fc = 2f32.powf(self.applied_log2);
        self.g = 1.0 - (-2.0 * std::f32::consts::PI * fc / self.sr).exp();
    }

    /// 逐采样调用：平滑值越过阈值（或 res 已变、或已稳定但未重算）→ 重算。
    #[inline]
    fn maybe_retune(&mut self) {
        let cur = self.cutoff_log2.step();
        let need = (cur - self.applied_log2).abs() > RETUNE_EPS_OCT || self.res != self.applied_res;
        if need || (self.cutoff_log2.settled() && cur != self.applied_log2) {
            self.applied_log2 = cur;
            self.applied_res = self.res;
            self.recompute_g();
        }
    }
}

impl EffectProcessor for FilterMoog {
    fn process(&mut self, out: &mut [f32], _frames: usize, _ctx: &FxContext) {
        for i in 0..out.len() / 2 {
            self.maybe_retune();
            let g = self.g;
            let k = 4.0 * self.res;
            let d = self.drive.step();
            for c in 0..2 {
                let x = out[i * 2 + c];
                let u = 8.0 * (0.125 * (x * d - k * self.s[c][3])).tanh();
                let mut y = u;
                for stage in 0..4 {
                    y = g * (y - self.s[c][stage]) + self.s[c][stage];
                    self.s[c][stage] = y;
                }
                out[i * 2 + c] = y;
            }
        }
    }

    fn set_params(&mut self, params: &[f32; 4]) {
        let cutoff = params[0].clamp(20.0, 20000.0);
        let res = params[1].clamp(0.0, 1.0);
        let drive_db = params[2].clamp(0.0, 24.0);
        self.cutoff_log2.set_target(cutoff.log2());
        self.res = res;
        self.drive.set_target(10f32.powf(drive_db / 20.0));
    }

    fn reset(&mut self) {
        // 清级联状态；drive 当前值归 1（目标保留，rack 每块重新快照）
        for c in 0..2 {
            for stage in 0..4 {
                self.s[c][stage] = 0.0;
            }
        }
        self.drive.set_current(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    /// 左声道正弦 RMS（响应测量；绕过 EffectProcessor 的交织约定）。
    fn rms(fx: &mut FilterMoog, freq: f32, secs: f32) -> f32 {
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

    /// 设置参数并让 smoother 稳定（0.5s 远超 10ms τ）。
    fn set(fx: &mut FilterMoog, cutoff: f32, res: f32, drive_db: f32) {
        fx.set_params(&[cutoff, res, drive_db, 0.0]);
        let mut out = [0.0; 2];
        for _ in 0..(0.5 * SR) as usize {
            fx.process(&mut out, 1, &FxContext::default());
        }
    }

    #[test]
    fn dc_gain_pins_feedback_convention() {
        // 小信号 DC 0.1（tanh 线性区）：DC 增益 ≈ 1/(1+4res) = 1/1.8
        let mut fx = FilterMoog::new(SR);
        set(&mut fx, 2000.0, 0.2, 0.0);
        // 注意：原位契约 = 每块新鲜内容（deck 每块拷入新数据）；
        // 复用处理后的 out 会把自己的输出当输入，闭环塌缩
        let mut out = [0.0; 2];
        for _ in 0..(0.1 * SR) as usize {
            out = [0.1, 0.1];
            fx.process(&mut out, 1, &FxContext::default());
        }
        let gain = out[0] / 0.1;
        assert!(
            (gain - 1.0 / 1.8).abs() < 0.03,
            "DC 增益应 ≈ 1/(1+4res)=0.556，实测 {gain}"
        );
    }

    #[test]
    fn high_cutoff_passes_440hz() {
        // cutoff 20k、res 0：440Hz 近乎恒等（每级 −3dB 点在 20k）
        let mut fx = FilterMoog::new(SR);
        set(&mut fx, 20000.0, 0.0, 0.0);
        let out_rms = rms(&mut fx, 440.0, 0.5);
        let in_rms = 0.5 / std::f32::consts::SQRT_2;
        assert!(
            (out_rms / in_rms - 1.0).abs() < 0.1,
            "高截止应近似恒等, 增益={}",
            out_rms / in_rms
        );
    }

    #[test]
    fn cutoff_attenuation_is_12db_per_4_stages() {
        // res≈0：截止处每级 −3dB × 4 级 ≈ −12dB（±1.5dB 钉死行为）
        let mut fx = FilterMoog::new(SR);
        set(&mut fx, 1000.0, 0.0, 0.0);
        let below = rms(&mut fx, 100.0, 0.5); // ≈ 0dB 参考
        let at = rms(&mut fx, 1000.0, 0.5);
        let db = 20.0 * (at / below).log10();
        assert!(
            (-13.5..=-10.5).contains(&db),
            "截止处应 ≈ −12dB，实测 {db:.1}dB"
        );
    }

    #[test]
    fn resonance_peak_exceeds_1_5x() {
        // res 0.95：−180° 交叉点（fc 略上方）窄峰 > 1.5× 输入；
        // 50Hz 步扫描找峰（峰宽窄，粗网格会错过）
        let mut fx = FilterMoog::new(SR);
        set(&mut fx, 1000.0, 0.95, 0.0);
        let in_rms = 0.5 / std::f32::consts::SQRT_2;
        let mut peak = 0.0f32;
        let mut peak_freq = 0.0;
        for hz in (800..=1500).step_by(25) {
            let r = rms(&mut fx, hz as f32, 0.2);
            let ratio = r / in_rms;
            if ratio > peak {
                peak = ratio;
                peak_freq = hz as f32;
            }
        }
        assert!(peak > 1.5, "共振峰应 > 1.5×，实测 {peak:.2}× @ {peak_freq}Hz");
    }

    #[test]
    fn torture_res_cutoff_sweep_no_nan() {
        let mut fx = FilterMoog::new(SR);
        let input: Vec<f32> = (0..512)
            .map(|i| (2.0 * std::f32::consts::PI * 997.0 * (i / 2) as f32 / SR).sin() * 0.8)
            .collect();
        for step in 0..100 {
            let res = 0.95 * step as f32 / 99.0;
            let cutoff = 20.0 * (20000.0f32 / 20.0).powf(step as f32 / 99.0);
            let drive = 24.0 * step as f32 / 99.0;
            fx.set_params(&[cutoff, res, drive, 0.0]);
            let mut out = input.clone();
            fx.process(&mut out, 256, &FxContext::default());
            assert!(out.iter().all(|v| v.is_finite()), "step {step} NaN");
            let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(peak < 8.0, "step {step} 峰值越界: {peak}");
        }
    }
}
