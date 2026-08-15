//! 失真：tanh 软削波 + 输入驱动 / 输出补偿（P1 参考效果，最早落地）。
//!
//! 增益（drive 与 makeup）各自经 10ms Smoother——参数阶跃逐采样
//! 平滑无 click。备用有理近似 x(27+x²)/(27+9x²)（fx_spike 若显示
//! tanhf 超支再换，const fn 切换 + 测试重跑）。

use super::{EffectProcessor, FxContext};
use crate::dsp::smoother::Smoother;

const SMOOTH_TAU_S: f32 = 0.010;

pub struct Distortion {
    /// 线性 drive 增益（10^(dB/20)），平滑。
    drive: Smoother,
    /// 线性 makeup 增益，平滑。
    makeup: Smoother,
}

impl Distortion {
    pub fn new(sr: f32) -> Self {
        let coeff = 1.0 - (-1.0 / (SMOOTH_TAU_S * sr)).exp();
        Self {
            drive: Smoother::new(1.0, coeff),
            makeup: Smoother::new(1.0, coeff),
        }
    }
}

impl EffectProcessor for Distortion {
    fn process(&mut self, out: &mut [f32], _frames: usize, _ctx: &FxContext) {
        for v in out.iter_mut() {
            let d = self.drive.step();
            let m = self.makeup.step();
            *v = (*v * d).tanh() * m;
        }
    }

    fn set_params(&mut self, params: &[f32; 4]) {
        let drive_db = params[0].clamp(0.0, 40.0);
        let makeup_db = params[1].clamp(-12.0, 12.0);
        self.drive.set_target(10f32.powf(drive_db / 20.0));
        self.makeup.set_target(10f32.powf(makeup_db / 20.0));
    }

    fn reset(&mut self) {
        // 只清当前值、保留目标（deck 每块重新快照参数；换型后 mix 从 0
        // 淡入期间 set_params 会重写目标）
        self.drive.set_current(1.0);
        self.makeup.set_current(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    fn process_blocks(fx: &mut Distortion, input: &[f32], frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        let mut all = Vec::with_capacity(input.len());
        for chunk in input.chunks(frames * 2) {
            out.copy_from_slice(chunk);
            fx.process(&mut out, frames, &FxContext::default());
            all.extend_from_slice(&out);
        }
        all
    }

    #[test]
    fn zero_drive_is_identity_for_small_signal() {
        let mut fx = Distortion::new(SR);
        fx.set_params(&[0.0, 0.0, 0.0, 0.0]);
        let input: Vec<f32> = (0..512)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * (i / 2) as f32 / SR).sin() * 0.01)
            .collect();
        let out = process_blocks(&mut fx, &input, 256);
        let err: f32 = out
            .iter()
            .zip(input.iter())
            .map(|(a, b)| (a - b).abs())
            .sum::<f32>()
            / 512.0;
        assert!(err < 0.02, "drive 0 应近似恒等, err={err}");
    }

    #[test]
    fn drive_clips_peak() {
        let mut fx = Distortion::new(SR);
        fx.set_params(&[40.0, 0.0, 0.0, 0.0]);
        let input = vec![0.5; 512];
        let out = process_blocks(&mut fx, &input, 256);
        let peak = out.iter().fold(0.0f32, |a, v| a.max(v.abs()));
        assert!(peak > 0.9, "40dB drive 应推到饱和区, peak={peak}");
        assert!(peak <= 1.01, "tanh 有界, peak={peak}");
    }

    #[test]
    fn odd_symmetry() {
        let mut fx = Distortion::new(SR);
        fx.set_params(&[12.0, 0.0, 0.0, 0.0]);
        let ramp: Vec<f32> = (-256..256).map(|i| i as f32 / 512.0).collect();
        let pos = process_blocks(&mut fx, &ramp, 256);
        // 复位只清当前值（set_current）——target 保留，两次运行的增益
        // 斜坡逐位一致，奇对称比较才不含斜坡差异
        fx.reset();
        let neg_in: Vec<f32> = ramp.iter().map(|v| -v).collect();
        let neg = process_blocks(&mut fx, &neg_in, 256);
        for (a, b) in pos.iter().zip(neg.iter()) {
            assert!((a + b).abs() < 1e-4, "out(-x) = -out(x): {a} vs {b}");
        }
    }

    #[test]
    fn drive_step_is_clickfree() {
        let mut fx = Distortion::new(SR);
        fx.set_params(&[0.0, 0.0, 0.0, 0.0]);
        // DC 输入：输出 Δ 只反映增益斜坡（正弦信号的固有斜率会随增益
        // 放大，混淆 click 判据）
        let input = vec![0.5; 512];
        let mut out = input.clone();
        fx.process(&mut out, 256, &FxContext::default());
        // 阶跃到 40dB：下一块起逐采样 Δ 有界（Smoother 斜坡）
        fx.set_params(&[40.0, 0.0, 0.0, 0.0]);
        let mut max_delta = 0.0f32;
        let mut prev = out[out.len() - 1];
        for _ in 0..4 {
            fx.process(&mut out, 256, &FxContext::default());
            for &v in out.iter() {
                max_delta = max_delta.max((v - prev).abs());
                prev = v;
            }
        }
        assert!(max_delta < 0.2, "drive 阶跃逐采样 Δ 过大: {max_delta}");
    }

    #[test]
    fn full_param_sweep_no_nan() {
        let mut fx = Distortion::new(SR);
        let input: Vec<f32> = (0..512)
            .map(|i| (2.0 * std::f32::consts::PI * 997.0 * (i / 2) as f32 / SR).sin() * 0.8)
            .collect();
        for step in 0..50 {
            let drive = 40.0 * step as f32 / 49.0;
            let makeup = -12.0 + 24.0 * step as f32 / 49.0;
            fx.set_params(&[drive, makeup, 0.0, 0.0]);
            let mut out = input.clone();
            fx.process(&mut out, 256, &FxContext::default());
            assert!(out.iter().all(|v| v.is_finite()), "step {step} NaN");
        }
    }
}
