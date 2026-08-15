//! echo：双声道分数延迟 + 反馈路径阻尼低通 + 可选拍同步。
//!
//! - 延迟线 2^17 帧/声道（2.73s @48k，参数上限 2.0s + 斜坡余量）；
//! - time 变化经 50ms Smoother 逐采样滑向目标（分数读 + 线性插值，
//!   扫动无 click，带磁带式滑音——延迟调制固有）；
//! - 反馈路径 Biquad LP：cutoff = 20000 × 0.01^damp（damp=0 ≈ 透明、
//!   damp=1 → 200Hz），damp 变化每块重算系数（状态保留）；
//! - sync=1：目标时值吸附最近 1/4 拍（round(time/period×4)/4×period，
//!   clamp 0.05–2.0s）；无网格回落自由时值。

use super::{EffectProcessor, FxContext};
use crate::dsp::biquad::{Biquad, BiquadKind};
use crate::dsp::smoother::Smoother;
use crate::fx::delay::DelayLine;

/// 延迟线容量（帧）。
const CAPACITY: usize = 1 << 17;
/// time 平滑时间常数（秒）。
const TIME_TAU_S: f32 = 0.050;

pub struct Echo {
    sr: f32,
    /// 延迟时间（秒），平滑。
    time: Smoother,
    /// 原始时值参数（sync 吸附的目标基准）。
    time_param: f32,
    feedback: f32,
    damp: f32,
    sync_on: bool,
    /// 反馈路径阻尼 LP（每声道一个）。
    damp_lp: [Biquad; 2],
    dl: [DelayLine; 2],
}

impl Echo {
    pub fn new(sr: f32) -> Self {
        let coeff = 1.0 - (-1.0 / (TIME_TAU_S * sr)).exp();
        let mk = || Biquad::new(BiquadKind::LowPass, sr, 20000.0, 0.707, 0.0);
        Self {
            sr,
            time: Smoother::new(0.375, coeff),
            time_param: 0.375,
            feedback: 0.35,
            damp: 0.3,
            sync_on: false,
            damp_lp: [mk(), mk()],
            dl: std::array::from_fn(|_| DelayLine::new(CAPACITY)),
        }
    }

    /// damp 变化 → 重算阻尼 LP 系数（每块一次，状态保留）。
    fn retune_damp(&mut self) {
        let cutoff = 20000.0 * 0.01f32.powf(self.damp);
        for b in self.damp_lp.iter_mut() {
            b.set_params(cutoff, 0.707, 0.0);
        }
    }
}

impl EffectProcessor for Echo {
    fn process(&mut self, out: &mut [f32], _frames: usize, ctx: &FxContext) {
        // 每块重算目标：sync 吸附依赖出声节奏（rate 可能在块间变化）
        if self.sync_on && ctx.grid_valid {
            let per = ctx.beat_period_secs as f32;
            let snapped = ((self.time_param / per * 4.0).round() / 4.0 * per).clamp(0.05, 2.0);
            self.time.set_target(snapped);
        } else {
            self.time.set_target(self.time_param);
        }
        let fb = self.feedback;
        for i in 0..out.len() / 2 {
            let t = self.time.step() * self.sr;
            for c in 0..2 {
                let x = out[i * 2 + c];
                let wet = self.dl[c].read_frac(t);
                let fb_in = self.damp_lp[c].process(wet);
                self.dl[c].write(x + fb_in * fb);
                out[i * 2 + c] = wet;
            }
        }
    }

    fn set_params(&mut self, params: &[f32; 4]) {
        self.time_param = params[0].clamp(0.01, 2.0);
        self.feedback = params[1].clamp(0.0, 0.95);
        let damp = params[2].clamp(0.0, 1.0);
        if damp != self.damp {
            self.damp = damp;
            self.retune_damp();
        }
        self.sync_on = params[3].round() >= 1.0;
    }

