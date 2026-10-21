//! Shared pixel rasteriser for the image backends.
//!
//! Everything is drawn into a small palette-indexed buffer, so Kitty and Sixel consume the same
//! projection and differ only in how those indices leave the process. The buffer is intentionally
//! low resolution: the Kitty backend lets the terminal scale it to the cell rectangle, and Sixel
//! gets the terminal's actual cell pixel size.

use hypermixx_core::BeatGrid;
use hypermixx_library::{StereoPeaks, Waveform};
use ratatui::style::Color;

use crate::preference::WaveformColors;

/// Palette slots. Shared by both encoders so a pixel means the same thing everywhere.
pub const BG: u8 = 0;
pub const LOW: u8 = 1;
pub const MID: u8 = 2;
pub const HIGH: u8 = 3;
pub const CENTER: u8 = 4;
pub const BEAT: u8 = 5;
pub const BAR: u8 = 6;
pub const PLAYHEAD: u8 = 7;
pub const PALETTE_LEN: usize = 8;

/// Palette derived from the three configurable band colours plus the fixed UI furniture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// RGBA for Kitty. Sixel ignores alpha (index 0 is drawn as opaque black).
    pub rgba: [[u8; 4]; PALETTE_LEN],
}

impl Palette {
    pub fn from_colors(colors: WaveformColors) -> Self {
        let mut rgba = [[0u8, 0, 0, 255]; PALETTE_LEN];
        rgba[usize::from(BG)] = [0, 0, 0, 0];
        rgba[usize::from(LOW)] = rgb(colors.low);
        rgba[usize::from(MID)] = rgb(colors.mid);
        rgba[usize::from(HIGH)] = rgb(colors.high);
        rgba[usize::from(CENTER)] = [80, 80, 80, 255];
        rgba[usize::from(BEAT)] = [60, 60, 60, 255];
        rgba[usize::from(BAR)] = [96, 96, 96, 255];
        rgba[usize::from(PLAYHEAD)] = [255, 40, 40, 255];
        Self { rgba }
    }

    pub fn rgb(&self, index: u8) -> [u8; 3] {
        let [r, g, b, _] = self.rgba[usize::from(index).min(PALETTE_LEN - 1)];
        [r, g, b]
    }
}

fn rgb(color: Color) -> [u8; 4] {
    let (r, g, b) = match color {
        Color::Black => (0, 0, 0),
        Color::Red => (255, 0, 0),
        Color::Green => (0, 255, 0),
        Color::Yellow => (255, 255, 0),
        Color::Blue => (0, 0, 255),
        Color::Magenta => (255, 0, 255),
        Color::Cyan => (0, 255, 255),
        Color::Gray => (128, 128, 128),
        Color::DarkGray => (64, 64, 64),
        Color::LightRed => (255, 128, 128),
        Color::LightGreen => (128, 255, 128),
        Color::LightYellow => (255, 255, 128),
        Color::LightBlue => (128, 128, 255),
        Color::LightMagenta => (255, 128, 255),
        Color::LightCyan => (128, 255, 255),
        Color::White => (255, 255, 255),
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (255, 255, 255),
    };
    [r, g, b, 255]
}

/// Palette-indexed pixels, row-major.
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub indices: Vec<u8>,
}

impl Raster {
    pub fn rgba(&self, palette: &Palette) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.indices.len() * 4);
        for index in &self.indices {
            out.extend_from_slice(&palette.rgba[usize::from(*index)]);
        }
        out
    }
}

