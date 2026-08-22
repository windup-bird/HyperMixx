//! 固定 beatgrid 拟合：把 timestretch 动态拍列表（DP 追踪，间隔不定、
//! 整体滞后于真实起音）折叠为单 BPM + 单相位锚点的均匀网格。
//!
//! 算法（纯概率论，不回扫音频）：
//! ① 拍号 nᵢ = i（追踪输出为连续拍），残差 rᵢ = bᵢ − nᵢ·T；
//! ② 残差相位 θᵢ = rᵢ mod T 做圆上核密度估计（周期延拓的高斯核）——
//!    锚点按分布偏斜方向去偏：检测滞后（右偏，真实素材常态）取峰簇左缘、
//!    检测提前（左偏，如孤立 click 经能量通道先响）取右缘、近对称取顶点；
//!    高水位穿越（90% 峰高）定位边缘，线性插值到亚格点精度；
//! ③ 从锚点外推铺满全曲，严格等间距；首点即 offset（补齐回曲首）；
//! ④ 旧下拍映射到新网格做 mod-4 投票定旋转，全量重建下拍序列；
//! ⑤ 内点率 = 动态拍位落在新网格容差内的比例；置信度由调用方按其打折，
//!    真变速素材自动降权（低于阈值走既有低置信路径）。

/// 圆上核密度估计的格点数（一个拍周期内的采样数）。
const KDE_LATTICE: usize = 1024;
/// KDE 带宽占拍周期比例（实际带宽夹在 [3ms, 15ms]）。
const KDE_BANDWIDTH_FRAC: f64 = 0.01;
/// 峰簇扩张的地板（相对全局峰高的比例）：低于此值的密度视为簇间谷。
const CLUSTER_FLOOR: f64 = 0.5;
/// 偏斜判定阈值：半高簇两臂跨度差超过此格点数才视为偏斜。
const ANCHOR_SKEW_CELLS: f64 = 1.5;
/// 簇扩张步数上限（格点比例），防平坦密度绕圈一整周。
const CLUSTER_MAX_SPAN: usize = KDE_LATTICE / 4;
/// 内点容差占拍周期比例（实际值夹在 [10ms, 40ms]）。
const INLIER_TOL_FRAC: f64 = 0.12;

/// 固定网格拟合结果（秒坐标）。
#[derive(Debug, Clone)]
pub struct FixedGrid {
    /// 网格首点秒：∈ [0, 拍周期)，即锚点 mod T，可能早于首个实际拍点。
    pub offset_secs: f64,
    /// 固定 BPM（透传输入）。
    pub bpm: f64,
    /// 均匀网格拍位（秒）：严格等间距，覆盖 [offset, duration]。
    pub beats_secs: Box<[f64]>,
    /// 下拍拍位（秒）：新网格中索引 ≡ rotation (mod 4) 的点。
    pub downbeats_secs: Box<[f64]>,
    /// 下拍旋转 ∈ 0..4。
    pub downbeat_rotation: usize,
    /// 内点率 ∈ [0,1]：动态拍位落在新网格容差内的比例。
    pub inlier_ratio: f32,
}

