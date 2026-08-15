//! filter-linear：双二阶 LP/HP/BP（L/R 各自状态）。
//!
//! cutoff 走 log2 域 10ms Smoother（对数轴听感均匀，扫频无 zipper）；
//! 逐采样 maybe_retune 仿 EQ（平滑值变化超过 0.02 倍频程才重算系数）；
//! mode 切换 → set_kind + 状态清零（DF2T 旧模式状态无意义，清零后
//! 从干声连续响应，无 click）。Q 不单独平滑（用户操作频率低，
//! 换值后经 retune 即时生效）。

use super::{EffectProcessor, FxContext};
use crate::dsp::biquad::{Biquad, BiquadKind};
use crate::dsp::smoother::Smoother;

const SMOOTH_TAU_S: f32 = 0.010;
/// 平滑值变化超过此阈值（倍频程）才重算系数。
const RETUNE_EPS_OCT: f32 = 0.02;

pub struct FilterLinear {
    /// 0=LP 1=HP 2=BP（stepped 参数，已吸附）。
    mode: f32,
    /// log2(cutoff)，平滑。
    cutoff_log2: Smoother,
    q: f32,
    /// 上次重算时的平滑值与 Q（仿 EQ applied 快照）。
    applied_log2: f32,
    applied_q: f32,
    l: Biquad,
    r: Biquad,
}

impl FilterLinear {
    pub fn new(sr: f32) -> Self {
        let coeff = 1.0 - (-1.0 / (SMOOTH_TAU_S * sr)).exp();
        let mk = |kind| Biquad::new(kind, sr, 1000.0, 0.707, 0.0);
        Self {
            mode: 0.0,
            cutoff_log2: Smoother::new(1000.0f32.log2(), coeff),
            q: 0.707,
            applied_log2: 1000.0f32.log2(),
            applied_q: 0.707,
            l: mk(BiquadKind::LowPass),
            r: mk(BiquadKind::LowPass),
        }
    }

    fn kind_for(mode: f32) -> BiquadKind {
        match mode as i32 {
            1 => BiquadKind::HighPass,
            2 => BiquadKind::BandPass,
            _ => BiquadKind::LowPass,
        }
    }

    fn retune(&mut self, log2_fc: f32) {
        let fc = 2f32.powf(log2_fc);
        self.l.set_params(fc, self.q, 0.0);
        self.r.set_params(fc, self.q, 0.0);
        self.applied_log2 = log2_fc;
        self.applied_q = self.q;
    }

    /// 逐采样调用：平滑值越过阈值（或 Q 已变、或已稳定但未重算）→ 重算。
    #[inline]
    fn maybe_retune(&mut self) {
        let cur = self.cutoff_log2.step();
        let need = (cur - self.applied_log2).abs() > RETUNE_EPS_OCT || self.q != self.applied_q;
        if need || (self.cutoff_log2.settled() && cur != self.applied_log2) {
            self.retune(cur);
        }
    }
}

impl EffectProcessor for FilterLinear {
    fn process(&mut self, out: &mut [f32], _frames: usize, _ctx: &FxContext) {
        for i in 0..out.len() / 2 {
            self.maybe_retune();
            out[i * 2] = self.l.process(out[i * 2]);
            out[i * 2 + 1] = self.r.process(out[i * 2 + 1]);
        }
    }

    fn set_params(&mut self, params: &[f32; 4]) {
        let mode = (params[0].clamp(0.0, 2.0)).round();
        let cutoff = params[1].clamp(20.0, 20000.0);
        let q = params[2].clamp(0.5, 16.0);
        if mode != self.mode {
            self.mode = mode;
            let kind = Self::kind_for(mode);
            self.l.set_kind(kind);
            self.r.set_kind(kind);
            // 旧模式状态对新系数无意义：清零（DF2T 首样本输出由输入
            // 与 b 系数决定，与旧输出连续，无 click）
            self.l.reset();
            self.r.reset();
            // 强制下一次 maybe_retune 重算（NaN 哨兵绕过阈值）
            self.applied_log2 = f32::NAN;
        }
        self.cutoff_log2.set_target(cutoff.log2());
        self.q = q;
    }

