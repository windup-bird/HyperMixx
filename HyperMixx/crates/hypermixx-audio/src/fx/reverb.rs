//! reverb：Freeverb 拓扑（public domain 结构重新实现）——4 并联 comb
//! + 2 串联 allpass 每声道。
//!
//! - comb 延迟 [1116,1188,1277,1356] 采样，R 声道 +23 去相关；
//!   反馈路径一阶 LP 阻尼（damp=0 透明，damp→1 截止趋 0；内部 clamp
//!   0.99 避开 c=1 的临界极点）；fb = (0.28 + 0.7·roomsize).min(0.988)
//!   ——最高 0.988 使尾音极长但始终衰减（有界测试钉死）；
//! - allpass [556,441]，Freeverb 转置形式 a=0.5：`y=−x+buf; buf=x+buf·0.5`
//!   （DC 与 Nyquist 单位增益，中频 ±<1.7dB 偏差）；
//! - 输入 (L+R)/2 单声道求和送双 comb 库（不同长度 → 立体去相关）；
//!   width 交叉：outL = (w1·wetL + w2·wetR)·WET_GAIN，w1=(1+w)/2；
//!   width=0 → L≡R（逐位，测试钉死），width=1 → 全立体；
//! - WET_GAIN：湿声归一化常数，由标定测试钉死
//!   （默认参数下平均 RMS(湿)/RMS(干) ∈ [0.5,1.5]）。

use super::{EffectProcessor, FxContext};
use crate::fx::delay::DelayLine;

/// 每声道 comb 延迟（采样，Freeverb 经典值）。
const COMB_LENS: [usize; 4] = [1116, 1188, 1277, 1356];
/// R 声道额外偏移（去相关）。
const STEREO_SPREAD: usize = 23;
/// 每声道串联 allpass 延迟（采样）。
const AP_LENS: [usize; 2] = [556, 441];
/// allpass 反馈系数（Freeverb 转置形式）。
const AP_FB: f32 = 0.5;
/// 湿声归一化增益（标定测试钉行为）。
/// 实测：WET_GAIN=1 时默认参数平均湿干比 ≈4.17（4 comb 稳态增益
/// 相干叠加）→ 取 1/4.17 ≈ 0.24 使平均比 ≈1.0。
const WET_GAIN: f32 = 0.24;
/// damp 内部上限（c=1 → 反馈极点停在单位圆，临界不稳定）。
const DAMP_MAX: f32 = 0.99;

struct Comb {
    dl: DelayLine,
    /// 固定读出延迟（采样）。
    delay: usize,
    /// 反馈系数（roomsize 变化时更新）。
    fb: f32,
    /// 反馈路径一阶 LP 状态。
    lp_state: f32,
}

impl Comb {
    fn new(delay: usize) -> Self {
        Self {
            dl: DelayLine::new(delay),
            delay,
            fb: 0.63,
            lp_state: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32, damp: f32) -> f32 {
        let out = self.dl.read(self.delay);
        let filtered = out * (1.0 - damp) + self.lp_state * damp;
        // 状态防 denormal
        if filtered.abs() < 1e-30 {
            self.lp_state = 0.0;
        } else {
            self.lp_state = filtered;
        }
        self.dl.write(x + filtered * self.fb);
        out
    }

    fn clear(&mut self) {
        self.dl.clear();
        self.lp_state = 0.0;
    }
}

struct Allpass {
    dl: DelayLine,
    delay: usize,
}

impl Allpass {
    fn new(delay: usize) -> Self {
        Self {
            dl: DelayLine::new(delay),
            delay,
        }
    }

    /// Freeverb 转置形式：y = −x + buf; buf = x + buf·a（a=0.5）。
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let bufout = self.dl.read(self.delay);
        let out = -x + bufout;
        self.dl.write(x + bufout * AP_FB);
        out
    }

    fn clear(&mut self) {
        self.dl.clear();
    }
}

pub struct Reverb {
    combs_l: [Comb; 4],
    combs_r: [Comb; 4],
    ap_l: [Allpass; 2],
    ap_r: [Allpass; 2],
    damp: f32,
    width: f32,
}

impl Reverb {
    pub fn new(_sr: f32) -> Self {
        let mk_comb = |i: usize| {
            std::array::from_fn(|j| Comb::new(COMB_LENS[j] + if i == 1 { STEREO_SPREAD } else { 0 }))
        };
        let mk_ap = || std::array::from_fn(|j| Allpass::new(AP_LENS[j]));
        Self {
            combs_l: mk_comb(0),
            combs_r: mk_comb(1),
            ap_l: mk_ap(),
            ap_r: mk_ap(),
            damp: 0.3,
            width: 0.7,
        }
    }
}

