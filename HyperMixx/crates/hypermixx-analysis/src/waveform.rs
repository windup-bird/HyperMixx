//! RGB 波形预计算：离线分析线程，把整首歌按频率段压缩成列。
//! 分频：低 <150Hz（红）、中 150-3000Hz（绿）、高 >3000Hz（蓝），
//! 每列取段内正负双峰（p=正峰、n=负半折叠幅度），√ 压缩后归一化到 0..255；
//! 每带共享标度（max(全局 p, 全局 n)）→ 正半输出与旧 max|·| 归一化逐字节一致。
//! 渐进式分段分析见 segment.rs（与本文件共享列累积/归一化助手）。
//!
//! P11.4c：低频上限 300→150Hz——红色只表征鼓点（kick/snare 基频区），
//! 不再包含贝斯/低音主旋律的主体能量。

use std::path::Path;

use anyhow::Result;

use hypermixx_audio::decode::{To48k, TrackDecoder};

/// detail 每列帧数（48kHz）。
pub const DETAIL_FRAMES_PER_COL: usize = 128;
/// overview 每列 = 4 个 detail 列（512 帧）。
pub const OVERVIEW_RATIO: usize = 4;

const LOW_HZ: f32 = 150.0;
const HIGH_HZ: f32 = 3000.0;

/// 每列 8 值：`*_p` = 正峰、`*_n` = 负半折叠幅度（|min|），全部 ≥0。
/// `*_n` 与 `*_p` 共用同一带标度 → `max(p, n)` 即旧版 max|·| 峰值。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Column {
    pub low_p: u8,
    pub low_n: u8,
    pub mid_p: u8,
    pub mid_n: u8,
    pub high_p: u8,
    pub high_n: u8,
    pub all_p: u8,
    pub all_n: u8,
}

pub struct WaveformData {
    /// 128 帧/列。
    pub detail: Vec<Column>,
    /// 512 帧/列（由 detail 聚合）。
    pub overview: Vec<Column>,
    pub frames_per_col: u32,
    pub sample_rate: u32,
    /// 48kHz 总帧数。
    pub duration_frames: u64,
}

/// 一阶分频滤波器（低 = lp1，中 = lp2-lp1，高 = x-lp2）。
/// 连续解码时状态跨段延续、seek 后重建——保证顺序模式的分段分析
/// 与整曲 analyze() 产生完全相同的输出。
pub(crate) struct BandFilters {
    lp1: f32,
    lp2: f32,
    a1: f32,
    a2: f32,
}

impl BandFilters {
    pub(crate) fn new(sr: f32) -> Self {
        Self {
            lp1: 0.0,
            lp2: 0.0,
            a1: 1.0 - (-2.0 * std::f32::consts::PI * LOW_HZ / sr).exp(),
            a2: 1.0 - (-2.0 * std::f32::consts::PI * HIGH_HZ / sr).exp(),
        }
    }

    /// 处理一帧（单声道），返回 (低, 中, 高) 分量。
    pub(crate) fn process(&mut self, x: f32) -> (f32, f32, f32) {
        self.lp1 += self.a1 * (x - self.lp1);
        self.lp2 += self.a2 * (x - self.lp2);
        (self.lp1, self.lp2 - self.lp1, x - self.lp2)
    }
}

/// 每列原始双峰累积（归一化前；Done 时用同一份数据做全局归一化）。
/// 与 Column 同构：p=正峰、n=负半折叠幅度。
#[derive(Clone, Copy, Default)]
pub(crate) struct ColPeak {
    pub(crate) low_p: f32,
    pub(crate) low_n: f32,
    pub(crate) mid_p: f32,
    pub(crate) mid_n: f32,
    pub(crate) high_p: f32,
    pub(crate) high_n: f32,
    pub(crate) all_p: f32,
    pub(crate) all_n: f32,
    pub(crate) n: usize,
}

/// 带符号列累积：v>0 计入正峰 p，v<0 折叠计入 n（|v|）。
fn fold_max(p: &mut f32, n: &mut f32, v: f32) {
    if v > 0.0 {
        *p = p.max(v);
    } else {
        *n = n.max(-v);
    }
}

