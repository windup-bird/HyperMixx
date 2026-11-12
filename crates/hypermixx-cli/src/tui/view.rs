//! Everything that draws the TUI.
//!
//! The screen is a stack of bands, top to bottom:
//!
//! ```text
//! header                                                    (1 row)
//! deck row 1: deck info for decks 1..=2 · track overview     (4 or 5 rows)
//!             waveform, one per deck on that page            (2 or 3 rows each)
//! deck row 2: the same for decks 3..=4 (only when there are more than two decks)
//! 1trans | 1pads/1fx | 1eq/1beatctl | faders | 2trans | 2pads/2fx | 2eq/2beatctl
//! browser : command                                         (fills the rest)
//! ```
//!
//! The deck area always stacks *every* page of decks, so four decks show four waveforms; only the
//! control band below it picks its pair, and each trans column's toggle picks its own: the left
//! column cycles deck1 ⇄ deck3, the right one deck2 ⇄ deck4. That keeps the tracks in view — a DJ
//! reads all four — while the pads, knobs and transport, which need a wide column each, stay at
//! their designed size.
//!
//! The control band is the part that needs room, so it is drawn only when the terminal is large
//! enough to keep the browser usable; on a small screen the whole band disappears and the interface
//! falls back to the browser plus the decks, which is still a working library and transport. The
//! same rule the plan states as "小屏把 pads/trans 那一行全隐藏".
//!
//! Every clickable region is recorded in [`App::hits`] while drawing and consumed by the mouse
//! handler. The keyboard stays with the command line: a pointer is the only way to reach a pad.

use std::sync::Arc;

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use hypermixx_core::{BeatGrid, DeckState, FxSlotStatus, Key, Stem};

use crate::preference::Theme;
use crate::response::{bpm_label, loop_length, sync_label, time, Level, LogLine};
use crate::tui::app::{
    App, BeatMode, EqPanel, Hit, Knob, LoopHit, Overlay, PadMode, PadPanel, Target, TransportHit,
    COMPLETION_ROWS, DECKS_PER_PAGE,
};
use crate::tui::text::{center_to_width, pad_to_width, truncate_to_width, width};
use crate::tui::{browser, overview, popup, settings, text};

/// The header is always one line.
const HEADER_H: u16 = 1;
/// Deck info: a border, the status row, and a two-row overview.
const INFO_H: u16 = 5;
/// Deck info when the deck area is stacked (more than one page): a border, the status row and a
/// single overview row. The stacked pages pay for the info row twice, so each one gives up its
/// second overview row rather than pushing the browser off the screen.
const INFO_H_STACK: u16 = 4;
/// Height of the control band, borders included: the panel content plus the mode row, the group
/// switch row, and the borders.
const CONTROL_H: u16 = 10;
/// The control band when the deck area is stacked: the two pads rows keep their labels but lose the
/// third row of each pad, which pays for the second info row above.
const CONTROL_H_STACK: u16 = 8;
/// The borderless transport column. Four buttons stacked vertically, with air between them.
const TRANS_W: u16 = 8;
/// The fader / volume-bar / crossfader box in the middle of the band: two channel faders and the
/// output meter, with the crossfader in the bottom row inside the same frame. The width is what
/// keeps a column of air between each fader and the meter, and two columns between the meter pair.
const CENTER_W: u16 = 14;
/// Width of one channel fader inside that box: two columns, which is the pixel thickness of the
/// crossfader's single row (a cell is about twice as tall as it is wide), so the two faders and the
/// crossfader read as the same instrument.
const FADER_W: u16 = 2;
/// Breathing room inside the box. Two columns here read the same size as the one blank row that
/// separates the faders from the crossfader (a cell is about twice as tall as it is wide), so the
/// horizontal and vertical gaps look alike.
const GAP: u16 = 2;
/// Width of one meter column inside that box. The two peaks are separate slots, one on each side of
/// the frame's middle, so neither of them can straddle the centre.
const METER_W: u16 = 1;
/// The browser/command band keeps at least this many rows before anything else may grow.
const MIN_BOTTOM_H: u16 = 6;
/// Below this width the control band gives each deck less than ~19 cells per group, which is not
/// enough for a four-column pad grid *and* a readable mixer column.
const MIN_CONTROL_WIDTH: u16 = 112;
/// Below this height the band would eat the browser, so it is dropped entirely.
const MIN_CONTROL_HEIGHT: u16 = 26;
/// Upper bound on the cover's width. The actual width comes from the band's height and the terminal's
/// cell aspect (a square in pixels), so a taller cell simply gets a wider cover.
const COVER_W: u16 = 10;

/// One page of decks: the info row above the waveforms of the decks it holds.
struct PageBand {
    info: Rect,
    wave: Rect,
}

/// The vertical bands of one frame.
struct Bands {
    header: Rect,
    /// Top to bottom: the first page's info row and waveforms, then the second page's, … one page
    /// per [`DECKS_PER_PAGE`] decks.
    pages: Vec<PageBand>,
    controls: Option<Rect>,
    bottom: Rect,
}

fn bands(area: Rect, decks: usize) -> Bands {
    let decks = decks.max(1);
    // More than one page means the deck area is stacked: every page is drawn, one under the other.
    let stacked = decks > DECKS_PER_PAGE;
    let pages = decks.div_ceil(DECKS_PER_PAGE);
    // Waveform rows per deck: three on a tall terminal (the waveform is what a DJ actually reads),
    // two when height is tight. A stacked deck area starts a row lower on the budget, so it only
    // takes the third row when there is real room for it.
    let per_deck = match (stacked, area.height) {
        (false, height) if height >= 34 => 3,
        (true, height) if height >= 40 => 3,
        _ => 2,
    };
    let info_h = if stacked { INFO_H_STACK } else { INFO_H };
    let want_control = if stacked { CONTROL_H_STACK } else { CONTROL_H };

    let mut y = area.top();
    let header = Rect::new(area.x, y, area.width, HEADER_H.min(area.height));
    y += header.height;

    // Waveforms first (they are what a DJ actually reads), then the info rows, then the control
    // band; the browser keeps its floor. Each page then takes its share in that order, so a short
    // terminal loses the later pages before it loses the first one.
    let mut available = area.bottom().saturating_sub(y).saturating_sub(MIN_BOTTOM_H);
    let mut wave_budget = (per_deck * decks as u16).min(available);
    available -= wave_budget;
    let mut info_budget = (info_h * pages as u16).min(available);
    available -= info_budget;
    // All or nothing: half a control band is worse than none.
    let controls_h = if area.width >= MIN_CONTROL_WIDTH
        && area.height >= MIN_CONTROL_HEIGHT
        && available >= want_control
    {
        want_control
    } else {
        0
    };

    let mut page_bands = Vec::with_capacity(pages);
    for page in 0..pages {
        let on_page = (decks - page * DECKS_PER_PAGE).min(DECKS_PER_PAGE) as u16;
        let info = info_h.min(info_budget);
        info_budget -= info;
        let wave = (per_deck * on_page).min(wave_budget);
        wave_budget -= wave;
        page_bands.push(PageBand {
            info: Rect::new(area.x, y, area.width, info),
            wave: Rect::new(area.x, y + info, area.width, wave),
        });
        y += info + wave;
    }
    let controls = (controls_h > 0).then(|| Rect::new(area.x, y, area.width, controls_h));
    y += controls_h;
    let bottom = Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y));
    Bands {
        header,
        pages: page_bands,
        controls,
        bottom,
    }
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.screen = area;
    let bands = bands(area, app.decks);
    let mut hits: Vec<(Rect, Hit)> = Vec::new();

    // A modal owns the screen. Kitty images live *under* the text layer, so redrawing cells does
    // not remove them: the moment one opens, every image is deleted in the terminal and the deck is
    // told it holds nothing. Braille is drawn while it is open, and the next real frame transmits
    // again — which is also what removes the stale image after the popup closes.
    let suppress_images = app.overlay.is_some();
    if suppress_images != app.images_hidden {
        app.images_hidden = suppress_images;
        if suppress_images {
            hide_images(app);
        }
    }

    render_top(frame, bands.header, app, &mut hits);
    for (page, deck_band) in bands.pages.iter().enumerate() {
        let first = page * DECKS_PER_PAGE;
        let count = (app.decks - first).min(DECKS_PER_PAGE);
        if deck_band.info.height > 0 {
            let columns =
                Layout::horizontal(vec![Constraint::Fill(1); count]).split(deck_band.info);
            for (slot, column) in columns.iter().enumerate() {
                render_deck_info(frame, *column, app, first + slot, suppress_images);
            }
        }
        if deck_band.wave.height > 0 {
            let rows = Layout::vertical(vec![Constraint::Fill(1); count]).split(deck_band.wave);
            for (slot, row) in rows.iter().enumerate() {
                render_waveform(frame, *row, app, first + slot, suppress_images);
            }
        }
    }
    if let Some(controls) = bands.controls {
        render_controls(frame, controls, app, &mut hits);
    }
    app.hits = hits;

    render_bottom(frame, bands.bottom, app);
    render_completion(frame, bands.bottom, app);

    // A modal draws over everything, including the completion popup.
    if let Some(overlay) = &app.overlay {
        let theme = app.theme;
        match overlay {
            Overlay::Ports { list } => popup::render(
                frame,
                frame.area(),
                &theme,
                &list.view("controller › input"),
            ),
            Overlay::Files { picker, .. } => {
                popup::render(frame, frame.area(), &theme, &picker.view())
            }
            Overlay::LoadTarget { x, y, selected } => {
                render_load_target(frame, frame.area(), *x, *y, *selected, app.decks)
            }
            Overlay::Settings(screen) => settings::render(frame, frame.area(), screen, &theme),
        }
    }
}

/// Deletes every Kitty image and forgets the transmitted bitmaps, so the deck re-sends them after
/// the modal closes. Written straight to stdout: it has to be gone *before* the popup's cells land,
/// and ratatui only diffs cells.
fn hide_images(app: &mut App) {
    use std::io::Write;
    if app.waveform_view.backend() != crate::tui::waveform_view::Backend::Kitty {
        // Braille owns its cells: there is nothing in the terminal to delete.
        return;
    }
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(crate::tui::waveform_view::kitty::delete_all().as_bytes());
    let _ = stdout.flush();
    app.waveform_view.forget_images();
    for deck in app.deck_images.iter_mut() {
        deck.reset();
    }
}

/// A clickable area, registered in draw order.
fn hit(hits: &mut Vec<(Rect, Hit)>, area: Rect, hit: Hit) {
    if area.width > 0 && area.height > 0 {
        hits.push((area, hit));
    }
}

/// Rows of `area`, top to bottom, clamped to what actually fits.
fn line(area: Rect, row: u16, column: u16, width: u16) -> Rect {
    let y = area.y.saturating_add(row);
    if y >= area.bottom() || column >= area.width {
        return Rect::new(area.x + area.width, area.y, 0, 0);
    }
    Rect::new(area.x + column, y, width.min(area.width - column), 1)
}

// ---- header -----------------------------------------------------------------

/// Gain reduction (dB) at which the header's `out` field turns red. Below this the limiter is
/// only trimming rounding noise, not holding the signal down.
const LIMITING_DB: f32 = -0.5;

fn render_top(frame: &mut Frame, area: Rect, app: &App, hits: &mut Vec<(Rect, Hit)>) {
    if area.height == 0 {
        return;
    }
    let (peak, cue, _) = app.peaks();
    let overruns = app.meters.as_ref().map_or(0, |meters| meters.overruns);
    // A transient can be over the ceiling for a block without an audible reduction; half a dB is
    // the point where the limiter is actually holding the signal down.
    let limiting = app
        .meters
        .as_ref()
        .is_some_and(|meters| meters.master_reduction_db < LIMITING_DB);
    let ring_ms = app.ring_frames as f64 * 1000.0 / f64::from(app.sample_rate);
    let theme = app.theme;
    let out_color = if limiting { theme.error } else { theme.text };

    let badge = Span::styled(
        format!(" HYPERMIXX {} ", app.version),
        Style::default()
            .fg(Color::Black)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD),
    );
    // Most useful first: the tail is what gets dropped when the terminal is narrow, and a meter
    // the user cannot see is worth less than the settings button.
    let fields = [
        (format!(" {}Hz ", app.sample_rate), theme.text),
        (format!(" out {} ", db(peak)), out_color),
        (format!(" cue {} ", db(cue)), theme.text),
        (format!(" drop {overruns} "), theme.text),
        (format!(" cpu {} ", cpu_label(app.process.cpu_percent())), theme.text),
        (format!(" mem {} ", mem_label(app.process.memory_bytes())), theme.text),
        (
            format!(" ui {:.1}ms {:.0}fps ", app.ui_draw_ms, app.ui_fps),
            theme.text,
        ),
        (format!(" ring {ring_ms:.0}ms "), theme.text),
    ];
    let button = " ⚙ settings ";
    let button_w = width(button) as u16;
    let show_button = area.width >= button_w + 24;
    let text_w = if show_button {
        area.width.saturating_sub(button_w)
    } else {
        area.width
    };

    let mut spans = vec![badge];
    let mut remaining = (text_w as usize).saturating_sub(width(" HYPERMIXX 0.1.0 "));
    for (field, color) in fields {
        let field_width = width(&field);
        if field_width > remaining {
            break;
        }
        spans.push(Span::styled(field, Style::default().fg(color)));
        remaining -= field_width;
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(area.x, area.y, text_w, 1),
    );

    if show_button {
        let rect = Rect::new(area.x + area.width - button_w, area.y, button_w, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                button,
                Style::default()
                    .fg(Color::Black)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )))
            .alignment(Alignment::Right),
            rect,
        );
        hit(hits, rect, Hit::Settings);
    }
}