impl EffectProcessor for Reverb {
    fn process(&mut self, out: &mut [f32], _frames: usize, _ctx: &FxContext) {
        let damp = self.damp;
        let w1 = 0.5 * (1.0 + self.width);
        let w2 = 0.5 * (1.0 - self.width);
        for i in 0..out.len() / 2 {
            let x = (out[i * 2] + out[i * 2 + 1]) * 0.5;
            let mut wl = 0.0;
            for c in self.combs_l.iter_mut() {
                wl += c.process(x, damp);
            }
            let mut wr = 0.0;
            for c in self.combs_r.iter_mut() {
                wr += c.process(x, damp);
            }
            // 注意：接收者借用先于参数求值保留整个 self.ap_*，
            // 嵌套调用需拆成两步（E0499）
            wl = self.ap_l[0].process(wl);
            wl = self.ap_l[1].process(wl);
            wr = self.ap_r[0].process(wr);
            wr = self.ap_r[1].process(wr);
            out[i * 2] = (w1 * wl + w2 * wr) * WET_GAIN;
            out[i * 2 + 1] = (w2 * wl + w1 * wr) * WET_GAIN;
        }
    }

    fn set_params(&mut self, params: &[f32; 4]) {
        let roomsize = params[0].clamp(0.0, 1.0);
        let fb = (0.28 + 0.7 * roomsize).min(0.988);
        for c in self.combs_l.iter_mut().chain(self.combs_r.iter_mut()) {
            c.fb = fb;
        }
        self.damp = params[1].clamp(0.0, DAMP_MAX);
        self.width = params[2].clamp(0.0, 1.0);
    }

