//! gate：beat-synced trance gate——按拍网格周期静音/开启。
//!
//! - 相位锚定绝对拍号：每块块首 `cycle_pos = (beats_total + offset) mod
//!   period`（beats_total 为块首绝对拍，seek/sync 改率后自校正；
//!   不受 rack 旁路冻结影响——重开时按当前网格重锚）；
//! - 逐采样目标 = cycle_pos < duty·period ? 1 : 0（开/关各持续
//!   duty×period / (1−duty)×period 拍，如 1 拍内静音 0.5 播放 0.5）；
//!   块内 cycle_pos += 1/cycle_len_frames（cycle_len_frames =
//!   period 拍 × beat_period_secs × sr，rate/bpm 变化随块自校正）；
//! - 一阶包络同速攻释（coeff = 1−exp(−1/(τ·sr))，τ = smooth ms）
//!   ——方波开关本身有台阶，靠包络斜坡无 click（测试钉 Δ 界）；
//! - 无网格（grid_valid=false / period 非法）→ 逐位直通，不碰状态；
//!   网格恢复时包络从旧值斜坡到新目标，无 click。

use super::{EffectProcessor, FxContext};

pub struct Gate {
    sr: f32,
    /// 门周期（拍，0.25 步进吸附）。
    period: f32,
    /// 开启占比 0.05..0.95。
    duty: f32,
    /// 包络平滑（ms）。
    smooth_ms: f32,
    /// 相位偏移（拍）。
    offset: f32,
    /// 一阶包络系数（smooth 变化时更新）。
    coeff: f32,
    /// 包络状态（共享双声道）。
    env: f32,
}

impl Gate {
    pub fn new(sr: f32) -> Self {
        let mut fx = Self {
            sr,
            period: 1.0,
            duty: 0.5,
            smooth_ms: 5.0,
            offset: 0.0,
            coeff: 0.0,
            env: 0.0,
        };
        fx.recompute_coeff();
        fx
    }

    fn recompute_coeff(&mut self) {
        self.coeff = 1.0 - (-1.0 / (self.smooth_ms * 0.001 * self.sr)).exp();
    }
}

impl EffectProcessor for Gate {
    fn process(&mut self, out: &mut [f32], _frames: usize, ctx: &FxContext) {
        // 无网格 / 非法周期 → 逐位直通（不碰 env；恢复时包络自然斜坡）
        if !ctx.grid_valid || !ctx.beat_period_secs.is_finite() || ctx.beat_period_secs <= 0.0 {
            return;
        }
        let period = self.period;
        let cycle_len_frames = period as f64 * ctx.beat_period_secs * self.sr as f64;
        if cycle_len_frames < 1.0 {
            return; // 防御：周期短于 1 帧无意义
        }
        let on_beats = self.duty * period;
        let inc = 1.0 / cycle_len_frames as f32;
        let coeff = self.coeff;
        let mut cycle_pos = ((ctx.beats_total + self.offset as f64) % period as f64) as f32;
        for i in 0..out.len() / 2 {
            let target = if cycle_pos < on_beats { 1.0 } else { 0.0 };
            self.env += coeff * (target - self.env);
            let g = self.env;
            out[i * 2] *= g;
            out[i * 2 + 1] *= g;
            cycle_pos += inc;
            if cycle_pos >= period {
                cycle_pos -= period;
            }
        }
    }

    fn set_params(&mut self, params: &[f32; 4]) {
        // period 0.25 步进吸附（stepped 参数）
        self.period = ((params[0].clamp(0.25, 8.0) * 4.0).round() / 4.0).max(0.25);
        self.duty = params[1].clamp(0.05, 0.95);
        self.smooth_ms = params[2].clamp(1.0, 50.0);
        self.offset = params[3].clamp(0.0, 1.0);
        self.recompute_coeff();
    }