/// 从交织立体声样本累积列峰值，至多处理 max_frames 帧，返回实际处理帧数。
/// 剩余样本由调用方保留（carry 到下一段），保证分段路径与整曲连续解码逐字节一致。
pub(crate) fn accumulate_upto(
    cols: &mut Vec<ColPeak>,
    f: &mut BandFilters,
    samples: &[f32],
    max_frames: usize,
) -> usize {
    let n = (samples.len() / 2).min(max_frames);
    for s in samples[..n * 2].chunks_exact(2) {
        let x = (s[0] + s[1]) * 0.5;
        let (low, mid, high) = f.process(x);
        let col = cols.last_mut().expect("cols 非空");
        fold_max(&mut col.low_p, &mut col.low_n, low);
        fold_max(&mut col.mid_p, &mut col.mid_n, mid);
        fold_max(&mut col.high_p, &mut col.high_n, high);
        fold_max(&mut col.all_p, &mut col.all_n, x);
        col.n += 1;
        if col.n >= DETAIL_FRAMES_PER_COL {
            cols.push(ColPeak::default());
        }
    }
    n
}

/// 丢弃末尾空列（帧数恰好顶到 128 边界时残留）。
pub(crate) fn pop_trailing_empty(cols: &mut Vec<ColPeak>) {
    if cols.last().is_some_and(|c| c.n == 0) {
        cols.pop();
    }
}

/// 全局归一化：各频段取全局最大值，√ 压缩到 0..255（整曲完成后用）。
/// 每带共享标度 = max(全局正峰, 全局负半)——与旧 max|·| 归一化同值，
/// 正半字段输出逐字节不变；负半按同标度相对显示（正负比例真实）。
pub(crate) fn normalize_detail(cols: &[ColPeak]) -> Vec<Column> {
    let (mut glp, mut gln, mut gmp, mut gmn, mut ghp, mut ghn, mut gap, mut gan) =
        (1e-9f32, 1e-9f32, 1e-9f32, 1e-9f32, 1e-9f32, 1e-9f32, 1e-9f32, 1e-9f32);
    for c in cols {
        glp = glp.max(c.low_p);
        gln = gln.max(c.low_n);
        gmp = gmp.max(c.mid_p);
        gmn = gmn.max(c.mid_n);
        ghp = ghp.max(c.high_p);
        ghn = ghn.max(c.high_n);
        gap = gap.max(c.all_p);
        gan = gan.max(c.all_n);
    }
    let (sl, sm, sh, sa) = (glp.max(gln), gmp.max(gmn), ghp.max(ghn), gap.max(gan));
    let to_u8 = |v: f32, m: f32| -> u8 { ((v / m).sqrt() * 255.0).min(255.0) as u8 };
    cols.iter()
        .map(|c| Column {
            low_p: to_u8(c.low_p, sl),
            low_n: to_u8(c.low_n, sl),
            mid_p: to_u8(c.mid_p, sm),
            mid_n: to_u8(c.mid_n, sm),
            high_p: to_u8(c.high_p, sh),
            high_n: to_u8(c.high_n, sh),
            all_p: to_u8(c.all_p, sa),
            all_n: to_u8(c.all_n, sa),
        })
        .collect()
}

/// 满刻度 √ 压缩（渐进显示用，不依赖全局值所以显示不会跳动；
/// 全曲完成后由 normalize_detail 的全局归一化替换）。
pub(crate) fn fixed_scale(cols: &[ColPeak]) -> Vec<Column> {
    let fx = |v: f32| -> u8 { (v.sqrt() * 255.0).min(255.0) as u8 };
    cols.iter()
        .map(|c| Column {
            low_p: fx(c.low_p),
            low_n: fx(c.low_n),
            mid_p: fx(c.mid_p),
            mid_n: fx(c.mid_n),
            high_p: fx(c.high_p),
            high_n: fx(c.high_n),
            all_p: fx(c.all_p),
            all_n: fx(c.all_n),
        })
        .collect()
}

/// overview 聚合：每 OVERVIEW_RATIO 个 detail 列逐字段取最大
/// （正负半均为折叠幅度 ≥0，"最负 = 最大 n"，两边都走 max）。
pub(crate) fn build_overview(detail: &[Column]) -> Vec<Column> {
    detail
        .chunks(OVERVIEW_RATIO)
        .map(|ch| {
            let mut c = Column::default();
            for d in ch {
                c.low_p = c.low_p.max(d.low_p);
                c.low_n = c.low_n.max(d.low_n);
                c.mid_p = c.mid_p.max(d.mid_p);
                c.mid_n = c.mid_n.max(d.mid_n);
                c.high_p = c.high_p.max(d.high_p);
                c.high_n = c.high_n.max(d.high_n);
                c.all_p = c.all_p.max(d.all_p);
                c.all_n = c.all_n.max(d.all_n);
            }
            c
        })
        .collect()
}

