//! The `--midi-guide [map.toml]` front-end.
//!
//! A learn-mode editor for a MIDI map. It deliberately starts **no audio engine**: it needs only a
//! MIDI input, the map file and a terminal, so a controller can be mapped on a machine with no
//! sound card. The interaction lives in [`hypermixx_midi::guide`]; this module owns the terminal,
//! the raw-event monitor, the in-TUI port/file pickers and the file write.
//!
//! ```text
//! ┌─ targets ──────────────┬─ raw events ─────────┐
//! │ ▶ ✓ deck0  play        │ 12:03:41  CC  ch1 #7 = 64 │
//! │   — deck0  pause       │ ...                  │
//! └────────────────────────┴──────────────────────┘
//! │ status / detected control                        │
//! └──────────────────────────────────────────────────┘
//! ```

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hypermixx_midi::{describe, event_label, ports, BindStatus, Guide, Input, Received};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::tui::picker::{self, FilePicker, PortList};

/// ~30Hz, matching the main TUI.
const TICK: Duration = Duration::from_millis(33);
/// Raw monitor lines kept; older ones scroll off.
const MONITOR_CAP: usize = 500;
/// Rows a `PageUp`/`PageDown` moves the monitor.
const MONITOR_PAGE: usize = 10;

/// Runs the guide until the user quits. Returns an error for a bad file or terminal; a missing
/// MIDI port is not fatal — the port picker opens instead.
pub fn run(map_path: &str, port: Option<&str>, decks: usize) -> Result<(), String> {
    let mut session = Session::new(map_path, port, decks)?;
    let mut terminal = ratatui::try_init().map_err(|err| format!("terminal: {err}"))?;
    let outcome = event_loop(&mut terminal, &mut session);
    ratatui::restore();
    outcome
}

/// Everything the guide UI owns: the map being edited, the live input, and the raw monitor.
struct Session {
    guide: Guide,
    decks: usize,
    map_path: String,
    port: Option<String>,
    input: Option<Input>,
    received: Option<Receiver<Received>>,
    monitor: VecDeque<String>,
    /// Lines scrolled up from the bottom (0 = following).
    monitor_offset: usize,
    overlay: Overlay,
    confirm_quit: bool,
}

/// A modal popup. While one is open, keys go to it rather than the target list.
enum Overlay {
    None,
    Ports(PortList),
    Files(FilePicker),
}

impl Session {
    fn new(map_path: &str, port: Option<&str>, decks: usize) -> Result<Self, String> {
        let text = read_map(map_path)?;
        let guide = Guide::new(decks, text.as_deref()).map_err(|err| format!("{map_path}: {err}"))?;
        let mut session = Self {
            guide,
            decks,
            map_path: map_path.to_owned(),
            port: None,
            input: None,
            received: None,
            monitor: VecDeque::new(),
            monitor_offset: 0,
            overlay: Overlay::None,
            confirm_quit: false,
        };
        // An explicit port must work; with none, a single attached controller is unambiguous.
        // Anything else (zero or several ports) opens the picker rather than failing the launch.
        match port {
            Some(port) => {
                session.open_port(port)?;
            }
            None => match single_port() {
                Some(port) => {
                    let _ = session.open_port(&port);
                }
                None => {
                    session.overlay = Overlay::Ports(PortList::refresh());
                    session.guide.note("选择 MIDI 输入端口");
                }
            },
        }
        Ok(session)
    }

    /// Closes any current input and opens `name`. On failure the port picker stays open so another
    /// port can be tried.
    fn open_port(&mut self, name: &str) -> Result<(), String> {
        let (tx, rx): (Sender<Received>, Receiver<Received>) = crossbeam_channel::unbounded();
        let input = ports::open(name, tx).map_err(|err| format!("midi: {err}"))?;
        // Replacing `input` drops the previous connection, closing its callback thread.
        self.input = Some(input);
        self.received = Some(rx);
        self.port = Some(name.to_owned());
        self.overlay = Overlay::None;
        self.guide.note(format!("已连接 MIDI:{name}"));
        Ok(())
    }

    /// Replaces the edited map with the file at `path` (must parse and validate).
    fn load_map(&mut self, path: &Path) -> Result<(), String> {
        let path_str = path.display().to_string();
        let text = read_map(&path_str)?;
        self.guide = Guide::new(self.decks, text.as_deref())
            .map_err(|err| format!("{path_str}: {err}"))?;
        self.map_path = path_str.clone();
        self.overlay = Overlay::None;
        self.guide.note(format!("已加载 {path_str}"));
        Ok(())
    }

    /// Reads every buffered event into the monitor and the guide.
    fn drain_events(&mut self) {
        let Some(received) = &self.received else {
            return;
        };
        while let Ok(received) = received.try_recv() {
            self.monitor.push_back(format!(
                "{}  {}",
                clock(received.at),
                event_label(&received.event)
            ));
            if self.monitor.len() > MONITOR_CAP {
                self.monitor.pop_front();
            }
            self.guide.feed(&received.event);
        }
    }

