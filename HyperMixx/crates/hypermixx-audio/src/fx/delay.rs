//! 2 的幂掩码环形延迟线（f32 单声道）。
//!
//! 手写而非 dasp：dasp 仅为传递依赖，且项目惯例自研 DSP
//! （dsp/biquad.rs 先于 `biquad` crate 存在）；掩码回绕避免
//! aarch64 缺 sdiv 快路径的取模开销。

pub struct DelayLine {
    buf: Vec<f32>,
    mask: usize,
    /// 下一个写入位置。
    pos: usize,
}

impl DelayLine {
    /// 容量向上取整到 2 的幂（≥2）。
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two().max(2);
        Self {
            buf: vec![0.0; cap],
            mask: cap - 1,
            pos: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub fn write(&mut self, v: f32) {
        self.buf[self.pos] = v;
        self.pos = (self.pos + 1) & self.mask;
    }

    /// 读 delay 采样前的值（delay=0 → 最近写入）。
    #[inline]
    pub fn read(&self, delay: usize) -> f32 {
        self.buf[(self.pos + self.len() - delay - 1) & self.mask]
    }

    /// 分数延迟读（线性插值；延迟时间平滑变化用，无 zipper）。
    #[inline]
    pub fn read_frac(&self, delay: f32) -> f32 {
        let d = delay.max(0.0);
        let i = d.floor() as usize;
        let frac = d - i as f32;
        let a = self.read(i);
        let b = self.read(i + 1);
        a + (b - a) * frac
    }

    pub fn clear(&mut self) {
        self.buf.fill(0.0);
        self.pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_roundtrip() {
        let mut dl = DelayLine::new(8);
        for i in 1..=5 {
            dl.write(i as f32);
        }
        assert_eq!(dl.read(0), 5.0, "最近写入");
        assert_eq!(dl.read(1), 4.0);
        assert_eq!(dl.read(4), 1.0);
    }

    #[test]
    fn mask_wraps() {
        let mut dl = DelayLine::new(8); // 2 的幂 = 8
        assert_eq!(dl.len(), 8);
        for i in 0..10 {
            dl.write(i as f32);
        }
        // 只保留最近 8 次写入（写 #2..#9）；超出保留范围的读会回绕，
        // 语义无保证——调用方保证 delay < len。
        assert_eq!(dl.read(0), 9.0);
        assert_eq!(dl.read(1), 8.0);
        assert_eq!(dl.read(7), 2.0, "最早保留样本 = 第 2 次写入");
    }

    #[test]
    fn capacity_rounds_up_to_pow2() {
        assert_eq!(DelayLine::new(9).len(), 16);
        assert_eq!(DelayLine::new(1).len(), 2);
    }

    #[test]
    fn fractional_read_interpolates() {
        let mut dl = DelayLine::new(8);
        dl.write(0.0);
        dl.write(1.0);
        // delay 0.5 = 最近与次近的中点
        assert!((dl.read_frac(0.5) - 0.5).abs() < 1e-6);
        // delay 1.25 = 0.75×read(1) + 0.25×read(2)
        let expect = 0.75 * dl.read(1) + 0.25 * dl.read(2);
        assert!((dl.read_frac(1.25) - expect).abs() < 1e-6);
    }

    #[test]
    fn unwritten_region_is_zero() {
        let mut dl = DelayLine::new(8);
        dl.write(1.0);
        dl.write(2.0);
        assert_eq!(dl.read(2), 0.0);
        assert_eq!(dl.read(7), 0.0);
    }
}
