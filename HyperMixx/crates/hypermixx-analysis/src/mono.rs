//! MonoAccumulator：48k 交织立体声 → 12k 单声道（BPM/key/beatgrid 分析源）。
//!
//! 65 抽头 Blackman sinc 低通（截止 5.5kHz，8kHz ≥80dB 衰减——实测
//! 33 抽头只有 30dB，不满足 40dB 混叠线）+ 抽 4。12k 是 key 检测
//! 色度带 100–5000Hz 的硬下限（Nyquist 6k），329s 曲 ≈15.8MB 瞬态。
//! 段边界重建 FIR 状态：SEG_FRAMES=768000 整除 4，每段恰出 192000 帧，
//! 拼接无相位漂移；重启暂态 ≈65/48k ≈1.4ms，对拍/调性检测无影响。

pub const TRACK_MONO_RATE: u32 = 12_000;

const TAPS: usize = 65;
const DECIM: usize = 4;
const CUTOFF_HZ: f64 = 5_500.0;
const SR_IN: f64 = 48_000.0;

/// 48k 交织 → 12k 单声道抽取器（每段重建；分析线程独占，无需 Send）。
pub struct MonoAccumulator {
    coeffs: [f32; TAPS],
    /// 最近 TAPS 个单声道样本的环形历史（写指针滚动的顺序无关紧要：
    /// 滤波器系数对称）。
    hist: [f32; TAPS],
    pos: usize,
    /// 抽取相位：每 DECIM 个输入帧出一个样本。
    phase: usize,
}

impl MonoAccumulator {
    pub fn new() -> Self {
        let mut coeffs = [0.0f32; TAPS];
        let fc = CUTOFF_HZ / SR_IN;
        let mut sum = 0.0;
        for (k, c) in coeffs.iter_mut().enumerate() {
            let d = k as f64 - (TAPS as f64 - 1.0) / 2.0;
            // Blackman 窗：主瓣宽、旁瓣 −58dB。
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / (TAPS - 1) as f64).cos()
                + 0.08 * (4.0 * std::f64::consts::PI * k as f64 / (TAPS - 1) as f64).cos();
            let sinc = |x: f64| {
                if x.abs() < 1e-12 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                }
            };
            *c = (2.0 * fc * sinc(2.0 * fc * d) * w) as f32;
            sum += *c as f64;
        }
        for c in coeffs.iter_mut() {
            *c = (*c as f64 / sum) as f32; // DC 增益 1
        }
        Self {
            coeffs,
            hist: [0.0; TAPS],
            pos: 0,
            phase: 0,
        }
    }

    /// 处理交织立体声帧（48k），把抽取出的 12k 单声道样本追加到 out。
    pub fn process(&mut self, samples: &[f32], out: &mut Vec<f32>) {
        for s in samples.chunks_exact(2) {
            let x = (s[0] + s[1]) * 0.5;
            self.hist[self.pos] = x;
            self.pos += 1;
            if self.pos == TAPS {
                self.pos = 0;
            }
            self.phase += 1;
            if self.phase == DECIM {
                self.phase = 0;
                let mut acc = 0.0f32;
                for (k, c) in self.coeffs.iter().enumerate() {
                    // 最新样本在 hist[pos-1]；x[n−k] = hist[(pos−1−k) mod TAPS]
                    acc += c * self.hist[(self.pos + TAPS - 1 - k) % TAPS];
                }
                out.push(acc);
            }
        }
    }
}

