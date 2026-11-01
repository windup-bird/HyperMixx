//! The track overview under a deck's status row.
//!
//! The detail waveform scrolls; this one shows the whole track, so a DJ can see where the breakdown
//! and the outro are without zooming out. It is a *folded* rendering, as the plan asks: the stereo
//! waveform's lower half is folded up onto the upper half (one silhouette instead of two mirrored
//! ones), which doubles the useful height in the two rows the band has.
//!
//! Three things keep it readable at that size:
//!
//! * **Folding** — per band, `max(left, right)`, so the two channels overlap into one shape.
//! * **Normalisation** — every column is divided by the loudest column of the track, so a quiet
//!   master still fills the box and a loud one does not clip it.
//! * **An S-curve** — `smoothstep`, which keeps quiet material quiet and lets loud material fill the
//!   box, without a log scale's lumpy dynamics.
//!
//! The three bands are stacked (low at the bottom, high on top) rather than drawn over each other.

use hypermixx_core::{BeatGrid, DeckState, HOT_CUE_SLOTS};
use hypermixx_library::Waveform;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::symbols::Marker;
use ratatui::widgets::canvas::{Canvas, Context, Line as CanvasLine};
use ratatui::Frame;

use crate::preference::{Theme, WaveformColors};

/// One column of the overview: the folded, normalised, S-curved heights of the three bands, each in
/// `0.0..=1.0` and summing to at most 1.
pub type Profile = [f64; 3];

/// Folds, normalises and curves the whole track into `columns` profiles.
pub fn profiles(waveform: Option<&Waveform>, total_frames: u64, columns: usize) -> Vec<Profile> {
    let columns = columns.max(1);
    let Some(waveform) = waveform.filter(|_| total_frames > 0) else {
        return Vec::new();
    };
    let frames_per_column = total_frames as f64 / columns as f64;
    let mut raw: Vec<Profile> = Vec::with_capacity(columns);
    for peaks in waveform.columns(0.0, frames_per_column, columns) {
        // Fold: the lower half is turned up onto the upper half, so the two channels overlap.
        let fold = |left: u8, right: u8| f64::from(left.max(right)) / 255.0;
        raw.push([
            fold(peaks.left.low, peaks.right.low),
            fold(peaks.left.mid, peaks.right.mid),
            fold(peaks.left.high, peaks.right.high),
        ]);
    }
    // Normalise against the loudest column of the track, not of the viewport: the overview shows the
    // whole track, so its scale must not jump around as the playhead moves.
    let loudest = raw
        .iter()
        .map(|bands| bands[0] + bands[1] + bands[2])
        .fold(0.0f64, f64::max);
    if loudest <= 0.0 {
        return vec![[0.0; 3]; raw.len()];
    }
    raw.into_iter()
        .map(|bands| {
            let sum = bands[0] + bands[1] + bands[2];
            let height = s_curve((sum / loudest).clamp(0.0, 1.0));
            // Each band keeps its share of the column, so the stack keeps its colours as it grows.
            if sum <= 0.0 {
                [0.0; 3]
            } else {
                [
                    height * bands[0] / sum,
                    height * bands[1] / sum,
                    height * bands[2] / sum,
                ]
            }
        })
        .collect()
}

/// `smoothstep`: the cheap S-curve the plan asks for. It flattens the two ends of the amplitude
/// range and steepens the middle, so a quiet intro stays visibly quiet while a loud chorus fills the
/// box instead of everything landing in the middle.
fn s_curve(value: f64) -> f64 {
    value * value * (3.0 - 2.0 * value)
}

/// Draws `waveform` across `area` as braille, with the playhead, the loop wash and the cue marks.
///
/// A deck with no waveform yet draws only the playhead line: an empty box would look like a bug.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    waveform: Option<&Waveform>,
    grid: Option<&BeatGrid>,
    state: &DeckState,
    colors: WaveformColors,
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 || state.total_frames == 0 {
        return;
    }
    let dots_x = f64::from(area.width) * 2.0;
    let height = f64::from(area.height) * 4.0;
    let total = state.total_frames as f64;
    let playhead = state.current_frame.min(state.total_frames) as f64 / total;
    let profiles = profiles(waveform, state.total_frames, dots_x as usize);

    let canvas = Canvas::default()
        .x_bounds([0.0, dots_x])
        .y_bounds([0.0, height])
        .marker(Marker::Braille)
        .paint(|ctx| {
            if let Some(grid) = grid.filter(|grid| !grid.is_empty()) {
                draw_beats(ctx, grid, state.total_frames, dots_x, height, theme.dim);
            }
            for (index, profile) in profiles.iter().enumerate() {
                let x = index as f64;
                let low = profile[0] * height;
                let mid = (profile[0] + profile[1]) * height;
                let high = (profile[0] + profile[1] + profile[2]) * height;
                ctx.draw(&CanvasLine::new(x, 0.0, x, low, colors.low));
                ctx.draw(&CanvasLine::new(x, low, x, mid, colors.mid));
                ctx.draw(&CanvasLine::new(x, mid, x, high, colors.high));
            }
            let x = playhead * dots_x;
            ctx.draw(&CanvasLine::new(x, 0.0, x, height, Color::Red));
        });
    frame.render_widget(canvas, area);

    paint_loop(frame, area, state, total);
    draw_cues(frame, area, state);
}