    fn reset(&mut self) {
        for dl in self.dl.iter_mut() {
            dl.clear();
        }
        for b in self.damp_lp.iter_mut() {
            b.reset();
        }
        // 时值目标由 rack 每块重新快照；current 保留到目标重设前无害
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    /// 逐块喂入交织立体声（左=impulse），返回全部左声道输出。
    /// frames = 捕获帧数（每块贡献 256 帧）。
    fn impulse_echo(fx: &mut Echo, frames: usize, impulse_at: usize) -> Vec<f32> {
        let mut out = vec![0.0; 512];
        let mut rec = Vec::new();
        let mut sample = 0;
        for _ in 0..frames / 256 {
            out.fill(0.0);
            if sample <= impulse_at && impulse_at < sample + 256 {
                let i = (impulse_at - sample) * 2;
                out[i] = 1.0;
                out[i + 1] = 1.0;
            }
            fx.process(&mut out, 256, &FxContext::default());
            for i in 0..256 {
                rec.push(out[i * 2]);
            }
            sample += 256;
        }
        rec
    }

    /// 持续正弦块序列（块间相位连续）。
    fn sine_blocks(freq: f32, blocks: usize) -> Vec<Vec<f32>> {
        (0..blocks)
            .map(|b| {
                (0..512)
                    .map(|i| {
                        let t = (b * 512 + i) as f32 / SR;
                        (2.0 * std::f32::consts::PI * freq * t).sin() * 0.5
                    })
                    .collect::<Vec<f32>>()
            })
            .collect()
    }

    /// 设置参数并让 time smoother 稳定（0.5s 远超 50ms τ；空输入无害）。
    fn set(fx: &mut Echo, time: f32, feedback: f32, damp: f32, sync: f32) {
        fx.set_params(&[time, feedback, damp, sync]);
        let mut out = vec![0.0; 512];
        for _ in 0..(0.5 * SR / 256.0) as usize {
            fx.process(&mut out, 256, &FxContext::default());
        }
    }

    /// 找 [lo, hi) 内峰值及其位置（帧索引）。
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

    #[test]
    fn pulse_echoes_at_delay() {
        let mut fx = Echo::new(SR);
        set(&mut fx, 0.1, 0.0, 0.0, 0.0);
        let rec = impulse_echo(&mut fx, 7200, 0); // 0.15s > 0.1s 回声
        let (peak, pos) = peak_in(&rec, 100, rec.len());
        assert!((peak - 1.0).abs() < 0.05, "首回声幅度应 ≈1.0, peak={peak}");
        assert!(
            (pos as i64 - 4800).abs() < 4,
            "回声应在 0.1s=4800 帧处, pos={pos}"
        );
    }

    #[test]
    fn feedback_peaks_decay_geometrically() {
        let mut fx = Echo::new(SR);
        set(&mut fx, 0.1, 0.5, 0.0, 0.0);
        let rec = impulse_echo(&mut fx, 4 * 4800 + 2400, 0); // 回声 #1..#4 全覆盖
        let d = 4800usize;
        // 窗内能量和（分数延迟插值涂抹不敏感）：相邻回声能量比 =
        // fb²×LP 能量增益（≈0.25×0.945≈0.236），且几何恒定
        let energy = |k: usize| -> f32 {
            let lo = k * d - d / 2;
            let hi = (k * d + d / 2).min(rec.len());
            rec[lo..hi].iter().map(|v| v * v).sum::<f32>()
        };
        let e1 = energy(1);
        let e2 = energy(2);
        let e3 = energy(3);
        let r21 = e2 / e1;
        let r32 = e3 / e2;
        assert!(
            (0.15..=0.35).contains(&r21),
            "回声能量比应 ≈ fb²·G≈0.236, r21={r21}"
        );
        assert!(
            (r32 - r21).abs() < 0.05,
            "几何衰减：相邻能量比应恒定, r21={r21} r32={r32}"
        );
        // 回声 k 位置钉在 k×4800 帧
        let (_, pos) = peak_in(&rec, d - d / 2, d + d / 2);
        assert!(
            (pos as i64 - d as i64).abs() < 8,
            "回声 1 应在 4800 帧, pos={pos}"
        );
    }

    #[test]
    fn damp_attenuates_high_feedback_loop() {
        // 回声 #2 绝对能量（一次循环的能量增益）：
        // open：LP 近透明（20kHz 截止，G≈0.95）→ E2 ≈ fb²·G ≈ 0.77；
        // damped：宽带脉冲过 200Hz LP，循环能量增益 G≈0.01 → E2 ≈ 0.009。
        // 能量比 E3/E2 不可用：2 次循环后回声能量集中于 LP 通带
        // （≈170Hz 环频，|H|≈0.75）→ damped 比 ≈0.9²·0.75≈0.61，
        // 与 open 的 ≈0.77 无法区分。持续正弦同理（尾窗是直接回声）。
        let run = |damp: f32| -> f32 {
            let mut fx = Echo::new(SR);
            set(&mut fx, 0.1, 0.9, damp, 0.0);
            let rec = impulse_echo(&mut fx, 4 * 4800 + 2400, 0);
            let lo = 2 * 4800 - 2400;
            let hi = (2 * 4800 + 2400).min(rec.len());
            rec[lo..hi].iter().map(|v| v * v).sum::<f32>()
        };
        let open = run(0.0);
        let damped = run(1.0);
        assert!(
            damped < open * 0.05,
            "damp 应压制高频反馈: E2 open={open} damped={damped}"
        );
    }

    #[test]
    fn time_change_is_clickfree() {
        let mut fx = Echo::new(SR);
        // 30Hz 低频正弦：信号固有斜率小，Δ 预算留给时间斜坡的滑音
        set(&mut fx, 0.1, 0.0, 0.0, 0.0);
        let blocks = sine_blocks(30.0, 24);
        let mut out = vec![0.0; 512];
        for block in &blocks[..8] {
            out.fill(0.0);
            for i in 0..256 {
                out[i * 2] = block[i * 2];
                out[i * 2 + 1] = block[i * 2 + 1];
            }
            fx.process(&mut out, 256, &FxContext::default());
        }
        // 阶跃 time 0.1 → 1.0s（50ms 斜坡，τ' 最大 ≈18 帧/采样 → 滑音）
        fx.set_params(&[1.0, 0.0, 0.0, 0.0]);
        let mut max_delta = 0.0f32;
        let mut prev = out[510];
        for block in &blocks[8..24] {
            out.fill(0.0);
            for i in 0..256 {
                out[i * 2] = block[i * 2];
                out[i * 2 + 1] = block[i * 2 + 1];
            }
            fx.process(&mut out, 256, &FxContext::default());
            for i in 0..256 {
                max_delta = max_delta.max((out[i * 2] - prev).abs());
                prev = out[i * 2];
            }
        }
        assert!(max_delta < 0.1, "time 阶跃逐采样 Δ 过大: {max_delta}");
    }

    #[test]
    fn sync_snaps_to_quarter_beat() {
        let ctx = FxContext {
            beats_total: 0.0,
            beat_phase_01: 0.0,
            beat_period_secs: 0.5, // 120 BPM
            grid_valid: true,
        };
        // 自由模式：0.51s → 24480 帧
        let mut fx = Echo::new(SR);
        fx.set_params(&[0.51, 0.0, 0.0, 0.0]);
        let mut out = vec![0.0; 512];
        for _ in 0..(0.5 * SR / 256.0) as usize {
            fx.process(&mut out, 256, &ctx);
        }
        out.fill(0.0);
        out[0] = 1.0;
        out[1] = 1.0;
        let mut rec = Vec::new();
        for _ in 0..(0.7 * SR / 256.0) as usize {
            fx.process(&mut out, 256, &ctx);
            for i in 0..256 {
                rec.push(out[i * 2]);
            }
            out.fill(0.0);
        }
        let (_, pos_free) = peak_in(&rec, 1000, rec.len());
        assert!(
            (pos_free as i64 - 24480).abs() < 32,
            "自由模式回声应在 0.51s=24480 帧, pos={pos_free}"
        );
        // sync=1：吸附到 1.0 拍 = 0.5s → 24000 帧
        let mut fx = Echo::new(SR);
        fx.set_params(&[0.51, 0.0, 0.0, 1.0]);
        let mut out = vec![0.0; 512];
        for _ in 0..(0.5 * SR / 256.0) as usize {
            fx.process(&mut out, 256, &ctx);
        }
        out.fill(0.0);
        out[0] = 1.0;
        out[1] = 1.0;
        let mut rec = Vec::new();
        for _ in 0..(0.7 * SR / 256.0) as usize {
            fx.process(&mut out, 256, &ctx);
            for i in 0..256 {
                rec.push(out[i * 2]);
            }
            out.fill(0.0);
        }
        let (_, pos_sync) = peak_in(&rec, 1000, rec.len());
        assert!(
            (pos_sync as i64 - 24000).abs() < 32,
            "sync 回声应吸附 0.5s=24000 帧, pos={pos_sync}"
        );
    }

    #[test]
    fn full_param_sweep_no_nan() {
        let mut fx = Echo::new(SR);
        let blocks = sine_blocks(997.0, 4);
        for step in 0..40 {
            let time = 0.01 + 1.99 * step as f32 / 39.0;
            let fb = 0.95 * step as f32 / 39.0;
            let damp = step as f32 / 39.0;
            fx.set_params(&[time, fb, damp, 0.0]);
            let mut out = vec![0.0; 512];
            for block in &blocks {
                out.fill(0.0);
                for i in 0..256 {
                    out[i * 2] = block[i * 2];
                    out[i * 2 + 1] = block[i * 2 + 1];
                }
                fx.process(&mut out, 256, &FxContext::default());
                assert!(out.iter().all(|v| v.is_finite()), "step {step} NaN");
            }
        }
    }
}
