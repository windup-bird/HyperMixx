//! Everything that draws the TUI.
//!
//! Layout is fixed top-to-bottom: a one-line global header, one bordered band per deck (a single
//! status line plus the waveform), then a bottom band split browser : command = 2 : 1. The decks
//! together never take more than half the terminal, so the browser and response log still have
//! room on a short screen.
//!
//! Inside the command panel, the response log sits above a two-row input band: its top rule is a
//! horizontal line with the command target (`deck0`, `deck1`, …) embedded on the left, aligned
//! with the panel title. The panel's own bottom border closes the input from below.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use hypermixx_core::{DeckState, Stem};

use crate::response::{bpm_label, sync_badge, time, Level, LogLine};
use crate::tui::app::{App, Overlay, Target, COMPLETION_ROWS};
use crate::tui::{browser, picker, text, waveform_view};

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Both decks together are capped at half the terminal. The browser/command band takes the
    // rest, which is what keeps a tall track list possible without starving the waveforms.
    let deck_band = area.height.saturating_sub(1) / 2;
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(deck_band),
        Constraint::Fill(1),
    ])
    .split(area);

    render_top(frame, chunks[0], app);
    let deck_rows = Layout::vertical(vec![Constraint::Fill(1); app.decks]).split(chunks[1]);
    for (index, row) in deck_rows.iter().enumerate() {
        render_deck(frame, *row, app, index);
    }
    render_bottom(frame, chunks[2], app);
    render_completion(frame, chunks[2], app);

    // A picker draws over everything, including the completion popup.
    if let Some(overlay) = &app.overlay {
        match overlay {
            Overlay::Ports { list } => {
                picker::render_port_list(frame, frame.area(), list, "MIDI input (Enter)")
            }
            Overlay::Files { picker, .. } => {
                picker::render_file_picker(frame, frame.area(), picker)
            }
        }
    }
}

fn render_top(frame: &mut Frame, area: Rect, app: &App) {
    let peak = app.meters.map_or(0.0, |meters| meters.master_peak);
    let cue = app.meters.map_or(0.0, |meters| meters.cue_peak);
    let overruns = app.meters.map_or(0, |meters| meters.overruns);
    let ring_ms = app.ring_frames as f64 * 1000.0 / f64::from(app.sample_rate);

    let mut spans = vec![
        Span::styled(
            format!(" HYPERMIXX {} ", app.version),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {}Hz ", app.sample_rate)),
        Span::raw(format!(" {} decks ", app.decks)),
        Span::raw(format!(" ring {ring_ms:.0}ms ")),
        Span::raw(format!(" out {} ", db(peak))),
        Span::raw(format!(" cue {} ", db(cue))),
    ];
    let dropped = if overruns > 0 {
        Color::Red
    } else {
        Color::DarkGray
    };
    spans.push(Span::styled(
        format!(" drop {overruns} "),
        Style::default().fg(dropped),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn db(peak: f32) -> String {
    if peak <= 0.0 {
        "-inf dB".to_owned()
    } else {
        format!("{:+.1} dB", 20.0 * peak.log10())
    }
}

fn render_deck(frame: &mut Frame, area: Rect, app: &mut App, index: usize) {
    if area.width < 2 || area.height < 1 {
        return;
    }
    let focused = app.target == Target::Deck(index);
    let border = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let title = format!(
        " Deck{index} · {} ",
        app.titles[index].as_deref().unwrap_or("<no track>")
    );
    let title = text::truncate_to_width(&title, area.width.saturating_sub(3) as usize);
    let block = Block::bordered()
        .border_style(Style::default().fg(border))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    let wave_area = rows[0];
    let info_area = rows[1];

    // `fit` reads this back next keypress, so the viewport width is always fresh.
    app.wave_width_dots = wave_area.width as usize * 2;
    waveform_view::render(
        frame,
        wave_area,
        app.waveforms[index].as_ref(),
        app.grids[index].as_ref(),
        app.states[index].current_frame as f64,
        app.frames_per_dot,
    );
    render_deck_info(frame, info_area, app, index);
}

/// The single deck status row: musical state on the left, `elapsed / -remaining` pinned right.
fn render_deck_info(frame: &mut Frame, area: Rect, app: &App, index: usize) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let clock = deck_clock(&app.states[index]);
    let clock_width = text::width(&clock).min(area.width as usize) as u16;
    let columns =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(clock_width)]).split(area);
    frame.render_widget(
        Paragraph::new(deck_status_line(app, index, columns[0].width)),
        columns[0],
    );
    if clock_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                clock,
                Style::default().add_modifier(Modifier::BOLD),
            )))
            .alignment(Alignment::Right),
            columns[1],
        );
    }
}

