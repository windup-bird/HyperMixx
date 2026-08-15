//! 一阶指数参数平滑器：UI 参数 → 音频线程的防爆音桥梁。

pub struct Smoother {
    target: f32,
    current: f32,
    coeff: f32,
}

impl Smoother {
    /// `coeff` ∈ (0,1]，越大越快。
    /// 典型时间常数 τ 秒：`coeff = 1 - exp(-1 / (tau * sr))`。
    pub fn new(initial: f32, coeff: f32) -> Self {
        Self {
            target: initial,
            current: initial,
            coeff: coeff.clamp(1e-4, 1.0),
        }
    }

    pub fn set_target(&mut self, target: f32) {
        if target.is_finite() {
            self.target = target;
        }
    }

    /// 立即跳变（载入新音轨时用，避免从旧值滑过去）。
    pub fn set_immediate(&mut self, value: f32) {
        self.target = value;
        self.current = value;
    }

    /// 只改当前值、保留目标（效果 reset 清状态用：配置保留，
    /// 斜坡从给定值重启）。
    pub fn set_current(&mut self, value: f32) {
        if value.is_finite() {
            self.current = value;
        }
    }

    #[inline]
    pub fn step(&mut self) -> f32 {
        if self.current == self.target {
            return self.current; // 稳定快路径
        }
        self.current += (self.target - self.current) * self.coeff;
        if (self.target - self.current).abs() < 1e-5 {
            self.current = self.target;
        }
        self.current
    }

    pub fn settled(&self) -> bool {
        self.current == self.target
    }

    pub fn current(&self) -> f32 {
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_to_target() {
        let mut s = Smoother::new(0.0, 0.1);
        s.set_target(1.0);
        let mut last = 0.0;
        for _ in 0..1000 {
            let v = s.step();
            assert!(v >= last && v <= 1.0);
            last = v;
        }
        assert!(s.settled());
        assert!((s.current() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn immediate_jump() {
        let mut s = Smoother::new(0.0, 0.1);
        s.set_target(1.0);
        let _ = s.step();
        s.set_immediate(0.5);
        assert_eq!(s.current(), 0.5);
    }

    #[test]
    fn set_current_keeps_target() {
        let mut s = Smoother::new(0.0, 0.1);
        s.set_target(1.0);
        for _ in 0..50 {
            let _ = s.step();
        }
        s.set_current(0.2);
        assert_eq!(s.current(), 0.2, "当前值立即跳");
        assert!(!s.settled(), "目标仍是 1.0，未稳定");
        let v = s.step();
        assert!(v > 0.2 && v < 1.0, "下一步应朝 1.0 移动");
    }

    #[test]
    fn nan_target_ignored() {
        let mut s = Smoother::new(0.0, 0.1);
        s.set_target(f32::NAN);
        assert_eq!(s.step(), 0.0);
    }
}