fn cpu_label(cpu: Option<f32>) -> String {
    cpu.map_or_else(|| "--".to_owned(), |percent| format!("{percent:.1}%"))
}

fn mem_label(bytes: Option<u64>) -> String {
    match bytes {
        None => "--".to_owned(),
        Some(bytes) if bytes >= 1 << 30 => {
            format!("{:.2}G", bytes as f64 / f64::from(1 << 30))
        }
        Some(bytes) => format!("{:.0}M", bytes as f64 / f64::from(1 << 20)),
    }
}

fn db(peak: f32) -> String {
    if peak <= 0.0 {
        "-inf dB".to_owned()
    } else {
        format!("{:+.1} dB", 20.0 * peak.log10())
    }
}

// ---- deck info --------------------------------------------------------------

/// Title, status and track overview for one deck, in a boxed band.
///
/// The box matches the control panels below it (same border, same accent when the deck owns the
/// command line), which is what makes the four bands read as one instrument rather than as loose
/// strips.
fn render_deck_info(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    deck: usize,
    suppress_images: bool,
) {
    if area.width < 8 || area.height < 3 {
        return;
    }
    let theme = app.theme;
    let focused = app.target == Target::Deck(deck);
    // With stacked pages the command target can sit on the other page, so the decks the control
    // band below drives keep a white border: the screen always shows which pair the pads and
    // transport belong to, even while the command line speaks for a deck on the other page.
    let border = if focused {
        theme.accent
    } else if app.decks > DECKS_PER_PAGE && app.slots.contains(&deck) {
        theme.text
    } else {
        theme.dim
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            format!(
                " Deck{} · {} ",
                deck + 1,
                app.titles[deck].as_deref().unwrap_or("<no track>")
            ),
            Style::default()
                .fg(if focused { theme.accent } else { theme.text })
                .add_modifier(Modifier::BOLD),
        ));
    let body = block.inner(area);
    frame.render_widget(block, area);
    if body.width == 0 || body.height == 0 {
        return;
    }
    let cell = app.waveform_view.cell_px();
    let cover_w = if body.width > 24 && body.height >= 2 {
        // A square in pixels, as in `render_cover`.
        (u32::from(body.height) * u32::from(cell.height.max(1)) / u32::from(cell.width.max(1)))
            .clamp(1, u32::from(COVER_W)) as u16
    } else {
        0
    };
    if cover_w > 0 {
        render_cover(
            frame,
            Rect::new(body.x, body.y, cover_w, body.height),
            app,
            deck,
            suppress_images,
        );
    }
    let gap = u16::from(cover_w > 0);
    let text_area = Rect::new(
        body.x + cover_w + gap,
        body.y,
        body.width.saturating_sub(cover_w + gap),
        body.height,
    );
    if text_area.height == 0 {
        return;
    }
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).split(text_area);
    render_status(frame, rows[0], app, deck);
    render_overview(frame, rows[1], app, deck, suppress_images);
}

/// The deck status row: musical state on the left, `elapsed / -remaining` pinned right.
fn render_status(frame: &mut Frame, area: Rect, app: &App, deck: usize) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let clock = deck_clock(&app.states[deck], app.sample_rate);
    let clock_width = width(&clock).min(area.width as usize) as u16;
    let columns =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(clock_width)]).split(area);
    frame.render_widget(
        Paragraph::new(deck_status_line(app, deck, columns[0].width)),
        columns[0],
    );
    if clock_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                clock,
                Style::default()
                    .fg(app.theme.text)
                    .add_modifier(Modifier::BOLD),
            )))
            .alignment(Alignment::Right),
            columns[1],
        );
    }
}

/// `played / remaining`. Total duration is deliberately absent: the two numbers already imply it.
fn deck_clock(state: &DeckState, sample_rate: u32) -> String {
    let glyph = if state.total_frames == 0 {
        "·"
    } else if state.playing {
        "▶"
    } else {
        "‖"
    };
    if state.total_frames == 0 {
        return format!("{glyph} --:--.--- / --:--.--- ");
    }
    let elapsed = time(state.current_frame.min(state.total_frames), sample_rate);
    let remaining = time(
        state.total_frames.saturating_sub(state.current_frame),
        sample_rate,
    );
    format!("{glyph} {elapsed} / -{remaining} ")
}

/// Builds the left-hand status text, dropping the least important tail when the width runs out.
///
/// Priority is the join order: BPM, key, tempo/keylock, sync, loop, stems, beat phase. The clock
/// is rendered in its own right-aligned column and never competes for this space.
fn deck_status_line(app: &App, deck: usize, width: u16) -> Line<'static> {
    let state = &app.states[deck];
    let theme = app.theme;
    let mut parts: Vec<(usize, Vec<Span<'static>>)> = Vec::new();

    parts.push(part(vec![Span::raw(bpm_label(state))]));
    let key = state
        .key
        .as_deref()
        .and_then(Key::parse)
        .map(|key| key.format(app.key_format))
        .unwrap_or_else(|| "--".to_owned());
    parts.push(part(vec![Span::raw(key)]));
    parts.push(part(vec![Span::raw(format!(
        "tempo {:.3} (±{:.0}%)",
        state.tempo,
        state.tempo_range * 100.0
    ))]));
    parts.push(part(vec![Span::raw(format!(
        "keylock {}",
        state.keylock.label()
    ))]));
    let sync = sync_label(state);
    let sync_style = if sync == "sync off" {
        Style::default()
    } else {
        Style::default().fg(theme.accent)
    };
    parts.push(part(vec![Span::styled(sync, sync_style)]));
    if let Some(loop_text) = loop_badge(state, app.grids[deck].as_ref(), app.sample_rate) {
        parts.push(part(vec![Span::styled(
            loop_text,
            Style::default().fg(theme.accent),
        )]));
    }
    if state.stems.ready {
        let mut spans = Vec::new();
        for stem in Stem::ALL {
            let marker = if state.stems.is_soloed(stem) {
                'S'
            } else if state.stems.mute[stem.index()] {
                'M'
            } else {
                ' '
            };
            spans.push(Span::raw(format!(
                "{}{:+.1}{marker} ",
                stem.name().chars().next().unwrap().to_ascii_uppercase(),
                state.stems.level[stem.index()]
            )));
        }
        parts.push(part(spans));
    }
    if let Some(grid) = app.grids[deck].as_ref().filter(|grid| !grid.is_empty()) {
        let beat = grid.floor_beat(state.current_frame) as f64
            + f64::from(grid.phase(state.current_frame));
        parts.push(part(vec![Span::raw(format!("beat {beat:.1}"))]));
    }

    let mut spans = Vec::new();
    let mut used = 0usize;
    for (position, (part_width, part_spans)) in parts.into_iter().enumerate() {
        if position == 0 {
            spans.push(Span::raw(" "));
            used = 1;
        } else {
            if used + 2 + part_width > width as usize {
                break;
            }
            spans.push(Span::raw("  "));
            used += 2;
        }
        spans.extend(part_spans);
        used += part_width;
    }
    Line::from(spans)
}

fn part(spans: Vec<Span<'static>>) -> (usize, Vec<Span<'static>>) {
    let width = spans.iter().map(Span::width).sum();
    (width, spans)
}

fn loop_badge(state: &DeckState, grid: Option<&BeatGrid>, sample_rate: u32) -> Option<String> {
    let mut parts = Vec::new();
    if state.loop_range.is_some() {
        if let Some(length) = loop_length(state, grid, sample_rate) {
            parts.push(format!("loop {length}"));
        }
    }
    if let Some(armed) = state.loop_in_armed {
        parts.push(format!("in armed@{armed}"));
    }
    (!parts.is_empty()).then(|| parts.join("  "))
}

/// Draws the cover: a Kitty image when the waveform backend has graphics, otherwise half blocks.
///
/// The artwork is always drawn square (a cell is about twice as tall as it is wide, so a square is
/// twice as many columns as rows) and letterboxed inside that square when the image's own aspect
/// differs, so nothing is ever stretched.
fn render_cover(frame: &mut Frame, area: Rect, app: &mut App, deck: usize, suppress_images: bool) {
    if area.width == 0 || area.height < 2 {
        return;
    }
    let cell = app.waveform_view.cell_px();
    // A square in pixels: columns = rows × (cell height / cell width). The square is the whole slot,
    // so the artwork fills it edge to edge instead of sitting in a letterbox.
    let square_w = (u32::from(area.height) * u32::from(cell.height.max(1))
        / u32::from(cell.width.max(1)))
    .clamp(1, u32::from(area.width)) as u16;
    let square = Rect::new(area.x, area.y, square_w, area.height);

    let cover = app.covers[deck].clone();
    let image_backend = app.waveform_view.backend() == crate::tui::waveform_view::Backend::Kitty;
    if image_backend {
        if suppress_images {
            return;
        }
        match &cover {
            Some(art) => {
                let key = Arc::as_ptr(art) as usize as u64;
                let side = art.width.min(art.height).max(1);
                let (ox, oy) = ((art.width - side) / 2, (art.height - side) / 2);
                let source = art.rgba.clone();
                let (width, height) = (art.width, art.height);
                let slot = &mut app.deck_images[deck].cover;
                slot.draw(frame, square, key, || {
                    (
                        side,
                        side,
                        crop_square(&source, width, height, ox, oy, side),
                    )
                });
                return;
            }
            None => app.deck_images[deck].cover.reset(),
        }
    }

    let buffer = frame.buffer_mut();
    match cover.as_deref() {
        Some(art) => {
            // Centre crop to a square, then sample that square into the slot: the artwork keeps its
            // aspect ratio and reaches every edge, so nothing looks nudged up or to one side.
            let side = art.width.min(art.height).max(1);
            let (ox, oy) = ((art.width - side) / 2, (art.height - side) / 2);
            for row in 0..square.height {
                for column in 0..square.width {
                    let x = ox + u32::from(column) * side / u32::from(square.width.max(1));
                    let top = art.pixel(
                        x,
                        oy + u32::from(row) * 2 * side / u32::from(square.height.max(1) * 2),
                    );
                    let bottom = art.pixel(
                        x,
                        oy + (u32::from(row) * 2 + 1) * side / u32::from(square.height.max(1) * 2),
                    );
                    if let Some(cell) = buffer.cell_mut((square.x + column, square.y + row)) {
                        cell.set_symbol("▀")
                            .set_fg(Color::Rgb(top[0], top[1], top[2]))
                            .set_bg(Color::Rgb(bottom[0], bottom[1], bottom[2]));
                    }
                }
            }
        }
        None => {
            // No artwork: a plain chrome square marks where it would be.
            for row in 0..square.height {
                for column in 0..square.width {
                    if let Some(cell) = buffer.cell_mut((square.x + column, square.y + row)) {
                        cell.set_symbol(" ").set_bg(app.theme.dim);
                    }
                }
            }
        }
    }
}

/// The centred `side × side` crop of an RGBA image, for the Kitty path (which cannot crop at
/// placement time without a source rectangle).
fn crop_square(
    rgba: &[u8],
    width: u32,
    height: u32,
    offset_x: u32,
    offset_y: u32,
    side: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity((side * side * 4) as usize);
    for row in 0..side {
        let y = (offset_y + row).min(height.saturating_sub(1));
        let start = ((y * width + offset_x.min(width.saturating_sub(1))) * 4) as usize;
        let len = ((side * 4) as usize).min(rgba.len().saturating_sub(start));
        out.extend_from_slice(&rgba[start..start + len]);
        // A short row (a truncated decode) is padded rather than panicking.
        out.resize(out.len() + (side * 4) as usize - len, 0);
    }
    out
}

/// The whole-track overview: the same folded silhouette as braille, transmitted as an image when the
/// backend has graphics.
fn render_overview(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    deck: usize,
    suppress_images: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let image_backend = app.waveform_view.backend() == crate::tui::waveform_view::Backend::Kitty;
    if !image_backend {
        overview::render(
            frame,
            area,
            app.waveforms[deck].as_ref().map(AsRef::as_ref),
            app.grids[deck].as_ref(),
            &app.states[deck],
            app.waveform.colors,
            &app.theme,
        );
        // Braille repaints the old column with the waveform, so there is no stale glyph to clear.
        let column = overview::draw_playhead(frame, area, &app.states[deck], Color::Red, None, false);
        app.overview_playhead[deck] = column;
        return;
    }
    if suppress_images {
        return;
    }
    let Some(waveform) = app.waveforms[deck].clone() else {
        app.deck_images[deck].overview.reset();
        overview::render(
            frame,
            area,
            None,
            app.grids[deck].as_ref(),
            &app.states[deck],
            app.waveform.colors,
            &app.theme,
        );
        let previous = app.overview_playhead[deck].take();
        let column =
            overview::draw_playhead(frame, area, &app.states[deck], Color::Red, previous, false);
        app.overview_playhead[deck] = column;
        return;
    };
    let state = app.states[deck].clone();
    let grid = app.grids[deck].clone();
    let colors = app.waveform.colors;
    let theme = app.theme;
    let cell = app.waveform_view.cell_px();
    let width_px = u32::from(area.width) * u32::from(cell.width.max(1));
    let height_px = u32::from(area.height) * u32::from(cell.height.max(1));
    let key = fingerprint(&[
        Arc::as_ptr(&waveform) as usize as u64,
        state.total_frames,
        state.loop_range.map_or(0, |(start, _)| start),
        state.cue_frame,
        u64::from(width_px) << 32 | u64::from(height_px),
        color_key(colors.low) ^ color_key(colors.mid) << 8 ^ color_key(colors.high) << 16,
        color_key(theme.dim),
        grid.as_ref().map_or(0, |grid| grid.frame_at_beat(0)),
    ]);
    {
        let slot = &mut app.deck_images[deck].overview;
        slot.draw(frame, area, key, || {
            (
                width_px.max(1),
                height_px.max(1),
                overview::rgba(
                    Some(waveform.as_ref()),
                    grid.as_ref(),
                    &state,
                    colors,
                    width_px.max(1),
                    height_px.max(1),
                    &theme,
                ),
            )
        });
    }
    let previous = app.overview_playhead[deck].take();
    let column = overview::draw_playhead(frame, area, &state, Color::Red, previous, true);
    app.overview_playhead[deck] = column;
}