/// `played / remaining`. Total duration is deliberately absent: the two numbers already imply it.
fn deck_clock(state: &DeckState) -> String {
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
    let elapsed = time(state.current_frame.min(state.total_frames));
    let remaining = time(state.total_frames.saturating_sub(state.current_frame));
    format!("{glyph} {elapsed} / -{remaining} ")
}

/// Builds the left-hand status text, dropping the least important tail when the width runs out.
///
/// Priority is the join order: BPM, key, tempo/keylock, sync, loop, stems, beat phase. The clock
/// is rendered in its own right-aligned column and never competes for this space.
fn deck_status_line(app: &App, index: usize, width: u16) -> Line<'static> {
    let state = &app.states[index];
    let mut parts: Vec<(usize, Vec<Span<'static>>)> = Vec::new();

    parts.push(part(vec![Span::styled(
        bpm_label(state),
        Style::default().add_modifier(Modifier::BOLD),
    )]));
    parts.push(part(vec![Span::styled(
        state.key.clone().unwrap_or_else(|| "--".to_owned()),
        Style::default().fg(Color::Cyan),
    )]));
    parts.push(part(vec![Span::raw(format!(
        "tempo {:.3} (±{:.0}%)",
        state.tempo,
        state.tempo_range * 100.0
    ))]));
    parts.push(part(vec![Span::raw(format!(
        "keylock {}",
        state.keylock.label()
    ))]));
    if let Some(badge) = sync_badge(state) {
        parts.push(part(vec![Span::styled(
            badge,
            Style::default().fg(Color::Yellow),
        )]));
    }
    if let Some(loop_text) = loop_badge(state) {
        parts.push(part(vec![Span::styled(
            loop_text,
            Style::default().fg(Color::Magenta),
        )]));
    }
    if state.stems.ready {
        let mut spans = Vec::new();
        for stem in Stem::ALL {
            let (marker, style) = if state.stems.is_soloed(stem) {
                (
                    'S',
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else if state.stems.mute[stem.index()] {
                ('M', Style::default().fg(Color::DarkGray))
            } else {
                (' ', Style::default().fg(Color::Green))
            };
            spans.push(Span::styled(
                format!(
                    "{}{:+.1}{marker} ",
                    stem.name().chars().next().unwrap().to_ascii_uppercase(),
                    state.stems.level[stem.index()]
                ),
                style,
            ));
        }
        parts.push(part(spans));
    }
    if let Some(grid) = app.grids[index].as_ref().filter(|grid| !grid.is_empty()) {
        let beat = grid.floor_beat(state.current_frame) as f64
            + f64::from(grid.phase(state.current_frame));
        parts.push(part(vec![Span::styled(
            format!("beat {beat:.1}"),
            Style::default().fg(Color::DarkGray),
        )]));
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

fn loop_badge(state: &DeckState) -> Option<String> {
    let mut parts = Vec::new();
    if let Some((in_frame, out_frame)) = state.loop_range {
        parts.push(format!("loop [{in_frame}-{out_frame}]"));
    }
    if let Some(armed) = state.loop_in_armed {
        parts.push(format!("in armed@{armed}"));
    }
    (!parts.is_empty()).then(|| parts.join("  "))
}

fn render_bottom(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let browser_width = (area.width as u32 * 2 / 3) as u16;
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
        &app.browser,
        app.target == Target::Browser,
    );
    render_command(frame, command_area, app);
}

fn render_response(frame: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let height = area.height as usize;
    let bottom = app.log.len().saturating_sub(app.log_offset);
    let start = bottom.saturating_sub(height);
    let lines: Vec<Line> = app
        .log
        .iter()
        .skip(start)
        .take(height)
        .map(log_line)
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn log_line(line: &LogLine) -> Line<'static> {
    match line.level {
        Level::Error => Line::styled(line.text.clone(), Style::default().fg(Color::Red)),
        Level::Info => Line::raw(line.text.clone()),
    }
}

fn render_command(frame: &mut Frame, area: Rect, app: &App) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Cyan))
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
/// `deck0` lands on exactly the same column.
fn target_rule(app: &App, width: u16) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let width = width as usize;
    if width < 4 {
        return Line::from(Span::styled("─".repeat(width), rule_style()));
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

    let mut spans = vec![Span::styled("├ ", rule_style())];
    for (index, label) in labels.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", rule_style()));
        }
        spans.push(Span::styled(label.clone(), tab_style(index == active)));
    }
    spans.push(Span::styled(format!(" {}", "─".repeat(fill)), rule_style()));
    spans.push(Span::styled("┤", rule_style()));
    Line::from(spans)
}

