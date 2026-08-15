//! 三段 EQ：low shelf / mid peak / high shelf，立体声联动（L/R 各自滤波器状态）。

use super::biquad::{Biquad, BiquadKind};
use super::smoother::Smoother;

const LOW_FREQ: f32 = 250.0;
const MID_FREQ: f32 = 1000.0;
const MID_Q: f32 = 0.7;
const HIGH_FREQ: f32 = 4000.0;
const SHELF_Q: f32 = 0.7;
/// "kill" 用足够低的增益近似（-40dB 听感上已全切）。
pub const MIN_GAIN_DB: f32 = -40.0;
pub const MAX_GAIN_DB: f32 = 6.0;
/// 平滑增益变化超过此阈值才重算系数。
const RETUNE_EPS: f32 = 0.05;

/// 三段 EQ（参数联动，L/R 独立状态）。
pub struct ThreeBandEq {
    low: (Biquad, Biquad),
    mid: (Biquad, Biquad),
    high: (Biquad, Biquad),
    low_gain: Smoother,
    mid_gain: Smoother,
    high_gain: Smoother,
    applied: (f32, f32, f32),
}

impl ThreeBandEq {
    pub fn new(sr: f32) -> Self {
        let tau = 0.005; // 5ms 平滑
        let coeff = 1.0 - (-1.0 / (tau * sr)).exp();
        let mk = |kind, freq, q| {
            (
                Biquad::new(kind, sr, freq, q, 0.0),
                Biquad::new(kind, sr, freq, q, 0.0),
            )
        };
        Self {
            low: mk(BiquadKind::LowShelf, LOW_FREQ, SHELF_Q),
            mid: mk(BiquadKind::Peaking, MID_FREQ, MID_Q),
            high: mk(BiquadKind::HighShelf, HIGH_FREQ, SHELF_Q),
            low_gain: Smoother::new(0.0, coeff),
            mid_gain: Smoother::new(0.0, coeff),
            high_gain: Smoother::new(0.0, coeff),
            applied: (0.0, 0.0, 0.0),
        }
    }

    pub fn set_low_db(&mut self, db: f32) {
        self.low_gain.set_target(db.clamp(MIN_GAIN_DB, MAX_GAIN_DB));
    }
    pub fn set_mid_db(&mut self, db: f32) {
        self.mid_gain.set_target(db.clamp(MIN_GAIN_DB, MAX_GAIN_DB));
    }
    pub fn set_high_db(&mut self, db: f32) {
        self.high_gain
            .set_target(db.clamp(MIN_GAIN_DB, MAX_GAIN_DB));
    }

    /// 载入新音轨时立即归位。
    pub fn reset(&mut self) {
        self.low_gain.set_immediate(0.0);
        self.mid_gain.set_immediate(0.0);
        self.high_gain.set_immediate(0.0);
        self.applied = (0.0, 0.0, 0.0);
        self.low.0.reset();
        self.low.1.reset();
        self.mid.0.reset();
        self.mid.1.reset();
        self.high.0.reset();
        self.high.1.reset();
    }

    fn maybe_retune(&mut self) {
        let l = self.low_gain.step();
        let m = self.mid_gain.step();
        let h = self.high_gain.step();
        if (l - self.applied.0).abs() > RETUNE_EPS
            || (self.low_gain.settled() && l != self.applied.0)
        {
            self.low.0.set_params(LOW_FREQ, SHELF_Q, l);
            self.low.1.set_params(LOW_FREQ, SHELF_Q, l);
            self.applied.0 = l;
        }
        if (m - self.applied.1).abs() > RETUNE_EPS
            || (self.mid_gain.settled() && m != self.applied.1)
        {
            self.mid.0.set_params(MID_FREQ, MID_Q, m);
            self.mid.1.set_params(MID_FREQ, MID_Q, m);
            self.applied.1 = m;
        }
        if (h - self.applied.2).abs() > RETUNE_EPS
            || (self.high_gain.settled() && h != self.applied.2)
        {
            self.high.0.set_params(HIGH_FREQ, SHELF_Q, h);
            self.high.1.set_params(HIGH_FREQ, SHELF_Q, h);
            self.applied.2 = h;
        }
    }

    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        self.maybe_retune();
        let l = self.low.0.process(l);
        let r = self.low.1.process(r);
        let l = self.mid.0.process(l);
        let r = self.mid.1.process(r);
        let l = self.high.0.process(l);
        let r = self.high.1.process(r);
        (l, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_at_zero() {
        let mut eq = ThreeBandEq::new(48000.0);
        let (l, r) = eq.process(0.5, 0.5);
        assert!((l - 0.5).abs() < 1e-4 && (r - 0.5).abs() < 1e-4);
    }

    #[test]
    fn kill_reduces_signal() {
        let sr = 48000.0;
        let mut eq = ThreeBandEq::new(sr as f32);
        eq.set_low_db(MIN_GAIN_DB);
        eq.set_mid_db(MIN_GAIN_DB);
        eq.set_high_db(MIN_GAIN_DB);
        // 1000Hz 落在中频段峰顶（−40dB），三段全 kill 时输出应接近 0
        let mut sum = 0.0f32;
        for i in 0..sr as usize {
            let x = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr as f32).sin();
            let (l, _) = eq.process(x, x);
            sum += l * l;
        }
        let rms = (sum / sr as f32).sqrt();
        assert!(rms < 0.05, "kill 后 rms={rms}");
    }

    #[test]
    fn smoothing_no_discontinuity() {
        let mut eq = ThreeBandEq::new(48000.0);
        let mut prev = eq.process(0.8, 0.8).0;
        eq.set_low_db(6.0);
        eq.set_mid_db(-40.0);
        eq.set_high_db(3.0);
        for _ in 0..2000 {
            let cur = eq.process(0.8, 0.8).0;
            // 5ms 平滑下逐样本变化应远小于整体响应范围
            assert!((cur - prev).abs() < 0.2, "跳变 {} → {}", prev, cur);
            prev = cur;
        }
    }
}