/// One colour as a number, for an image cache key. `Color` is not a field-less enum, so it cannot be
/// cast; its three channels are enough identity for a cache key.
fn color_key(color: Color) -> u64 {
    match color {
        Color::Rgb(r, g, b) => u64::from(r) << 16 | u64::from(g) << 8 | u64::from(b),
        _ => 1,
    }
}

/// A cheap content hash for an image slot's cache key. Not cryptographic: it only has to change when
/// the picture would.
fn fingerprint(parts: &[u64]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for part in parts {
        hash ^= *part;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---- waveform ---------------------------------------------------------------

fn render_waveform(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    deck: usize,
    suppress_images: bool,
) {
    if area.width < 2 || area.height < 1 {
        return;
    }
    app.wave_width_cells = area.width as usize;
    {
        // Split the borrow: the transport mutates its image state while the peaks/grid are read.
        let App {
            waveform_view,
            waveforms,
            grids,
            states,
            frames_per_cell,
            ..
        } = app;
        waveform_view.render(
            frame,
            deck,
            area,
            waveforms[deck].as_ref(),
            grids[deck].as_ref(),
            &states[deck],
            *frames_per_cell,
            suppress_images,
        );
    }
}

// ---- control band -----------------------------------------------------------

fn render_controls(frame: &mut Frame, area: Rect, app: &App, hits: &mut Vec<(Rect, Hit)>) {
    if app.decks == 0 {
        return;
    }
    // Literal order, repeated for the page's second deck: trans | pads/fx | eq/beatctl | faders |
    // the same again. Which decks those are is `App::page` — the header of each trans column says.
    let columns = Layout::horizontal([
        Constraint::Length(TRANS_W),
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Length(CENTER_W),
        Constraint::Length(TRANS_W),
        Constraint::Fill(1),
        Constraint::Fill(1),
    ])
    .split(area);

    // The two columns' decks are independent: each trans toggle cycles its own half only.
    let mut columns_decks = app.page_decks();
    let first = columns_decks.next().unwrap_or(0);
    let second = columns_decks.next();
    render_trans(frame, columns[0], app, first, 0, hits);
    render_pad_group(frame, columns[1], app, first, hits);
    render_eq_group(frame, columns[2], app, first, hits);
    render_center(frame, columns[3], app, hits);
    if let Some(second) = second {
        render_trans(frame, columns[4], app, second, 1, hits);
        render_pad_group(frame, columns[5], app, second, hits);
        render_eq_group(frame, columns[6], app, second, hits);
    }
}

/// The pads slot: the pad grid or the FX panel.
fn render_pad_group(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    deck: usize,
    hits: &mut Vec<(Rect, Hit)>,
) {
    match app.pad_panel.get(deck).copied().unwrap_or_default() {
        PadPanel::Pads => render_pads(frame, area, app, deck, hits),
        PadPanel::Fx => render_fx(frame, area, app, deck, hits),
    }
}

/// The eq slot: the five tone/trim knobs or the beat controls.
fn render_eq_group(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    deck: usize,
    hits: &mut Vec<(Rect, Hit)>,
) {
    match app.eq_panel.get(deck).copied().unwrap_or_default() {
        EqPanel::Eq => render_mixer(frame, area, app, deck, hits),
        EqPanel::Beat => render_beatctl(frame, area, app, deck, hits),
    }
}

/// The borderless transport column. Four buttons, vertically centred with a blank row between
/// them; no box, because the deck group already reads as one instrument. When there is more than one
/// page of decks it grows a fifth row on top — the deck toggle, which names the deck this column
/// drives and cycles only this column to the next one.
fn render_trans(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    deck: usize,
    column: usize,
    hits: &mut Vec<(Rect, Hit)>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let state = &app.states[deck];
    let theme = app.theme;
    let rows: [(TransportHit, &str); 4] = [
        (TransportHit::Sync, "sync"),
        (TransportHit::Keylock, "key"),
        (TransportHit::Cue, "cue"),
        (TransportHit::Play, "play"),
    ];
    let paged = app.decks > DECKS_PER_PAGE;
    let slots = rows.len() as u16 + u16::from(paged);
    let step = (area.height / slots).max(1);
    let used = (slots - 1) * step + 1;
    let top = area.y + area.height.saturating_sub(used) / 2;
    if paged {
        // `deck3` rather than an arrow: the button answers "which deck am I editing?" before it is
        // clicked, and the two columns answer for themselves.
        let rect = Rect::new(area.x, top, area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                center_to_width(
                    &format!("deck{}", deck + 1),
                    rect.width as usize,
                ),
                mode_style(app, true),
            ))),
            rect,
        );
        hit(hits, rect, Hit::DeckSlot { column, delta: 1 });
    }
    for (index, (action, text)) in rows.iter().enumerate() {
        let y = top + (index as u16 + u16::from(paged)) * step;
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(area.x, y, area.width, 1);
        let active = match action {
            TransportHit::Play => state.playing,
            TransportHit::Cue => state.cue_frame > 0 && !state.playing,
            TransportHit::Sync => state.lock || state.sync_mode != "free",
            TransportHit::Keylock => state.keylock == hypermixx_core::KeylockMode::On,
        };
        let label = match action {
            TransportHit::Sync if active => "sync*",
            TransportHit::Play if active => "paus",
            _ => text,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                center_to_width(label, rect.width as usize),
                if active {
                    mode_style(app, true)
                } else {
                    Style::default().fg(theme.text)
                },
            ))),
            rect,
        );
        hit(
            hits,
            rect,
            Hit::Transport {
                deck,
                action: *action,
            },
        );
    }
}

/// The fader / volume-bar / crossfader box: one fader and one level column per control column, with
/// the crossfader (drawn exactly like a fader, but horizontal) in the bottom row of the same frame.
///
/// The two level columns are the *decks'* levels — left for the deck the left columns drive, right
/// for the deck the right ones drive — and they sit on their own side of the middle, which leaves
/// the centre of the frame empty so the crossfader's halfway point lands on it.
fn render_center(frame: &mut Frame, area: Rect, app: &App, hits: &mut Vec<(Rect, Hit)>) {
    if area.width < 10 || area.height < 5 {
        return;
    }
    // No readout: the bar is the position, exactly as it is on a channel fader.
    let block = Block::bordered()
        .border_style(Style::default().fg(app.theme.dim))
        .title(Span::styled(
            " faders ",
            Style::default().fg(app.theme.text),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 6 || inner.height < 2 {
        return;
    }
    // The faders and the meters, then a blank row, then the crossfader: the two groups must read as
    // separate controls rather than as one tall gauge.
    let top = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(2),
    );
    // `fader │ gap │ meter │ gap │ meter │ gap │ fader`: every gap the same width, and the middle
    // one keeps the frame's centre free.
    let fixed = 3 * GAP + 2 * METER_W;
    let fader_w = FADER_W.min(inner.width.saturating_sub(fixed) / 2).max(1);
    let middle = inner
        .width
        .saturating_sub(2 * fader_w + 2 * GAP + 2 * METER_W);
    let columns = Layout::horizontal([
        Constraint::Length(fader_w),
        Constraint::Length(GAP),
        Constraint::Length(METER_W),
        Constraint::Length(middle),
        Constraint::Length(METER_W),
        Constraint::Length(GAP),
        Constraint::Length(fader_w),
    ])
    .split(top);
    let mut columns_decks = app.page_decks();
    let first = columns_decks.next().unwrap_or(0);
    let second = columns_decks.next();
    render_gauge(frame, columns[0], app, first, hits);
    render_meter(frame, columns[2], app, first);
    if let Some(second) = second {
        render_meter(frame, columns[4], app, second);
        render_gauge(frame, columns[6], app, second, hits);
    }
    render_crossfader(
        frame,
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        app,
        hits,
    );
}

/// One panel of the control band: same border, accent when this deck owns the command line.
fn panel(app: &App, deck: usize, title: &str) -> Block<'static> {
    let focused = app.target == Target::Deck(deck);
    Block::bordered()
        .border_style(Style::default().fg(if focused {
            app.theme.accent
        } else {
            app.theme.dim
        }))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(if focused {
                app.theme.accent
            } else {
                app.theme.text
            }),
        ))
}

/// A pad that is set/armed versus one that is not.
fn pad_style(app: &App, active: bool) -> Style {
    let theme = app.theme;
    if active {
        Style::default()
            .fg(Color::Black)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text).bg(theme.pad)
    }
}

/// A mode/transport button: highlighted when it is the active choice.
fn mode_style(app: &App, active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Black)
            .bg(app.theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.dim)
    }
}

/// Two rows of four pads, each pad three rows tall, with the mode switches on the bottom row and
/// nothing else: the pads fill the panel, the modes name themselves.
fn render_pads(frame: &mut Frame, area: Rect, app: &App, deck: usize, hits: &mut Vec<(Rect, Hit)>) {
    let mode = app.pad_mode[deck];
    let state = &app.states[deck];
    let block = panel(app, deck, &format!("{}-pads", deck + 1));
    let inner = block.inner(area);
    if inner.width < 4 || inner.height < 3 {
        frame.render_widget(block, area);
        return;
    }
    frame.render_widget(block, area);

    let labels = mode.pads();
    let pad_w = (inner.width / 4).max(1);
    // Two rows of pads over the content rows (everything above the mode and group-switch rows).
    let pad_h = (inner.height.saturating_sub(2) / 2).max(1);
    // A one-column gutter between pads, but only when the pad stays wide enough for its label
    // (`1/4`, `-16`): a separated grid with truncated labels would be worse than a solid one.
    let gutter = u16::from(pad_w >= 4);
    for (slot, label) in labels.iter().enumerate() {
        let column = (slot % 4) as u16;
        let row = (slot / 4) as u16 * pad_h;
        let cell = Rect::new(
            inner.x + column * pad_w,
            inner.y + row,
            pad_w.min(inner.right().saturating_sub(inner.x + column * pad_w)),
            pad_h.min(inner.bottom().saturating_sub(inner.y + row)),
        );
        if cell.width == 0 || cell.height == 0 {
            continue;
        }
        // The click target is the whole cell (gutter included); the block is inset by the gutter.
        let rect = Rect::new(
            cell.x,
            cell.y,
            cell.width.saturating_sub(gutter),
            cell.height,
        );
        if rect.width == 0 {
            continue;
        }
        let active = match mode {
            PadMode::HotCue => state.hot_cues[slot].is_some(),
            PadMode::BeatLoop => {
                state.loop_range.is_some()
                    && mode
                        .loop_beats(slot)
                        .is_some_and(|beats| beats == state.loop_beats)
            }
            PadMode::BeatJump => mode
                .jump_beats(slot)
                .is_some_and(|beats| beats.abs() == state.jump_beats),
        };
        let style = pad_style(app, active);
        // The pad is a filled block, not a word: that is what separates eight pads without gaps.
        let buffer = frame.buffer_mut();
        for y in rect.top()..rect.bottom() {
            for x in rect.left()..rect.right() {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_symbol(" ").set_style(style);
                }
            }
        }
        // Label on the middle row of the pad.
        let middle = Rect::new(rect.x, rect.y + pad_h / 2, rect.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                center_to_width(label, middle.width as usize),
                style,
            ))),
            middle,
        );
        hit(hits, cell, Hit::Pad { deck, index: slot });
    }

    render_modes(
        frame,
        app,
        Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1),
        PadMode::ALL.map(PadMode::label).to_vec(),
        |index| Hit::PadMode {
            deck,
            mode: PadMode::ALL[index],
        },
        |index| PadMode::ALL[index] == mode,
        hits,
    );
    render_group_switch(
        frame,
        app,
        inner,
        deck,
        PadPanel::ALL.map(PadPanel::label).to_vec(),
        |index| Hit::PadPanel {
            deck,
            panel: PadPanel::ALL[index],
        },
        |index| PadPanel::ALL[index] == app.pad_panel[deck],
        hits,
    );
}

