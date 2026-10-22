//! The waveform view and its three transports.
//!
//! Braille is the always-available renderer: one dot column per peak column, left channel above the
//! centre line and right below it. Kitty and Sixel draw the same view through a pixel rasteriser;
//! they differ only in how the pixels leave the process. The playhead is pinned to the horizontal
//! centre and the viewport is recomputed from the current playback frame every tick.
//!
//! Kitty renders a wide, native-resolution strip once and then moves a source rectangle of it with
//! `a=p`; edges therefore land 1:1 on terminal pixels instead of being scaled. Sixel cannot scale
//! or reuse an image, so it redraws at the terminal's real cell size at a 30Hz cap. Both fall back
//! to braille whenever detection is unconvincing.

mod detect;
mod kitty;
mod raster;
mod sixel;

use std::fmt::Write;
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hypermixx_core::BeatGrid;
use hypermixx_library::Waveform;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::symbols::Marker;
use ratatui::widgets::canvas::{Canvas, Context, Line as CanvasLine};
use ratatui::Frame;

use crate::preference::{WaveformColors, WaveformSettings};

pub use detect::{Backend, CellPx};

const PLAYHEAD_COLOR: Color = Color::Red;
const CENTER_COLOR: Color = Color::DarkGray;
/// Beat lines are dim so they read as a grid, not as audio; bar starts (every 4th) are slightly
/// brighter.
const BEAT_COLOR: Color = Color::Rgb(60, 60, 60);
const BAR_COLOR: Color = Color::Rgb(96, 96, 96);
/// More than this many beat lines in one view means we are zoomed far out; stop drawing rather than
/// spend the frame on the grid.
const MAX_BEATS: usize = 512;
/// Image backends never go past this. A 30Hz sixel is already a large stream; kitty can do 60.
const SIXEL_MAX_HZ: u16 = 30;
/// Sixel cannot scale, so an unusually large deck rectangle would make the encoder the frame.
/// Above this many pixels the view keeps braille rather than risking the 30Hz budget.
const MAX_IMAGE_PIXELS: u32 = 1_500_000;
/// Even an unchanged image is re-sent once a second. That is cheap (it is only a few kilobytes)
/// and recovers from a terminal/modal that cleared the placement behind our back.
const IMAGE_REFRESH_HEAL: Duration = Duration::from_secs(1);
const UNIT_WIDTH: CellDiffOption = CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap());
/// Kitty image ids are 32-bit and app-local. A fresh id on every rebuild lets the old image stay
/// visible until the replacement has been transmitted and placed.
static NEXT_IMAGE_ID: AtomicU32 = AtomicU32::new(0x484D_0001);
static NEXT_PLACEMENT_ID: AtomicU32 = AtomicU32::new(1);

fn next_image_id() -> u32 {
    NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed)
}

fn next_placement_id() -> u32 {
    NEXT_PLACEMENT_ID.fetch_add(1, Ordering::Relaxed).max(1)
}

/// A cheap fingerprint of the beat grid. `Option<BeatGrid>` lives inline, so a pointer is not a
/// content identity; the first two beats plus the average tempo are enough to notice an analysis
/// change without hashing every beat.
#[derive(Clone, Copy, PartialEq, Eq)]
struct GridKey {
    first: u64,
    second: u64,
    bpm_bits: u32,
}

impl GridKey {
    fn of(grid: Option<&BeatGrid>) -> Self {
        grid.map_or(
            Self {
                first: 0,
                second: 0,
                bpm_bits: 0,
            },
            |grid| Self {
                first: grid.frame_at_beat(0),
                second: grid.frame_at_beat(1),
                bpm_bits: grid.average_bpm().to_bits(),
            },
        )
    }
}

/// Everything that determines the image content. When it is unchanged (paused, same zoom, same
/// track), the existing placement stays and no raster/encode work happens.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FrameKey {
    playhead_bits: u64,
    frames_per_cell_bits: u64,
    pixel_width: u32,
    pixel_height: u32,
    waveform: usize,
    grid: GridKey,
}