    fn scroll_monitor(&mut self, delta: isize) {
        let max = self.monitor.len();
        let current = self.monitor_offset as isize;
        self.monitor_offset = (current + delta).clamp(0, max as isize) as usize;
    }
}

/// With no explicit `--midi`, a single attached controller is unambiguous.
fn single_port() -> Option<String> {
    match ports::list_ports() {
        Ok(available) if available.len() == 1 => Some(available[0].name.clone()),
        _ => None,
    }
}

fn read_map(path: &str) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{path}: {err}")),
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    session: &mut Session,
) -> Result<(), String> {
    let mut next_tick = Instant::now() + TICK;
    loop {
        session.drain_events();
        terminal
            .draw(|frame| render(frame, session))
            .map_err(|err| format!("draw: {err}"))?;

        let mut quit = false;
        loop {
            let now = Instant::now();
            if now >= next_tick {
                break;
            }
            if event::poll(next_tick - now).map_err(|err| format!("input: {err}"))? {
                let Event::Key(key) = event::read().map_err(|err| format!("input: {err}"))? else {
                    continue;
                };
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                if handle_key(session, key) {
                    quit = true;
                }
            } else {
                break;
            }
        }
        if quit {
            break;
        }
        next_tick += TICK;
        if next_tick <= Instant::now() {
            next_tick = Instant::now() + TICK;
        }
    }
    Ok(())
}

/// Returns `true` when the loop should exit.
fn handle_key(session: &mut Session, key: KeyEvent) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    // A popup owns the keyboard while it is open.
    if !matches!(session.overlay, Overlay::None) {
        return handle_overlay(session, key);
    }
    match key.code {
        KeyCode::Char('q') => {
            if session.confirm_quit || !session.guide.is_dirty() {
                return true;
            }
            session.confirm_quit = true;
            session.guide.note("有未保存改动 — 再按 q 放弃并退出,或 s 保存");
        }
        KeyCode::Char('n') => {
            session.confirm_quit = false;
            session.guide.note("继续编辑");
        }
        KeyCode::Char('s') => match save_map(&session.map_path, &session.guide) {
            Ok(()) => session.guide.mark_saved(),
            Err(err) => session.guide.note(format!("保存失败: {err}")),
        },
        KeyCode::Char('p') => {
            session.overlay = Overlay::Ports(PortList::refresh());
            session.guide.note("选择 MIDI 输入端口(Enter 连接)");
        }
        KeyCode::Char('f') => {
            session.overlay = Overlay::Files(FilePicker::new(
                Some(Path::new(&session.map_path)),
                &["toml"],
                "map file",
            ));
            session.guide.note("选择映射文件(Enter 加载)");
        }
        KeyCode::Up => session.guide.move_selection(-1),
        KeyCode::Down => session.guide.move_selection(1),
        KeyCode::PageUp => session.scroll_monitor(MONITOR_PAGE as isize),
        KeyCode::PageDown => session.scroll_monitor(-(MONITOR_PAGE as isize)),
        KeyCode::Enter => session.guide.arm(),
        KeyCode::Char(' ') => {
            if let Err(err) = session.guide.confirm() {
                session.guide.note(err);
            }
        }
        KeyCode::Char('m') => session.guide.cycle_mode(),
        KeyCode::Char('x') => session.guide.clear(),
        KeyCode::Esc => {
            if session.confirm_quit {
                session.confirm_quit = false;
                session.guide.note("继续编辑");
            } else {
                session.guide.disarm();
            }
        }
        _ => {}
    }
    false
}

fn handle_overlay(session: &mut Session, key: KeyEvent) -> bool {
    // Take the overlay out so the handlers can borrow `session` mutably for connect/load.
    let overlay = std::mem::replace(&mut session.overlay, Overlay::None);
    match overlay {
        Overlay::None => {}
        Overlay::Ports(mut list) => match key.code {
            KeyCode::Esc => {}
            KeyCode::Up => {
                list.move_selection(-1);
                session.overlay = Overlay::Ports(list);
            }
            KeyCode::Down => {
                list.move_selection(1);
                session.overlay = Overlay::Ports(list);
            }
            KeyCode::Enter => {
                if let Some(port) = list.selected_port() {
                    if session.open_port(&port.name).is_err() {
                        // Keep the picker so another port can be chosen.
                        session.overlay = Overlay::Ports(list);
                    }
                } else {
                    session.overlay = Overlay::Ports(list);
                }
            }
            _ => session.overlay = Overlay::Ports(list),
        },
        Overlay::Files(mut picker) => match key.code {
            KeyCode::Esc => {}
            KeyCode::Up => {
                picker.move_selection(-1);
                session.overlay = Overlay::Files(picker);
            }
            KeyCode::Down => {
                picker.move_selection(1);
                session.overlay = Overlay::Files(picker);
            }
            KeyCode::Backspace | KeyCode::Left => {
                picker.up();
                session.overlay = Overlay::Files(picker);
            }
            KeyCode::Enter => match picker.activate() {
                Some(path) => {
                    if session.load_map(&path).is_err() {
                        session.overlay = Overlay::Files(picker);
                    }
                }
                None => session.overlay = Overlay::Files(picker),
            },
            _ => session.overlay = Overlay::Files(picker),
        },
    }
    false
}