/// Draws the current viewport into `width × height` pixels.
///
/// `frames_per_pixel` is the only projection parameter; the caller derives it from the
/// backend-independent `frames_per_cell`.
#[allow(clippy::too_many_arguments)]
pub fn rasterize(
    waveform: Option<&Waveform>,
    grid: Option<&BeatGrid>,
    playhead: f64,
    frames_per_pixel: f64,
    width: u32,
    height: u32,
    include_playhead: bool,
) -> Raster {
    let width = width.max(1);
    let height = height.max(1);
    let mut indices = vec![BG; (width as usize) * (height as usize)];
    let center_y = height / 2;
    let center_x = width / 2;
    let frames_per_pixel = frames_per_pixel.max(1.0);
    let playhead = playhead.max(0.0);
    let start_frame = playhead - f64::from(center_x) * frames_per_pixel;

    if let Some(grid) = grid {
        draw_beats(
            &mut indices,
            width,
            height,
            grid,
            playhead,
            frames_per_pixel,
        );
    }

    // Centre line first, waveform over it.
    for x in 0..width {
        indices[(center_y as usize) * (width as usize) + x as usize] = CENTER;
    }

    if let Some(waveform) = waveform {
        for (index, peaks) in waveform
            .columns(start_frame, frames_per_pixel, width as usize)
            .enumerate()
        {
            draw_stereo_column(&mut indices, width, height, center_y, index as u32, peaks);
        }
    }

    if include_playhead {
        for y in 0..height {
            indices[(y as usize) * (width as usize) + center_x as usize] = PLAYHEAD;
        }
    }

    Raster {
        width,
        height,
        indices,
    }
}

fn draw_stereo_column(
    indices: &mut [u8],
    width: u32,
    height: u32,
    center_y: u32,
    x: u32,
    peaks: StereoPeaks,
) {
    let up = center_y;
    let down = height - center_y;
    for (band, color) in [
        (peaks.left.low, LOW),
        (peaks.left.mid, MID),
        (peaks.left.high, HIGH),
    ] {
        draw_band(indices, width, center_y, x, up, true, band, color);
    }
    for (band, color) in [
        (peaks.right.low, LOW),
        (peaks.right.mid, MID),
        (peaks.right.high, HIGH),
    ] {
        draw_band(indices, width, center_y, x, down, false, band, color);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_band(
    indices: &mut [u8],
    width: u32,
    center_y: u32,
    x: u32,
    extent: u32,
    upward: bool,
    amplitude: u8,
    color: u8,
) {
    let length = (f64::from(amplitude) / 255.0 * f64::from(extent)).round() as u32;
    for offset in 0..length {
        let y = if upward {
            if offset >= center_y {
                break;
            }
            center_y - 1 - offset
        } else {
            let y = center_y + offset;
            if y >= center_y + extent {
                break;
            }
            y
        };
        indices[(y as usize) * (width as usize) + x as usize] = color;
    }
}

fn draw_beats(
    indices: &mut [u8],
    width: u32,
    height: u32,
    grid: &BeatGrid,
    playhead: f64,
    frames_per_pixel: f64,
) {
    if grid.is_empty() {
        return;
    }
    let center = f64::from(width) / 2.0;
    let start_frame = playhead - center * frames_per_pixel;
    let end_frame = playhead + center * frames_per_pixel;
    if end_frame <= 0.0 {
        return;
    }
    let first = grid.floor_beat(start_frame.max(0.0) as u64);
    for beat in (first..).take(512) {
        let frame = grid.frame_at_beat(beat) as f64;
        if frame > end_frame {
            break;
        }
        let x = center + (frame - playhead) / frames_per_pixel;
        if !(0.0..f64::from(width)).contains(&x) {
            continue;
        }
        let x = x as u32;
        let color = if beat.is_multiple_of(4) { BAR } else { BEAT };
        for y in 0..height {
            indices[(y as usize) * (width as usize) + x as usize] = color;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_library::BandPeaks;
    use hypermixx_library::StereoPeaks;

    fn stereo(left: u8, right: u8) -> StereoPeaks {
        StereoPeaks {
            left: BandPeaks {
                low: left,
                mid: left,
                high: left,
            },
            right: BandPeaks {
                low: right,
                mid: right,
                high: right,
            },
        }
    }

    #[test]
    fn left_paints_above_and_right_below_the_centre() {
        let waveform = Waveform::from_buckets(vec![stereo(255, 0)], 256, 256);
        // Centre the viewport on the one bucket so every column has data.
        let raster = rasterize(Some(&waveform), None, 128.0, 1.0, 21, 12, false);
        let center = raster.height / 2;
        let left = (0..center).any(|y| {
            (0..raster.width)
                .any(|x| raster.indices[(y * raster.width + x) as usize] != BG)
        });
        let right = (center + 1..raster.height).any(|y| {
            (0..raster.width)
                .any(|x| raster.indices[(y * raster.width + x) as usize] != BG)
        });
        assert!(left, "left channel should paint above the centre");
        assert!(!right, "right channel is empty");
    }
}