/// The same overview as straight RGBA, for the Kitty image path: `width × height` pixels, with the
/// folded silhouette growing from the bottom and the playhead, loop and cues drawn into it.
pub fn rgba(
    waveform: Option<&Waveform>,
    grid: Option<&BeatGrid>,
    state: &DeckState,
    colors: WaveformColors,
    width: u32,
    height: u32,
    theme: &Theme,
) -> Vec<u8> {
    let width = width.max(1);
    let height = height.max(1);
    let mut image = vec![0u8; (width * height * 4) as usize];
    if waveform.is_none() || state.total_frames == 0 {
        return image;
    }
    let profiles = profiles(waveform, state.total_frames, width as usize);
    let bands = [rgb(colors.low), rgb(colors.mid), rgb(colors.high)];
    let looped = state.loop_range.map(|(start, end)| {
        let scale = f64::from(width) / state.total_frames as f64;
        (
            (start as f64 * scale) as u32,
            (end as f64 * scale).ceil() as u32,
        )
    });
    // A free function rather than a closure: the loop below also touches `image` directly.
    fn put(image: &mut [u8], width: u32, x: u32, y: u32, color: [u8; 4]) {
        let offset = ((y * width + x) * 4) as usize;
        if let Some(pixel) = image.get_mut(offset..offset + 4) {
            pixel.copy_from_slice(&color);
        }
    }
    for (index, profile) in profiles.iter().enumerate() {
        let x = index as u32;
        if x >= width {
            break;
        }
        let low = (profile[0] * f64::from(height)).round() as u32;
        let mid = ((profile[0] + profile[1]) * f64::from(height)).round() as u32;
        let high = ((profile[0] + profile[1] + profile[2]) * f64::from(height)).round() as u32;
        for row in 0..high.min(height) {
            let color = if row < low {
                bands[0]
            } else if row < mid {
                bands[1]
            } else {
                bands[2]
            };
            // Rows are counted from the bottom: the silhouette grows upward.
            put(&mut image, width, x, height - 1 - row, color);
        }
        if let Some((start, end)) = looped {
            if x >= start && x < end {
                for row in 0..height {
                    let offset = ((row * width + x) * 4) as usize;
                    if let Some(pixel) = image.get_mut(offset..offset + 4) {
                        if pixel[3] == 0 {
                            pixel.copy_from_slice(&[40, 40, 40, 255]);
                        }
                    }
                }
            }
        }
    }
    if let Some(grid) = grid.filter(|grid| !grid.is_empty()) {
        draw_bars(
            &mut image,
            grid,
            state.total_frames,
            width,
            height,
            theme.dim,
        );
    }
    // Playhead last, so it wins over the bars and the loop wash.
    let playhead = (state.current_frame.min(state.total_frames) as f64 / state.total_frames as f64
        * f64::from(width)) as u32;
    for row in 0..height {
        put(
            &mut image,
            width,
            playhead.min(width - 1),
            row,
            [255, 40, 40, 255],
        );
    }
    for (slot, frame) in state.hot_cues.iter().enumerate() {
        if let Some(frame) = frame {
            let x = (frame.frame as f64 / state.total_frames as f64 * f64::from(width)) as u32;
            for row in height.saturating_sub(2)..height {
                put(
                    &mut image,
                    width,
                    x.min(width - 1),
                    row,
                    HOT_RGB[slot.min(HOT_RGB.len() - 1)],
                );
            }
        }
    }
    if state.cue_frame > 0 {
        let x = (state.cue_frame as f64 / state.total_frames as f64 * f64::from(width)) as u32;
        for row in height.saturating_sub(2)..height {
            put(
                &mut image,
                width,
                x.min(width - 1),
                row,
                [245, 245, 245, 255],
            );
        }
    }
    image
}

