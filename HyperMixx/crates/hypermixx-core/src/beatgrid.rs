//! BeatGrid / BeatClock：节拍网格纯数学（无 DSP 依赖）。
//! P4 显示/手动编辑与 P5 同步相位共用；网格 = 恒定 BPM + 首拍秒偏移，
//! 拍点 = offset + k·period（k ∈ ℤ，可外推到曲首之前）。

/// 刚性节拍网格。bpm ≤ 0 表示无网格（所有查询退化为恒等/相位 0）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeatGrid {
    pub bpm: f64,
    /// 首拍秒偏移（网格外推回曲首的锚点；见 analysis 的 offset 语义）。
    pub offset_secs: f64,
}

impl BeatGrid {
    /// 拍周期（秒）；无网格时 ∞。
    pub fn period_secs(&self) -> f64 {
        if self.bpm > 0.0 { 60.0 / self.bpm } else { f64::INFINITY }
    }

    pub fn is_valid(&self) -> bool {
        self.bpm > 0.0
    }

    /// t 秒处的拍内相位（0..1）。
    pub fn phase_at(&self, t_secs: f64) -> f64 {
        let p = self.period_secs();
        if !p.is_finite() {
            return 0.0;
        }
        ((t_secs - self.offset_secs) / p).rem_euclid(1.0)
    }

    /// t 秒处所在拍的序号（floor；可负——网格外推到曲首之前）。
    /// 无网格恒 0。按拍 FX（gate 周期定位）需要绝对拍号：
    /// 仅拍内相位 0..1 无法区分周期 >1 拍时的第 0 拍与第 1 拍。
    pub fn beat_index_at(&self, t_secs: f64) -> i64 {
        let p = self.period_secs();
        if !p.is_finite() {
            return 0;
        }
        ((t_secs - self.offset_secs) / p).floor() as i64
    }

    /// t 秒之后（含）的第一个拍点。
    pub fn next_beat_after(&self, t_secs: f64) -> f64 {
        let p = self.period_secs();
        if !p.is_finite() {
            return t_secs;
        }
        let k = ((t_secs - self.offset_secs) / p).ceil();
        self.offset_secs + k * p
    }

    /// 吸附 t 到最近的拍点（quantize seek 用）。
    pub fn snap(&self, t_secs: f64) -> f64 {
        let p = self.period_secs();
        if !p.is_finite() {
            return t_secs;
        }
        let k = ((t_secs - self.offset_secs) / p).round();
        self.offset_secs + k * p
    }
}

impl Default for BeatGrid {
    fn default() -> Self {
        Self {
            bpm: 0.0,
            offset_secs: 0.0,
        }
    }
}

/// 节拍时钟：任意时刻的 (BPM, 拍内相位)，同步相位计算用。
#[derive(Clone, Copy, Debug)]
pub struct BeatClock {
    pub bpm: f64,
    /// 拍内相位 0..1。
    pub phase: f64,
}

impl BeatClock {
    pub fn from_grid_at(grid: &BeatGrid, t_secs: f64) -> Self {
        Self {
            bpm: grid.bpm,
            phase: grid.phase_at(t_secs),
        }
    }

    /// dt 秒后的相位（假设 bpm 不变）。
    pub fn phase_after(&self, dt_secs: f64) -> f64 {
        if self.bpm <= 0.0 {
            return self.phase;
        }
        (self.phase + dt_secs * self.bpm / 60.0).rem_euclid(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g120() -> BeatGrid {
        BeatGrid {
            bpm: 120.0,
            offset_secs: 1.0,
        }
    }

    #[test]
    fn period_and_validity() {
        assert_eq!(g120().period_secs(), 0.5);
        assert!(g120().is_valid());
        let bad = BeatGrid::default();
        assert!(!bad.is_valid());
        assert_eq!(bad.period_secs(), f64::INFINITY);
    }

    #[test]
    fn phase_wraps_and_aligns() {
        let g = g120();
        assert!((g.phase_at(1.0) - 0.0).abs() < 1e-12, "首拍处相位 0");
        assert!((g.phase_at(1.25) - 0.5).abs() < 1e-12, "半拍处相位 0.5");
        assert!((g.phase_at(1.5) - 0.0).abs() < 1e-12, "整拍回到 0");
        assert!((g.phase_at(0.5) - 0.0).abs() < 1e-12, "offset 之前的整拍同样对齐");
        assert_eq!(BeatGrid::default().phase_at(3.7), 0.0, "无网格相位恒 0");
    }

    #[test]
    fn next_beat_and_snap() {
        let g = g120();
        assert!((g.next_beat_after(1.0) - 1.0).abs() < 1e-12, "恰好落在拍上取自身");
        assert!((g.next_beat_after(1.01) - 1.5).abs() < 1e-12);
        assert!((g.next_beat_after(0.9) - 1.0).abs() < 1e-12, "offset 之前 → 下一个是首拍");
        assert!((g.snap(1.24) - 1.0).abs() < 1e-12, "snap 就近取拍");
        assert!((g.snap(1.26) - 1.5).abs() < 1e-12);
        assert!((g.snap(1.25) - 1.5).abs() < 1e-12, "正中取后拍（round 半取偶）");
        let bad = BeatGrid::default();
        assert_eq!(bad.snap(4.2), 4.2, "无网格 snap 恒等");
    }

    #[test]
    fn beat_index_counts_and_goes_negative() {
        let g = g120();
        assert_eq!(g.beat_index_at(1.0), 0, "首拍处序号 0");
        assert_eq!(g.beat_index_at(1.25), 0);
        assert_eq!(g.beat_index_at(1.5), 1);
        assert_eq!(g.beat_index_at(1.99), 1);
        assert_eq!(g.beat_index_at(0.5), -1, "offset 之前的整拍序号为负");
        assert_eq!(BeatGrid::default().beat_index_at(3.7), 0, "无网格恒 0");
    }

    #[test]
    fn beat_clock_advances() {
        let g = g120();
        let c = BeatClock::from_grid_at(&g, 1.1);
        assert!((c.phase - 0.2).abs() < 1e-12);
        assert!((c.phase_after(0.25) - 0.7).abs() < 1e-12);
        assert!(
            (c.phase_after(0.25) - 0.7).abs() < 1e-12,
            "phase_after 是纯函数：重复调用同值（不改变时钟本身）"
        );
        // 跨过整拍回绕：0.8 + 0.1s×2拍/s = 1.0 → 回绕到 0
        // （浮点会落在 1.0 的两侧，验收 |x|≈0 或 ≈1 即 0 mod 1）
        let c2 = BeatClock::from_grid_at(&g, 1.4);
        let x = c2.phase_after(0.1);
        assert!(
            x.abs() < 1e-9 || (x - 1.0).abs() < 1e-9,
            "0.8+0.2=1.0 → 回绕到 0（实得 {x}）"
        );
    }
}