fn rule_style() -> Style {
    // The rule is the input box's top edge and the target strip at once; cyan keeps it connected
    // to the command panel's border instead of reading as an unrelated dim line.
    Style::default().fg(Color::Cyan)
}

fn tab_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let buffer = app.command_string();
    let mut spans = vec![Span::styled("> ", Style::default().fg(Color::DarkGray))];
    if buffer.is_empty() {
        spans.push(Span::styled(
            "输入命令…",
            Style::default().fg(Color::DarkGray),
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
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" tab "))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    let mut list_state = ListState::default();
    list_state.select(Some(state.selected));
    frame.render_stateful_widget(list, rect, &mut list_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use hypermixx_core::{Backend, BeatGrid};
    use hypermixx_library::{BandPeaks, Waveform};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn test_app() -> App {
        let (_notice_tx, notice_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(2, Backend::Auto, notice_rx, event_rx, Default::default());
        let buckets = (0..400)
            .map(|index| BandPeaks {
                low: (index % 7) as f32 / 7.0,
                mid: (index % 5) as f32 / 5.0,
                high: (index % 3) as f32 / 3.0,
            })
            .collect();
        app.waveforms[0] = Some(Arc::new(Waveform::from_buckets(buckets, 256, 100_000)));
        app.grids[0] = Some(BeatGrid::from_constant_bpm(120.0, 0, 100_000, 44_100));
        app.states[0].current_frame = 50_000;
        app.states[0].total_frames = 100_000;
        app.states[0].bpm = 120.0;
        app.titles[0] = Some("test.mp3".to_owned());
        app
    }

    fn render_buffer(app: &mut App) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(120, 40);
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

    fn find_row(buffer: &ratatui::buffer::Buffer, needle: &str) -> u16 {
        (0..buffer.area.height)
            .find(|y| row_text(buffer, *y).contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}"))
    }

    /// Terminal column of `needle` (byte offsets are useless once box-drawing glyphs are in play).
    fn column_of(row: &str, needle: &str) -> u16 {
        let byte = row.find(needle).expect("needle in row");
        text::width(&row[..byte]) as u16
    }

    #[test]
    fn header_lands_on_the_first_row() {
        let mut app = test_app();
        let buffer = render_buffer(&mut app);
        assert!(row_text(&buffer, 0).contains("HYPERMIXX"));
    }

    #[test]
    fn header_hides_the_analysis_backend_and_ui_timings() {
        let mut app = test_app();
        let buffer = render_buffer(&mut app);
        let row = row_text(&buffer, 0);
        assert!(!row.contains("Auto"), "analysis backend leaked into the header: {row}");
        assert!(!row.contains("poll"), "poll latency leaked into the header: {row}");
        assert!(!row.contains(" ui "), "render timing leaked into the header: {row}");
    }

    #[test]
    fn header_reports_the_configured_ring() {
        let mut app = test_app();
        app.ring_frames = 8_192;
        let buffer = render_buffer(&mut app);
        let row = row_text(&buffer, 0);
        assert!(row.contains("ring 186ms"), "{row}");
    }

    #[test]
    fn deck_status_is_one_line_with_the_clock_on_the_right() {
        let mut app = test_app();
        let buffer = render_buffer(&mut app);

        let status_rows: Vec<u16> = (0..buffer.area.height)
            .filter(|y| row_text(&buffer, *y).contains("120.0 BPM"))
            .collect();
        assert_eq!(
            status_rows.len(),
            1,
            "status must be a single row: {status_rows:?}"
        );

        let row = row_text(&buffer, status_rows[0]);
        let clock = format!("{} / -{}", time(50_000), time(50_000));
        assert!(row.contains(&clock), "{row}");
        assert!(
            !row.contains("[50000/100000]"),
            "frame counter should be gone: {row}"
        );
        // The title moved to the border, not the status row.
        assert!(!row.contains("test.mp3"), "{row}");
    }

    #[test]
    fn the_two_decks_share_at_most_half_the_screen() {
        let mut app = test_app();
        let buffer = render_buffer(&mut app);
        let first = find_row(&buffer, "Deck0");
        let browser = find_row(&buffer, "browser");
        assert_eq!(first, 1);
        assert!(
            browser <= buffer.area.height / 2,
            "decks must stop by half height: browser at {browser}"
        );
    }

    #[test]
    fn the_browser_divider_follows_the_focused_border() {
        let mut app = test_app();
        let buffer = render_buffer(&mut app);
        let row = find_row(&buffer, "folders/playlists");
        let text = row_text(&buffer, row);
        let mut bars = text
            .match_indices('│')
            .map(|(byte, _)| text::width(&text[..byte]) as u16);
        bars.next(); // browser's left border
        let divider_x = bars.next().expect("a divider between the panes");
        assert_eq!(
            buffer.cell((divider_x, row)).map(|cell| cell.fg),
            Some(Color::DarkGray),
            "an unfocused browser keeps a dim divider"
        );

        app.target = Target::Browser;
        let buffer = render_buffer(&mut app);
        assert_eq!(
            buffer.cell((divider_x, row)).map(|cell| cell.fg),
            Some(Color::Cyan),
            "the divider should follow the focused border"
        );
    }

    #[test]
    fn browser_and_command_are_split_two_to_one() {
        let mut app = test_app();
        let buffer = render_buffer(&mut app);
        let row = find_row(&buffer, "browser");
        let text = row_text(&buffer, row);
        let browser_x = column_of(&text, "browser");
        let command_x = column_of(&text, "command");
        let boundary = buffer.area.width * 2 / 3;
        assert!(
            browser_x < boundary,
            "browser starts left of {boundary}: {text}"
        );
        assert!(
            command_x >= boundary,
            "command starts right of {boundary}: {text}"
        );
    }

    #[test]
    fn the_command_rule_lists_the_targets_aligned_with_the_title() {
        let mut app = test_app();
        let buffer = render_buffer(&mut app);
        let title_row = find_row(&buffer, "command");
        let rule_row = find_row(&buffer, "deck0");
        assert_ne!(title_row, rule_row);

        let title = row_text(&buffer, title_row);
        let rule = row_text(&buffer, rule_row);
        let title_text_x = column_of(&title, "command");
        let rule_text_x = column_of(&rule, "deck0");
        assert_eq!(
            title_text_x, rule_text_x,
            "target prefix must align with `command`:\n{title}\n{rule}"
        );
        assert!(rule.contains("deck1") && rule.contains("browser"), "{rule}");
        assert!(rule.contains('├') && rule.contains('┤'), "{rule}");
        let rule_x = column_of(&rule, "├");
        assert_eq!(
            buffer.cell((rule_x, rule_row)).map(|cell| cell.fg),
            Some(Color::Cyan),
            "the divider should be highlighted, not dim"
        );
    }

    #[test]
    fn tab_cycles_through_decks_and_browser() {
        let mut app = test_app();
        assert_eq!(app.target, Target::Deck(0));
        app.cycle_target(1);
        assert_eq!(app.target, Target::Deck(1));
        app.cycle_target(1);
        assert_eq!(app.target, Target::Browser);
        assert_eq!(app.target.prefix(), "");
        assert_eq!(app.default_deck(), 1);
        app.cycle_target(1);
        assert_eq!(app.target, Target::Deck(0));
        app.cycle_target(-1);
        assert_eq!(app.target, Target::Browser);
    }

    #[test]
    fn playhead_is_a_red_line_near_the_center() {
        let mut app = test_app();
        let buffer = render_buffer(&mut app);
        let reds: Vec<u16> = buffer
            .content()
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.fg == Color::Red)
            .map(|(index, _)| (index % buffer.area.width as usize) as u16)
            .collect();
        assert!(!reds.is_empty(), "no playhead drawn");
        let center = buffer.area.width / 2;
        assert!(
            reds.iter().all(|x| x.abs_diff(center) <= 3),
            "playhead off center: {reds:?}"
        );
    }

    #[test]
    fn waveform_uses_braille() {
        let mut app = test_app();
        let buffer = render_buffer(&mut app);
        let has_braille = buffer.content().iter().any(|cell| {
            cell.symbol()
                .chars()
                .any(|ch| (0x2800..=0x28FF).contains(&(ch as u32)))
        });
        assert!(has_braille, "waveform drew no braille");
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let mut app = test_app();
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::tui::view::render(frame, &mut app))
            .unwrap();
    }
}
