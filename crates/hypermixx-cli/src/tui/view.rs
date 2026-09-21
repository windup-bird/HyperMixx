//! Everything that draws the TUI.
//!
//! Layout is fixed top-to-bottom: a one-line global header, one bordered band per deck (file name /
//! waveform / transport), the response log, and the command line. Only the decks use percentages,
//! so a taller terminal gives the waveforms the extra rows; global metrics live in the header and
//! command line rather than in a block of their own.

use hypermixx_audio::OUTPUT_RING_CAPACITY;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::response::{time, Level, LogLine};
use crate::tui::app::{App, COMPLETION_ROWS};
use crate::tui::waveform_view;

/// Share of the height the response log gets. The decks split the rest evenly, so the log stays
/// readable while the waveforms still grow with the terminal.
const RESPONSE_PCT: u16 = 34;

pub fn render(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical(layout_constraints(app.decks)).split(frame.area());
    render_top(frame, chunks[0], app);
    for index in 0..app.decks {
        render_deck(frame, chunks[1 + index], app, index);
    }
    render_log(frame, chunks[1 + app.decks], app);
    let command_area = chunks[2 + app.decks];
    render_command(frame, command_area, app);
    render_completion(frame, frame.area(), command_area, app);
}

fn layout_constraints(decks: usize) -> Vec<Constraint> {
    let decks = decks.max(1) as u16;
    let deck_pct = ((100 - RESPONSE_PCT) / decks).max(1);
    let mut constraints = vec![Constraint::Length(1)];
    for _ in 0..decks {
        constraints.push(Constraint::Percentage(deck_pct));
    }
    constraints.push(Constraint::Percentage(RESPONSE_PCT));
    // Three rows: a border top and bottom around the one-line input field.
    constraints.push(Constraint::Length(3));
    constraints
}

fn render_top(frame: &mut Frame, area: Rect, app: &App) {
    let peak = app.meters.map_or(0.0, |meters| meters.master_peak);
    let cue = app.meters.map_or(0.0, |meters| meters.cue_peak);
    let overruns = app.meters.map_or(0, |meters| meters.overruns);
    let ring_ms = OUTPUT_RING_CAPACITY as f64 * 1000.0 / f64::from(app.sample_rate);

    let mut spans = vec![
        Span::styled(
            format!(" HYPERMIXX {} ", app.version),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {}Hz ", app.sample_rate)),
        Span::raw(format!(" {:?} ", app.backend)),
        Span::raw(format!(" {} decks ", app.decks)),
        Span::raw(format!(" ui {:.1}ms ", app.ui_frame_ms)),
        Span::raw(format!(
            " poll {:.1}ms ",
            app.state_latency.as_secs_f64() * 1000.0
        )),
        Span::raw(format!(" ring {ring_ms:.0}ms ")),
        Span::raw(format!(" out {} ", db(peak))),
        Span::raw(format!(" cue {} ", db(cue))),
    ];
    let dropped = if overruns > 0 { Color::Red } else { Color::DarkGray };
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
    let focused = index == app.focused;
    let border = if focused { Color::Cyan } else { Color::DarkGray };
    let block = Block::bordered()
        .border_style(Style::default().fg(border))
        .title(format!(" Deck{index} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    frame.render_widget(Paragraph::new(deck_header(app, index)), rows[0]);

    let playhead = app.states[index].current_frame as f64;
    // `fit` reads this back next keypress, so the viewport width is always fresh.
    app.wave_width_dots = rows[1].width as usize * 2;
    waveform_view::render(
        frame,
        rows[1],
        app.waveforms[index].as_ref(),
        app.grids[index].as_ref(),
        playhead,
        app.frames_per_dot,
    );

    frame.render_widget(Paragraph::new(deck_info(app, index)), rows[2]);
}

fn deck_header(app: &App, index: usize) -> Line<'static> {
    let state = &app.states[index];
    let title = app.titles[index].as_deref().unwrap_or("<no track>");
    let bpm = if state.bpm > 0.0 {
        format!("{:.1} BPM", state.bpm)
    } else {
        "-- BPM".to_owned()
    };
    let key = state.key.clone().unwrap_or_else(|| "--".to_owned());
    Line::from(vec![
        Span::styled(
            format!(" {title}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "   {bpm}  {key}   rate {:.2}  {}",
            app.rates[index], app.profiles[index]
        )),
    ])
}

fn deck_info(app: &App, index: usize) -> Line<'static> {
    let state = &app.states[index];
    let glyph = if state.total_frames == 0 {
        "·"
    } else if state.playing {
        "▶"
    } else {
        "‖"
    };
    let total = if state.total_frames > 0 {
        time(state.total_frames)
    } else {
        "--:--.---".to_owned()
    };
    let remaining = if state.total_frames > state.current_frame {
        format!("-{}", time(state.total_frames - state.current_frame))
    } else {
        "-0:00.000".to_owned()
    };
    let phase = app.grids[index]
        .as_ref()
        .filter(|grid| !grid.is_empty())
        .map(|grid| {
            let beat = grid.floor_beat(state.current_frame) as f64
                + f64::from(grid.phase(state.current_frame));
            format!("   beat {beat:.1}")
        })
        .unwrap_or_default();
    Line::raw(format!(
        " {glyph} {} / {total}   remain {remaining}{phase}   [{}]",
        time(state.current_frame),
        state.current_frame,
    ))
}

fn render_log(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" response ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let height = inner.height as usize;
    let bottom = app.log.len().saturating_sub(app.log_offset);
    let start = bottom.saturating_sub(height);
    let lines: Vec<Line> = app
        .log
        .iter()
        .skip(start)
        .take(height)
        .map(log_line)
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn log_line(line: &LogLine) -> Line<'static> {
    match line.level {
        Level::Error => Line::styled(line.text.clone(), Style::default().fg(Color::Red)),
        Level::Info => Line::raw(line.text.clone()),
    }
}

fn render_command(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Cyan))
        .title(" command — Enter 执行/补全 · Tab 切换 deck · ↑↓ 历史 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // The focused deck is the implicit target, shown at the front of the line.
    let prefix = format!("deck{} ▸ ", app.focused);
    let text = app.command_string();
    let mut spans = vec![Span::styled(prefix.clone(), Style::default().fg(Color::Cyan))];
    if text.is_empty() {
        spans.push(Span::styled(
            "输入命令，如 load test.mp3 128 或 fx add eq",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::raw(text));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);

    let offset = prefix.chars().count() as u16 + app.cursor as u16;
    let x = (inner.x + offset).min(inner.right().saturating_sub(1));
    frame.set_cursor_position((x, inner.y));
}

fn render_completion(frame: &mut Frame, area: Rect, command_area: Rect, app: &App) {
    let Some(state) = &app.completion else {
        return;
    };
    if state.completion.items.is_empty() {
        return;
    }
    let rows = state.completion.items.len().min(COMPLETION_ROWS) as u16;
    let height = rows + 2;
    if command_area.y < height + 1 {
        return;
    }
    let width = area.width.saturating_sub(4).clamp(20, 64);
    let rect = Rect::new(area.x + 1, command_area.y - height, width, height);
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
        app.waveforms[0] = Some(Arc::new(Waveform::from_buckets(
            buckets,
            256,
            100_000,
        )));
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

    #[test]
    fn header_lands_on_the_first_row() {
        let mut app = test_app();
        let buffer = render_buffer(&mut app);
        assert!(row_text(&buffer, 0).contains("HYPERMIXX"));
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
        terminal.draw(|frame| crate::tui::view::render(frame, &mut app)).unwrap();
    }
}