/// What a Kitty strip was rendered from. A change in any of these invalidates the image.
#[derive(Clone, Copy, PartialEq, Eq)]
struct StripKey {
    frames_per_cell_bits: u64,
    viewport_width: u32,
    viewport_height: u32,
    cell_width: u16,
    cell_height: u16,
    waveform: usize,
    grid: GridKey,
}

/// A wide, native-resolution image transmitted once. Frames show a source rectangle of it.
struct KittyStrip {
    key: StripKey,
    start_frame: f64,
    width: u32,
    /// Pixels of lookahead on each side; also the region in which the viewport may move before a
    /// rebuild is needed.
    margin: u32,
}

/// One deck's image state. Kitty rotates to a new image id on each strip rebuild; Sixel uses
/// `key` to decide whether the cached content is still current.
struct DeckImage {
    id: u32,
    placement: u32,
    last: Option<Instant>,
    /// Sixel content key (Kitty uses the strip state instead).
    key: Option<FrameKey>,
    /// Kitty's cached strip, if one has been transmitted.
    strip: Option<KittyStrip>,
    last_playhead: Option<u64>,
    /// The superseded image is deleted on the next placement, after the replacement has had one
    /// frame to become visible.
    pending_delete: Option<u32>,
}

/// The renderer selected at startup plus per-deck image bookkeeping.
pub struct WaveformView {
    settings: WaveformSettings,
    backend: Backend,
    cell: CellPx,
    decks: Vec<DeckImage>,
}

impl WaveformView {
    /// A safe initial renderer, used before the terminal has reported what it supports.
    pub fn new(settings: WaveformSettings, decks: usize) -> Self {
        Self {
            settings,
            backend: Backend::Braille,
            cell: CellPx::default(),
            decks: (0..decks)
                .map(|_index| DeckImage {
                    id: next_image_id(),
                    placement: next_placement_id(),
                    last: None,
                    key: None,
                    strip: None,
                    last_playhead: None,
                    pending_delete: None,
                })
                .collect(),
        }
    }