/// Bar lines, faint, as in the braille path.
fn draw_bars(
    image: &mut [u8],
    grid: &BeatGrid,
    total_frames: u64,
    width: u32,
    height: u32,
    color: Color,
) {
    const MAX_BARS: usize = 256;
    let bars = (total_frames as f64 / (grid.average_bpm() as f64 * 2.0) * 60.0 / 4.0) as usize;
    if bars == 0 || bars > MAX_BARS {
        return;
    }
    let ink = rgb(color);
    for bar in 0..bars {
        let frame = grid.frame_at_beat((bar * 4) as u64);
        let x = (frame as f64 / total_frames as f64 * f64::from(width)) as u32;
        if x >= width {
            break;
        }
        for row in 0..(height / 6).max(1) {
            let offset = (((height - 1 - row) * width + x) * 4) as usize;
            if let Some(pixel) = image.get_mut(offset..offset + 4) {
                pixel.copy_from_slice(&ink);
            }
        }
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

const HOT_RGB: [[u8; 4]; HOT_CUE_SLOTS] = [
    [255, 72, 72, 255],
    [255, 160, 64, 255],
    [255, 232, 64, 255],
    [80, 255, 96, 255],
    [64, 240, 240, 255],
    [80, 160, 255, 255],
    [200, 96, 255, 255],
    [255, 96, 200, 255],
];

fn draw_beats(
    ctx: &mut Context,
    grid: &BeatGrid,
    total_frames: u64,
    width: f64,
    height: f64,
    color: Color,
) {
    // Bars only: at overview scale the individual beats would be a solid wash.
    const MAX_BARS: usize = 256;
    let total = total_frames as f64;
    let bars = (total / (grid.average_bpm() as f64 * 2.0) * 60.0 / 4.0) as usize;
    if bars == 0 || bars > MAX_BARS {
        return;
    }
    for bar in 0..bars {
        let frame = grid.frame_at_beat((bar * 4) as u64) as f64;
        let x = frame / total * width;
        ctx.draw(&CanvasLine::new(x, 0.0, x, height * 0.15, color));
    }
}

fn paint_loop(frame: &mut Frame, area: Rect, state: &DeckState, total: f64) {
    let Some((start, end)) = state.loop_range else {
        return;
    };
    let first = (start as f64 / total * f64::from(area.width)).floor() as i64;
    let last = (end as f64 / total * f64::from(area.width)).ceil() as i64;
    let buffer = frame.buffer_mut();
    for column in first.max(0)..last.min(i64::from(area.width)) {
        let x = area.x + column as u16;
        for y in area.top()..area.bottom() {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_bg(Color::Rgb(60, 60, 60));
            }
        }
    }
}

/// Cue marks on the bottom row: white for the memory cue, slot colours for the hot cues.
fn draw_cues(frame: &mut Frame, area: Rect, state: &DeckState) {
    if state.total_frames == 0 || area.height == 0 {
        return;
    }
    let y = area.bottom() - 1;
    let mut marks: Vec<(u64, Color)> = Vec::new();
    if state.cue_frame > 0 {
        marks.push((state.cue_frame, Color::White));
    }
    for (slot, cue) in state.hot_cues.iter().enumerate() {
        if let Some(cue) = cue {
            marks.push((cue.frame, HOT_COLORS[slot.min(HOT_COLORS.len() - 1)]));
        }
    }
    let buffer = frame.buffer_mut();
    for (frame, color) in marks {
        let column = (frame as f64 / state.total_frames as f64 * f64::from(area.width)) as u16;
        if let Some(cell) = buffer.cell_mut((area.x + column.min(area.width - 1), y)) {
            cell.set_symbol("▁").set_fg(color);
        }
    }
}

const HOT_COLORS: [Color; HOT_CUE_SLOTS] = [
    Color::Rgb(255, 72, 72),
    Color::Rgb(255, 160, 64),
    Color::Rgb(255, 232, 64),
    Color::Rgb(80, 255, 96),
    Color::Rgb(64, 240, 240),
    Color::Rgb(80, 160, 255),
    Color::Rgb(200, 96, 255),
    Color::Rgb(255, 96, 200),
];

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_library::{BandPeaks, StereoPeaks};

    fn column(left: u8, right: u8) -> StereoPeaks {
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

    fn waveform(buckets: Vec<StereoPeaks>) -> Waveform {
        let frames = buckets.len() as u64 * 256;
        Waveform::from_buckets(buckets, 256, frames)
    }

    #[test]
    fn folding_takes_the_louder_channel_of_each_band() {
        let right_only = waveform(vec![column(0, 200)]);
        let left_only = waveform(vec![column(200, 0)]);
        let both = waveform(vec![column(180, 200)]);
        let folded = profiles(Some(&right_only), 256, 1);
        assert_eq!(folded, profiles(Some(&left_only), 256, 1));
        assert_eq!(folded, profiles(Some(&both), 256, 1));
    }

    #[test]
    fn the_loudest_column_is_normalised_to_the_full_height() {
        // A quiet master still fills the box: normalisation is against the track, not full scale.
        let quiet = waveform(vec![column(64, 64), column(32, 32)]);
        let profiles = profiles(Some(&quiet), 512, 2);
        let height = |profile: &Profile| profile[0] + profile[1] + profile[2];
        assert!(
            (height(&profiles[0]) - 1.0).abs() < 1e-9,
            "{:?}",
            profiles[0]
        );
        assert!(
            (height(&profiles[1]) - s_curve(0.5)).abs() < 1e-9,
            "half as loud, then the S-curve: {:?}",
            profiles[1]
        );
    }

    #[test]
    fn the_s_curve_keeps_the_ends_and_adds_contrast_in_the_middle() {
        assert_eq!(s_curve(0.0), 0.0);
        assert_eq!(s_curve(1.0), 1.0);
        assert_eq!(s_curve(0.5), 0.5);
        // Below halfway the curve sits under the diagonal, above it sits over: quiet material stays
        // quiet instead of being smeared across the box, loud material fills it.
        assert!(s_curve(0.25) < 0.25, "{}", s_curve(0.25));
        assert!(s_curve(0.75) > 0.75, "{}", s_curve(0.75));
    }

    #[test]
    fn the_three_bands_keep_their_share_of_the_column() {
        let buckets = vec![StereoPeaks {
            left: BandPeaks {
                low: 255,
                mid: 128,
                high: 64,
            },
            right: BandPeaks::ZERO,
        }];
        let profiles = profiles(Some(&waveform(buckets)), 256, 1);
        let profile = profiles[0];
        assert!(
            profile[0] > profile[1] && profile[1] > profile[2],
            "{profile:?}"
        );
        let total = profile[0] + profile[1] + profile[2];
        assert!((total - 1.0).abs() < 1e-9, "{profile:?}");
        assert!(
            ((profile[0] + profile[1]) / total - (255.0 + 128.0) / 447.0).abs() < 1e-6,
            "{profile:?}"
        );
    }

    #[test]
    fn a_deck_without_a_waveform_has_no_profile() {
        assert!(profiles(None, 1_000, 10).is_empty());
        assert!(profiles(Some(&waveform(vec![column(9, 9)])), 0, 10).is_empty());
    }

    #[test]
    fn the_image_path_draws_the_silhouette_from_the_bottom_up() {
        let state = DeckState {
            deck_id: 0,
            current_frame: 0,
            playing: false,
            total_frames: 4096,
            bpm: 120.0,
            bpm_at_frame: 120.0,
            key: None,
            virtual_frame: 0,
            loop_range: None,
            loop_in_armed: None,
            jump_beats: 8,
            loop_beats: hypermixx_core::BeatLen::beats(8),
            tempo: 1.0,
            tempo_fader: 0.0,
            tempo_range: 0.1,
            nudgerate: 0.0,
            playing_rate: 1.0,
            lock: false,
            align: None,
            nudge: 0.0,
            sync_leader: None,
            sync_mode: "free".to_owned(),
            group_bpm: 0.0,
            cue_frame: 0,
            hot_cues: [None; HOT_CUE_SLOTS],
            vinyl: false,
            keylock: hypermixx_core::KeylockMode::On,
            key_shift: 0,
            stems: Default::default(),
        };
        let colors = WaveformColors::default();
        let theme = Theme::default();
        let image = rgba(
            // One loud bucket with a total length four times longer: the tail columns are silent.
            Some(&Waveform::from_buckets(vec![column(255, 255)], 256, 4096)),
            None,
            &state,
            colors,
            8,
            8,
            &theme,
        );
        let alpha = |x: u32, y: u32| image[((y * 8 + x) * 4 + 3) as usize];
        // The single loud column fills its full height, bottom rows included.
        assert!(alpha(0, 7) > 0, "the silhouette starts at the bottom");
        assert!(alpha(0, 0) > 0, "a full-height column reaches the top");
        assert_eq!(alpha(7, 7), 0, "columns without data stay empty");
    }
}