/// The FX panel: one slot from the deck chain, its two most useful parameters, an optional beat
/// division, and a selector listing every non-eq/filter slot the mixer exposes.
fn render_fx(frame: &mut Frame, area: Rect, app: &App, deck: usize, hits: &mut Vec<(Rect, Hit)>) {
    let block = panel(app, deck, &format!("{}-fx", deck + 1));
    let inner = block.inner(area);
    if inner.width < 4 || inner.height < 4 {
        frame.render_widget(block, area);
        return;
    }
    frame.render_widget(block, area);
    let theme = app.theme;
    let selected = app.selected_fx(deck).cloned();
    let choices: Vec<(usize, String)> = app
        .fx_panel_slots(deck)
        .iter()
        .map(|slot| (slot.index, fx_short_label(slot)))
        .collect();

    // Name / enable row.
    if let Some(slot) = &selected {
        let what = if slot.role.eq_ignore_ascii_case(&slot.kind) {
            slot.role.clone()
        } else {
            format!("{} · {}", slot.role, slot.kind)
        };
        let state = if slot.enabled { "on" } else { "off" };
        let name = format!(
            " {} {} ",
            truncate_to_width(&what, inner.width.saturating_sub(5) as usize),
            state
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                name,
                if slot.enabled {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.dim)
                },
            ))),
            line(inner, 0, 0, inner.width),
        );
        hit(
            hits,
            line(inner, 0, 0, inner.width),
            Hit::FxToggle {
                deck,
                slot: slot.index,
            },
        );
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " no fx ",
                Style::default().fg(theme.dim),
            ))),
            line(inner, 0, 0, inner.width),
        );
    }

    let content_rows = inner.height.saturating_sub(3); // name, selector, group switch
    if let Some(slot) = &selected {
        if content_rows == 0 {
            render_group_switch(
                frame,
                app,
                inner,
                deck,
                PadPanel::ALL.map(PadPanel::label).to_vec(),
                |index| Hit::PadPanel {
                    deck,
                    panel: PadPanel::ALL[index],
                },
                |index| PadPanel::ALL[index] == app.pad_panel[deck],
                hits,
            );
            return;
        }
        // The first two non-beat parameters, in declaration order. The last content row belongs to
        // the beat division, so the stacked band (three content rows) shows one parameter rather
        // than drawing the second one under the beats row.
        let params: Vec<(usize, &hypermixx_core::FxParamStatus)> = slot
            .params
            .iter()
            .enumerate()
            .filter(|(_, param)| param.beats == 0)
            .take(2)
            .take(content_rows.saturating_sub(2) as usize)
            .collect();
        for (row, (param_index, param)) in params.iter().enumerate() {
            if row as u16 + 1 >= content_rows {
                break;
            }
            let rect = line(inner, 1 + row as u16, 0, inner.width);
            render_fx_param(
                frame,
                rect,
                app,
                deck,
                slot.index,
                *param_index,
                param,
                hits,
            );
        }
        // Beat division: `-` halves, `+` doubles.
        if let Some((_, param)) = slot
            .params
            .iter()
            .enumerate()
            .find(|(_, param)| param.beats != 0)
        {
            let row = (1 + params.len() as u16)
                .min(content_rows.saturating_sub(1))
                .max(1);
            let rect = line(inner, row, 0, inner.width);
            let half = Rect::new(rect.x, rect.y, rect.width / 2, 1);
            let double = Rect::new(
                rect.x + rect.width / 2,
                rect.y,
                rect.width - rect.width / 2,
                1,
            );
            let label = beats_label(param.beats);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "-",
                    Style::default().fg(theme.dim),
                ))),
                half,
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "+",
                    Style::default().fg(theme.dim),
                )))
                .alignment(Alignment::Right),
                double,
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    center_to_width(
                        &format!("beats:{label}"),
                        rect.width.saturating_sub(2) as usize,
                    ),
                    Style::default().fg(theme.text),
                ))),
                Rect::new(rect.x + 1, rect.y, rect.width.saturating_sub(2), 1),
            );
            hit(
                hits,
                half,
                Hit::FxBeats {
                    deck,
                    slot: slot.index,
                    double: false,
                },
            );
            hit(
                hits,
                double,
                Hit::FxBeats {
                    deck,
                    slot: slot.index,
                    double: true,
                },
            );
        }
    }

    // Slot selector, styled like the pad-mode row.
    let selector = Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1);
    let labels: Vec<String> = choices.iter().map(|(_, label)| label.clone()).collect();
    let indices: Vec<usize> = choices.iter().map(|(index, _)| *index).collect();
    let selected_index = selected.as_ref().map(|slot| slot.index);
    render_modes(
        frame,
        app,
        selector,
        labels,
        |index| Hit::FxSelect {
            deck,
            slot: indices[index],
        },
        |index| Some(indices[index]) == selected_index,
        hits,
    );

    render_group_switch(
        frame,
        app,
        inner,
        deck,
        PadPanel::ALL.map(PadPanel::label).to_vec(),
        |index| Hit::PadPanel {
            deck,
            panel: PadPanel::ALL[index],
        },
        |index| PadPanel::ALL[index] == app.pad_panel[deck],
        hits,
    );
}

// Drawing one parameter needs the frame, its rectangle, the shared app state and the hit list at
// once. Bundling them into a context struct would only hide the same eight values behind a name.
#[allow(clippy::too_many_arguments)]
fn render_fx_param(
    frame: &mut Frame,
    rect: Rect,
    app: &App,
    deck: usize,
    slot: usize,
    param_index: usize,
    param: &hypermixx_core::FxParamStatus,
    hits: &mut Vec<(Rect, Hit)>,
) {
    if rect.width == 0 {
        return;
    }
    let theme = app.theme;
    let name_w = (width(&param.name) + 1)
        .clamp(3, (rect.width as usize / 2).max(3))
        .min(rect.width as usize);
    let value_w = if rect.width >= 14 { 5 } else { 0 };
    let bar_w = (rect.width as usize)
        .saturating_sub(name_w)
        .saturating_sub(value_w);
    let mut spans = vec![Span::styled(
        pad_to_width(&format!("{} ", param.name), name_w),
        Style::default().fg(theme.dim),
    )];
    let mut interactive = false;
    let range = param.range.filter(|(min, max)| bar_w > 0 && max > min);
    if let Some((min, max)) = range {
        interactive = true;
        let fraction = ((param.value - min) / (max - min)).clamp(0.0, 1.0);
        let filled = (fraction * bar_w as f32).round() as usize;
        spans.push(Span::styled(
            "▬".repeat(filled),
            Style::default().fg(theme.accent),
        ));
        spans.push(Span::styled(
            "▬".repeat(bar_w.saturating_sub(filled)),
            Style::default().fg(theme.dim),
        ));
    }
    if value_w > 0 {
        spans.push(Span::styled(
            format!("{:>value_w$.2}", param.value),
            Style::default().fg(theme.text),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
    if interactive {
        if let Some((min, max)) = range {
            for column in 0..bar_w {
                let value = min + (column as f32 + 0.5) / bar_w as f32 * (max - min);
                hit(
                    hits,
                    Rect::new(rect.x + name_w as u16 + column as u16, rect.y, 1, 1),
                    Hit::FxParam {
                        deck,
                        slot,
                        param: param_index,
                        value,
                    },
                );
            }
        }
    }
}

fn fx_short_label(slot: &FxSlotStatus) -> String {
    let role = slot.role.as_str();
    for (prefix, short) in [("flowfx", "f"), ("knobfx", "k"), ("padfx", "p")] {
        if let Some(rest) = role.strip_prefix(prefix) {
            return format!("{short}{rest}");
        }
    }
    if role.len() <= 3 {
        role.to_owned()
    } else {
        slot.kind.clone()
    }
}

fn beats_label(code: i32) -> String {
    match code {
        n if n > 0 => n.to_string(),
        -1 => "1".to_owned(),
        n if n < 0 => format!("1/{}", -n),
        _ => "--".to_owned(),
    }
}

/// The bottom button row shared by every panel in the band: evenly sized buttons, nothing else.
fn render_modes<S: AsRef<str>>(
    frame: &mut Frame,
    app: &App,
    inner: Rect,
    labels: Vec<S>,
    hit_of: impl Fn(usize) -> Hit,
    active: impl Fn(usize) -> bool,
    hits: &mut Vec<(Rect, Hit)>,
) {
    if labels.is_empty() || inner.height == 0 {
        return;
    }
    let row = inner.bottom() - 1;
    let button_w = inner.width / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let x = inner.x + index as u16 * button_w;
        let width = if index + 1 == labels.len() {
            inner.right().saturating_sub(x)
        } else {
            button_w
        };
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, row, width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                center_to_width(label.as_ref(), rect.width as usize),
                mode_style(app, active(index)),
            ))),
            rect,
        );
        hit(hits, rect, hit_of(index));
    }
}

/// The two-button group switch on the last row of a panel.
#[allow(clippy::too_many_arguments)]
fn render_group_switch<S: AsRef<str>>(
    frame: &mut Frame,
    app: &App,
    inner: Rect,
    _deck: usize,
    labels: Vec<S>,
    hit_of: impl Fn(usize) -> Hit,
    active: impl Fn(usize) -> bool,
    hits: &mut Vec<(Rect, Hit)>,
) {
    if inner.height == 0 {
        return;
    }
    render_modes(
        frame,
        app,
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        labels,
        hit_of,
        active,
        hits,
    );
}

/// Tempo / bend, or the manual loop / jump controls, in three evenly spaced bands.
fn render_beatctl(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    deck: usize,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let mode = app.beat_mode[deck];
    let state = &app.states[deck];
    let block = panel(app, deck, &format!("{}-beatctl", deck + 1));
    let inner = block.inner(area);
    if inner.width < 6 || inner.height < 3 {
        frame.render_widget(block, area);
        return;
    }
    frame.render_widget(block, area);
    let theme = app.theme;

    // The beat bands fill the content rows above the mode row and the group switch.
    let content_h = inner.height.saturating_sub(2);
    let base = content_h / 3;
    let extra = content_h % 3;
    let band_h = |index: u16| base + u16::from(index < extra);
    let band = |index: u16| {
        let y = inner.y + (0..index).map(band_h).sum::<u16>();
        Rect::new(inner.x, y, inner.width, band_h(index))
    };
    let band_row = |rect: Rect| rect.y + rect.height / 2;
    let mut label = |text: &str, style: Style, rect: Rect| {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_to_width(text, rect.width as usize),
                style,
            )))
            .alignment(Alignment::Center),
            Rect::new(rect.x, band_row(rect), rect.width, 1),
        );
    };

    match mode {
        BeatMode::Tempo => {
            let tempo = band(0);
            let range = band(1);
            let bend = band(2);
            label(
                &format!("{:+.2}%", (state.tempo - 1.0) * 100.0),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                tempo,
            );
            label(
                &format!("range ±{:.0}%", state.tempo_range * 100.0),
                Style::default().fg(theme.dim),
                range,
            );
            label(
                &if state.nudge.abs() > 0.0 {
                    format!("bend {:+.1}%", state.nudge * 100.0)
                } else {
                    "bend".to_owned()
                },
                Style::default().fg(theme.text),
                bend,
            );
            for direction in [-1i8, 1] {
                let rect = if direction < 0 {
                    Rect::new(inner.x, band_row(band(0)), 1, 1)
                } else {
                    Rect::new(
                        inner.x + inner.width.saturating_sub(1),
                        band_row(band(0)),
                        1,
                        1,
                    )
                };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        if direction < 0 { "◀" } else { "▶" },
                        Style::default().fg(theme.dim),
                    ))),
                    rect,
                );
                hit(hits, rect, Hit::TempoFader { deck, direction });
            }
            for direction in [-1i8, 1] {
                let rect = if direction < 0 {
                    Rect::new(inner.x, band_row(band(2)), 1, 1)
                } else {
                    Rect::new(
                        inner.x + inner.width.saturating_sub(1),
                        band_row(band(2)),
                        1,
                        1,
                    )
                };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        if direction < 0 { "◀" } else { "▶" },
                        Style::default().fg(theme.dim),
                    ))),
                    rect,
                );
                hit(hits, rect, Hit::Bend { deck, direction });
            }
        }
        BeatMode::ManualLoop | BeatMode::ManualJump => {
            let value = if mode == BeatMode::ManualLoop {
                state.loop_beats.label()
            } else {
                state.jump_beats.to_string()
            };
            let step = band(0);
            label(
                &format!("{value} beats"),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                step,
            );
            let half = Rect::new(inner.x, band_row(step), 1, 1);
            let double = Rect::new(
                inner.x + inner.width.saturating_sub(1),
                band_row(step),
                1,
                1,
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "-",
                    Style::default().fg(theme.dim),
                ))),
                half,
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "+",
                    Style::default().fg(theme.dim),
                ))),
                double,
            );
            if mode == BeatMode::ManualLoop {
                hit(hits, half, Hit::LoopBeats { deck, halve: true });
                hit(hits, double, Hit::LoopBeats { deck, halve: false });
            } else {
                hit(hits, half, Hit::JumpBeats { deck, halve: true });
                hit(hits, double, Hit::JumpBeats { deck, halve: false });
            }

            if mode == BeatMode::ManualLoop {
                let rows: [[(&str, LoopHit); 2]; 2] = [
                    [("in", LoopHit::In), ("out", LoopHit::Out)],
                    [("toggle", LoopHit::Toggle), ("exit", LoopHit::Exit)],
                ];
                for (row_index, buttons) in rows.iter().enumerate() {
                    // One two-row band per button row, so the loop controls line up with the tempo
                    // bands the same panel shows in its other mode.
                    let band_rect = band(1 + row_index as u16);
                    let half = inner.width / 2;
                    for (column, (text, op)) in buttons.iter().enumerate() {
                        let rect = Rect::new(
                            band_rect.x + column as u16 * half,
                            band_rect.y,
                            if column == 0 {
                                half
                            } else {
                                inner.width - half
                            },
                            band_rect.height,
                        );
                        let active = match op {
                            LoopHit::In => state.loop_in_armed.is_some(),
                            LoopHit::Out | LoopHit::Toggle | LoopHit::Exit => {
                                state.loop_range.is_some()
                            }
                        };
                        frame.render_widget(
                            Paragraph::new(Line::from(Span::styled(
                                center_to_width(text, rect.width as usize),
                                if active {
                                    mode_style(app, true)
                                } else {
                                    Style::default().fg(theme.text)
                                },
                            ))),
                            Rect::new(rect.x, band_row(rect), rect.width, 1),
                        );
                        hit(hits, rect, Hit::Loop { deck, op: *op });
                    }
                }
            } else {
                let rest = band(1);
                let jump = Rect::new(
                    rest.x,
                    rest.y,
                    rest.width,
                    band(2).bottom().saturating_sub(rest.y),
                );
                let back = Rect::new(jump.x, jump.y, inner.width / 2, jump.height);
                let forward = Rect::new(
                    jump.x + inner.width / 2,
                    jump.y,
                    inner.width - inner.width / 2,
                    jump.height,
                );
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "◀ back",
                        Style::default().fg(theme.text),
                    ))),
                    Rect::new(back.x, band_row(back), back.width, 1),
                );
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "fwd ▶",
                        Style::default().fg(theme.text),
                    )))
                    .alignment(Alignment::Right),
                    Rect::new(forward.x, band_row(forward), forward.width, 1),
                );
                hit(
                    hits,
                    back,
                    Hit::Jump {
                        deck,
                        direction: -1,
                    },
                );
                hit(hits, forward, Hit::Jump { deck, direction: 1 });
            }
        }
    }

    render_modes(
        frame,
        app,
        Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1),
        BeatMode::ALL.map(BeatMode::label).to_vec(),
        |index| Hit::BeatMode {
            deck,
            mode: BeatMode::ALL[index],
        },
        |index| BeatMode::ALL[index] == mode,
        hits,
    );
    render_group_switch(
        frame,
        app,
        inner,
        deck,
        EqPanel::ALL.map(EqPanel::label).to_vec(),
        |index| Hit::EqPanel {
            deck,
            panel: EqPanel::ALL[index],
        },
        |index| EqPanel::ALL[index] == app.eq_panel[deck],
        hits,
    );
}

