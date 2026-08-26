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

/// 小节时钟：拍号（默认 4/4）+ 乐句对齐的小节/拍号/相位。
/// 小节边界默认对齐 grid 第 0 拍；`downbeat_rotation` 来自分析产出的
/// 乐句真起点（某拍序号相对于 grid offset 的偏移），bars 从它数起。
#[derive(Clone, Copy, Debug)]
pub struct BarClock {
    pub bpm: f64,
    /// 小节序号（0 起，起点 = 首个 downbeat；可负——外推到曲首之前）。
    pub bar_index: i64,
    /// 小节内拍号 0..beats_per_bar。
    pub beat_in_bar: u32,
    /// 拍内相位 0..1。
    pub beat_phase: f64,
}

impl BarClock {
    /// 默认 4/4（无拍号概念时的保守选择）。
    pub fn from_grid_at(grid: &BeatGrid, t_secs: f64, downbeat_rotation: i64) -> Self {
        Self::from_grid_at_bpb(grid, t_secs, downbeat_rotation, 4)
    }

    /// 从网格 + downbeat 旋转构造小节时钟。
    ///
    /// 数学：拍号序号 `beat = floor((t - offset)/period)`。downbeat 每隔
    /// `beats_per_bar` 拍发生一次，第 0 个 downbeat 位于拍号 `rotation`。
    /// 故 `beat_in_bar = (beat - rotation) mod beats_per_bar`，
    /// `bar_index = floor((beat - rotation)/beats_per_bar)`（Euclid，兼容负）。
    pub fn from_grid_at_bpb(
        grid: &BeatGrid,
        t_secs: f64,
        downbeat_rotation: i64,
        beats_per_bar: u32,
    ) -> Self {
        let bpm = grid.bpm;
        if bpm <= 0.0 {
            return Self {
                bpm: 0.0,
                bar_index: 0,
                beat_in_bar: 0,
                beat_phase: 0.0,
            };
        }
        let bpb = beats_per_bar.max(1) as i64;
        let beat = grid.beat_index_at(t_secs);
        let shifted = beat - downbeat_rotation;
        Self {
            bpm,
            bar_index: shifted.div_euclid(bpb),
            beat_in_bar: shifted.rem_euclid(bpb) as u32,
            beat_phase: grid.phase_at(t_secs),
        }
    }
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

    #[test]
    fn bar_clock_four_four_from_origin() {
        // 120bpm、offset=1.0、rotation=0（小节从 grid 第 0 拍数起）
        let g = g120();
        let b0 = BarClock::from_grid_at(&g, 1.0, 0);
        assert_eq!((b0.bar_index, b0.beat_in_bar, b0.beat_phase), (0, 0, 0.0));
        let b1 = BarClock::from_grid_at(&g, 1.5, 0);
        assert_eq!((b1.bar_index, b1.beat_in_bar), (0, 1), "第 2 拍 → 拍号 1");
        let b2 = BarClock::from_grid_at(&g, 2.5, 0);
        assert_eq!((b2.bar_index, b2.beat_in_bar), (0, 3), "第 4 拍 → 拍号 3");
        let b3 = BarClock::from_grid_at(&g, 3.0, 0);
        assert_eq!((b3.bar_index, b3.beat_in_bar), (1, 0), "第 5 拍 → 小节 1 拍号 0");
    }

    #[test]
    fn bar_clock_rotation_shifts_downbeat() {
        let g = g120();
        // rotation=2：downbeat（小节头）位于拍号 2、6、10…
        let b0 = BarClock::from_grid_at(&g, 2.0, 2);
        assert_eq!((b0.bar_index, b0.beat_in_bar), (0, 0), "拍号 2 → 小节 0 拍号 0");
        let b1 = BarClock::from_grid_at(&g, 2.5, 2);
        assert_eq!((b1.bar_index, b1.beat_in_bar), (0, 1), "拍号 3 → 小节 0 拍号 1");
        let b2 = BarClock::from_grid_at(&g, 4.0, 2);
        assert_eq!((b2.bar_index, b2.beat_in_bar), (1, 0), "拍号 6 是第二个 downbeat");
        // rotation=2 时拍号 0、1 属于上一小节的末尾两拍
        let b_neg = BarClock::from_grid_at(&g, 1.0, 2);
        assert_eq!(b_neg.beat_in_bar, 2, "拍号 0 → 上一小节拍号 2");
        assert_eq!(b_neg.bar_index, -1);
    }

    #[test]
    fn bar_clock_from_beats_per_bar_and_edge() {
        let g = g120();
        let b = BarClock::from_grid_at_bpb(&g, 1.9, 0, 3);
        assert_eq!((b.beat_in_bar, b.bar_index), (1, 0), "3/4：拍号 1 在小节 0");
        let bad = BeatGrid::default();
        let b2 = BarClock::from_grid_at(&bad, 3.7, 0);
        assert_eq!((b2.bar_index, b2.beat_in_bar, b2.beat_phase), (0, 0, 0.0), "无网格退化");
    }
}