    fn reset(&mut self) {
        for c in self.combs_l.iter_mut().chain(self.combs_r.iter_mut()) {
            c.clear();
        }
        for ap in self.ap_l.iter_mut().chain(self.ap_r.iter_mut()) {
            ap.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    /// 脉冲左声道输入，返回左声道输出。
    fn impulse_run(fx: &mut Reverb, secs: f32) -> Vec<f32> {
        let blocks = (secs * SR / 256.0) as usize;
        let mut out = vec![0.0; 512];
        let mut rec = Vec::new();
        for b in 0..blocks {
            out.fill(0.0);
            if b == 0 {
                out[0] = 1.0;
                out[1] = 1.0;
            }
            fx.process(&mut out, 256, &FxContext::default());
            for i in 0..256 {
                rec.push(out[i * 2]);
            }
        }
        rec
    }

    /// 正弦左声道输入，返回左声道输出。
    fn sine_run(fx: &mut Reverb, freq: f32, secs: f32) -> Vec<f32> {
        let blocks = (secs * SR / 256.0) as usize;
        let mut out = vec![0.0; 512];
        let mut rec = Vec::new();
        for b in 0..blocks {
            out.fill(0.0);
            for i in 0..256 {
                let t = (b * 256 + i) as f32 / SR;
                let x = (2.0 * std::f32::consts::PI * freq * t).sin() * 0.5;
                out[i * 2] = x;
                out[i * 2 + 1] = x;
            }
            fx.process(&mut out, 256, &FxContext::default());
            for i in 0..256 {
                rec.push(out[i * 2]);
            }
        }
        rec
    }

    fn window_peak(rec: &[f32], lo: f32, hi: f32) -> f32 {
        let lo = (lo * SR) as usize;
        let hi = ((hi * SR) as usize).min(rec.len());
        rec[lo..hi].iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    fn window_rms(rec: &[f32], lo: f32, hi: f32) -> f32 {
        let lo = (lo * SR) as usize;
        let hi = ((hi * SR) as usize).min(rec.len());
        let sum: f32 = rec[lo..hi].iter().map(|v| v * v).sum();
        (sum / (hi - lo) as f32).sqrt()
    }

    fn set(fx: &mut Reverb, roomsize: f32, damp: f32, width: f32) {
        fx.set_params(&[roomsize, damp, width, 0.0]);
    }

    #[test]
    fn impulse_tail_decays() {
        let mut fx = Reverb::new(SR);
        set(&mut fx, 0.5, 0.3, 0.7);
        let rec = impulse_run(&mut fx, 1.2);
        let early = window_peak(&rec, 0.02, 0.15); // 首回声簇 ≈ 23–77ms 处
        let late = window_peak(&rec, 0.9, 1.0);
        // 首回声 ≈ 1.0 × 宽度混合 0.85 × WET_GAIN 0.24 ≈ 0.204
        assert!(early > 0.1, "首回声应显著, early={early}");
        // fb=0.63：0.5s 间隔 ≈ 21 个 comb 环回，幅度比应 < 0.1
        assert!(late < early * 0.1, "尾音应衰减, early={early} late={late}");
    }

    #[test]
    fn roomsize_lengthens_tail() {
        let run = |rs: f32| -> f32 {
            let mut fx = Reverb::new(SR);
            set(&mut fx, rs, 0.3, 0.7);
            let rec = impulse_run(&mut fx, 1.2);
            window_peak(&rec, 0.6, 0.8)
        };
        let short = run(0.1); // fb=0.35
        let long = run(0.7); // fb=0.77
        assert!(
            long > short * 2.0,
            "大 roomsize 尾音应更长: short={short} long={long}"
        );
    }

    #[test]
    fn damp_attenuates_high_frequencies() {
        // 脉冲响应尾态的一阶差分能量（高频代理）：damp=0 的尾态是尖锐
        // 回声串（差分能量高）；damp→1 反馈 LP 把每级回声抹平
        // （差分能量骤降）。稳态正弦不行——过零数只跟随输入频率
        let run = |damp: f32| -> f32 {
            let mut fx = Reverb::new(SR);
            set(&mut fx, 0.5, damp, 0.7);
            let rec = impulse_run(&mut fx, 1.0);
            let lo = (0.4 * SR) as usize;
            let hi = (0.8 * SR) as usize;
            rec[lo..hi].windows(2).map(|w| (w[1] - w[0]).abs()).sum()
        };
        let open = run(0.0);
        let damped = run(0.95);
        assert!(
            damped < open / 10.0,
            "damp 应衰减高频（尾态差分能量）: open={open} damped={damped}"
        );
    }

    #[test]
    fn width_zero_collapses_to_mono() {
        let mut fx = Reverb::new(SR);
        set(&mut fx, 0.5, 0.3, 0.0);
        let blocks = 32;
        let mut out = vec![0.0; 512];
        for b in 0..blocks {
            out.fill(0.0);
            if b == 0 {
                out[0] = 1.0; // 仅左声道脉冲
            }
            fx.process(&mut out, 256, &FxContext::default());
            for i in 0..256 {
                assert_eq!(
                    out[i * 2], out[i * 2 + 1],
                    "width=0 时 L/R 应逐位相同（单声道混合）, block={b} frame={i}"
                );
            }
        }
    }

    #[test]
    fn wet_level_calibration() {
        // 标定：默认参数下平均 RMS(湿)/RMS(干) ∈ [0.5,1.5]（WET_GAIN 钉行为）
        let mut fx = Reverb::new(SR);
        set(&mut fx, 0.5, 0.3, 0.7);
        let in_rms = 0.5 / std::f32::consts::SQRT_2;
        let mut log_sum = 0.0f32;
        for &freq in &[200.0, 440.0, 1000.0, 3000.0] {
            let rec = sine_run(&mut fx, freq, 0.6);
            let wet = window_rms(&rec, 0.3, 0.5); // 稳态段
            log_sum += (wet / in_rms).ln();
        }
        let mean_ratio = (log_sum / 4.0).exp();
        assert!(
            (0.5..=1.5).contains(&mean_ratio),
            "平均湿干比应 ∈ [0.5,1.5], mean={mean_ratio}"
        );
    }

    #[test]
    fn full_param_sweep_bounded_no_nan() {
        let mut fx = Reverb::new(SR);
        for step in 0..60 {
            let t = step as f32 / 59.0;
            set(&mut fx, t, t, t);
            let rec = sine_run(&mut fx, 997.0, 0.4);
            assert!(rec.iter().all(|v| v.is_finite()), "step {step} NaN");
            let peak = rec.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            // 线性界：输入 0.25（(L+R)/2）·Σ|H_comb| ≤ 4/(1−0.988) = 333
            // × allpass ≤1.67² ≈ 2.78 → 理论最坏 ≈ 0.25·333·2.78 ≈ 232；
            // 997Hz 远离 comb 共振 → 实际远低于此，40 为宽裕门槛
            assert!(peak < 40.0, "step {step} 峰值越界: {peak}");
        }
    }
}