/// The EQ column: five tone/trim knobs and the group switch.
fn render_mixer(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    deck: usize,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let block = panel(app, deck, &format!("{}-eq", deck + 1));
    let inner = block.inner(area);
    if inner.width < 5 || inner.height < 3 {
        frame.render_widget(block, area);
        return;
    }
    frame.render_widget(block, area);
    let theme = app.theme;

    for (row, knob) in Knob::ALL.iter().enumerate() {
        let rect = line(inner, row as u16, 0, inner.width);
        if rect.y >= inner.bottom().saturating_sub(1) {
            break;
        }
        let value = app.knob_value(deck, *knob);
        // A narrow mixer column drops the numeric readout rather than the bar: the position is the
        // control, the number is only confirmation.
        let value_w = if inner.width >= 11 { 4 } else { 0 };
        let name_w = (inner.width as usize).saturating_sub(value_w).min(5);
        let bar_w = (inner.width as usize)
            .saturating_sub(name_w)
            .saturating_sub(value_w);
        let (filled, label) = match value {
            Some(value) => {
                let fraction = ((value + 1.0) / 2.0).clamp(0.0, 1.0);
                (
                    (fraction * bar_w as f32).round() as usize,
                    format!("{value:+.1}"),
                )
            }
            None => (0, "--".to_owned()),
        };
        let mut spans = vec![Span::styled(
            pad_to_width(&format!("{} ", knob.label()), name_w),
            Style::default().fg(theme.dim),
        )];
        if bar_w > 0 {
            // `▬` is the only glyph here that does not fill its cell: the ink sits in the middle of
            // the row, so every bar keeps a sliver of air above and below it without costing a row.
            // Filled and empty differ only in colour.
            spans.push(Span::styled(
                "▬".repeat(filled),
                Style::default().fg(theme.accent),
            ));
            spans.push(Span::styled(
                "▬".repeat(bar_w.saturating_sub(filled)),
                Style::default().fg(theme.dim),
            ));
        }
        if value_w > 0 {
            spans.push(Span::styled(
                format!("{label:>value_w$}"),
                Style::default().fg(theme.text),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), rect);
        if bar_w > 0 && value.is_some() {
            for column in 0..bar_w {
                let cell = Rect::new(rect.x + name_w as u16 + column as u16, rect.y, 1, 1);
                let value = 2.0 * (column as f32 + 0.5) / bar_w as f32 - 1.0;
                hit(
                    hits,
                    cell,
                    Hit::Knob {
                        deck,
                        knob: *knob,
                        value,
                    },
                );
            }
        }
    }

    render_group_switch(
        frame,
        app,
        inner,
        deck,
        EqPanel::ALL.map(EqPanel::label).to_vec(),
        |index| Hit::EqPanel {
            deck,
            panel: EqPanel::ALL[index],
        },
        |index| EqPanel::ALL[index] == app.eq_panel[deck],
        hits,
    );
}

/// One channel's vertical fader inside the shared box. The whole travel is the click target.
fn render_gauge(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    deck: usize,
    hits: &mut Vec<(Rect, Hit)>,
) {
    if area.width == 0 || area.height < 2 {
        return;
    }
    let value = app.fader_value(deck).unwrap_or(0.0);
    let fraction = ((value + 1.0) / 2.0).clamp(0.0, 1.0);
    let filled = (fraction * f32::from(area.height)).round() as u16;
    let buffer = frame.buffer_mut();
    for row in 0..area.height {
        // Row 0 is the top of the travel: 0 dB.
        let on = area.height - row <= filled;
        let y = area.y + row;
        for column in 0..area.width {
            if let Some(cell) = buffer.cell_mut((area.x + column, y)) {
                cell.set_symbol(if on { "█" } else { "░" }).set_fg(if on {
                    app.theme.accent
                } else {
                    app.theme.dim
                });
            }
        }
    }
    for row in 0..area.height {
        let value = 1.0 - 2.0 * (f32::from(row) + 0.5) / f32::from(area.height);
        hit(
            hits,
            Rect::new(area.x, area.y + row, area.width, 1),
            Hit::Fader { deck, value },
        );
    }
}

/// One deck's post-fader level, bottom-up on a -60 dB scale.
fn render_meter(frame: &mut Frame, area: Rect, app: &App, deck: usize) {
    if area.width == 0 || area.height < 2 {
        return;
    }
    let (_, _, deck_peaks) = app.peaks();
    let level = deck_peaks.get(deck).copied().unwrap_or(0.0).max(0.0);
    let fraction = if level <= 0.0 {
        0.0
    } else {
        ((20.0 * level.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
    };
    let filled = (fraction * f32::from(area.height)).round() as u16;
    let buffer = frame.buffer_mut();
    for column in 0..area.width {
        for row in 0..area.height {
            let on = area.height - row <= filled;
            if let Some(cell) = buffer.cell_mut((area.x + column, area.y + row)) {
                cell.set_symbol(if on { "█" } else { "░" }).set_fg(if on {
                    app.theme.accent
                } else {
                    app.theme.dim
                });
            }
        }
    }
}

fn render_crossfader(frame: &mut Frame, area: Rect, app: &App, hits: &mut Vec<(Rect, Hit)>) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    let position = (0..app.decks)
        .filter_map(|deck| {
            app.meters
                .as_ref()
                .and_then(|meters| meters.channels.get(deck))
                .map(|levels| levels.crossfader)
        })
        .next()
        .unwrap_or(0.0);
    // A channel fader turned sideways, but bipolar: the middle of the track is 0, and turning it
    // fills that side only. At centre nothing is filled, which is what makes "centre" mean silence
    // on both channels.
    let cells = area.width as usize;
    let center = cells / 2;
    let filled = (position.abs() * center as f32).round() as usize;
    let (from, to) = if position < 0.0 {
        (center.saturating_sub(filled), center)
    } else {
        (center, (center + filled).min(cells))
    };
    let buffer = frame.buffer_mut();
    for column in 0..area.width {
        let on = (column as usize) >= from && (column as usize) < to && filled > 0;
        for row in 0..area.height {
            if let Some(cell) = buffer.cell_mut((area.x + column, area.y + row)) {
                cell.set_symbol(if on { "█" } else { "░" }).set_fg(if on {
                    app.theme.accent
                } else {
                    app.theme.dim
                });
            }
        }
    }
    for column in 0..area.width {
        let value = -1.0 + 2.0 * (f32::from(column) + 0.5) / f32::from(area.width);
        hit(
            hits,
            Rect::new(area.x + column, area.y, 1, 1),
            Hit::Crossfader { value },
        );
    }
}

// ---- browser + command ------------------------------------------------------

fn render_bottom(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let browser_width = (area.width as u32 * 3 / 4) as u16;
    let browser_area = Rect::new(area.x, area.y, browser_width, area.height);
    let command_area = Rect::new(
        area.x + browser_width,
        area.y,
        area.width - browser_width,
        area.height,
    );
    browser::render(
        frame,
        browser_area,
        &mut app.browser,
        app.target == Target::Browser,
    );
    render_command(frame, command_area, app);
}

fn render_response(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let height = area.height as usize;
    let width = area.width as usize;
    // Only wrap enough lines to fill the viewport plus the scroll offset. The log is capped, but
    // wrapping all of it on every 60Hz tick is unnecessary work.
    let target = height.saturating_add(app.log_offset);
    let mut visual: Vec<Line> = Vec::new();
    for line in app.log.iter().rev() {
        let style = log_style(line, &app.theme);
        for part in text::wrap_to_width(&line.text, width).into_iter().rev() {
            visual.push(Line::styled(part, style));
            if visual.len() >= target {
                break;
            }
        }
        if visual.len() >= target {
            break;
        }
    }
    visual.reverse();
    if visual.len() < target {
        app.log_offset = visual.len().saturating_sub(height).min(app.log_offset);
    }
    visual.truncate(height);
    frame.render_widget(Paragraph::new(Text::from(visual)), area);
}

fn log_style(line: &LogLine, theme: &Theme) -> Style {
    match line.level {
        Level::Error => Style::default().fg(theme.error),
        Level::Info => Style::default().fg(theme.text),
    }
}

fn render_command(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let block = Block::bordered()
        .border_style(Style::default().fg(app.theme.accent))
        .title(" command ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // The bottom two rows are the rule + input line; everything above is the response log.
    if inner.height < 2 {
        render_input(frame, inner, app);
        return;
    }
    let response_height = inner.height - 2;
    let response_area = Rect::new(inner.x, inner.y, inner.width, response_height);
    let rule_row = inner.y + response_height;
    let input_area = Rect::new(inner.x, rule_row + 1, inner.width, 1);

    render_response(frame, response_area, app);
    // The rule spans the full panel width (including the side borders) so its `├`/`┤` connect.
    let rule = target_rule(app, area.width);
    frame.render_widget(
        Paragraph::new(rule),
        Rect::new(area.x, rule_row, area.width, 1),
    );
    render_input(frame, input_area, app);
}

/// The divider between response and input, with the command target embedded at the left.
///
/// `command` sits at `x + 2` in the panel title (`┌ command ─…`); the rule starts with `├ ` so
/// `deck1` lands on exactly the same column.
fn target_rule(app: &App, width: u16) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let width = width as usize;
    if width < 4 {
        return Line::from(Span::styled("─".repeat(width), rule_style(app)));
    }

    let active = match app.target {
        Target::Deck(index) => index,
        Target::Browser => app.decks,
    };
    let count = app.decks + 1;
    let full: Vec<String> = (0..count)
        .map(|index| {
            if index == app.decks {
                Target::Browser.label()
            } else {
                Target::Deck(index).label()
            }
        })
        .collect();
    let compact: Vec<String> = (0..count)
        .map(|index| {
            if index == app.decks {
                Target::Browser.compact_label()
            } else {
                Target::Deck(index).compact_label()
            }
        })
        .collect();
    let labels_width = |labels: &[String]| {
        labels.iter().map(|label| text::width(label)).sum::<usize>()
            + 2 * labels.len().saturating_sub(1)
    };
    let labels = if width >= labels_width(&full) + 4 {
        full
    } else if width >= labels_width(&compact) + 4 {
        compact
    } else {
        let active_label = if active == app.decks {
            Target::Browser.compact_label()
        } else {
            Target::Deck(active).compact_label()
        };
        if width >= text::width(&active_label) + 4 {
            vec![active_label]
        } else {
            Vec::new()
        }
    };
    let fill = width.saturating_sub(labels_width(&labels) + 4);

    let mut spans = vec![Span::styled("├ ", rule_style(app))];
    for (index, label) in labels.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", rule_style(app)));
        }
        spans.push(Span::styled(label.clone(), tab_style(app, index == active)));
    }
    spans.push(Span::styled(
        format!(" {}", "─".repeat(fill)),
        rule_style(app),
    ));
    spans.push(Span::styled("┤", rule_style(app)));
    Line::from(spans)
}

fn rule_style(app: &App) -> Style {
    // The rule is the input box's top edge and the target strip at once; the accent keeps it
    // connected to the command panel's border instead of reading as an unrelated dim line.
    Style::default().fg(app.theme.accent)
}

fn tab_style(app: &App, active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Black)
            .bg(app.theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.dim)
    }
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let buffer = app.command_string();
    let mut spans = vec![Span::styled("> ", Style::default().fg(app.theme.dim))];
    if buffer.is_empty() {
        spans.push(Span::styled(
            "输入命令…",
            Style::default().fg(app.theme.dim),
        ));
    } else {
        spans.push(Span::raw(buffer));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    let offset = 2 + app.cursor as u16;
    let x = (area.x + offset).min(area.right().saturating_sub(1));
    frame.set_cursor_position((x, area.y));
}