/// Serialises and atomically replaces the map: a crash mid-write leaves the old file intact.
fn save_map(path: &str, guide: &Guide) -> Result<(), String> {
    let text = guide.to_toml().map_err(|err| err.to_string())?;
    let temp = format!("{path}.tmp");
    std::fs::write(&temp, &text).map_err(|err| format!("{temp}: {err}"))?;
    std::fs::rename(&temp, path).map_err(|err| format!("{path}: {err}"))?;
    Ok(())
}

fn render(frame: &mut Frame, session: &Session) {
    let area = frame.area();
    let chunks = Layout::vertical([Constraint::Min(5), Constraint::Length(3)]).split(area);
    let columns = Layout::horizontal([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(chunks[0]);
    render_targets(frame, columns[0], &session.guide);
    render_monitor(frame, columns[1], session);
    render_status(frame, chunks[1], session);
    match &session.overlay {
        Overlay::None => {}
        Overlay::Ports(list) => picker::render_port_list(frame, area, list, "MIDI input (Enter)"),
        Overlay::Files(picker) => picker::render_file_picker(frame, area, picker),
    }
}

fn render_targets(frame: &mut Frame, area: Rect, guide: &Guide) {
    let items: Vec<ListItem> = guide
        .targets()
        .iter()
        .map(|target| {
            let (marker, color) = match guide.status(target) {
                BindStatus::Bound => ("✓", Color::Green),
                BindStatus::Unbound => ("—", Color::Blue),
                BindStatus::Edited => ("✎", Color::Yellow),
            };
            ListItem::new(format!("{marker} {}", target.label))
                .style(Style::default().fg(color))
        })
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" targets "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(Some(guide.selected()));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_monitor(frame: &mut Frame, area: Rect, session: &Session) {
    let inner = Block::bordered().inner(area);
    let height = inner.height as usize;
    let total = session.monitor.len();
    let bottom = total.saturating_sub(session.monitor_offset);
    let start = bottom.saturating_sub(height);
    let lines: Vec<Line> = session
        .monitor
        .iter()
        .skip(start)
        .take(height)
        .map(|line| Line::from(line.clone()))
        .collect();
    let title = if session.monitor_offset > 0 {
        format!(" raw events [↑{}] ", session.monitor_offset)
    } else {
        " raw events ".to_owned()
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered().title(title)),
        area,
    );
}

fn render_status(frame: &mut Frame, area: Rect, session: &Session) {
    let guide = &session.guide;
    let dirty = if guide.is_dirty() { " *" } else { "" };
    let port = session.port.as_deref().unwrap_or("<no port>");
    let title = format!(" guide — {port} → {}{dirty} ", session.map_path);
    let status = guide
        .message()
        .map(str::to_owned)
        .unwrap_or_else(|| "Enter 捕获控件,↑↓ 选择目标".to_owned());
    let detection = match guide.candidate() {
        Some(candidate) => format!("检出:{}", describe(candidate)),
        None if guide.is_armed() => "检出:等待按压/拨动…".to_owned(),
        None => "检出:—".to_owned(),
    };
    let hints = if session.confirm_quit {
        "再按 q 放弃并退出 / n 继续编辑".to_owned()
    } else {
        "[空格]确认 [m]模式 [x]清除 [s]保存 [p]端口 [f]文件 [PgUp/PgDn]滚动 [q]退出"
            .to_owned()
    };
    let line = Line::from(vec![
        Span::styled(detection, Style::default().fg(Color::Cyan)),
        Span::raw("   "),
        Span::styled(hints, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(
        Paragraph::new(Text::from(vec![Line::from(status), line]))
            .block(Block::bordered().title(title)),
        area,
    );
}

/// Wall-clock `HH:MM:SS` (UTC) for a monitor line. No timezone crate is worth a dependency just
/// for this; the ordering of events is what matters.
fn clock(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_secs())
        .unwrap_or(0);
    format!(
        "{:02}:{:02}:{:02}",
        (seconds / 3600) % 24,
        (seconds / 60) % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_formats_within_a_day() {
        let at = UNIX_EPOCH + Duration::from_secs(3600 + 2 * 60 + 3);
        assert_eq!(clock(at), "01:02:03");
    }

    #[test]
    fn monitor_scrolling_clamps_to_the_buffered_lines() {
        let mut session = Session {
            guide: Guide::new(2, None).unwrap(),
            decks: 2,
            map_path: "midi-map.toml".to_owned(),
            port: None,
            input: None,
            received: None,
            monitor: (0..5).map(|index| index.to_string()).collect(),
            monitor_offset: 0,
            overlay: Overlay::None,
            confirm_quit: false,
        };
        session.scroll_monitor(3);
        assert_eq!(session.monitor_offset, 3);
        // Past the top, it stops at the number of lines.
        session.scroll_monitor(100);
        assert_eq!(session.monitor_offset, 5);
        // And back down to following.
        session.scroll_monitor(-100);
        assert_eq!(session.monitor_offset, 0);
    }
}