    /// Resolves the preference against the environment and reads the terminal's cell size.
    pub fn detect(settings: WaveformSettings, decks: usize) -> (Self, Option<String>) {
        let mut view = Self::new(settings, decks);
        let (backend, note) = detect::resolve(settings.backend);
        view.backend = backend;
        view.cell = settings
            .cell
            .map(|(width, height)| CellPx { width, height })
            .unwrap_or_else(detect::cell_px);
        (view, note)
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Draws one deck. `frames_per_cell` is backend-independent; image backends derive their
    /// frames-per-pixel from it.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        frame: &mut Frame,
        deck: usize,
        area: Rect,
        waveform: Option<&Arc<Waveform>>,
        grid: Option<&BeatGrid>,
        playhead_frame: f64,
        frames_per_cell: f64,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        match self.backend {
            Backend::Braille => render_braille(
                frame,
                area,
                waveform,
                grid,
                playhead_frame,
                frames_per_cell,
                self.settings.colors,
            ),
            Backend::Kitty => {
                self.render_kitty(frame, deck, area, waveform, grid, playhead_frame, frames_per_cell)
            }
            Backend::Sixel => {
                self.render_sixel(frame, deck, area, waveform, grid, playhead_frame, frames_per_cell)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_kitty(
        &mut self,
        frame: &mut Frame,
        deck: usize,
        area: Rect,
        waveform: Option<&Arc<Waveform>>,
        grid: Option<&BeatGrid>,
        playhead_frame: f64,
        frames_per_cell: f64,
    ) {
        let Some(state) = self.decks.get_mut(deck) else {
            return;
        };
        let viewport_width = u32::from(area.width).saturating_mul(u32::from(self.cell.width));
        let viewport_height = u32::from(area.height).saturating_mul(u32::from(self.cell.height));
        if viewport_width == 0 || viewport_height == 0 {
            render_braille(
                frame,
                area,
                waveform,
                grid,
                playhead_frame,
                frames_per_cell,
                self.settings.colors,
            );
            return;
        }
        let frames_per_pixel = (frames_per_cell / f64::from(self.cell.width)).max(1.0);
        let strip_key = StripKey {
            frames_per_cell_bits: frames_per_cell.to_bits(),
            viewport_width,
            viewport_height,
            cell_width: self.cell.width,
            cell_height: self.cell.height,
            waveform: waveform.map_or(0, |waveform| Arc::as_ptr(waveform) as usize),
            grid: GridKey::of(grid),
        };

        let mut rebuild = state
            .strip
            .as_ref()
            .is_none_or(|strip| strip.key != strip_key);
        let mut offset = 0u32;
        if !rebuild {
            let strip = state.strip.as_ref().expect("just checked");
            let raw = (playhead_frame.max(0.0) - strip.start_frame) / frames_per_pixel
                - f64::from(viewport_width) / 2.0;
            let max_offset = f64::from(strip.width.saturating_sub(viewport_width));
            let safe = strip.margin / 4;
            if raw < 0.0
                || raw > max_offset
                || raw < f64::from(safe)
                || raw > max_offset - f64::from(safe)
            {
                rebuild = true;
            } else {
                offset = raw.round() as u32;
            }
        }

        let playhead_bits = playhead_frame.max(0.0).to_bits();
        let hz = self.settings.refresh_hz.max(1);
        let interval = Duration::from_secs_f64(1.0 / f64::from(hz));
        let tolerance = Duration::from_millis(8);
        let due = state.pending_delete.is_some()
            || state
                .last
                .is_none_or(|last| last.elapsed() >= interval.saturating_sub(tolerance));
        // A paused deck does not need a placement every tick. Re-place once a second anyway, so a
        // terminal/modal that dropped the image heals without waiting for playback.
        let static_frame = state.pending_delete.is_none()
            && state.last_playhead == Some(playhead_bits)
            && state
                .last
                .is_some_and(|last| last.elapsed() < IMAGE_REFRESH_HEAL);
        if !rebuild && (!due || static_frame) {
            skip_area(frame, area);
            draw_playhead_overlay(frame, area);
            return;
        }

        let palette = raster::Palette::from_colors(self.settings.colors);
        let sequence = if rebuild {
            // One viewport of lookahead on each side: at the default zoom that is several seconds
            // of playback between transmissions, while the placement itself stays 1:1.
            let margin = viewport_width.max(64);
            let strip_width = viewport_width.saturating_add(margin.saturating_mul(2));
            let start_frame = playhead_frame.max(0.0)
                - (f64::from(margin) + f64::from(viewport_width) / 2.0) * frames_per_pixel;
            let virtual_playhead = start_frame + f64::from(strip_width) / 2.0 * frames_per_pixel;
            let image = raster::rasterize(
                waveform.map(AsRef::as_ref),
                grid,
                virtual_playhead,
                frames_per_pixel,
                strip_width,
                viewport_height,
                false,
            );
            let source = kitty::SourceRect {
                x: margin,
                y: 0,
                width: viewport_width,
                height: viewport_height,
            };
            // Transmit under a fresh id first, then place it, then delete the old image. Replacing
            // an image in place would blank the deck while this payload is still arriving.
            let old_id = state.strip.is_some().then_some(state.id);
            let new_id = next_image_id();
            // A fresh placement id means the old placement is not replaced while the new image is
            // still being decoded; the old image is deleted one frame later. IDs are global so two
            // decks can never collide.
            state.placement = next_placement_id();
            let mut sequence = String::new();
            if old_id.is_none() {
                clear_cells(&mut sequence, area);
            }
            sequence.push_str(&kitty::transmit(&image, &palette, new_id));
            sequence.push_str(&kitty::place(
                new_id,
                state.placement,
                area,
                source,
            ));
            // Free the image from the *previous* rebuild now; keep the immediately superseded one
            // for one more frame so the terminal has definitely rendered the replacement.
            if let Some(old) = state.pending_delete.take() {
                sequence.push_str(&kitty::delete(old));
            }
            state.pending_delete = old_id;
            state.id = new_id;
            state.strip = Some(KittyStrip {
                key: strip_key,
                start_frame,
                width: strip_width,
                margin,
            });
            sequence
        } else {
            let strip = state.strip.as_ref().expect("a strip exists when not rebuilding");
            let max_offset = strip.width.saturating_sub(viewport_width);
            let offset = offset.min(max_offset);
            let mut sequence = kitty::place(
                state.id,
                state.placement,
                area,
                kitty::SourceRect {
                    x: offset,
                    y: 0,
                    width: viewport_width,
                    height: viewport_height,
                },
            );
            if let Some(old) = state.pending_delete.take() {
                sequence.push_str(&kitty::delete(old));
            }
            sequence
        };

        skip_area(frame, area);
        set_image_cell(frame, area, &sequence);
        draw_playhead_overlay(frame, area);
        state.last = Some(Instant::now());
        state.last_playhead = Some(playhead_bits);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_sixel(
        &mut self,
        frame: &mut Frame,
        deck: usize,
        area: Rect,
        waveform: Option<&Arc<Waveform>>,
        grid: Option<&BeatGrid>,
        playhead_frame: f64,
        frames_per_cell: f64,
    ) {
        let Some(state) = self.decks.get_mut(deck) else {
            return;
        };
        let pixel_width = u32::from(area.width).saturating_mul(u32::from(self.cell.width));
        let pixel_height = u32::from(area.height).saturating_mul(u32::from(self.cell.height));
        let key = FrameKey {
            playhead_bits: playhead_frame.max(0.0).to_bits(),
            frames_per_cell_bits: frames_per_cell.to_bits(),
            pixel_width,
            pixel_height,
            waveform: waveform.map_or(0, |waveform| Arc::as_ptr(waveform) as usize),
            grid: GridKey::of(grid),
        };
        if state.key == Some(key)
            && state
                .last
                .is_some_and(|last| last.elapsed() < IMAGE_REFRESH_HEAL)
        {
            skip_area(frame, area);
            return;
        }
        let hz = self.settings.refresh_hz.clamp(1, SIXEL_MAX_HZ);
        let interval = Duration::from_secs_f64(1.0 / f64::from(hz));
        let tolerance = Duration::from_millis(8);
        let due = state
            .last
            .is_none_or(|last| last.elapsed() >= interval.saturating_sub(tolerance));
        if !due {
            skip_area(frame, area);
            return;
        }
        if pixel_width == 0
            || pixel_height == 0
            || pixel_width.saturating_mul(pixel_height) > MAX_IMAGE_PIXELS
        {
            render_braille(
                frame,
                area,
                waveform,
                grid,
                playhead_frame,
                frames_per_cell,
                self.settings.colors,
            );
            return;
        }

        let frames_per_pixel = (frames_per_cell / f64::from(self.cell.width)).max(1.0);
        let palette = raster::Palette::from_colors(self.settings.colors);
        let image = raster::rasterize(
            waveform.map(AsRef::as_ref),
            grid,
            playhead_frame,
            frames_per_pixel,
            pixel_width,
            pixel_height,
            true,
        );
        let sequence = sixel::sequence(&image, &palette, area);
        skip_area(frame, area);
        set_image_cell(frame, area, &sequence);
        state.last = Some(Instant::now());
        state.key = Some(key);
    }
}

/// Erase the character cells of `area` without moving the final cursor away from its origin. Shared
/// by both image transports; only the first image uses it, because clearing later would blink.
fn clear_cells(out: &mut String, area: Rect) {
    out.push_str("\x1b[0m\x1b7");
    for row in 0..area.height {
        write!(
            out,
            "\x1b[{};{}H\x1b[{}X",
            area.y + row + 1,
            area.x + 1,
            area.width
        )
        .expect("writing to a String cannot fail");
    }
    out.push_str("\x1b8");
}

/// Marks the whole image rectangle as owned by the terminal-side image. The first cell is
/// overridden with the actual escape sequence; the rest must not be rewritten, or ratatui would
/// erase the image.
fn skip_area(frame: &mut Frame, area: Rect) {
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol(" ").set_diff_option(CellDiffOption::Skip);
            }
        }
    }
}

fn set_image_cell(frame: &mut Frame, area: Rect, sequence: &str) {
    if let Some(cell) = frame.buffer_mut().cell_mut((area.x, area.y)) {
        cell.set_symbol(sequence)
            .set_fg(Color::Reset)
            .set_bg(Color::Reset)
            .set_diff_option(UNIT_WIDTH);
    }
}

/// Kitty places images below text, so the playhead can stay a normal braille-style cell and move
/// at the UI tick without re-transmitting the waveform.
fn draw_playhead_overlay(frame: &mut Frame, area: Rect) {
    let x = area.x + area.width / 2;
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        if let Some(cell) = buffer.cell_mut((x, y)) {
            cell.set_symbol("│")
                .set_fg(PLAYHEAD_COLOR)
                .set_bg(Color::Reset)
                .set_diff_option(CellDiffOption::AlwaysUpdate);
        }
    }
}