/// 把动态拍列表拟合到固定网格。
///
/// - `beats`：timestretch 追踪的连续拍位（秒，升序）；
/// - `downbeat_indices`：旧下拍在 `beats` 中的索引（用于旋转投票）；
/// - `bpm`：固定 BPM（timestretch 的多拍基线中位数）；
/// - `duration_secs`：曲长（网格外推终点）。
///
/// 拍不足两拍或 BPM 非正时返回 `None`（调用方透传原列表）。
pub fn fit_fixed_grid(
    beats: &[f64],
    downbeat_indices: &[usize],
    bpm: f64,
    duration_secs: f64,
) -> Option<FixedGrid> {
    if beats.len() < 2 || !bpm.is_finite() || bpm <= 0.0 {
        return None;
    }
    if beats.iter().any(|b| !b.is_finite()) {
        return None;
    }
    let period = 60.0 / bpm;

    // ①② 残差相位 → 圆上核密度 → 峰簇左缘局部极大。
    let anchor = phase_anchor(beats, period);

    // ③ 外推铺满全曲。
    let est = (duration_secs / period).ceil().max(0.0) as usize + 2;
    let mut beats_out = Vec::with_capacity(est);
    let mut t = anchor;
    while t <= duration_secs + period * 1e-9 {
        beats_out.push(t);
        t += period;
    }
    if beats_out.is_empty() {
        return None;
    }
    let beats_out: Box<[f64]> = beats_out.into();

    // ④ 下拍旋转投票：旧下拍时间 → 最近新网格索引 → mod 4 计票。
    let mut votes = [0u32; 4];
    for &d in downbeat_indices {
        let Some(&td) = beats.get(d) else { continue };
        let k = ((td - anchor) / period).round();
        votes[k.rem_euclid(4.0) as usize] += 1;
    }
    let mut rotation = 0usize;
    for (r, &v) in votes.iter().enumerate() {
        if v > votes[rotation] {
            rotation = r;
        }
    }
    let downbeats: Box<[f64]> = beats_out
        .iter()
        .enumerate()
        .filter(|&(i, _)| i % 4 == rotation)
        .map(|(_, &t)| t)
        .collect();

    // ⑤ 内点率：动态拍位到最近网格点的圆周距离 ≤ 容差。
    let tol = (INLIER_TOL_FRAC * period).clamp(0.010, 0.040);
    let mut inliers = 0usize;
    for &b in beats {
        let x = (b - anchor).rem_euclid(period);
        let dist = x.min(period - x);
        if dist <= tol {
            inliers += 1;
        }
    }
    let inlier_ratio = inliers as f32 / beats.len() as f32;

    Some(FixedGrid {
        offset_secs: anchor,
        bpm,
        beats_secs: beats_out,
        downbeats_secs: downbeats,
        downbeat_rotation: rotation,
        inlier_ratio,
    })
}