    fn reset(&mut self) {
        // 只清滤波状态；目标值由 rack 每块重新快照
        self.l.reset();
        self.r.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    /// 单声道正弦 RMS（响应测量用；绕过 EffectProcessor 的交织约定）。
    fn rms(fx: &mut FilterLinear, freq: f32, secs: f32) -> f32 {
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

    fn set(fx: &mut FilterLinear, mode: f32, cutoff: f32, q: f32) {
        fx.set_params(&[mode, cutoff, q, 0.0]);
        // 让 smoother 稳定到目标（0.5s 远超 10ms τ）
        let mut out = [0.0; 2];
        for _ in 0..(0.5 * SR) as usize {
            fx.process(&mut out, 1, &FxContext::default());
        }
    }

    #[test]
    fn lp_passes_low_stops_high() {
        let mut fx = FilterLinear::new(SR);
        set(&mut fx, 0.0, 1000.0, 0.707);
        let low = rms(&mut fx, 100.0, 0.5);
        let high = rms(&mut fx, 8000.0, 0.5);
        assert!(low > high * 10.0, "LP: low={low} high={high}");
    }

    #[test]
    fn hp_passes_high_stops_low() {
        let mut fx = FilterLinear::new(SR);
        set(&mut fx, 1.0, 1000.0, 0.707);
        let low = rms(&mut fx, 100.0, 0.5);
        let high = rms(&mut fx, 8000.0, 0.5);
        assert!(high > low * 10.0, "HP: low={low} high={high}");
    }

    #[test]
    fn bp_passes_center_stops_flanks() {
        let mut fx = FilterLinear::new(SR);
        set(&mut fx, 2.0, 1000.0, 4.0);
        let center = rms(&mut fx, 1000.0, 0.5);
        let low = rms(&mut fx, 125.0, 0.5);
        let high = rms(&mut fx, 8000.0, 0.5);
        assert!(center > low * 10.0, "BP: center={center} low={low}");
        assert!(center > high * 10.0, "BP: center={center} high={high}");
    }

    #[test]
    fn cutoff_step_is_clickfree() {
        let mut fx = FilterLinear::new(SR);
        // HP + 1kHz 正弦：cutoff 20→20k 把输出从满幅压到近 0。
        // 连续输入跨块（原位契约：每块拷入新鲜内容，杜绝块界相位跳变）
        fx.set_params(&[1.0, 20.0, 0.707, 0.0]);
        let blocks = 9;
        let input: Vec<f32> =
            (0..blocks * 512).map(|i| (2.0 * std::f32::consts::PI * 1000.0 * (i / 2) as f32 / SR).sin() * 0.5).collect();
        // 第 0 块：cutoff 从初始 1kHz 平滑滑到 20Hz（HP 已通过 1kHz）
        let mut out = input[..512].to_vec();
        fx.process(&mut out, 256, &FxContext::default());
        // 阶跃到 20kHz：后续块输出包络衰减，逐采样 Δ 有界
        fx.set_params(&[1.0, 20000.0, 0.707, 0.0]);
        let mut max_delta = 0.0f32;
        let mut prev = out[510];
        for b in 1..blocks {
            out.copy_from_slice(&input[b * 512..(b + 1) * 512]);
            fx.process(&mut out, 256, &FxContext::default());
            for i in 0..256 {
                max_delta = max_delta.max((out[i * 2] - prev).abs());
                prev = out[i * 2];
            }
        }
        assert!(max_delta < 0.2, "cutoff 阶跃逐采样 Δ 过大: {max_delta}");
    }

    #[test]
    fn mode_switch_no_nan_and_settles() {
        let mut fx = FilterLinear::new(SR);
        // DC 输入：LP(1k) 输出 ≈0.5，切 HP(1k) 后衰减到 0（HP 杀 DC）。
        // DF2T 高通对 DC 阶跃有合法振铃（Q=0.707 时峰值 ≈1.1× 输入），
        // 逐采样 Δ 不作 click 判据——真实链路 mode 切换发生在 rack 的
        // mix 淡入斜坡下（换型 mix 从 0 爬升），此处只钉：无 NaN、
        // 有界、稳态正确
        fx.set_params(&[0.0, 1000.0, 0.707, 0.0]);
        let mut out = vec![0.5; 512];
        fx.process(&mut out, 256, &FxContext::default());
        assert!((out[510] - 0.5).abs() < 0.05, "LP 稳态应透传 DC");
        fx.set_params(&[1.0, 1000.0, 0.707, 0.0]);
        let mut peak = 0.0f32;
        let mut last = 0.0;
        for _ in 0..16 {
            out.fill(0.5); // 原位契约：每块新鲜 DC
            fx.process(&mut out, 256, &FxContext::default());
            for i in 0..256 {
                peak = peak.max(out[i * 2].abs());
                last = out[i * 2];
            }
        }
        assert!(out.iter().all(|v| v.is_finite()), "无 NaN");
        assert!(peak < 1.5, "振铃有界, peak={peak}");
        assert!(last.abs() < 0.05, "HP 稳态应杀 DC, last={last}");
    }

    #[test]
    fn full_param_sweep_no_nan() {
        let mut fx = FilterLinear::new(SR);
        let input: Vec<f32> = (0..512)
            .map(|i| (2.0 * std::f32::consts::PI * 997.0 * (i / 2) as f32 / SR).sin() * 0.8)
            .collect();
        for step in 0..60 {
            let mode = (step % 3) as f32;
            let cutoff = 20.0 * (20000.0f32 / 20.0).powf(step as f32 / 59.0);
            let q = 0.5 + 15.5 * step as f32 / 59.0;
            fx.set_params(&[mode, cutoff, q, 0.0]);
            let mut out = input.clone();
            fx.process(&mut out, 256, &FxContext::default());
            assert!(out.iter().all(|v| v.is_finite()), "step {step} NaN");
        }
    }
}
