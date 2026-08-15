//! flanger：LFO 调制短延迟 + 反馈（经典梳状镶边）。
//!
//! - 双声道 DelayLine(2^10 = 1024 帧 ≈ 21.3ms @48k，参数上限
//!   base+depth = 14ms = 672 帧，留余量）；
//! - 逐采样 delay = (base + depth·(0.5+0.5·lfo))·sr，lfo ∈ [−1,1]
//!   → delay ∈ [base, base+depth]，分数读 + 线性插值（扫动无 zipper，
//!   镶边音高滑动为调制固有）；
//! - 反馈 tap 插值回写 `write(x + tap·fb)`，fb 上限 |0.9| < 1 → 有界；
//! - LFO 波表（aarch64 无 SIMD sinf，逐采样 libm 调用太贵）。

use super::{EffectProcessor, FxContext};
use crate::fx::delay::DelayLine;
use crate::fx::lfo::SineLfo;

/// 延迟线容量（帧）。
const CAPACITY: usize = 1 << 10;

pub struct Flanger {
    sr: f32,
    rate: f32,
    /// 基础延迟（秒）。
    base_s: f32,
    /// 调制深度（秒）。
    depth_s: f32,
    feedback: f32,
    lfo: SineLfo,
    dl: [DelayLine; 2],
}

impl Flanger {
    pub fn new(sr: f32) -> Self {
        let mut fx = Self {
            sr,
            rate: 0.5,
            base_s: 0.002,
            depth_s: 0.003,
            feedback: 0.4,
            lfo: SineLfo::new(sr),
            dl: std::array::from_fn(|_| DelayLine::new(CAPACITY)),
        };
        fx.lfo.set_rate(fx.rate, fx.sr);
        fx
    }
}

impl EffectProcessor for Flanger {
    fn process(&mut self, out: &mut [f32], _frames: usize, _ctx: &FxContext) {
        let fb = self.feedback;
        let base = self.base_s;
        let depth = self.depth_s;
        for i in 0..out.len() / 2 {
            let lfo_v = self.lfo.next();
            let delay = (base + depth * (0.5 + 0.5 * lfo_v)) * self.sr;
            for c in 0..2 {
                let x = out[i * 2 + c];
                let tap = self.dl[c].read_frac(delay);
                self.dl[c].write(x + tap * fb);
                out[i * 2 + c] = tap;
            }
        }
    }

    fn set_params(&mut self, params: &[f32; 4]) {
        self.rate = params[0].clamp(0.05, 5.0);
        self.base_s = params[1].clamp(0.2, 8.0) / 1000.0;
        self.depth_s = params[2].clamp(0.0, 6.0) / 1000.0;
        self.feedback = params[3].clamp(-0.9, 0.9);
        self.lfo.set_rate(self.rate, self.sr);
    }

    fn reset(&mut self) {
        for dl in self.dl.iter_mut() {
            dl.clear();
        }
        self.lfo.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    /// 在给定帧位置注入脉冲（左声道），返回全部左声道输出。
    fn run_impulses(fx: &mut Flanger, blocks: usize, impulses: &[usize]) -> Vec<f32> {
        let mut out = vec![0.0; 512];
        let mut rec = Vec::new();
        let mut sample = 0;
        for _ in 0..blocks {
            out.fill(0.0);
            for &at in impulses {
                if sample <= at && at < sample + 256 {
                    let i = (at - sample) * 2;
                    out[i] = 1.0;
                    out[i + 1] = 1.0;
                }
            }
            fx.process(&mut out, 256, &FxContext::default());
            for i in 0..256 {
                rec.push(out[i * 2]);
            }
            sample += 256;
        }
        rec
    }

    fn peak_in(rec: &[f32], lo: usize, hi: usize) -> (f32, usize) {
        let mut best = (0.0f32, lo);
        for (i, v) in rec[lo..hi.min(rec.len())].iter().enumerate() {
            let v = v.abs();
            if v > best.0 {
                best = (v, lo + i);
            }
        }
        best
    }

    fn min_in(rec: &[f32], lo: usize, hi: usize) -> (f32, usize) {
        let mut best = (f32::INFINITY, lo);
        for (i, v) in rec[lo..hi.min(rec.len())].iter().enumerate() {
            if *v < best.0 {
                best = (*v, lo + i);
            }
        }
        best
    }

    #[test]
    fn depth_zero_fixed_comb_position() {
        let mut fx = Flanger::new(SR);
        fx.set_params(&[0.5, 4.0, 0.0, 0.0]); // base=4ms，depth=0 → 延迟恒定
        let rec = run_impulses(&mut fx, 4, &[0]);
        let (peak, pos) = peak_in(&rec, 50, rec.len());
        assert!((peak - 1.0).abs() < 0.05, "首回声幅度应 ≈1.0, peak={peak}");
        assert!(
            (pos as i64 - 192).abs() < 4,
            "回声应在 4ms=192 帧处, pos={pos}"
        );
    }

    #[test]
    fn negative_feedback_inverts_second_tap() {
        let mut fx = Flanger::new(SR);
        fx.set_params(&[0.5, 4.0, 0.0, -0.9]); // depth=0：回声间距固定 192 帧
        let rec = run_impulses(&mut fx, 4, &[0]);
        let (min, pos) = min_in(&rec, 384 - 8, 384 + 8);
        assert!(
            (min + 0.9).abs() < 0.1,
            "负反馈第二回声应 ≈ −0.9, min={min} pos={pos}"
        );
    }

    #[test]
    fn lfo_moves_tap() {
        let mut fx = Flanger::new(SR);
        // rate=5Hz：50ms 后相位推进 1/4 周期；depth=6ms 全摆幅
        fx.set_params(&[5.0, 0.2, 6.0, 0.0]);
        let rec = run_impulses(&mut fx, 12, &[0, 2400]);
        let (_, pos1) = peak_in(&rec, 100, 600);
        let (_, pos2) = peak_in(&rec, 2500, 3000);
        // 两脉冲相隔 2400 帧；LFO 相位 0→0.25 使 delay 3.2ms→6.2ms，
        // 回声间距应偏离固定间距 >20 帧
        let drift = (pos2 as i64 - pos1 as i64) - 2400;
        assert!(drift > 20, "LFO 应移动抽头: pos1={pos1} pos2={pos2} drift={drift}");
    }

    #[test]
    fn full_param_sweep_no_nan() {
        let mut fx = Flanger::new(SR);
        let input: Vec<f32> = (0..512)
            .map(|i| (2.0 * std::f32::consts::PI * 997.0 * (i / 2) as f32 / SR).sin() * 0.8)
            .collect();
        for step in 0..40 {
            let t = step as f32 / 39.0;
            fx.set_params(&[
                0.05 + 4.95 * t,
                0.2 + 7.8 * t,
                6.0 * t,
                -0.9 + 1.8 * t,
            ]);
            for _ in 0..4 {
                let mut out = input.clone();
                fx.process(&mut out, 256, &FxContext::default());
                assert!(out.iter().all(|v| v.is_finite()), "step {step} NaN");
            }
        }
    }
}