/// 从动态拍位的残差相位分布估计网格锚点 ∈ [0, period)。
fn phase_anchor(beats: &[f64], period: f64) -> f64 {
    let step = period / KDE_LATTICE as f64;
    let bandwidth = (KDE_BANDWIDTH_FRAC * period).clamp(0.003, 0.015);

    // 高斯核逐拍累加进环形格点（窗口 ±3σ）。
    let mut lat = vec![0.0f64; KDE_LATTICE];
    let kw = (3.0 * bandwidth / step).ceil() as isize;
    let two_h2 = 2.0 * bandwidth * bandwidth;
    for (i, &b) in beats.iter().enumerate() {
        let theta = (b - i as f64 * period).rem_euclid(period);
        let center = theta / step;
        let c0 = center.floor() as isize;
        for j in (c0 - kw)..=(c0 + kw) {
            let dist = (j as f64 - center) * step;
            let w = (-dist * dist / two_h2).exp();
            let idx = j.rem_euclid(KDE_LATTICE as isize) as usize;
            lat[idx] += w;
        }
    }

    // 全局峰 → 半高地板扩张出峰簇（环形、步数封顶）。
    let mut g = 0usize;
    for (i, &v) in lat.iter().enumerate() {
        if v > lat[g] {
            g = i;
        }
    }
    let peak = lat[g];
    let floor = peak * CLUSTER_FLOOR;
    let n = KDE_LATTICE as isize;
    let gi = g as isize;
    let mut lo = gi;
    let mut hi = gi;
    for step_out in 1..=CLUSTER_MAX_SPAN as isize {
        if lat[(gi - step_out).rem_euclid(n) as usize] >= floor {
            lo -= 1;
        } else {
            break;
        }
    }
    for step_out in 1..=CLUSTER_MAX_SPAN as isize {
        if lat[(gi + step_out).rem_euclid(n) as usize] >= floor {
            hi += 1;
        } else {
            break;
        }
    }

    // 去偏方向：比较半高簇两臂跨度。峰顶附近任何光滑函数都近似抛物线
    // （局部对称），偏斜信息在裙摆上——拖尾长的一侧是检测偏差方向，
    // 真实起音在另一侧的簇边缘。检测滞后（右臂长，真实素材常态）→ 取
    // 簇左缘；检测提前（左臂长，如孤立 click 的能量通道先响）→ 取右缘；
    // 两臂近等 → 顶点抛物线细化。边缘按半高穿越点向簇外邻格插值。
    let dl = (gi - lo) as f64;
    let dr = (hi - gi) as f64;

    let m: f64;
    if dr > dl + ANCHOR_SKEW_CELLS {
        // 右偏：簇左缘。
        let v = lat[lo.rem_euclid(n) as usize];
        let vo = lat[(lo - 1).rem_euclid(n) as usize];
        let frac = if v > vo && vo < floor {
            ((floor - vo) / (v - vo)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        m = lo as f64 - 1.0 + frac;
    } else if dl > dr + ANCHOR_SKEW_CELLS {
        // 左偏：簇右缘。
        let v = lat[hi.rem_euclid(n) as usize];
        let vo = lat[(hi + 1).rem_euclid(n) as usize];
        let frac = if v > vo && vo < floor {
            ((floor - vo) / (v - vo)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        m = hi as f64 + 1.0 - frac;
    } else {
        // 近对称：顶点抛物线细化。
        let vm = lat[gi.rem_euclid(n) as usize];
        let vp = lat[(gi - 1).rem_euclid(n) as usize];
        let vn = lat[(gi + 1).rem_euclid(n) as usize];
        let denom = vp - 2.0 * vm + vn;
        let delta = if denom.abs() > 1e-12 {
            (0.5 * (vp - vn) / denom).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        m = gi as f64 + delta;
    }

    (m * step).rem_euclid(period)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性伪随机（LCG），避免引入 rand 依赖。
    struct Lcg(u64);
    impl Lcg {
        fn next_unit(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 32) as f64 / (1u64 << 32) as f64
        }
    }

    const BPM: f64 = 128.0;
    const PERIOD: f64 = 60.0 / BPM;
    const TRUTH: f64 = 0.3;

    /// 合成动态拍位：真实网格 + 右偏滞后（多数 2–5ms，平方分布长尾
    /// 拖到 ~20ms），模拟 DP 追踪的系统性延迟（小滞后密集、大滞后稀疏）。
    fn lagged_beats(n: usize) -> Vec<f64> {
        let mut rng = Lcg(0x5EED_C0FF_EE);
        (0..n)
            .map(|k| {
                let u = rng.next_unit();
                let lag_ms = 2.0 + 18.0 * u * u;
                TRUTH + k as f64 * PERIOD + lag_ms / 1000.0
            })
            .collect()
    }

    #[test]
    fn constant_tempo_lag_recovers_true_phase_early() {
        let beats = lagged_beats(200);
        // 每第 4 拍（k≡0 mod 4）标记为旧下拍。
        let dbs: Vec<usize> = (0..beats.len()).step_by(4).collect();
        let mean_residual: f64 = beats
            .iter()
            .enumerate()
            .map(|(k, &b)| b - k as f64 * PERIOD)
            .sum::<f64>()
            / beats.len() as f64;

        let fit = fit_fixed_grid(&beats, &dbs, BPM, 120.0).expect("应拟合成功");

        // 锚点贴近真实起音位置（检测点全部滞后 ⇒ 锚点必然早于它们）。
        assert!(
            (fit.offset_secs - TRUTH).abs() <= 0.005,
            "锚点应 ≈{TRUTH:.3}s，实得 {:.4}",
            fit.offset_secs
        );
        assert!(
            fit.offset_secs < mean_residual - 0.001,
            "锚点应早于朴素均值估计 {mean_residual:.4}，实得 {:.4}",
            fit.offset_secs
        );
        // 严格等间距。
        for w in fit.beats_secs.windows(2) {
            assert!(
                (w[1] - w[0] - PERIOD).abs() < 1e-9,
                "网格间距应恒等于拍周期"
            );
        }
        // 干净恒速素材内点率应很高。
        assert!(fit.inlier_ratio > 0.85, "内点率 {}", fit.inlier_ratio);
        // 下拍每 4 点一个，且与真实下拍（truth + 4k·T）对齐。
        assert_eq!(fit.downbeat_rotation, 0, "无旋转时下拍应在 0 mod 4");
        for w in fit.downbeats_secs.windows(2) {
            assert!(((w[1] - w[0]) - 4.0 * PERIOD).abs() < 1e-9);
        }
        let first_db = fit.downbeats_secs[0];
        let k = ((TRUTH - first_db) / (4.0 * PERIOD)).round();
        assert!(
            (first_db + k * 4.0 * PERIOD - TRUTH).abs() <= 0.005,
            "下拍应落在真实重音上，实得 {first_db:.4}"
        );
    }

    #[test]
    fn rotated_downbeats_vote_correct_rotation() {
        let beats = lagged_beats(80);
        // 旧下拍标在 k≡1 mod 4 的拍上。
        let dbs: Vec<usize> = (1..beats.len()).step_by(4).collect();
        let fit = fit_fixed_grid(&beats, &dbs, BPM, 60.0).expect("应拟合成功");
        assert_eq!(fit.downbeat_rotation, 1, "应投票出旋转 1");
        // 新网格下拍（索引 ≡1 mod 4）应对齐真实重音位置
        // truth + (4j+1)·T——不能拿带滞后的检测点当基准。
        for (j, &t) in fit.downbeats_secs.iter().enumerate() {
            let truth_db = TRUTH + (4 * j + 1) as f64 * PERIOD;
            assert!(
                (t - truth_db).abs() <= 0.005,
                "第 {j} 个下拍应 ≈{truth_db:.4}s，实得 {t:.4}"
            );
        }
    }

    #[test]
    fn grid_starts_before_first_beat_and_covers_track() {
        let beats = lagged_beats(100);
        let fit = fit_fixed_grid(&beats, &[], BPM, 65.0).expect("应拟合成功");
        assert!(fit.offset_secs >= 0.0);
        // offset 是最早的"非负"网格点：要么不晚于首个检测拍（网格向曲首
        // 补齐），要么其前一格点本该落在负时间（首拍早于一个周期时）。
        assert!(
            fit.offset_secs <= beats[0] || fit.offset_secs < PERIOD,
            "offset {:.4} 应为最早非负网格点（首拍 {}）",
            fit.offset_secs,
            beats[0]
        );
        // 首检测拍必须落在网格上。
        let k = ((beats[0] - fit.offset_secs) / PERIOD).round();
        assert!(
            (fit.offset_secs + k * PERIOD - beats[0]).abs() <= 0.005,
            "首个检测拍应落在网格 ±5ms 内"
        );
        let last = fit.beats_secs[fit.beats_secs.len() - 1];
        assert!(last <= 65.0 + PERIOD, "网格不应超出曲长一拍以上");
        assert!(last >= 65.0 - PERIOD, "网格应铺到接近曲尾");
        // 空下拍输入 → 旋转 0 兜底。
        assert_eq!(fit.downbeat_rotation, 0);

        // 曲首之前的负拍不存在（anchor 已是首个非负网格点）。
        assert!(fit.beats_secs.first().copied() == Some(fit.offset_secs));
    }

    #[test]
    fn tempo_ramp_still_single_grid_with_lower_inliers() {
        // 间隔从 120 BPM 线性漂移到 126 BPM：强制单网格后中段大量脱网。
        let n = 240;
        let t_lo = 60.0 / 120.0;
        let t_hi = 60.0 / 126.0;
        let mut beats = Vec::with_capacity(n);
        let mut t = TRUTH;
        for k in 0..n {
            beats.push(t);
            t += t_lo + (t_hi - t_lo) * k as f64 / (n - 1) as f64;
        }
        let clean = fit_fixed_grid(&lagged_beats(200), &[], BPM, 120.0).unwrap();
        let fit = fit_fixed_grid(&beats, &[], 123.0, 130.0).expect("渐变速仍应出单网格");
        assert_eq!(fit.bpm, 123.0);
        assert!(
            fit.inlier_ratio < clean.inlier_ratio - 0.3,
            "渐变速内点率应显著更低：{} vs {}",
            fit.inlier_ratio,
            clean.inlier_ratio
        );
    }

    #[test]
    fn degenerate_inputs_return_none() {
        assert!(fit_fixed_grid(&[], &[], BPM, 60.0).is_none(), "空拍表");
        assert!(fit_fixed_grid(&[0.3], &[], BPM, 60.0).is_none(), "单拍");
        assert!(
            fit_fixed_grid(&[0.3, 0.9], &[], 0.0, 60.0).is_none(),
            "零 BPM"
        );
        assert!(
            fit_fixed_grid(&[0.3, 0.9], &[], -128.0, 60.0).is_none(),
            "负 BPM"
        );
        assert!(
            fit_fixed_grid(&[0.3, f64::NAN], &[], BPM, 60.0).is_none(),
            "非有限拍位"
        );
    }
}
