//! RBJ Cookbook 双二阶滤波器（DF2T，f32）。
//! 用于三段 EQ（low-shelf / peaking / high-shelf）与后续 filter/FX。

#[derive(Clone, Copy, PartialEq)]
pub enum BiquadKind {
    LowPass,
    HighPass,
    LowShelf,
    HighShelf,
    Peaking,
    /// RBJ 恒 0dB 峰值带通（center = freq，Q 决定带宽）。
    BandPass,
}

pub struct Biquad {
    kind: BiquadKind,
    sr: f32,
    freq: f32,
    q: f32,
    gain_db: f32,
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub fn new(kind: BiquadKind, sr: f32, freq: f32, q: f32, gain_db: f32) -> Self {
        let mut b = Self {
            kind,
            sr,
            freq,
            q,
            gain_db,
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        };
        b.recompute();
        b
    }

    /// 更新参数并重算系数；滤波器状态保留（配合小步进参数平滑可避免爆音）。
    pub fn set_params(&mut self, freq: f32, q: f32, gain_db: f32) {
        self.freq = freq;
        self.q = q;
        self.gain_db = gain_db;
        self.recompute();
    }

    /// 换滤波类型（filter FX 的 mode 切换；不分配，状态由调用方决定保留或 reset）。
    pub fn set_kind(&mut self, kind: BiquadKind) {
        self.kind = kind;
        self.recompute();
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    fn recompute(&mut self) {
        let sr = self.sr;
        let freq = self.freq.clamp(10.0, sr * 0.45);
        let w0 = 2.0 * std::f32::consts::PI * freq / sr;
        let (sw, cw) = w0.sin_cos();
        let alpha = sw / (2.0 * self.q.max(0.05));
        // RBJ cookbook 的 A = sqrt(线性增益)；shelf 直流/高频增益 = A²，即 10^(dB/20)
        let a = 10f32.powf(self.gain_db / 40.0);
        let ap = a + 1.0;
        let am = a - 1.0;
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

        let (b0, b1, b2, a0, a1, a2) = match self.kind {
            BiquadKind::LowPass => {
                let b = (1.0 - cw) / 2.0;
                (b, 1.0 - cw, b, 1.0 + alpha, -2.0 * cw, 1.0 - alpha)
            }
            BiquadKind::HighPass => {
                let b = (1.0 + cw) / 2.0;
                (b, -(1.0 + cw), b, 1.0 + alpha, -2.0 * cw, 1.0 - alpha)
            }
            BiquadKind::LowShelf => (
                a * (ap - am * cw + two_sqrt_a_alpha),
                2.0 * a * (am - ap * cw),
                a * (ap - am * cw - two_sqrt_a_alpha),
                ap + am * cw + two_sqrt_a_alpha,
                -2.0 * (am + ap * cw),
                ap + am * cw - two_sqrt_a_alpha,
            ),
            BiquadKind::HighShelf => (
                a * (ap + am * cw + two_sqrt_a_alpha),
                -2.0 * a * (am + ap * cw),
                a * (ap + am * cw - two_sqrt_a_alpha),
                ap - am * cw + two_sqrt_a_alpha,
                2.0 * (am - ap * cw),
                ap - am * cw - two_sqrt_a_alpha,
            ),
            BiquadKind::Peaking => (
                1.0 + alpha * a,
                -2.0 * cw,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cw,
                1.0 - alpha / a,
            ),
            BiquadKind::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
        };
        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        // 状态防 denormal
        if self.z1.abs() < 1e-30 {
            self.z1 = 0.0;
        }
        if self.z2.abs() < 1e-30 {
            self.z2 = 0.0;
        }
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_dc(b: &mut Biquad, samples: usize) -> f32 {
        let mut y = 0.0;
        for _ in 0..samples {
            y = b.process(1.0);
        }
        y
    }

    #[test]
    fn low_shelf_boost_dc_gain() {
        // +6dB @ 250Hz：DC 增益应接近 10^(6/20) ≈ 1.995
        let mut b = Biquad::new(BiquadKind::LowShelf, 48000.0, 250.0, 0.7, 6.0);
        let y = run_dc(&mut b, 20_000);
        assert!((y - 1.995).abs() < 0.05, "got {y}");
    }

    #[test]
    fn low_shelf_kill_dc_gain() {
        // -40dB：DC 增益应接近 0.01
        let mut b = Biquad::new(BiquadKind::LowShelf, 48000.0, 250.0, 0.7, -40.0);
        let y = run_dc(&mut b, 20_000);
        assert!(y.abs() < 0.02, "got {y}");
    }

    #[test]
    fn peaking_zero_db_is_identity() {
        let mut b = Biquad::new(BiquadKind::Peaking, 48000.0, 1000.0, 0.7, 0.0);
        let y = run_dc(&mut b, 20_000);
        assert!((y - 1.0).abs() < 1e-4, "got {y}");
    }

    #[test]
    fn high_pass_kills_low_sine() {
        const SR: f32 = 48000.0;
        fn rms(b: &mut Biquad, freq: f32) -> f32 {
            let mut sum = 0.0f32;
            for i in 0..SR as usize {
                let x = (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
                let y = b.process(x);
                sum += y * y;
            }
            (sum / SR).sqrt()
        }
        // 100Hz 正弦 vs 12kHz 正弦，均方根比较
        let mut b = Biquad::new(BiquadKind::HighPass, SR, 4000.0, 0.7, 0.0);
        let low = rms(&mut b, 100.0);
        b.reset();
        let high = rms(&mut b, 12000.0);
        assert!(high > low * 10.0, "low={low} high={high}");
    }

    #[test]
    fn bandpass_center_passes_flanks_stop() {
        const SR: f32 = 48000.0;
        fn rms(b: &mut Biquad, freq: f32) -> f32 {
            let mut sum = 0.0f32;
            for i in 0..SR as usize {
                let x = (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
                let y = b.process(x);
                sum += y * y;
            }
            (sum / SR).sqrt()
        }
        // 中心 1kHz、Q=4：中心响应应远大于 ±3 倍频程
        let mut b = Biquad::new(BiquadKind::BandPass, SR, 1000.0, 4.0, 0.0);
        let center = rms(&mut b, 1000.0);
        b.reset();
        let low = rms(&mut b, 125.0);
        b.reset();
        let high = rms(&mut b, 8000.0);
        assert!(center > low * 10.0, "center={center} low={low}");
        assert!(center > high * 10.0, "center={center} high={high}");
    }

    #[test]
    fn no_nan_on_retune() {
        let mut b = Biquad::new(BiquadKind::Peaking, 48000.0, 1000.0, 0.7, 0.0);
        for g in (-40..40).map(|i| i as f32) {
            b.set_params(1000.0, 0.7, g);
            for _ in 0..256 {
                let y = b.process(0.5);
                assert!(y.is_finite());
            }
        }
    }
}