fn render_braille(
    frame: &mut Frame,
    area: Rect,
    waveform: Option<&Arc<Waveform>>,
    grid: Option<&BeatGrid>,
    playhead_frame: f64,
    frames_per_cell: f64,
    colors: WaveformColors,
) {
    let dots_x = f64::from(area.width) * 2.0;
    let dots_y = f64::from(area.height) * 4.0;
    let center_x = (dots_x - 1.0) / 2.0;
    let center_y = (dots_y - 1.0) / 2.0;
    let playhead = playhead_frame.max(0.0);
    // A braille cell has two horizontal dots, so a cell-sized zoom maps to half as many frames per
    // dot. The pyramid sees the same unit the user chose.
    let frames_per_dot = (frames_per_cell / 2.0).max(1.0);

    let canvas = Canvas::default()
        .x_bounds([0.0, dots_x])
        .y_bounds([0.0, dots_y])
        .marker(Marker::Braille)
        .paint(|ctx| {
            // Grid first, waveform over it, playhead last: each later shape should win.
            if let Some(grid) = grid {
                draw_beats(
                    ctx,
                    grid,
                    playhead,
                    frames_per_dot,
                    center_x,
                    dots_x,
                    dots_y,
                );
            }
            ctx.draw(&CanvasLine::new(
                0.0,
                center_y,
                dots_x,
                center_y,
                CENTER_COLOR,
            ));
            if let Some(waveform) = waveform {
                draw_stereo(
                    ctx,
                    waveform,
                    playhead,
                    frames_per_dot,
                    center_x,
                    center_y,
                    dots_x,
                    dots_y,
                    colors,
                );
            }
            ctx.draw(&CanvasLine::new(
                center_x,
                0.0,
                center_x,
                dots_y,
                PLAYHEAD_COLOR,
            ));
        });
    frame.render_widget(canvas, area);
}