impl Default for MonoAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// 48k 交织立体声 → 48k 单声道（(L+R)/2 直通，无滤波无抽取）。
///
/// 与 12k 抽取路径并存：细拍位检测（superflux @48k）需要全速率信号，
/// 12k 信号只用于 key 与粗 tempo。奇数尾帧丢弃（半帧无意义）。
pub fn mixdown_48k(samples: &[f32], out: &mut Vec<f32>) {
    for s in samples.chunks_exact(2) {
        out.push((s[0] + s[1]) * 0.5);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 48k 交织正弦（s 秒、频率 hz、振幅 amp），返回 (交织样本, 帧数)。
    fn sine_stereo(hz: f32, secs: f32, amp: f32) -> Vec<f32> {
        let n = (SR_IN as f32 * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let t = i as f32 / SR_IN as f32;
                let s = amp * (2.0 * std::f32::consts::PI * hz * t).sin();
                [s, s]
            })
            .collect()
    }

    /// 上升沿过零测频（对 440Hz@12k 足够精确：每周期 ≈27 样本）。
    fn zero_cross_freq(mono: &[f32], sr: f64) -> f64 {
        let (start, end) = (mono.len() / 4, mono.len() * 3 / 4); // 跳过 FIR 暂态
        let mut crossings = 0usize;
        let (mut first, mut last) = (0usize, 0usize);
        for i in start + 1..end {
            if mono[i - 1] <= 0.0 && mono[i] > 0.0 {
                crossings += 1;
                if first == 0 {
                    first = i;
                }
                last = i;
            }
        }
        if crossings < 2 {
            return 0.0;
        }
        (crossings - 1) as f64 * sr / (last - first) as f64
    }

    #[test]
    fn decimation_keeps_440hz_pitch() {
        let input = sine_stereo(440.0, 4.0, 0.8);
        let mut acc = MonoAccumulator::new();
        let mut out = Vec::new();
        acc.process(&input, &mut out);
        assert_eq!(out.len(), input.len() / 2 / DECIM, "每 4 帧恰出一个样本");
        let f = zero_cross_freq(&out, TRACK_MONO_RATE as f64);
        assert!(
            (f - 440.0).abs() < 3.0,
            "440Hz 抽取后测频应 ≈440（实得 {f:.1}）"
        );
    }

    #[test]
    fn decimation_attenuates_8k_alias() {
        // 8kHz 抽 4 后混叠到 4kHz；滤波器需 ≥40dB 衰减。
        let input = sine_stereo(8_000.0, 4.0, 1.0);
        let mut acc = MonoAccumulator::new();
        let mut out = Vec::new();
        acc.process(&input, &mut out);
        let (start, end) = (out.len() / 4, out.len() * 3 / 4);
        let rms: f64 = (out[start..end]
            .iter()
            .map(|v| (*v as f64).powi(2))
            .sum::<f64>()
            / (end - start) as f64)
            .sqrt();
        let db = 20.0 * rms.log10();
        assert!(
            db < -40.0,
            "8kHz 混叠应 ≥40dB 衰减（实得 {db:.1}dB，输入 RMS 0dB）"
        );
    }

    #[test]
    fn dc_gain_is_unity() {
        let n = 4800;
        let input: Vec<f32> = (0..n).flat_map(|_| [0.5f32, 0.5]).collect();
        let mut acc = MonoAccumulator::new();
        let mut out = Vec::new();
        acc.process(&input, &mut out);
        let tail = out[out.len() / 2..].iter().copied().sum::<f32>() / (out.len() / 2) as f32;
        assert!((tail - 0.5).abs() < 1e-3, "直流增益应为 1（实得 {tail}）");
    }

    /// 直通路径：(L+R)/2、帧数减半、尾帧丢弃。
    #[test]
    fn mixdown_48k_halves_frames_and_averages_channels() {
        let input: Vec<f32> = (0..100).flat_map(|i| [i as f32, 100.0 + i as f32]).collect();
        let mut out = Vec::new();
        mixdown_48k(&input, &mut out);
        assert_eq!(out.len(), 100, "100 帧立体声 → 100 样本单声道");
        for (i, &v) in out.iter().enumerate() {
            assert_eq!(v, (i as f32 + 100.0 + i as f32) * 0.5, "样本 {i} 应为 (L+R)/2");
        }

        // 奇数尾帧丢弃。
        let mut odd = Vec::new();
        mixdown_48k(&[0.1, 0.2, 0.3], &mut odd);
        assert_eq!(odd, vec![0.15]);
    }
}
