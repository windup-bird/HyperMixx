//! The waveform canvas.
//!
//! One braille dot column per peak column: the playhead is pinned to the horizontal center and the
//! viewport is recomputed from the current playback frame every tick, so the track scrolls
//! right-to-left under the cursor. Both decks are drawn with the same `frames_per_dot`, which is
//! what makes their waveforms comparable.
//!
//! Band layout follows spec B, overlaid rather than in three lanes: all three bands grow from the
//! same baseline to their own amplitude, so the boundaries are real levels instead of fixed
//! thirds. Drawing low -> mid -> high leaves the bottom the high band's colour and the top the low
//! band's (bottom white = high, middle green = mid, top blue = low).

use std::sync::Arc;

use hypermixx_core::BeatGrid;
use hypermixx_library::Waveform;
use ratatui::style::Color;
use ratatui::symbols::Marker;
use ratatui::widgets::canvas::{Canvas, Context, Line as CanvasLine};
use ratatui::Frame;

use ratatui::layout::Rect;

const HIGH_COLOR: Color = Color::White;
const MID_COLOR: Color = Color::Green;
const LOW_COLOR: Color = Color::Blue;
const PLAYHEAD_COLOR: Color = Color::Red;
/// Beat lines are dim so they read as a grid, not as audio; bar starts (every 4th) are slightly
/// brighter.
const BEAT_COLOR: Color = Color::Rgb(60, 60, 60);
const BAR_COLOR: Color = Color::Rgb(96, 96, 96);
/// More than this many beat lines in one view means we are zoomed far out; stop drawing rather than
/// spend the frame on the grid.
const MAX_BEATS: usize = 512;

/// Draws `waveform` into `area`. `playhead_frame` sits on the center column.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    waveform: Option<&Arc<Waveform>>,
    grid: Option<&BeatGrid>,
    playhead_frame: f64,
    frames_per_dot: f64,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let dots_x = f64::from(area.width) * 2.0;
    let dots_y = f64::from(area.height) * 4.0;
    let center = (dots_x - 1.0) / 2.0;
    let playhead = playhead_frame.max(0.0);
    let frames_per_dot = frames_per_dot.max(1.0);

    let canvas = Canvas::default()
        .x_bounds([0.0, dots_x])
        .y_bounds([0.0, dots_y])
        .marker(Marker::Braille)
        .paint(|ctx| {
            // Grid first, waveform over it, playhead last: each later shape should win.
            if let Some(grid) = grid {
                draw_beats(ctx, grid, playhead, frames_per_dot, center, dots_x, dots_y);
            }
            if let Some(waveform) = waveform {
                draw_bands(
                    ctx,
                    waveform,
                    playhead,
                    frames_per_dot,
                    center,
                    dots_x as usize,
                    dots_y,
                );
            }
            ctx.draw(&CanvasLine::new(
                center,
                0.0,
                center,
                dots_y,
                PLAYHEAD_COLOR,
            ));
        });
    frame.render_widget(canvas, area);
}

/// Per braille column, the three bands overlaid on one baseline. Each band spans `0..amplitude`
/// of the full height; the last drawn wins a dot, which is what makes the colour order read as
/// bottom high (white) / middle mid (green) / top low (blue).
fn draw_bands(
    ctx: &mut Context,
    waveform: &Waveform,
    playhead: f64,
    frames_per_dot: f64,
    center: f64,
    columns: usize,
    dots_y: f64,
) {
    let start_frame = playhead - center * frames_per_dot;
    for (index, peak) in waveform
        .columns(start_frame, frames_per_dot, columns)
        .enumerate()
    {
        let x = index as f64;
        let high = peak.high.clamp(0.0, 1.0) as f64;
        let mid = peak.mid.clamp(0.0, 1.0) as f64;
        let low = peak.low.clamp(0.0, 1.0) as f64;
        segment_line(ctx, x, 0.0, low * dots_y, LOW_COLOR);
        segment_line(ctx, x, 0.0, mid * dots_y, MID_COLOR);
        segment_line(ctx, x, 0.0, high * dots_y, HIGH_COLOR);
    }
}

fn segment_line(ctx: &mut Context, x: f64, base: f64, height: f64, color: Color) {
    if height <= 0.0 {
        return;
    }
    ctx.draw(&CanvasLine::new(x, base, x, base + height, color));
}

fn draw_beats(
    ctx: &mut Context,
    grid: &BeatGrid,
    playhead: f64,
    frames_per_dot: f64,
    center: f64,
    dots_x: f64,
    dots_y: f64,
) {
    if grid.is_empty() {
        return;
    }
    let start_frame = playhead - center * frames_per_dot;
    let end_frame = playhead + center * frames_per_dot;
    if end_frame <= 0.0 {
        return;
    }
    let first = grid.floor_beat(start_frame.max(0.0) as u64);
    for beat in (first..).take(MAX_BEATS) {
        let frame = grid.frame_at_beat(beat) as f64;
        if frame > end_frame {
            break;
        }
        let x = center + (frame - playhead) / frames_per_dot;
        if (0.0..=dots_x).contains(&x) {
            let color = if beat.is_multiple_of(4) {
                BAR_COLOR
            } else {
                BEAT_COLOR
            };
            ctx.draw(&CanvasLine::new(x, 0.0, x, dots_y, color));
        }
    }
}