/// Per braille column, the left channel above the centre and the right channel below it. Each band
/// is drawn from the centre outward; the last band wins the dots near the centre.
#[allow(clippy::too_many_arguments)]
fn draw_stereo(
    ctx: &mut Context,
    waveform: &Waveform,
    playhead: f64,
    frames_per_dot: f64,
    center_x: f64,
    center_y: f64,
    dots_x: f64,
    dots_y: f64,
    colors: WaveformColors,
) {
    let start_frame = playhead - center_x * frames_per_dot;
    let up = center_y;
    let down = dots_y - center_y;
    for (index, peaks) in waveform
        .columns(start_frame, frames_per_dot, dots_x as usize)
        .enumerate()
    {
        let x = index as f64;
        draw_channel(ctx, x, center_y, up, true, peaks.left, colors);
        draw_channel(ctx, x, center_y, down, false, peaks.right, colors);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_channel(
    ctx: &mut Context,
    x: f64,
    center_y: f64,
    extent: f64,
    upward: bool,
    peaks: hypermixx_library::BandPeaks,
    colors: WaveformColors,
) {
    let low = f64::from(peaks.low) / 255.0;
    let mid = f64::from(peaks.mid) / 255.0;
    let high = f64::from(peaks.high) / 255.0;
    segment_line(ctx, x, center_y, extent, upward, low, colors.low);
    segment_line(ctx, x, center_y, extent, upward, mid, colors.mid);
    segment_line(ctx, x, center_y, extent, upward, high, colors.high);
}

fn segment_line(
    ctx: &mut Context,
    x: f64,
    base: f64,
    extent: f64,
    upward: bool,
    amplitude: f64,
    color: Color,
) {
    let length = amplitude.clamp(0.0, 1.0) * extent;
    if length <= 0.0 {
        return;
    }
    let end = if upward { base + length } else { base - length };
    ctx.draw(&CanvasLine::new(x, base, x, end, color));
}

#[allow(clippy::too_many_arguments)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preference::{WaveformBackend, WaveformColors};
    use hypermixx_library::{BandPeaks, StereoPeaks};

    fn settings(backend: WaveformBackend) -> WaveformSettings {
        WaveformSettings {
            backend,
            refresh_hz: 60,
            cell: None,
            low_hz: 200,
            high_hz: 4_000,
            colors: WaveformColors::default(),
        }
    }

    #[test]
    fn explicit_braille_never_detects_an_image_backend() {
        let (view, note) = WaveformView::detect(settings(WaveformBackend::Braille), 2);
        assert_eq!(view.backend(), Backend::Braille);
        assert!(note.is_none());
    }

    #[test]
    fn a_new_view_owns_one_image_id_per_deck() {
        let view = WaveformView::new(settings(WaveformBackend::Auto), 2);
        assert_eq!(view.decks.len(), 2);
        assert_ne!(view.decks[0].id, view.decks[1].id);
    }

    #[test]
    fn the_kitty_path_puts_the_image_in_the_first_cell_and_skips_the_rest() {
        let mut view = WaveformView::new(settings(WaveformBackend::Kitty), 1);
        view.backend = Backend::Kitty;
        view.cell = CellPx {
            width: 8,
            height: 16,
        };
        let waveform = Waveform::from_buckets(
            vec![StereoPeaks {
                left: BandPeaks {
                    low: 255,
                    mid: 128,
                    high: 64,
                },
                right: BandPeaks {
                    low: 64,
                    mid: 32,
                    high: 16,
                },
            }],
            256,
            256,
        );
        let backend = ratatui::backend::TestBackend::new(40, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                view.render(
                    frame,
                    0,
                    Rect::new(0, 0, 40, 4),
                    Some(&Arc::new(waveform)),
                    None,
                    128.0,
                    2048.0,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let image = buffer.cell((0, 0)).expect("image cell");
        assert!(image.symbol().contains("a=t"), "{}", image.symbol());
        assert!(image.symbol().contains("a=p"), "{}", image.symbol());
        assert!(image.symbol().contains("f=32"));
        let playhead = buffer.cell((20, 0)).expect("playhead cell");
        assert_eq!(playhead.symbol(), "│");
    }

    #[test]
    fn a_kitty_rebuild_transmits_before_replacing_the_old_image() {
        let mut view = WaveformView::new(settings(WaveformBackend::Kitty), 1);
        view.backend = Backend::Kitty;
        view.cell = CellPx {
            width: 8,
            height: 16,
        };
        let backend = ratatui::backend::TestBackend::new(40, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut draw = |terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>, frames_per_cell| {
            terminal
                .draw(|frame| {
                    view.render(
                        frame,
                        0,
                        Rect::new(0, 0, 40, 4),
                        None,
                        None,
                        0.0,
                        frames_per_cell,
                    );
                })
                .unwrap();
            terminal.backend().buffer().cell((0, 0)).unwrap().symbol().to_owned()
        };
        let first = draw(&mut terminal, 2048.0);
        assert!(first.contains("a=t") && first.contains("a=p"));
        assert!(first.starts_with("\x1b[0m\x1b7"), "first image clears text");
        let second = draw(&mut terminal, 1024.0);
        assert!(second.contains("a=t") && second.contains("a=p"));
        assert!(
            !second.contains("a=d,d=I"),
            "the superseded image survives at least the rebuild frame: {second}"
        );
        assert!(
            !second.starts_with("\x1b[0m\x1b7"),
            "a rebuild must not clear the deck before the new image arrives: {second}"
        );
        let transmit = second.find("a=t").unwrap();
        let place = second.find("a=p").unwrap();
        assert!(transmit < place, "transmit → place");
        let third = draw(&mut terminal, 1024.0);
        assert!(third.contains("a=p") && third.contains("a=d,d=I"));
        let place = third.find("a=p").unwrap();
        let delete = third.find("a=d").unwrap();
        assert!(place < delete, "the old image is deleted after the new placement");
    }

    #[test]
    fn the_sixel_path_puts_a_sixel_payload_in_the_first_cell() {
        let mut view = WaveformView::new(settings(WaveformBackend::Sixel), 1);
        view.backend = Backend::Sixel;
        view.cell = CellPx {
            width: 8,
            height: 16,
        };
        let backend = ratatui::backend::TestBackend::new(40, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                view.render(
                    frame,
                    0,
                    Rect::new(0, 0, 40, 4),
                    None,
                    None,
                    0.0,
                    2048.0,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let image = buffer.cell((0, 0)).expect("image cell");
        assert!(image.symbol().contains("\x1bP0;0;0q"), "{}", image.symbol());
        assert!(image.symbol().contains("\"1;1;320;64"));
    }

    /// Rough per-frame cost of the two image paths. Ignored by default; run with
    /// `cargo test -p hypermixx-cli image_frame_cost_probe -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn image_frame_cost_probe() {
        let buckets = (0..4096)
            .map(|index| StereoPeaks {
                left: BandPeaks {
                    low: (index % 251) as u8,
                    mid: (index % 127) as u8,
                    high: (index % 61) as u8,
                },
                right: BandPeaks {
                    low: (index % 239) as u8,
                    mid: (index % 113) as u8,
                    high: (index % 53) as u8,
                },
            })
            .collect();
        let waveform = Waveform::from_buckets(buckets, 256, 4096 * 256);
        let palette = raster::Palette::from_colors(WaveformColors::default());
        let rounds = 60;

        let source = kitty::SourceRect {
            x: 944,
            y: 0,
            width: 944,
            height: 112,
        };
        let start = Instant::now();
        let mut raster_ms = 0.0f64;
        let mut encode_ms = 0.0f64;
        for _ in 0..rounds.min(6) {
            let t0 = Instant::now();
            let image = raster::rasterize(Some(&waveform), None, 128.0, 2048.0 / 8.0, 2832, 112, false);
            raster_ms += t0.elapsed().as_secs_f64() * 1000.0;
            let t1 = Instant::now();
            let _ = kitty::transmit(&image, &palette, 1);
            encode_ms += t1.elapsed().as_secs_f64() * 1000.0;
        }
        println!(
            "kitty strip rebuild: {:.3} ms (raster {:.3} + transmit {:.3})",
            start.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds.min(6)),
            raster_ms / f64::from(rounds.min(6)),
            encode_ms / f64::from(rounds.min(6))
        );
        let start = Instant::now();
        for _ in 0..rounds {
            let _ = kitty::place(1, 1, Rect::new(0, 0, 118, 7), source);
        }
        println!(
            "kitty placement: {:.3} ms/frame",
            start.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds)
        );

        let start = Instant::now();
        for _ in 0..rounds {
            let image = raster::rasterize(Some(&waveform), None, 128.0, 2048.0 / 8.0, 944, 112, true);
            let _ = sixel::sequence(&image, &palette, Rect::new(0, 0, 118, 7));
        }
        println!("sixel: {:.3} ms/frame", start.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds));
    }

    #[test]
    fn the_rasteriser_keeps_channels_on_their_own_side() {
        let left = Waveform::from_buckets(
            vec![StereoPeaks {
                left: BandPeaks {
                    low: 255,
                    mid: 255,
                    high: 255,
                },
                right: BandPeaks::ZERO,
            }],
            256,
            256,
        );
        let image = raster::rasterize(Some(&left), None, 128.0, 1.0, 21, 13, false);
        let center = image.height / 2;
        assert!((0..center).any(|y| {
            (0..image.width)
                .any(|x| image.indices[(y * image.width + x) as usize] != raster::BG)
        }));
        assert!(!(center + 1..image.height).any(|y| {
            (0..image.width)
                .any(|x| image.indices[(y * image.width + x) as usize] != raster::BG)
        }));
    }
}
