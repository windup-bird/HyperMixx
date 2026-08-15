//! 波形纹理渲染：把波形数据的一段窗口画成 RGBA 像素缓冲。
//! 颜色 = 频段叠加（低→红、中→绿、高→蓝），亮度包络来自 all 频段。
//! beatgrid 网格线叠加：普通拍 α=0.45 白竖线、downbeat α=0.9。

use hypermixx_analysis::waveform::DETAIL_FRAMES_PER_COL;
use hypermixx_analysis::{Column, SEG_COLS, WaveformData};
use slint::{Rgba8Pixel, SharedPixelBuffer};

/// 纹理尺寸（窗口缩放后由 UI 侧拉伸）。
pub const TEXTURE_W: u32 = 1200;
pub const TEXTURE_H: u32 = 140;

/// 每 deck 的波形数据状态：渐进分析期间为稀疏分段，完成后为整曲数据。
pub enum WaveState {
    None,
    /// 分段渐进中：segs[i] 为已分析的第 i 段（16s/6000 列一段）；
    /// 未分析段为 None，渲染时透明跳过（露出深色背景）。
    Partial {
        segs: Vec<Option<Box<[Column]>>>,
    },
    /// 全曲分析完成（全局归一化，与整曲 analyze() 输出一致）。
    Full(WaveformData),
}

impl WaveState {
    /// 已分配列数上界（渲染裁剪用）。
    pub fn cols_total(&self) -> usize {
        match self {
            WaveState::None => 0,
            WaveState::Partial { segs } => segs.len() * SEG_COLS,
            WaveState::Full(w) => w.detail.len(),
        }
    }
}

/// 列聚合：取窗口内最大（RGB 波形惯用 max，保留瞬态）。
/// 对称显示 = 每带 max(p, n)（折叠幅度下等价旧版 max|·|，逐字节一致）。
fn agg(acc: &mut (u8, u8, u8, u8), c: Column) {
    acc.0 = acc.0.max(c.low_p.max(c.low_n));
    acc.1 = acc.1.max(c.mid_p.max(c.mid_n));
    acc.2 = acc.2.max(c.high_p.max(c.high_n));
    acc.3 = acc.3.max(c.all_p.max(c.all_n));
}

/// `beats`/`downbeats`：秒坐标拍点（可空 = 无网格线），升序。
pub fn render(
    data: &WaveState,
    start_sec: f64,
    dur_sec: f64,
    beats: Option<&[f64]>,
    downbeats: Option<&[f64]>,
) -> SharedPixelBuffer<Rgba8Pixel> {
    let w = TEXTURE_W;
    let h = TEXTURE_H;
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    let bytes = buf.make_mut_bytes();

    // 分析恒为 48kHz / 128 帧每列（WaveformData 的字段未来才有变化）
    let sr = 48_000.0f64;
    let fpc = DETAIL_FRAMES_PER_COL as f64;
    let cols_total = data.cols_total();
    let cols_per_px = (dur_sec * sr / fpc / w as f64).max(1.0);
    let start_col = (start_sec * sr / fpc).floor().max(0.0) as usize;
    // 窗口起点可为负（指针恒居中：曲头前留白），曲目内容从 lead_px 像素起画
    let lead_px = if start_sec < 0.0 {
        (-start_sec / dur_sec * w as f64) as i32
    } else {
        0
    };
    let half_h = h as i32 / 2;

    for x in 0..w {
        let xoff = x as i32 - lead_px;
        if xoff < 0 {
            continue; // 曲头前：透明（深色背景透出）
        }
        let c0 = start_col + (xoff as f64 * cols_per_px).floor() as usize;
        let c1 = (start_col + ((x + 1) as f64 * cols_per_px).floor() as usize).min(cols_total);
        let mut acc = (0u8, 0u8, 0u8, 0u8);
        match data {
            WaveState::Full(w) => {
                for c in w.detail.iter().take(c1).skip(c0) {
                    agg(&mut acc, *c);
                }
            }
            WaveState::Partial { segs } => {
                for c in c0..c1 {
                    if let Some(s) = segs.get(c / SEG_COLS).and_then(|s| s.as_ref()) {
                        agg(&mut acc, s[c % SEG_COLS]);
                    }
                }
            }
            WaveState::None => {}
        }
        let (lo, mi, hi, al) = acc;
        if al == 0 {
            continue;
        }
        // 幅度包络（√ 压缩 + 高度余量）
        let amp = ((al as f32 / 255.0).sqrt() * (half_h as f32 - 3.0)) as i32;
        // 频段归一化到每列：暗列不显示，亮列全白
        let mx = lo.max(mi).max(hi).max(1) as f32;
        let r = (lo as f32 / mx * 255.0) as u8;
        let g = (mi as f32 / mx * 255.0) as u8;
        let b = (hi as f32 / mx * 255.0) as u8;
        for dy in -amp..=amp {
            let y = half_h + dy;
            if y < 0 || y >= h as i32 {
                continue;
            }
            let idx = ((y as u32 * w + x) * 4) as usize;
            bytes[idx] = r;
            bytes[idx + 1] = g;
            bytes[idx + 2] = b;
            bytes[idx + 3] = 255;
        }
    }

    // beatgrid 网格线（拍点秒坐标 → 像素列；升序扫描，窗口外即停）
    if let Some(beats) = beats {
        let end_sec = start_sec + dur_sec;
        let first = beats.partition_point(|&b| b < start_sec);
        for &t in beats.iter().skip(first) {
            if t >= end_sec {
                break;
            }
            let x = ((t - start_sec) / dur_sec * w as f64) as i32;
            if !(0..w as i32).contains(&x) {
                continue;
            }
            // downbeat 判定：t 在 downbeats 秒表里
            let is_down = downbeats
                .is_some_and(|d| d.binary_search_by(|&v| v.partial_cmp(&t).unwrap()).is_ok());
            let alpha: u8 = if is_down { 230 } else { 115 };
            for y in 0..h as i32 {
                let idx = ((y as u32 * w + x as u32) * 4) as usize;
                bytes[idx] = 255;
                bytes[idx + 1] = 255;
                bytes[idx + 2] = 255;
                bytes[idx + 3] = alpha;
            }
        }
    }
    buf
}
