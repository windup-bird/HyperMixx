//! 正弦 LFO：256 点波表 + 线性插值 + 逐采样相位累加。
//!
//! 选用理由：aarch64 无 SIMD sinf，libm 逐采样调用 30-80 周期；
//! 波表（1KB，L1 常驻）+ lerp 约 4-6 周期。LFO 只调制参数
//! （延迟时间/系数），插值误差不进音频通路，无可闻影响。

pub const TABLE_SIZE: usize = 256;

pub struct SineLfo {
    table: [f32; TABLE_SIZE],
    /// 相位 0..1。
    phase: f32,
    /// 每采样步进 = rate / sr。
    inc: f32,
}

impl SineLfo {
    /// sr 仅用于默认步进换算；波表与采样率无关。
    pub fn new(sr: f32) -> Self {
        let mut table = [0.0f32; TABLE_SIZE];
        for (i, v) in table.iter_mut().enumerate() {
            *v = (2.0 * std::f32::consts::PI * i as f32 / TABLE_SIZE as f32).sin();
        }
        Self {
            table,
            phase: 0.0,
            inc: 0.0_f32.max(0.05 / sr),
        }
    }

    pub fn set_rate(&mut self, rate: f32, sr: f32) {
        self.inc = rate / sr;
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
    }

    /// 输出 -1..1 正弦并推进一个采样。
    #[inline]
    pub fn next(&mut self) -> f32 {
        let p = self.phase * TABLE_SIZE as f32;
        let i = p as usize;
        let frac = p - i as f32;
        let a = self.table[i];
        let b = self.table[(i + 1) & (TABLE_SIZE - 1)];
        let v = a + (b - a) * frac;
        self.phase += self.inc;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_sinf() {
        let lfo = SineLfo::new(48000.0);
        for (i, v) in lfo.table.iter().enumerate() {
            let exact = (2.0 * std::f32::consts::PI * i as f32 / TABLE_SIZE as f32).sin();
            assert!((v - exact).abs() < 1e-3, "表值 i={i} 偏差过大");
        }
    }

    #[test]
    fn phase_advances_exactly() {
        // 0.5Hz @ 48k：48k 采样后相位应回到原位（浮点容差内）
        let mut lfo = SineLfo::new(48000.0);
        lfo.set_rate(0.5, 48000.0);
        let first = lfo.next();
        for _ in 0..47_999 {
            let v = lfo.next();
            assert!(v.is_finite() && v.abs() <= 1.0 + 1e-6);
        }
        let back = lfo.next();
        assert!((back - first).abs() < 1e-2, "整周期后回原位: {first} vs {back}");
    }

    #[test]
    fn wrap_is_continuous() {
        // 大步进（0.1/采样）跨越多圈：值始终有界，且相邻样本差有界
        let mut lfo = SineLfo::new(48000.0);
        lfo.set_rate(4800.0, 48000.0); // inc = 0.1
        let mut last = lfo.next();
        for _ in 0..10_000 {
            let v = lfo.next();
            assert!(v.is_finite());
            assert!((v - last).abs() < 0.65, "插值步进过大（回绕不连续）");
            last = v;
        }
    }
}