fn render_completion(frame: &mut Frame, command_area: Rect, app: &App) {
    let Some(state) = &app.completion else {
        return;
    };
    if state.completion.items.is_empty() {
        return;
    }
    if command_area.width < 6 || command_area.height < 6 {
        return;
    }
    let inner = Rect::new(
        command_area.x + 1,
        command_area.y + 1,
        command_area.width - 2,
        command_area.height - 2,
    );
    let response_height = inner.height.saturating_sub(2);
    let rows = state.completion.items.len().min(COMPLETION_ROWS) as u16;
    let height = (rows + 2).min(response_height);
    if height < 3 {
        return;
    }
    let rect = Rect::new(
        inner.x,
        inner.y + response_height - height,
        inner.width,
        height,
    );
    frame.render_widget(Clear, rect);

    let items: Vec<ListItem> = state
        .completion
        .items
        .iter()
        .map(|candidate| {
            let mut spans = vec![Span::raw(candidate.text.clone())];
            if !candidate.hint.is_empty() {
                spans.push(Span::styled(
                    format!("   {}", candidate.hint),
                    Style::default().fg(app.theme.dim),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" tab "))
        .highlight_style(
            Style::default()
                .bg(app.theme.accent)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        );
    let mut list_state = ListState::default();
    list_state.select(Some(state.selected));
    frame.render_stateful_widget(list, rect, &mut list_state);
}

// ---- overlays ---------------------------------------------------------------

/// A two-row deck chooser anchored near a double-clicked track.
pub(crate) fn load_target_rect(area: Rect, x: u16, y: u16, decks: usize) -> Rect {
    let width = 18.min(area.width);
    let height = (decks as u16 + 2).min(area.height);
    let left = x.min(area.width.saturating_sub(width));
    let top = y.min(area.height.saturating_sub(height));
    Rect::new(left, top, width, height)
}

fn render_load_target(
    frame: &mut Frame,
    area: Rect,
    x: u16,
    y: u16,
    selected: usize,
    decks: usize,
) {
    let popup = load_target_rect(area, x, y, decks);
    if popup.width < 4 || popup.height < 3 {
        return;
    }
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(" load ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines: Vec<Line> = (0..decks)
        .map(|deck| {
            let style = if deck == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::styled(format!(" deck{} ", deck + 1), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_audio::{ChannelLevels, Meters};
    use hypermixx_core::BeatGrid;
    use hypermixx_library::{analyser::Backends, BandPeaks, StereoPeaks, Waveform};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    fn fx_status(
        index: usize,
        role: &str,
        kind: &str,
        enabled: bool,
        params: Vec<hypermixx_core::FxParamStatus>,
    ) -> FxSlotStatus {
        FxSlotStatus {
            index,
            role: role.to_owned(),
            kind: kind.to_owned(),
            enabled,
            flows: None,
            params,
        }
    }

    fn test_app() -> App {
        let (_notice_tx, notice_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(
            2,
            Backends::default(),
            notice_rx,
            event_rx,
            Default::default(),
            Default::default(),
        );
        let buckets: Vec<StereoPeaks> = (0..400)
            .map(|index| StereoPeaks {
                left: BandPeaks {
                    low: ((index % 7) * 36) as u8,
                    mid: ((index % 5) * 51) as u8,
                    high: ((index % 3) * 85) as u8,
                },
                right: BandPeaks {
                    low: ((index % 3) * 85) as u8,
                    mid: ((index % 5) * 51) as u8,
                    high: ((index % 7) * 36) as u8,
                },
            })
            .collect();
        app.waveforms[0] = Some(Arc::new(Waveform::from_buckets(buckets, 256, 100_000)));
        app.grids[0] = Some(BeatGrid::from_constant_bpm(120.0, 0, 100_000, 44_100));
        app.states[0].current_frame = 50_000;
        app.states[0].total_frames = 100_000;
        app.states[0].bpm = 120.0;
        app.titles[0] = Some("test.mp3".to_owned());
        app.meters = Some(Meters {
            master_peak: 0.1,
            cue_peak: 0.05,
            master_reduction_db: 0.0,
            overruns: 0,
            deck_peaks: vec![0.2, 0.1],
            channels: vec![
                ChannelLevels {
                    flow: 0.0,
                    deck: 0.0,
                    crossfader: 0.0,
                    cue_send: 1.0,
                    gain: 0.0,
                };
                2
            ],
        });
        app
    }

    /// Four decks: the deck area stacks a page per pair and the control band drives one pair.
    fn stacked_app() -> App {
        let (_notice_tx, notice_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(
            4,
            Backends::default(),
            notice_rx,
            event_rx,
            Default::default(),
            Default::default(),
        );
        app.meters = Some(Meters {
            master_peak: 0.1,
            cue_peak: 0.05,
            master_reduction_db: 0.0,
            overruns: 0,
            deck_peaks: vec![0.2, 0.1, 0.1, 0.2],
            channels: vec![
                ChannelLevels {
                    flow: 0.0,
                    deck: 0.0,
                    crossfader: 0.0,
                    cue_send: 1.0,
                    gain: 0.0,
                };
                4
            ],
        });
        for deck in 0..4 {
            app.titles[deck] = Some(format!("deck{}.mp3", deck + 1));
            app.states[deck].total_frames = 100_000;
            app.states[deck].bpm = 120.0;
        }
        app
    }

    fn render_at(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::tui::view::render(frame, app))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
            .collect()
    }

    fn all_text(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| row_text(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn find_row(buffer: &ratatui::buffer::Buffer, needle: &str) -> u16 {
        (1..buffer.area.height)
            .find(|y| row_text(buffer, *y).contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}"))
    }

    #[test]
    fn the_header_has_a_settings_button_and_registers_it_as_a_hit() {
        let mut app = test_app();
        let buffer = render_at(&mut app, 120, 40);
        assert!(row_text(&buffer, 0).contains("settings"));
        let settings = app
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Settings)
            .expect("settings button hit");
        assert_eq!(settings.0.y, 0);
        assert!(settings.0.right() <= 120);
    }

    #[test]
    fn the_out_field_turns_red_while_the_limiter_works() {
        let mut app = test_app();
        let colour = app.theme.error;
        let calm = render_at(&mut app, 120, 40);
        assert!(
            !header_has_colour(&calm, colour),
            "no reduction means the out field keeps the normal text colour"
        );

        app.meters.as_mut().unwrap().master_reduction_db = -3.0;
        let limiting = render_at(&mut app, 120, 40);
        assert!(
            header_has_colour(&limiting, colour),
            "the out field should turn red while the limiter holds the signal down"
        );
    }

    fn header_has_colour(buffer: &ratatui::buffer::Buffer, colour: Color) -> bool {
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, 0)))
            .any(|cell| cell.fg == colour)
    }

    #[test]
    fn the_control_band_needs_room_for_both_decks_and_the_browser() {
        let mut app = test_app();
        let wide = render_at(&mut app, 120, 40);
        assert!(all_text(&wide).contains("pads"), "120x40 shows the pads");
        assert!(
            all_text(&wide).contains("-eq "),
            "120x40 shows the EQ column"
        );

        let short = render_at(&mut app, 120, 22);
        assert!(
            !all_text(&short).contains("pads"),
            "a short terminal drops the whole control band"
        );
        assert!(all_text(&short).contains("browser"));

        let narrow = render_at(&mut app, 90, 40);
        assert!(
            !all_text(&narrow).contains("pads"),
            "a narrow terminal drops the whole control band"
        );
    }

    #[test]
    fn four_decks_stack_two_pages_of_bands() {
        let tall = bands(Rect::new(0, 0, 120, 40), 4);
        assert_eq!(tall.pages.len(), 2, "one page per pair of decks");
        assert_eq!(
            tall.pages[0].info.height, INFO_H_STACK,
            "a stacked info row gives up its second overview row"
        );
        assert_eq!(
            tall.pages[0].wave.height, 6,
            "three rows for each of the page's two decks"
        );
        assert_eq!(tall.pages[1].info.y, tall.pages[0].wave.bottom());
        assert_eq!(
            tall.controls.map(|rect| rect.height),
            Some(CONTROL_H_STACK),
            "two-row pads instead of three"
        );
        assert!(tall.bottom.height >= MIN_BOTTOM_H, "{:?}", tall.bottom);

        // A short terminal keeps both pages and drops to two waveform rows per deck.
        let short = bands(Rect::new(0, 0, 120, 32), 4);
        assert_eq!(short.pages.len(), 2);
        assert_eq!(short.pages[0].wave.height, 4);
        assert!(short.controls.is_some());
        assert!(short.bottom.height >= MIN_BOTTOM_H, "{:?}", short.bottom);

        // Three decks: the last page holds one deck and still gets its own bands.
        let three = bands(Rect::new(0, 0, 120, 40), 3);
        assert_eq!(three.pages.len(), 2);
        assert_eq!(
            three.pages[1].wave.height, 3,
            "one deck keeps all three rows"
        );
    }

    #[test]
    fn four_decks_show_every_waveform_but_one_pair_of_controls() {
        let mut app = stacked_app();
        let buffer = render_at(&mut app, 120, 40);
        let text = all_text(&buffer);
        for deck in 1..=4 {
            assert!(text.contains(&format!("Deck{deck}")), "Deck{deck}: {text}");
            assert!(
                text.contains(&format!("deck{deck}.mp3")),
                "deck{deck}.mp3: {text}"
            );
        }
        // The columns start on the first pair; the other two decks have no pads or knobs.
        for title in ["1-pads", "2-pads", "1-eq", "2-eq"] {
            assert!(text.contains(title), "{title}: {text}");
        }
        for title in ["3-pads", "4-pads", "3-eq", "4-eq"] {
            assert!(!text.contains(title), "{title} is not in a column: {text}");
        }
        assert_eq!(text.matches("sync").count(), 2, "{text}");

        // Each column's toggle names the deck that column drives.
        let toggles: Vec<(Rect, usize)> = app
            .hits
            .iter()
            .filter_map(|(rect, hit)| match hit {
                Hit::DeckSlot { column, .. } => Some((*rect, *column)),
                _ => None,
            })
            .collect();
        assert_eq!(toggles.len(), 2, "one toggle per trans column");
        for (rect, column) in &toggles {
            let row = row_text(&buffer, rect.y);
            assert!(
                row.contains(&format!("deck{}", column + 1)),
                "column {column} names its deck: {row}"
            );
        }

        // Flipping the left column alone brings deck 3 under the pads and leaves the right column
        // (deck 2) exactly where it was.
        app.target = Target::Deck(0);
        app.cycle_slot(0, 1);
        assert_eq!(app.slots, [2, 1]);
        let buffer = render_at(&mut app, 120, 40);
        let text = all_text(&buffer);
        for title in ["3-pads", "2-pads", "3-eq", "2-eq"] {
            assert!(text.contains(title), "{title}: {text}");
        }
        for title in ["1-pads", "1-eq", "4-pads", "4-eq"] {
            assert!(!text.contains(title), "{title} is out: {text}");
        }
        // Every deck keeps its info and waveform; only the columns change.
        for deck in 1..=4 {
            assert!(text.contains(&format!("Deck{deck}")), "Deck{deck}: {text}");
        }
        let labels: Vec<String> = toggles
            .iter()
            .map(|(rect, _)| row_text(&buffer, rect.y))
            .collect();
        assert!(
            labels.iter().any(|row| row.contains("deck3")),
            "the left toggle follows its column: {labels:?}"
        );
        assert!(
            labels.iter().any(|row| row.contains("deck2")),
            "the right toggle stays on deck 2: {labels:?}"
        );
        // The pad hits reach the decks the columns now drive.
        let pads: Vec<usize> = app
            .hits
            .iter()
            .filter_map(|(_, hit)| match hit {
                Hit::Pad { deck, .. } => Some(*deck),
                _ => None,
            })
            .collect();
        assert!(pads.contains(&2) && pads.contains(&1), "{pads:?}");
    }

    #[test]
    fn four_decks_survive_small_terminals_and_two_decks_have_no_toggle() {
        let mut app = stacked_app();
        for (width, height) in [(120, 26), (120, 30), (120, 32), (100, 40), (150, 60)] {
            let buffer = render_at(&mut app, width, height);
            assert!(
                all_text(&buffer).contains("browser"),
                "{width}x{height} keeps the browser"
            );
        }
        // With a single page there is nothing to flip, so the trans column stays four buttons.
        let mut app = test_app();
        let buffer = render_at(&mut app, 120, 40);
        assert!(
            !app.hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::DeckSlot { .. })),
            "two decks need no deck toggle"
        );
        assert_eq!(all_text(&buffer).matches("play").count(), 2);
    }

    #[test]
    fn the_decks_under_the_control_band_keep_a_white_border() {
        let mut app = stacked_app();
        // The command line speaks for deck 1 while the columns drive decks 3-4: no panel is
        // accented, but the two decks under the pads keep a white border so the pairing is on screen.
        app.target = Target::Deck(0);
        app.cycle_slot(0, 1);
        app.cycle_slot(1, 1);
        let buffer = render_at(&mut app, 120, 40);
        let pages = bands(buffer.area, 4).pages;
        let under = pages[1].info;
        let column = under.width / 2;
        assert_eq!(
            buffer.cell((under.x, under.y)).map(|cell| cell.fg),
            Some(app.theme.text),
            "the deck under the pads is white"
        );
        assert_eq!(
            buffer.cell((under.x + column, under.y)).map(|cell| cell.fg),
            Some(app.theme.text),
            "both of its decks are white"
        );
        assert_eq!(
            buffer
                .cell((pages[0].info.x, pages[0].info.y))
                .map(|cell| cell.fg),
            Some(app.theme.accent),
            "the command target keeps the accent on the other page"
        );
    }

    #[test]
    fn both_decks_get_their_own_pads_knobs_and_transport() {
        let mut app = test_app();
        let buffer = render_at(&mut app, 120, 40);
        let text = all_text(&buffer);
        // The default panels are pads and eq; beat controls share the eq slot.
        for title in ["pads", "eq"] {
            assert_eq!(
                text.matches(&format!("-{title} ")).count(),
                2,
                "two {title} panels, one per deck: {text}"
            );
        }
        // The transport moved out of the eq panel into its own borderless column.
        assert_eq!(text.matches("sync").count(), 2, "{text}");
        assert_eq!(text.matches("play").count(), 2, "{text}");
        let pads: Vec<usize> = app
            .hits
            .iter()
            .filter(|(_, hit)| matches!(hit, Hit::Pad { .. }))
            .map(|(_, hit)| match hit {
                Hit::Pad { deck, .. } => *deck,
                _ => unreachable!(),
            })
            .collect();
        assert!(pads.contains(&0) && pads.contains(&1), "{pads:?}");
    }

    #[test]
    fn a_pad_hit_lands_inside_its_pad_label() {
        let mut app = test_app();
        let buffer = render_at(&mut app, 120, 40);
        let (rect, _) = app
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Pad { deck: 0, index: 0 })
            .expect("first pad");
        // The label sits on the middle row of the three-row pad.
        let middle = rect.y + rect.height / 2;
        let label: String = (rect.x..rect.x + rect.width)
            .filter_map(|x| {
                buffer
                    .cell((x, middle))
                    .map(|cell| cell.symbol().to_owned())
            })
            .collect();
        assert!(label.contains('1'), "{label:?} at {rect:?}");
    }

    #[test]
    fn pad_modes_change_the_labels_and_the_highlight() {
        let mut app = test_app();
        app.pad_mode[0] = PadMode::BeatLoop;
        let buffer = render_at(&mut app, 120, 40);
        assert!(all_text(&buffer).contains("1/4"));

        app.pad_mode[0] = PadMode::BeatJump;
        let buffer = render_at(&mut app, 120, 40);
        assert!(all_text(&buffer).contains("-16"));
    }

    #[test]
    fn a_set_hot_cue_lights_its_pad() {
        let mut app = test_app();
        app.states[0].hot_cues[2] = Some(hypermixx_core::HotCue {
            slot: 2,
            frame: 1_000,
        });
        let buffer = render_at(&mut app, 120, 40);
        let (rect, _) = app
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Pad { deck: 0, index: 2 })
            .expect("pad 3");
        let cell = buffer.cell((rect.x, rect.y)).expect("cell");
        assert_eq!(
            cell.bg, app.theme.accent,
            "a set hot cue paints its pad with the accent"
        );
    }

    #[test]
    fn deck_info_carries_the_title_the_status_and_the_clock() {
        let mut app = test_app();
        let buffer = render_at(&mut app, 120, 40);
        let text = all_text(&buffer);
        assert!(text.contains("Deck1"), "{text}");
        assert!(text.contains("test.mp3"), "{text}");
        assert!(text.contains("120.0 BPM"), "{text}");
        let clock = format!(
            "{} / -{}",
            time(50_000, app.sample_rate),
            time(50_000, app.sample_rate)
        );
        assert!(text.contains(&clock), "clock missing: {text}");
    }

    #[test]
    fn the_overview_draws_the_whole_track_at_its_real_position() {
        let mut app = test_app();
        let buffer = render_at(&mut app, 120, 40);
        // The overview sits in the deck info band, below the status row.
        let band = bands(buffer.area, app.decks);
        assert!(band.pages[0].info.height >= 3);
        let overview_row = band.pages[0].info.y + 2;
        let reds: Vec<u16> = (0..band.pages[0].info.width)
            .filter(|x| {
                buffer
                    .cell((*x, overview_row))
                    .is_some_and(|cell| cell.fg == Color::Red)
            })
            .collect();
        assert!(!reds.is_empty(), "no playhead in the overview");
        // Halfway through the track, measured across the deck's own column (the cover takes the
        // first few cells of the band).
        let deck_column = band.pages[0].info.width / app.decks as u16;
        let center = deck_column / 2;
        assert!(
            reds.iter().any(|x| x.abs_diff(center) <= COVER_W + 2),
            "overview playhead not at half the track: {reds:?} (deck centre {center})"
        );
    }

    #[test]
    fn the_overview_playhead_does_not_eat_the_kitty_placement_cell() {
        let mut app = kitty_app();
        app.states[0].current_frame = 0;
        let buffer = render_at(&mut app, 120, 40);
        let symbols = cell_symbols(&buffer);
        assert!(
            symbols.contains("a=t"),
            "the overview's Kitty sequence must survive a playhead at column zero"
        );
        assert!(all_text(&buffer).contains('┃'));

        app.states[0].current_frame = 50_000;
        let buffer = render_at(&mut app, 120, 40);
        assert!(
            cell_symbols(&buffer).contains("a=p"),
            "the cached overview must still be placed after the playhead moves"
        );
        assert!(all_text(&buffer).contains('┃'));
    }

    #[test]
    fn the_overview_playhead_moves_with_the_frame() {
        let mut app = test_app();
        let red_x = |app: &mut App| {
            let buffer = render_at(app, 120, 40);
            let band = bands(buffer.area, app.decks);
            let row = band.pages[0].info.y + 2;
            (0..band.pages[0].info.width)
                .find(|x| {
                    buffer
                        .cell((*x, row))
                        .is_some_and(|cell| cell.fg == Color::Red)
                })
                .expect("overview playhead")
        };
        app.states[0].current_frame = 25_000;
        let early = red_x(&mut app);
        app.states[0].current_frame = 75_000;
        let late = red_x(&mut app);
        assert!(early < late, "playhead did not move: {early} → {late}");
    }

    /// The crossfader row, and the frame it lives in.
    fn crossfader_row(buffer: &ratatui::buffer::Buffer) -> u16 {
        let controls = bands(buffer.area, 2).controls.expect("control band");
        controls.y + CONTROL_H - 2
    }

    fn meter_app(deck_one: f32, deck_two: f32, crossfader: f32) -> App {
        let mut app = test_app();
        app.meters = Some(Meters {
            master_peak: deck_one.max(deck_two),
            cue_peak: 0.0,
            master_reduction_db: 0.0,
            overruns: 0,
            deck_peaks: vec![deck_one, deck_two],
            channels: vec![
                ChannelLevels {
                    // Both faders at the bottom, so a lit column can only be a meter.
                    flow: -1.0,
                    deck: -1.0,
                    crossfader,
                    cue_send: 0.0,
                    gain: 0.0,
                };
                2
            ],
        });
        app
    }

    /// The crossfader's track on its row: `(start column, cells)`.
    fn crossfader_track(row: &str) -> (usize, Vec<char>) {
        let cells: Vec<char> = row.chars().collect();
        let start = cells
            .iter()
            .position(|ch| *ch == '░' || *ch == '█')
            .expect("a crossfader track");
        let end = cells
            .iter()
            .rposition(|ch| *ch == '░' || *ch == '█')
            .expect("track end");
        (start, cells[start..=end].to_vec())
    }

    #[test]
    fn the_crossfader_is_bipolar_and_empty_at_centre() {
        for (position, expected_side) in [(-1.0, -1i8), (-0.5, -1), (0.0, 0), (0.5, 1), (1.0, 1)] {
            let mut app = meter_app(0.0, 0.0, position);
            let buffer = render_at(&mut app, 120, 40);
            let row = row_text(&buffer, crossfader_row(&buffer));
            let (start, track) = crossfader_track(&row);
            let lit: Vec<usize> = track
                .iter()
                .enumerate()
                .filter(|(_, ch)| **ch == '█')
                .map(|(index, _)| index)
                .collect();
            let half = track.len() / 2;
            match expected_side {
                0 => assert!(lit.is_empty(), "centre fills nothing on either side: {row}"),
                side => {
                    let expected = (position.abs() * half as f32).round() as usize;
                    assert_eq!(
                        lit.len(),
                        expected,
                        "position {position} should fill {expected} cells: {row}"
                    );
                    if side < 0 {
                        assert!(
                            lit.iter().all(|index| *index < half),
                            "a left position fills the left half: {row}"
                        );
                    } else {
                        assert!(
                            lit.iter().all(|index| *index >= half),
                            "a right position fills the right half: {row}"
                        );
                    }
                }
            }
            assert!(start > 0, "the track sits inside the box: {row}");
        }
    }

    #[test]
    fn the_two_level_columns_are_deck_one_on_the_left_and_deck_two_on_the_right() {
        // Only the fader box's own columns count: the EQ bars to its left are lit too.
        let lit = |deck_one: f32, deck_two: f32| {
            let mut app = meter_app(deck_one, deck_two, 0.0);
            let buffer = render_at(&mut app, 120, 40);
            let controls = bands(buffer.area, 2).controls.expect("control band");
            let title = row_text(&buffer, controls.y);
            let label = title.find("faders").expect("fader box title");
            let left = title[..label].chars().count() - 2;
            let text = row_text(&buffer, controls.y + 2);
            let columns: Vec<usize> = text
                .char_indices()
                .filter(|(_, ch)| *ch == '█')
                .map(|(byte, _)| text[..byte].chars().count())
                .filter(|column| *column >= left && *column < left + usize::from(CENTER_W))
                .collect();
            (columns, text)
        };

        let (deck_one, text) = lit(0.5, 0.0);
        assert_eq!(deck_one.len(), 1, "deck 1 lights one column: {text}");
        let (deck_two, text) = lit(0.0, 0.5);
        assert_eq!(deck_two.len(), 1, "deck 2 lights one column: {text}");
        assert!(
            deck_one[0] < deck_two[0],
            "deck 1's meter is left of deck 2's: {deck_one:?} {deck_two:?}"
        );

        let (none, text) = lit(0.0, 0.0);
        assert!(none.is_empty(), "silence lights nothing: {text}");

        // Both columns straddle the crossfader's halfway point, which is where the frame's centre is.
        let mut app = meter_app(0.0, 0.0, 0.0);
        let buffer = render_at(&mut app, 120, 40);
        let row = row_text(&buffer, crossfader_row(&buffer));
        let (start, track) = crossfader_track(&row);
        let middle = start + track.len() / 2;
        assert_eq!(
            middle,
            (deck_one[0] + deck_two[0]).div_ceil(2),
            "the crossfader's centre sits between the two level columns: {row}"
        );
    }

    #[test]
    fn the_crossfader_lives_inside_the_fader_box() {
        let mut app = test_app();
        let buffer = render_at(&mut app, 120, 40);
        let controls = bands(buffer.area, app.decks)
            .controls
            .expect("control band");
        let row = crossfader_row(&buffer);
        let text = row_text(&buffer, row);
        assert!(
            !text.contains('─'),
            "no box-drawing rule inside the crossfader row: {text}"
        );
        assert!(text.contains('░'), "the track is drawn: {text}");
        let title = row_text(&buffer, controls.y);
        assert!(title.contains("faders"), "{title}");
        // The faders and the meter share that frame, above the crossfader.
        assert!(row > controls.y + 2);
    }

    #[test]
    fn the_pad_and_eq_groups_switch_their_content() {
        let mut app = test_app();
        let buffer = render_at(&mut app, 120, 40);
        let text = all_text(&buffer);
        assert!(text.contains("1-pads "), "{text}");
        assert!(text.contains("1-eq "), "{text}");
        assert!(text.contains("pads") && text.contains("fx"), "{text}");
        assert!(text.contains("eq") && text.contains("beat"), "{text}");

        app.eq_panel[0] = EqPanel::Beat;
        let buffer = render_at(&mut app, 120, 40);
        let text = all_text(&buffer);
        assert!(text.contains("1-beatctl "), "{text}");
        assert!(!text.contains("1-eq "), "{text}");

        app.pad_panel[0] = PadPanel::Fx;
        let buffer = render_at(&mut app, 120, 40);
        let text = all_text(&buffer);
        assert!(text.contains("1-fx "), "{text}");
        assert!(!text.contains("1-pads "), "{text}");
    }

    #[test]
    fn every_fx_slot_gets_a_selector_button() {
        let mut app = test_app();
        app.deck_fx[0] = vec![
            fx_status(0, "eq", "eq", true, vec![]),
            fx_status(1, "filter", "filter", true, vec![]),
            fx_status(2, "flowfx1", "echo", true, vec![]),
            fx_status(3, "knobfx1", "reverb", false, vec![]),
            fx_status(4, "padfx1", "cut", false, vec![]),
        ];
        app.fx_slot[0] = 2;
        app.pad_panel[0] = PadPanel::Fx;
        let _buffer = render_at(&mut app, 120, 40);
        let selectors: Vec<usize> = app
            .hits
            .iter()
            .filter_map(|(_, hit)| match hit {
                Hit::FxSelect { deck: 0, slot } => Some(*slot),
                _ => None,
            })
            .collect();
        assert_eq!(
            selectors,
            vec![2, 3, 4],
            "eq/filter are EQ controls, not FX slots"
        );
    }

    #[test]
    fn the_mixer_panel_holds_the_knobs_and_the_transport() {
        let mut app = test_app();
        let buffer = render_at(&mut app, 120, 40);
        let text = all_text(&buffer);
        for label in [
            "gain", "high", "mid", "low", "filt", "sync", "cue", "play", "key",
        ] {
            assert!(text.contains(label), "{label} missing from the mixer panel");
        }
        assert!(
            !text.contains(" range ±") || text.contains("range ±10%"),
            "the tempo range belongs under the tempo readout"
        );
    }

    #[test]
    fn a_session_starts_with_the_browser_focused() {
        let mut app = test_app();
        let buffer = render_at(&mut app, 120, 40);
        assert_eq!(app.target, Target::Browser);
        assert_eq!(app.default_deck(), 0, "commands still land on deck 1");
        // The browser pane takes the accent; the deck panels stay dim.
        let row = find_row(&buffer, "browser");
        let browser_column = row_text(&buffer, row).find("browser").unwrap() as u16 - 2;
        assert_eq!(
            buffer.cell((browser_column, row)).map(|cell| cell.fg),
            Some(app.theme.accent),
            "the browser owns the focus"
        );
        let pads_row = find_row(&buffer, "1-pads");
        let pads_column = row_text(&buffer, pads_row).find('┌').unwrap() as u16;
        assert_eq!(
            buffer.cell((pads_column, pads_row)).map(|cell| cell.fg),
            Some(app.theme.dim),
            "deck 1's panels are not accented"
        );
    }

    #[test]
    fn a_tall_terminal_gives_the_waveform_an_extra_row() {
        let mut app = test_app();
        let tall = bands(Rect::new(0, 0, 120, 40), app.decks);
        let short = bands(Rect::new(0, 0, 120, 32), app.decks);
        assert_eq!(
            tall.pages[0].wave.height, 6,
            "three rows per deck at 40 rows"
        );
        assert_eq!(
            short.pages[0].wave.height, 4,
            "two rows per deck when height is tight"
        );
        assert!(short.controls.is_some(), "the band still fits at 32 rows");
        // The extra rows come from the browser, never from the control band.
        assert_eq!(
            tall.controls.map(|rect| rect.height),
            short.controls.map(|rect| rect.height)
        );
        let _ = &mut app;
    }

    #[test]
    fn the_load_popup_numbers_the_decks_from_one() {
        let mut app = test_app();
        app.target = Target::Deck(1);
        app.overlay = Some(Overlay::LoadTarget {
            x: 10,
            y: 10,
            selected: 0,
        });
        let buffer = render_at(&mut app, 120, 40);
        let text = all_text(&buffer);
        assert!(text.contains(" deck1 "), "the chooser lists deck 1: {text}");
        assert!(text.contains(" deck2 "), "the chooser lists deck 2: {text}");
        // The default row is the first one, even though deck 2 held the command line.
        let popup = load_target_rect(buffer.area, 10, 10, app.decks);
        let row = popup.y + 1;
        assert!(
            row_text(&buffer, row).contains("deck1"),
            "first row is deck 1"
        );
        assert_eq!(
            buffer.cell((popup.x + 1, row)).map(|cell| cell.bg),
            Some(app.theme.accent),
            "deck 1 starts selected"
        );
        assert_ne!(
            buffer.cell((popup.x + 1, row + 1)).map(|cell| cell.bg),
            Some(app.theme.accent),
            "deck 2 is not preselected"
        );
    }

    /// Every cell's symbol, joined: image escapes live in a cell, so a `contains` finds them.
    fn cell_symbols(buffer: &ratatui::buffer::Buffer) -> String {
        buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("\u{1}")
    }

    fn kitty_app() -> App {
        let mut app = test_app();
        app.waveform_view
            .force_backend(crate::tui::waveform_view::Backend::Kitty);
        app.covers[0] = Some(Arc::new(hypermixx_media::Cover {
            width: 2,
            height: 2,
            rgba: vec![255; 16],
            average: [255, 0, 0],
        }));
        app
    }

    #[test]
    fn opening_a_modal_deletes_the_images_and_closing_it_transmits_again() {
        let mut app = kitty_app();
        app.overlay = None;
        render_at(&mut app, 120, 40);
        assert!(!app.images_hidden);
        assert!(
            app.deck_images[0].cover.is_transmitted(),
            "the cover is transmitted while nothing is open"
        );

        // Opening a modal must delete what the terminal holds, not just stop drawing it: an image
        // drawn under the popup shows through its cells.
        app.overlay = Some(Overlay::LoadTarget {
            x: 0,
            y: 0,
            selected: 0,
        });
        render_at(&mut app, 120, 40);
        assert!(app.images_hidden);
        assert!(
            !app.deck_images[0].cover.is_transmitted(),
            "the slot forgets its image so the next frame transmits again"
        );

        app.overlay = None;
        render_at(&mut app, 120, 40);
        assert!(!app.images_hidden);
        assert!(
            app.deck_images[0].cover.is_transmitted(),
            "and it comes back"
        );
    }

    #[test]
    fn a_missing_cover_is_a_plain_chrome_square() {
        let mut app = test_app();
        let buffer = render_at(&mut app, 120, 40);
        let band = bands(buffer.area, app.decks);
        // Inside the deck-info border: a square (6 columns for 3 rows at the default cell size),
        // filled with chrome and nothing written over it.
        let cell = buffer
            .cell((band.pages[0].info.x + 1, band.pages[0].info.y + 1))
            .expect("cover cell");
        assert_eq!(cell.bg, app.theme.dim, "no cover means a dim block");
        assert_eq!(cell.symbol(), " ", "no letter over the artwork slot");
        let beside = buffer
            .cell((band.pages[0].info.x + 7, band.pages[0].info.y + 1))
            .expect("after the square");
        assert_ne!(beside.bg, app.theme.dim, "the square ends before the text");
    }

    #[test]
    fn the_cover_fills_a_square_slot_from_its_centre() {
        let mut app = test_app();
        // A 4x2 image, red on top and blue underneath: the centre crop is the middle two columns, so
        // the square slot must be painted edge to edge with red above blue, starting at its first
        // row — not nudged up by a letterbox.
        let mut rgba = Vec::new();
        for y in 0..2u8 {
            for _ in 0..4 {
                rgba.extend_from_slice(&if y == 0 {
                    [200, 0, 0, 255]
                } else {
                    [0, 0, 200, 255]
                });
            }
        }
        app.covers[0] = Some(Arc::new(hypermixx_media::Cover {
            width: 4,
            height: 2,
            rgba,
            average: [100, 0, 100],
        }));
        let buffer = render_at(&mut app, 120, 40);
        let band = bands(buffer.area, app.decks);
        let (left, top) = (band.pages[0].info.x + 1, band.pages[0].info.y + 1);
        let rows = band.pages[0].info.height.saturating_sub(2);
        let cell = app.waveform_view.cell_px();
        // The same rule the renderer uses: a square in pixels.
        let square_w = (u32::from(rows) * u32::from(cell.height) / u32::from(cell.width)) as u16;
        let square = Rect::new(left, top, square_w, rows);
        for y in square.top()..square.bottom() {
            for x in square.left()..square.right() {
                let cell = buffer.cell((x, y)).expect("cover cell");
                assert_eq!(cell.symbol(), "▀", "every cell of the square is painted");
            }
        }
        // Red above blue: the artwork spans the slot from its very first row to its last, so it is
        // neither nudged up nor letterboxed.
        let top_cell = buffer.cell((square.x, square.y)).expect("top cell");
        assert_eq!(top_cell.fg, Color::Rgb(200, 0, 0), "red at the top");
        let bottom_cell = buffer
            .cell((square.x, square.bottom() - 1))
            .expect("bottom cell");
        assert_eq!(bottom_cell.bg, Color::Rgb(0, 0, 200), "blue at the bottom");
        // One column of air after the square, then the status text.
        let air = buffer.cell((square.right(), square.y)).expect("gap");
        assert_ne!(air.symbol(), "▀", "the artwork stops at the square");
    }

    #[test]
    fn kitty_draws_the_waveform_the_cover_and_the_overview_as_images() {
        let mut app = kitty_app();
        let buffer = render_at(&mut app, 120, 40);
        let symbols = cell_symbols(&buffer);
        // Three transmits: waveform + overview + cover, each under its own id.
        assert!(
            symbols.matches("a=t").count() >= 3,
            "expected the deck's three images to be transmitted"
        );
        assert!(symbols.contains("a=p"), "images must also be placed");
        assert!(
            !all_text(&buffer).contains("▀"),
            "a kitty terminal should not get half-block fallback cover art"
        );
    }

    #[test]
    fn an_open_overlay_withholds_the_images_and_closing_it_redraws_them() {
        let mut app = kitty_app();
        let buffer = render_at(&mut app, 120, 40);
        assert!(cell_symbols(&buffer).contains("a=t"));

        // A popup over the deck: images are withheld, and the deck falls back to braille so the
        // area is not left showing a picture the terminal kept on screen.
        app.overlay = Some(Overlay::LoadTarget {
            x: 4,
            y: 4,
            selected: 0,
        });
        let buffer = render_at(&mut app, 120, 40);
        let symbols = cell_symbols(&buffer);
        assert!(
            !symbols.contains("a=t") && !symbols.contains("a=p"),
            "an open overlay must not place images"
        );

        app.overlay = None;
        let buffer = render_at(&mut app, 120, 40);
        let symbols = cell_symbols(&buffer);
        assert!(
            symbols.contains("a=t") && symbols.contains("a=p"),
            "closing the overlay must re-transmit and re-place, not leave a hole"
        );
        // The placement that was withheld while the overlay was open is deleted first: a kitty image
        // is drawn behind text, so a stale placement's opaque pixels would show through the new
        // one's transparent ones as a ghost.
        assert!(
            symbols.contains("a=d"),
            "the hidden placement must be deleted before the redraw"
        );
    }

    #[test]
    fn the_settings_screen_draws_over_the_mix() {
        let dir = std::env::temp_dir().join(format!(
            "hypermixx-view-settings-{}-{}",
            std::process::id(),
            hypermixx_library::library::db::unix_now()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let mixer = dir.join("mixer.toml");
        let _ = std::fs::write(&mixer, "[[output]]\nname = \"main\"\nrole = \"master\"\n");
        let mut app = test_app();
        app.overlay = Some(Overlay::Settings(Box::new(
            crate::tui::settings::SettingsScreen::open(crate::tui::settings::Context {
                mixer: Some(mixer.display().to_string()),
                controller_port: Some("Numark Party Mix III".to_owned()),
                ..Default::default()
            }),
        )));
        let buffer = render_at(&mut app, 120, 40);
        let text = all_text(&buffer);
        assert!(text.contains("settings"), "{text}");
        assert!(
            text.contains("controller"),
            "the controller row is on screen"
        );
        assert!(
            text.contains("preference"),
            "the preference row is on screen"
        );
        assert!(text.contains("mixer"), "the mixer row is on screen: {text}");
        assert!(text.contains("Esc close"), "the key hint is on screen");
    }

    /// Prints frames for eyeballing the layout. Not an assertion:
    /// The settings screen over a tall terminal, for eyeballing the shared popup.
    /// `cargo test -p hypermixx-cli settings_preview -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn settings_preview() {
        let dir =
            std::env::temp_dir().join(format!("hypermixx-settings-preview-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mixer = dir.join("mixer.toml");
        let _ = std::fs::write(
            &mixer,
            concat!(
                "[[output]]\nname = \"main\"\nrole = \"master\"\ndevice = \"sysdefault:CARD=III\"\n",
                "[[output]]\nname = \"phones\"\nrole = \"cue\"\n",
            ),
        );
        let devices = vec![
            hypermixx_audio::DeviceInfo {
                name: "sysdefault:CARD=III".to_owned(),
                description: "Numark Party Mix III".to_owned(),
                is_default: true,
                rates: "44100 48000".to_owned(),
                accepts: Some(true),
                channels: 4,
            },
            hypermixx_audio::DeviceInfo {
                name: "hdmi:CARD=HDMI".to_owned(),
                description: "HDMI".to_owned(),
                is_default: false,
                rates: "44100 48000".to_owned(),
                accepts: Some(true),
                channels: 2,
            },
        ];
        let mut screen =
            crate::tui::settings::SettingsScreen::open(crate::tui::settings::Context {
                profile: None,
                mixer: Some(mixer.display().to_string()),
                live: Some("config.toml".to_owned()),
                preference: Some("config.toml".to_owned()),
                controller_port: Some("Numark Party Mix III".to_owned()),
                controller_map: Some("mapping.toml".to_owned()),
                controller: Some("Numark Party Mix III".to_owned()),
                engine_rate: 48_000,
                devices,
            });
        let theme = Theme::default();
        for step in 0..4 {
            if step > 0 {
                screen.handle_key(crossterm::event::KeyCode::Enter);
            }
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
            terminal
                .draw(|frame| {
                    crate::tui::settings::render(frame, frame.area(), &screen, &theme);
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            println!("---- step {step} ----");
            for y in 0..buffer.area.height {
                let line: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect();
                if !line.trim().is_empty() {
                    println!("{line}");
                }
            }
        }
    }

    /// `cargo test -p hypermixx-cli layout_preview -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn layout_preview() {
        let mut app = test_app();
        app.pad_mode[0] = PadMode::BeatLoop;
        app.beat_mode[0] = BeatMode::ManualLoop;
        app.states[0].hot_cues[0] = Some(hypermixx_core::HotCue {
            slot: 0,
            frame: 2_000,
        });
        app.states[0].loop_range = Some((10_000, 24_000));
        for (width, height) in [(120, 40), (120, 26), (140, 42), (111, 40)] {
            let buffer = render_at(&mut app, width, height);
            println!("==== {width}x{height} ====\n{}", all_text(&buffer));
        }

        // The alternate panels: FX + beat controls, with an echo slot and its beat row.
        app.pad_panel[0] = PadPanel::Fx;
        app.eq_panel[0] = EqPanel::Beat;
        app.deck_fx[0] = vec![
            fx_status(0, "eq", "eq", true, vec![]),
            fx_status(1, "filter", "filter", true, vec![]),
            fx_status(
                2,
                "knobfx1",
                "echo",
                true,
                vec![
                    hypermixx_core::FxParamStatus {
                        name: "mix".to_owned(),
                        value: 0.35,
                        range: Some((0.0, 1.0)),
                        beats: 0,
                    },
                    hypermixx_core::FxParamStatus {
                        name: "feedback".to_owned(),
                        value: 0.45,
                        range: Some((0.0, 0.95)),
                        beats: 0,
                    },
                    hypermixx_core::FxParamStatus {
                        name: "time".to_owned(),
                        value: 0.5,
                        range: Some((1.0 / 32.0, 4.0)),
                        beats: -2,
                    },
                ],
            ),
            fx_status(3, "padfx1", "cut", true, vec![]),
        ];
        app.fx_slot[0] = 2;
        let buffer = render_at(&mut app, 120, 40);
        println!("==== 120x40 fx ====\n{}", all_text(&buffer));
    }

    #[test]
    fn tiny_terminals_do_not_panic_and_keep_the_command_line() {
        let mut app = test_app();
        for (width, height) in [(20, 6), (80, 12), (100, 20), (200, 8)] {
            let buffer = render_at(&mut app, width, height);
            assert!(row_text(&buffer, 0).contains("HYPERMIXX"));
        }
    }
}