    fn reset(&mut self) {
        // 清包络；cycle_pos 每块从 ctx 重锚，无需状态
        self.env = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    /// 120BPM 网格上下文（每拍 0.5s）。
    fn ctx_120(beats_total: f64) -> FxContext {
        FxContext {
            beats_total,
            beat_phase_01: 0.0,
            beat_period_secs: 0.5,
            grid_valid: true,
        }
    }

    /// DC 输入逐块跑 secs 秒；beats_total 按块推进（120BPM）。
    fn run_dc(fx: &mut Gate, secs: f32, dc: f32) -> Vec<f32> {
        let blocks = (secs * SR / 256.0) as usize;
        let beats_per_block = 256.0 / (0.5 * SR);
        let mut rec = Vec::new();
        for b in 0..blocks {
            let mut out = vec![dc; 512];
            fx.process(&mut out, 256, &ctx_120(b as f64 * beats_per_block as f64));
            for i in 0..256 {
                rec.push(out[i * 2]);
            }
        }
        rec
    }

    fn window_rms(rec: &[f32], lo: f32, hi: f32) -> f32 {
        let lo = (lo * SR) as usize;
        let hi = ((hi * SR) as usize).min(rec.len());
        let sum: f32 = rec[lo..hi].iter().map(|v| v * v).sum();
        (sum / (hi - lo) as f32).sqrt()
    }

    #[test]
    fn alternates_on_beat_grid() {
        // 120BPM、period=1 拍、duty=0.5：每 0.5s 半开半关，3 周期
        let mut fx = Gate::new(SR);
        fx.set_params(&[1.0, 0.5, 5.0, 0.0]);
        let rec = run_dc(&mut fx, 2.0, 0.5);
        // duty=0.5：0.5s 周期内开 0.25s 关 0.25s；开/关窗避开沿（±20ms+）
        for (lo, hi) in [(0.02, 0.23), (0.52, 0.73), (1.02, 1.23)] {
            let r = window_rms(&rec, lo, hi);
            assert!(r > 0.45, "开窗 [{lo},{hi}]s 应 ≈0.5, rms={r}");
        }
        for (lo, hi) in [(0.27, 0.48), (0.77, 0.98)] {
            let r = window_rms(&rec, lo, hi);
            assert!(r < 0.02, "关窗 [{lo},{hi}]s 应 ≈0, rms={r}");
        }
    }

    #[test]
    fn duty_one_passes_through() {
        // duty=1：目标恒 1，包络稳定后透传（RMS 比 ≈1）
        let mut fx = Gate::new(SR);
        fx.set_params(&[1.0, 0.95, 5.0, 0.0]);
        let rec = run_dc(&mut fx, 0.5, 0.5);
        let steady = window_rms(&rec, 0.1, 0.45);
        assert!((steady / 0.5 - 1.0).abs() < 1e-3, "duty≈1 应透传, rms={steady}");
    }

    #[test]
    fn period_two_alternates_every_other_beat() {
        // period=2 拍：拍 0-1 开、拍 1-2 关（0.5s 网格 × 2）
        let mut fx = Gate::new(SR);
        fx.set_params(&[2.0, 0.5, 5.0, 0.0]);
        let rec = run_dc(&mut fx, 3.0, 0.5);
        // duty=0.5：1.0s 周期内开 0.5s 关 0.5s
        let on1 = window_rms(&rec, 0.02, 0.48);
        let off1 = window_rms(&rec, 0.52, 0.98);
        let on2 = window_rms(&rec, 1.02, 1.48);
        assert!(on1 > 0.45, "拍 0-1 应开, rms={on1}");
        assert!(off1 < 0.02, "拍 1-2 应关, rms={off1}");
        assert!(on2 > 0.45, "拍 2-3 应开, rms={on2}");
    }

    #[test]
    fn no_grid_is_bitwise_passthrough() {
        let mut fx = Gate::new(SR);
        fx.set_params(&[1.0, 0.5, 5.0, 0.0]);
        let input: Vec<f32> = (0..512)
            .map(|i| (2.0 * std::f32::consts::PI * 997.0 * (i / 2) as f32 / SR).sin() * 0.8)
            .collect();
        let mut out = input.clone();
        fx.process(&mut out, 256, &FxContext::default());
        assert_eq!(out, input, "无网格逐位直通");
    }

    #[test]
    fn boundary_is_clickfree() {
        // 100Hz 正弦 + smooth=1ms（最陡包络）：开关沿逐采样 Δ 有界
        let mut fx = Gate::new(SR);
        fx.set_params(&[1.0, 0.5, 1.0, 0.0]);
        let blocks = (1.0 * SR / 256.0) as usize;
        let beats_per_block = 256.0 / (0.5 * SR);
        let mut max_delta = 0.0f32;
        let mut prev = 0.0f32;
        for b in 0..blocks {
            let mut out = vec![0.0; 512];
            for i in 0..256 {
                let t = (b * 256 + i) as f32 / SR;
                let x = (2.0 * std::f32::consts::PI * 100.0 * t).sin() * 0.5;
                out[i * 2] = x;
                out[i * 2 + 1] = x;
            }
            fx.process(&mut out, 256, &ctx_120(b as f64 * beats_per_block as f64));
            for i in 0..256 {
                max_delta = max_delta.max((out[i * 2] - prev).abs());
                prev = out[i * 2];
            }
        }
        assert!(max_delta < 0.05, "开关沿逐采样 Δ 过大: {max_delta}");
    }

    #[test]
    fn offset_inverts_phase() {
        // offset=0.5 拍：开/关窗互换
        let mut fx = Gate::new(SR);
        fx.set_params(&[1.0, 0.5, 5.0, 0.5]);
        let rec = run_dc(&mut fx, 1.0, 0.5);
        // offset=0.5 拍（0.25s）：开窗后移 0.25s——[0.05,0.2] 关、[0.3,0.45] 开
        let first = window_rms(&rec, 0.02, 0.23);
        let second = window_rms(&rec, 0.27, 0.48);
        assert!(first < 0.02, "offset=0.5 时首窗应关, rms={first}");
        assert!(second > 0.45, "offset=0.5 时次窗应开, rms={second}");
    }

    #[test]
    fn full_param_sweep_no_nan() {
        let mut fx = Gate::new(SR);
        let beats_per_block = 256.0 / (0.5 * SR);
        for step in 0..40 {
            let t = step as f32 / 39.0;
            fx.set_params(&[0.25 + 7.75 * t, 0.05 + 0.9 * t, 1.0 + 49.0 * t, t]);
            for b in 0..4 {
                let mut out = vec![0.0; 512];
                for i in 0..256 {
                    let s = (b * 256 + i) as f32 / SR;
                    let x = (2.0 * std::f32::consts::PI * 997.0 * s).sin() * 0.5;
                    out[i * 2] = x;
                    out[i * 2 + 1] = x;
                }
                let ctx = ctx_120(step as f64 * 4.0 * beats_per_block as f64);
                fx.process(&mut out, 256, &ctx);
                assert!(out.iter().all(|v| v.is_finite()), "step {step} NaN");
            }
        }
    }
}