/// 整曲一次性分析（保持与历史版本相同的输出）。
pub fn analyze(path: &Path) -> Result<WaveformData> {
    let t0 = std::time::Instant::now();
    let mut dec = TrackDecoder::open(path)?;
    let sr_in = dec.sample_rate;
    let mut rs = To48k::new(sr_in, 48_000)?;
    let mut filters = BandFilters::new(48_000.0);
    let mut cols = vec![ColPeak::default()];
    let mut total_frames = 0u64;

    while let Some(native) = dec.decode_next()? {
        let converted = rs.process(&native)?;
        total_frames += accumulate_upto(&mut cols, &mut filters, &converted, usize::MAX) as u64;
    }
    pop_trailing_empty(&mut cols);

    let detail = normalize_detail(&cols);
    let overview = build_overview(&detail);

    log::info!(
        "波形分析完成：{} 帧，detail {} 列，overview {} 列，用时 {:.2}s",
        total_frames,
        detail.len(),
        overview.len(),
        t0.elapsed().as_secs_f64()
    );

    Ok(WaveformData {
        detail,
        overview,
        frames_per_col: DETAIL_FRAMES_PER_COL as u32,
        sample_rate: 48_000,
        duration_frames: total_frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 半波整流输入：all 带只有正峰，负半为零（折叠语义钉死）。
    #[test]
    fn fold_asymmetry() {
        let mut f = BandFilters::new(48_000.0);
        let mut cols = vec![ColPeak::default()];
        let n = DETAIL_FRAMES_PER_COL;
        // |sin| 整流（立体声两声道同值 → mono 同值）
        let mut samples = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0)
                .sin()
                .abs();
            samples.push(v);
            samples.push(v);
        }
        let got = accumulate_upto(&mut cols, &mut f, &samples, usize::MAX);
        assert_eq!(got, n);
        pop_trailing_empty(&mut cols);
        assert!(cols[0].all_p > 0.5);
        assert_eq!(cols[0].all_n, 0.0);

        // 归一化后：正峰满刻度、负半 0（共享标度 = 全局正峰）
        let d = normalize_detail(&cols);
        assert_eq!(d[0].all_p, 255);
        assert_eq!(d[0].all_n, 0);
    }

    /// 每带共享标度：正负半按同标度相对显示（正负比例真实）。
    #[test]
    fn normalize_shared_scale() {
        let cols = vec![ColPeak {
            all_p: 1.0,
            all_n: 0.25,
            low_p: 0.5,
            low_n: 0.5,
            ..Default::default()
        }];
        let d = normalize_detail(&cols);
        assert_eq!(d[0].all_p, 255);
        assert_eq!(d[0].all_n, 127); // (0.25/1.0).sqrt()·255 = 127.5 → 127
        assert_eq!(d[0].low_p, 255);
        assert_eq!(d[0].low_n, 255);
        // 未涉及频段保持 0（标度 floor 1e-9 防除零）
        assert_eq!(d[0].mid_p, 0);
        assert_eq!(d[0].high_n, 0);
    }

    /// overview 聚合：正负半各自取最大（折叠幅度下"最负 = 最大 n"）。
    #[test]
    fn overview_aggregates_both_halves() {
        let detail = vec![
            Column { low_p: 10, low_n: 200, all_p: 0, all_n: 50, ..Column::default() },
            Column { low_p: 250, low_n: 30, all_p: 40, all_n: 0, ..Column::default() },
            Column { low_p: 5, low_n: 20, all_p: 10, all_n: 10, ..Column::default() },
            Column { low_p: 7, low_n: 90, all_p: 5, all_n: 60, ..Column::default() },
        ];
        let ov = build_overview(&detail);
        assert_eq!(ov.len(), 1);
        assert_eq!(ov[0].low_p, 250);
        assert_eq!(ov[0].low_n, 200);
        assert_eq!(ov[0].all_p, 40);
        assert_eq!(ov[0].all_n, 60);
    }
}
