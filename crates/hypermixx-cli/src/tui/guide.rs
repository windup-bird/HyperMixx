//! The `--midi-guide` front-end.
//!
//! Three columns (`deck | unit | slot`), an `info` card, a `watch dog` that carries the latest
//! prompt plus the raw MIDI stream, and five command buttons. The map editor stays deliberately
//! smaller than the main TUI: add units from presets, learn endpoints, clone a deck, save.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hypermixx_midi::guide::{Column, ColumnFocus, Entered, Row, View};
use hypermixx_midi::map::EventKind;
use hypermixx_midi::{ports, Guide, Input, Received};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::picker::{self, FilePicker, PortList};

const TICK: Duration = Duration::from_millis(33);
const WATCH_CAP: usize = 500;
const WATCH_PAGE: usize = 10;
const BLINK: Duration = Duration::from_millis(500);

pub fn run(map_path: &str, port: Option<&str>, decks: usize) -> Result<(), String> {
    let mut session = Session::new(map_path, port, decks)?;
    let mut terminal = ratatui::try_init().map_err(|err| format!("terminal: {err}"))?;
    let outcome = event_loop(&mut terminal, &mut session);
    ratatui::restore();
    outcome
}

struct Session {
    guide: Guide,
    open_hint: String,
    port: Option<String>,
    input: Option<Input>,
    received: Option<Receiver<Received>>,
    watch: VecDeque<WatchLine>,
    watch_scroll: usize,
    last_message: Option<String>,
    last_error: Option<String>,
    overlay: Overlay,
    command_focus: usize,
    command_active: bool,
    confirm_quit: bool,
    blink: bool,
    blink_at: Instant,
    quit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchLevel {
    Raw,
    Prompt,
    Ok,
    Warn,
    Error,
}

struct WatchLine {
    text: String,
    level: WatchLevel,
    key: Option<(EventKind, u8, u8)>,
}

enum Overlay {
    None,
    Ports(PortList),
    Files(FilePicker),
    Menu(Menu),
}

struct Menu {
    title: String,
    items: Vec<MenuItem>,
    selected: usize,
}

struct MenuItem {
    label: String,
    action: MenuAction,
}

#[derive(Clone, Debug)]
enum MenuAction {
    CloneDeck(usize),
    StartDeck,
    AddUnit(String),
}

impl Session {
    fn new(open_hint: &str, port: Option<&str>, decks: usize) -> Result<Self, String> {
        let guide = Guide::empty(decks)?;
        let mut session = Self {
            guide,
            open_hint: open_hint.to_owned(),
            port: None,
            input: None,
            received: None,
            watch: VecDeque::new(),
            watch_scroll: 0,
            last_message: None,
            last_error: None,
            overlay: Overlay::None,
            command_focus: 0,
            command_active: false,
            confirm_quit: false,
            blink: false,
            blink_at: Instant::now(),
            quit: false,
        };

        if Path::new(open_hint).exists() {
            session.open_map(Path::new(open_hint));
        } else {
            session.prompt(format!("open a map file — File ({open_hint})"));
        }

        let wanted = port.map(str::to_owned).or_else(|| session.guide.device());
        if let Some(name) = wanted {
            if let Err(err) = session.open_port(&name) {
                session.error(err);
                session.prompt("connect a controller — Connect");
            }
        } else {
            session.prompt("connect a controller — Connect");
        }
        session.sync_feedback();
        Ok(session)
    }

    fn open_port(&mut self, name: &str) -> Result<(), String> {
        let (tx, rx): (Sender<Received>, Receiver<Received>) = crossbeam_channel::unbounded();
        let input = ports::open(name, tx).map_err(|err| format!("midi: {err}"))?;
        self.input = Some(input);
        self.received = Some(rx);
        self.port = Some(name.to_owned());
        self.guide.set_device(Some(name.to_owned()));
        self.overlay = Overlay::None;
        self.ok(format!("connected `{name}`"));
        Ok(())
    }

    fn open_map(&mut self, path: &Path) {
        let path_text = path.display().to_string();
        match self.guide.open(&path_text) {
            Ok(()) => {
                self.open_hint = path_text.clone();
                self.overlay = Overlay::None;
                self.ok(format!("opened {path_text}"));
            }
            Err(err) => {
                self.error(err);
            }
        }
    }

    fn drain_events(&mut self) {
        let Some(received) = &self.received else {
            return;
        };
        let mut lines = Vec::new();
        while let Ok(received) = received.try_recv() {
            let key = event_key(&received.event);
            lines.push(WatchLine {
                text: format!("{}  {}", clock(received.at), event_label(&received.event)),
                level: WatchLevel::Raw,
                key,
            });
            let at_ms = received
                .at
                .duration_since(UNIX_EPOCH)
                .map(|delta| delta.as_millis() as u64)
                .ok();
            self.guide.feed_at(&received.event, at_ms);
        }
        for line in lines {
            // Keep the reader's place: the scroll is a distance from the bottom.
            if self.watch_scroll > 0 {
                self.watch_scroll += 1;
            }
            self.push_watch(line);
        }
    }

    fn push_watch(&mut self, line: WatchLine) {
        self.watch.push_back(line);
        while self.watch.len() > WATCH_CAP {
            self.watch.pop_front();
        }
    }

    fn prompt(&mut self, text: impl Into<String>) {
        self.push_watch(WatchLine {
            text: format!("› {}", text.into()),
            level: WatchLevel::Prompt,
            key: None,
        });
    }

    fn ok(&mut self, text: impl Into<String>) {
        self.push_watch(WatchLine {
            text: format!("✓ {}", text.into()),
            level: WatchLevel::Ok,
            key: None,
        });
        self.watch_scroll = 0;
    }

    fn warn(&mut self, text: impl Into<String>) {
        self.push_watch(WatchLine {
            text: format!("! {}", text.into()),
            level: WatchLevel::Warn,
            key: None,
        });
    }

    fn error(&mut self, text: impl Into<String>) {
        self.push_watch(WatchLine {
            text: format!("× {}", text.into()),
            level: WatchLevel::Error,
            key: None,
        });
    }

    /// Mirrors the model's current message/error into the watch dog exactly once.
    fn sync_feedback(&mut self) {
        let view = self.guide.view();
        if view.message != self.last_message {
            if let Some(message) = &view.message {
                self.prompt(message.clone());
            }
            self.last_message = view.message;
        }
        if view.error != self.last_error {
            if let Some(error) = &view.error {
                self.error(error.clone());
            }
            self.last_error = view.error;
        }
    }

    fn scroll_watch(&mut self, delta: isize) {
        let raw = self
            .watch
            .iter()
            .filter(|line| line.level == WatchLevel::Raw)
            .count();
        let current = self.watch_scroll as isize;
        self.watch_scroll = (current + delta).clamp(0, raw as isize) as usize;
    }

    fn tick_blink(&mut self) {
        if self.blink_at.elapsed() >= BLINK {
            self.blink = !self.blink;
            self.blink_at = Instant::now();
        }
    }

    fn reset_command(&mut self) {
        self.command_active = false;
        self.command_focus = 0;
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    session: &mut Session,
) -> Result<(), String> {
    let mut next_tick = Instant::now() + TICK;
    loop {
        session.drain_events();
        session.sync_feedback();
        session.tick_blink();
        terminal
            .draw(|frame| render(frame, session))
            .map_err(|err| format!("draw: {err}"))?;
        if session.quit {
            break;
        }

        loop {
            let now = Instant::now();
            if now >= next_tick {
                break;
            }
            if event::poll(next_tick - now).map_err(|err| format!("input: {err}"))? {
                match event::read().map_err(|err| format!("input: {err}"))? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        handle_key(session, key);
                    }
                    _ => {}
                }
                if session.quit {
                    break;
                }
            } else {
                break;
            }
        }
        if session.quit {
            break;
        }
        next_tick += TICK;
        if next_tick <= Instant::now() {
            next_tick = Instant::now() + TICK;
        }
    }
    Ok(())
}

fn handle_key(session: &mut Session, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        session.quit = true;
        return;
    }
    if !matches!(session.overlay, Overlay::None) {
        handle_overlay(session, key);
        return;
    }

    if session.command_active {
        match key.code {
            KeyCode::Tab | KeyCode::Right => {
                session.command_focus = (session.command_focus + 1) % COMMANDS;
            }
            KeyCode::BackTab | KeyCode::Left => {
                session.command_focus = (session.command_focus + COMMANDS - 1) % COMMANDS;
            }
            KeyCode::Enter => activate_command(session, session.command_focus),
            KeyCode::Esc => session.command_active = false,
            KeyCode::Up => {
                session.reset_command();
                session.guide.move_cursor(-1);
            }
            KeyCode::Down => {
                session.reset_command();
                session.guide.move_cursor(1);
            }
            KeyCode::PageUp => session.scroll_watch(WATCH_PAGE as isize),
            KeyCode::PageDown => session.scroll_watch(-(WATCH_PAGE as isize)),
            _ => {}
        }
        session.sync_feedback();
        return;
    }

    match key.code {
        KeyCode::Up => {
            session.reset_command();
            session.guide.move_cursor(-1);
        }
        KeyCode::Down => {
            session.reset_command();
            session.guide.move_cursor(1);
        }
        KeyCode::Left => {
            session.reset_command();
            session.guide.move_column(-1);
        }
        KeyCode::Right => {
            session.reset_command();
            session.guide.move_column(1);
        }
        KeyCode::Tab => {
            session.command_active = true;
            session.command_focus = 0;
        }
        KeyCode::BackTab => {
            session.command_active = true;
            session.command_focus = COMMANDS - 1;
        }
        KeyCode::Enter => column_enter(session),
        KeyCode::Esc => {
            if session.guide.is_capturing() {
                session.guide.cancel_learn();
            }
        }
        KeyCode::PageUp => session.scroll_watch(WATCH_PAGE as isize),
        KeyCode::PageDown => session.scroll_watch(-(WATCH_PAGE as isize)),
        _ => {}
    }
    session.sync_feedback();
}

fn column_enter(session: &mut Session) {
    if session.guide.is_capturing() {
        if let Err(err) = session.guide.confirm_learn() {
            session.error(err);
        }
        return;
    }
    match session.guide.enter() {
        Ok(Entered::Moved) | Ok(Entered::Learning) => {}
        Ok(Entered::AddUnit) => open_add_unit_menu(session),
        Ok(Entered::NewDeck) => open_new_deck_menu(session),
        Err(err) => session.error(err),
    }
}

fn open_add_unit_menu(session: &mut Session) {
    let group = session.guide.selected_group_label();
    let presets = session.guide.add_unit_presets();
    if presets.is_empty() {
        session.warn("no preset is available for this group");
        return;
    }
    session.overlay = Overlay::Menu(Menu {
        title: format!("add unit to {group} — preset"),
        items: presets
            .into_iter()
            .map(|preset| MenuItem {
                label: preset.clone(),
                action: MenuAction::AddUnit(preset),
            })
            .collect(),
        selected: 0,
    });
}

fn open_new_deck_menu(session: &mut Session) {
    session.overlay = Overlay::Menu(Menu {
        title: "how to map your new deck ?".to_owned(),
        items: vec![
            MenuItem {
                label: "clone from deck1".to_owned(),
                action: MenuAction::CloneDeck(0),
            },
            MenuItem {
                label: "map a new one".to_owned(),
                action: MenuAction::StartDeck,
            },
        ],
        selected: 0,
    });
}

const COMMANDS: usize = 6;

fn command_labels(view: &View, capturing: bool) -> [&'static str; COMMANDS] {
    let first = if capturing {
        "Confirm"
    } else if view.nav.focus == ColumnFocus::Slot && !view.slots.is_empty() {
        "Learn"
    } else {
        "Enter"
    };
    [first, "Delete", "Connect", "File", "Save", "Exit"]
}

fn activate_command(session: &mut Session, index: usize) {
    if index != COMMANDS - 1 {
        session.confirm_quit = false;
    }
    match index {
        0 => column_enter(session),
        1 => {
            if let Err(err) = session.guide.delete_selected() {
                session.warn(err);
            }
        }
        2 => session.overlay = Overlay::Ports(PortList::refresh()),
        3 => {
            let start = session
                .guide
                .path()
                .map(Path::new)
                .unwrap_or_else(|| Path::new(&session.open_hint));
            session.overlay = Overlay::Files(FilePicker::new(Some(start), &["toml"], "map file"));
        }
        4 => save(session),
        5 => {
            if session.guide.dirty() && !session.confirm_quit {
                session.confirm_quit = true;
                session.warn("unsaved changes — Exit again to discard, or Save");
            } else {
                session.quit = true;
            }
        }
        _ => {}
    }
    session.sync_feedback();
}

fn save(session: &mut Session) {
    if session.guide.path().is_some() {
        match session.guide.save() {
            Ok(()) => session.ok("saved"),
            Err(err) => session.error(err),
        }
    } else {
        let start = Path::new(&session.open_hint);
        let default_name = start
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("midi-map.toml");
        session.overlay = Overlay::Files(FilePicker::new_save(
            Some(start),
            &["toml"],
            "save map",
            default_name,
        ));
    }
}

fn handle_overlay(session: &mut Session, key: KeyEvent) {
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
                    if let Err(err) = session.open_port(&port.name) {
                        session.error(err);
                        session.overlay = Overlay::Ports(list);
                    }
                } else {
                    session.overlay = Overlay::Ports(list);
                }
            }
            _ => session.overlay = Overlay::Ports(list),
        },
        Overlay::Files(mut picker) if picker.save_mode() => match key.code {
            KeyCode::Esc => {}
            KeyCode::Up => {
                picker.move_selection(-1);
                session.overlay = Overlay::Files(picker);
            }
            KeyCode::Down => {
                picker.move_selection(1);
                session.overlay = Overlay::Files(picker);
            }
            KeyCode::Left => {
                picker.up();
                session.overlay = Overlay::Files(picker);
            }
            KeyCode::Right => {
                if let Some(entry) = picker.selected_entry() {
                    if entry.is_dir {
                        picker.activate();
                    } else {
                        picker.set_filename(entry.name);
                    }
                }
                session.overlay = Overlay::Files(picker);
            }
            KeyCode::Backspace => {
                picker.pop_filename_char();
                session.overlay = Overlay::Files(picker);
            }
            KeyCode::Enter => {
                if let Some(path) = picker.save_path() {
                    let path_text = path.display().to_string();
                    match session.guide.save_as(&path_text) {
                        Ok(()) => {
                            session.open_hint = path_text.clone();
                            session.ok(format!("saved {path_text}"));
                        }
                        Err(err) => {
                            session.error(err);
                            session.overlay = Overlay::Files(picker);
                        }
                    }
                } else {
                    session.error("save needs a file name");
                    session.overlay = Overlay::Files(picker);
                }
            }
            KeyCode::Char(ch) => {
                picker.push_filename_char(ch);
                session.overlay = Overlay::Files(picker);
            }
            _ => session.overlay = Overlay::Files(picker),
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
                    session.open_map(Path::new(&path));
                    if !matches!(session.overlay, Overlay::None) {
                        session.overlay = Overlay::Files(picker);
                    }
                }
                None => session.overlay = Overlay::Files(picker),
            },
            _ => session.overlay = Overlay::Files(picker),
        },
        Overlay::Menu(mut menu) => match key.code {
            KeyCode::Esc => {}
            KeyCode::Up => {
                if !menu.items.is_empty() {
                    menu.selected = (menu.selected + menu.items.len() - 1) % menu.items.len();
                }
                session.overlay = Overlay::Menu(menu);
            }
            KeyCode::Down => {
                if !menu.items.is_empty() {
                    menu.selected = (menu.selected + 1) % menu.items.len();
                }
                session.overlay = Overlay::Menu(menu);
            }
            KeyCode::Enter => {
                if let Some(item) = menu.items.get(menu.selected) {
                    match item.action.clone() {
                        MenuAction::CloneDeck(source) => match session.guide.clone_deck(source) {
                            Ok(target) => {
                                session.ok(format!("cloned into deck{}", target + 1));
                            }
                            Err(err) => session.error(err),
                        },
                        MenuAction::StartDeck => {
                            let target = session.guide.start_deck();
                            session.ok(format!("deck{} is ready for a unit", target + 1));
                            open_add_unit_menu(session);
                        }
                        MenuAction::AddUnit(preset) => {
                            if let Err(err) = session.guide.add_unit_selected(&preset) {
                                session.error(err);
                            }
                        }
                    }
                }
            }
            _ => session.overlay = Overlay::Menu(menu),
        },
    }
    session.sync_feedback();
}

fn render(frame: &mut Frame, session: &mut Session) {
    let view = session.guide.view();
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(3),
        Constraint::Fill(2),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    render_header(frame, chunks[0], session, &view);

    let columns = Layout::horizontal([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ])
    .split(chunks[1]);
    let focused = if session.command_active {
        None
    } else {
        Some(view.nav.focus)
    };
    render_column(
        frame,
        columns[0],
        " deck ",
        &view.decks,
        view.nav.deck,
        focused == Some(ColumnFocus::Deck),
        Column::Deck,
        session,
    );
    render_column(
        frame,
        columns[1],
        " unit ",
        &view.units,
        view.nav.unit,
        focused == Some(ColumnFocus::Unit),
        Column::Unit,
        session,
    );
    render_column(
        frame,
        columns[2],
        " slot ",
        &view.slots,
        view.nav.slot,
        focused == Some(ColumnFocus::Slot),
        Column::Slot,
        session,
    );

    let lower =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Fill(1)]).split(chunks[2]);
    render_info(frame, lower[0], &view);
    render_watch(frame, lower[1], session, &view);
    render_hint(frame, chunks[3], session, &view);
    render_commands(frame, chunks[4], session, &view);

    match &session.overlay {
        Overlay::None => {}
        Overlay::Ports(list) => picker::render_port_list(frame, area, list, "MIDI device (Enter)"),
        Overlay::Files(picker) => picker::render_file_picker(frame, area, picker),
        Overlay::Menu(menu) => render_menu(frame, area, menu),
    }
}

fn render_header(frame: &mut Frame, area: Rect, session: &Session, view: &View) {
    let file = view.path.as_deref().unwrap_or("<no file>");
    let controller = view
        .device
        .as_deref()
        .or(session.port.as_deref())
        .unwrap_or("<no controller>");
    let dirty = if view.dirty { " *" } else { "" };
    let white = Style::default().fg(Color::White);
    let spans = vec![
        Span::styled(
            " MIDI ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("file {file}{dirty}"), white),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("controller {controller}"), white),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[allow(clippy::too_many_arguments)]
fn render_column(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    rows: &[Row],
    selected: usize,
    focused: bool,
    column: Column,
    session: &Session,
) {
    let border = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(border))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("—", Style::default().fg(Color::DarkGray))),
            inner,
        );
        return;
    }
    let selected = selected.min(rows.len() - 1);
    let height = inner.height as usize;
    let start = if rows.len() <= height || selected < height {
        0
    } else {
        selected + 1 - height
    };
    let mut lines = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(start).take(height) {
        let active = index == selected;
        let style = if active {
            if column == Column::Slot && session.guide.is_capturing() {
                if session.blink {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                }
            } else if focused {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            }
        } else if row.muted {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        let marker = if active { "> " } else { "  " };
        lines.push(Line::from(Span::styled(
            format!("{marker}{}", row.label),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_info(frame: &mut Frame, area: Rect, view: &View) {
    let mut lines: Vec<Line> = view
        .details
        .iter()
        .map(|(key, value)| {
            Line::from(vec![
                Span::styled(format!(" {key}: "), Style::default().fg(Color::DarkGray)),
                Span::raw(value.clone()),
            ])
        })
        .collect();
    if view.capturing {
        if let Some(candidate) = &view.candidate {
            lines.push(Line::from(vec![
                Span::styled(" learn: ", Style::default().fg(Color::Cyan)),
                Span::styled(candidate.label(), Style::default().fg(Color::Cyan)),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                " learn: waiting for a control…",
                Style::default().fg(Color::Cyan),
            )));
        }
    }
    if let Some(error) = &view.error {
        lines.push(Line::from(Span::styled(
            format!(" {error}"),
            Style::default().fg(Color::Red),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" info "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_watch(frame: &mut Frame, area: Rect, session: &Session, view: &View) {
    let inner = Block::bordered().inner(area);
    let height = inner.height as usize;
    let mut lines: Vec<Line> = Vec::new();
    if let Some(prompt) = session
        .watch
        .iter()
        .rev()
        .find(|line| line.level != WatchLevel::Raw)
    {
        lines.push(watch_line(prompt, view, true));
    }
    let raw: Vec<&WatchLine> = session
        .watch
        .iter()
        .filter(|line| line.level == WatchLevel::Raw)
        .collect();
    let body = height.saturating_sub(lines.len());
    let bottom = raw.len().saturating_sub(session.watch_scroll);
    let start = bottom.saturating_sub(body);
    for line in raw.iter().skip(start).take(body) {
        lines.push(watch_line(line, view, false));
    }
    let title = if session.watch_scroll > 0 {
        format!(" watch dog [↑{}] ", session.watch_scroll)
    } else {
        " watch dog ".to_owned()
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered().title(title)),
        area,
    );
}

fn watch_line(line: &WatchLine, view: &View, pinned: bool) -> Line<'static> {
    let style = match line.level {
        WatchLevel::Prompt => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        WatchLevel::Ok => Style::default().fg(Color::Green),
        WatchLevel::Warn => Style::default().fg(Color::Yellow),
        WatchLevel::Error => Style::default().fg(Color::Red),
        WatchLevel::Raw if !pinned => Style::default().fg(Color::White),
        WatchLevel::Raw => Style::default().fg(Color::DarkGray),
    };
    let style = if line.key.is_some_and(|key| view.endpoints.contains(&key)) {
        Style::default().fg(Color::Black).bg(Color::Gray)
    } else {
        style
    };
    Line::styled(line.text.clone(), style)
}

fn render_hint(frame: &mut Frame, area: Rect, session: &Session, view: &View) {
    let text = if session.confirm_quit {
        "Exit again = discard · Save = write"
    } else if session.command_active {
        "Tab/←→ switch button · Enter run · Esc back"
    } else if view.capturing {
        "move the control · Enter confirm · Esc cancel"
    } else {
        "↑↓ move · ←→ column · Enter in/learn · Tab command · PgUp/PgDn watch"
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(Color::DarkGray))),
        area,
    );
}

fn render_commands(frame: &mut Frame, area: Rect, session: &Session, view: &View) {
    let labels = command_labels(view, view.capturing);
    let focus = session.command_focus.min(COMMANDS - 1);
    let spans: Vec<Span> = labels
        .iter()
        .enumerate()
        .flat_map(|(index, label)| {
            let style = if index == focus && session.command_active {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if index == focus {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::White)
            };
            [Span::styled(format!(" {label} "), style), Span::raw(" ")]
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

fn render_menu(frame: &mut Frame, area: Rect, menu: &Menu) {
    let height = (menu.items.len() as u16 + 2).clamp(3, (area.height.saturating_sub(4)).min(16));
    let width = (area.width * 3 / 4).clamp(24, area.width.saturating_sub(2));
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = menu
        .items
        .iter()
        .map(|item| ListItem::new(item.label.clone()))
        .collect();
    let list = List::new(items)
        .block(
            Block::bordered()
                .border_style(Style::default().fg(Color::Cyan))
                .title(menu.title.clone()),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut state = ListState::default().with_selected(Some(menu.selected));
    frame.render_stateful_widget(list, popup, &mut state);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

fn event_key(event: &hypermixx_midi::Event) -> Option<(EventKind, u8, u8)> {
    match *event {
        hypermixx_midi::Event::ControlChange {
            channel,
            controller,
            ..
        } => Some((EventKind::Cc, channel, controller)),
        hypermixx_midi::Event::NoteOn { channel, key, .. }
        | hypermixx_midi::Event::NoteOff { channel, key, .. } => {
            Some((EventKind::Note, channel, key))
        }
        hypermixx_midi::Event::PitchBend { channel, .. } => Some((EventKind::Bend, channel, 0)),
    }
}

fn event_label(event: &hypermixx_midi::Event) -> String {
    let channel = event.channel() + 1;
    match *event {
        hypermixx_midi::Event::ControlChange {
            controller, value, ..
        } => format!("CC   ch{channel} #{controller:<3} = {value}"),
        hypermixx_midi::Event::NoteOn { key, velocity, .. } => {
            format!("NOTE ch{channel} #{key:<3} on {velocity}")
        }
        hypermixx_midi::Event::NoteOff { key, .. } => format!("NOTE ch{channel} #{key:<3} off"),
        hypermixx_midi::Event::PitchBend { value, .. } => format!("BEND ch{channel} {value:+}"),
    }
}

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

    fn test_session() -> Session {
        Session::new("does-not-exist.toml", None, 2).unwrap()
    }

    #[test]
    fn the_clock_formats_within_a_day() {
        let at = UNIX_EPOCH + Duration::from_secs(3600 + 2 * 60 + 3);
        assert_eq!(clock(at), "01:02:03");
    }

    #[test]
    fn monitor_scrolling_clamps_to_the_raw_lines() {
        let mut session = test_session();
        session.watch.clear();
        for index in 0..5 {
            session.push_watch(WatchLine {
                text: index.to_string(),
                level: WatchLevel::Raw,
                key: None,
            });
        }
        session.scroll_watch(3);
        assert_eq!(session.watch_scroll, 3);
        session.scroll_watch(100);
        assert_eq!(session.watch_scroll, 5);
        session.scroll_watch(-100);
        assert_eq!(session.watch_scroll, 0);
    }

    #[test]
    fn arrow_keys_reset_the_command_focus() {
        let mut session = test_session();
        handle_key(
            &mut session,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
        );
        handle_key(
            &mut session,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
        );
        assert!(session.command_active);
        assert_eq!(session.command_focus, 1);
        handle_key(
            &mut session,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        assert!(!session.command_active);
        assert_eq!(session.command_focus, 0);
    }

    #[test]
    fn the_first_command_follows_the_column_and_capture() {
        let mut session = test_session();
        session.guide.select_deck(0);
        session.guide.add_unit_selected("transport").unwrap();
        let view = session.guide.view();
        assert_eq!(command_labels(&view, false)[0], "Enter");

        session.guide.select_slot(0, "play");
        let view = session.guide.view();
        assert_eq!(command_labels(&view, false)[0], "Learn");
        assert_eq!(command_labels(&view, true)[0], "Confirm");
        assert_eq!(
            &command_labels(&view, false)[1..],
            &["Delete", "Connect", "File", "Save", "Exit"]
        );
    }

    #[test]
    fn save_without_a_path_opens_a_named_file_picker() {
        let mut session = test_session();
        save(&mut session);
        let Overlay::Files(picker) = &session.overlay else {
            panic!("expected the save picker");
        };
        assert!(picker.save_mode());
        assert_eq!(picker.filename(), Some("does-not-exist.toml"));
    }

    #[test]
    fn the_new_deck_menu_always_offers_clone_and_map() {
        let mut session = test_session();
        while session.guide.selected_group_label() != "new" {
            session.guide.move_cursor(1);
        }
        open_new_deck_menu(&mut session);
        let Overlay::Menu(menu) = &session.overlay else {
            panic!("expected the new-deck menu");
        };
        let labels: Vec<&str> = menu.items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["clone from deck1", "map a new one"]);
    }

    #[test]
    fn the_shipped_map_renders() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = root.join("profile/midi-map.toml");
        let mut session = Session::new(&path.display().to_string(), None, 2).unwrap();
        assert!(
            session.guide.view().error.is_none(),
            "{:?}",
            session.guide.view().error
        );
        let text = render_once(&mut session, 100, 30);
        for label in [
            "global",
            "new",
            "info",
            "watch dog",
            "Delete",
            "Connect",
            "File",
            "Save",
            "Exit",
        ] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
        render_once(&mut session, 40, 15);
    }

    #[test]
    fn the_deck_column_starts_with_global_and_ends_with_new() {
        let session = test_session();
        let view = session.guide.view();
        assert_eq!(
            view.decks.first().map(|row| row.label.as_str()),
            Some("global")
        );
        assert_eq!(view.decks.last().map(|row| row.label.as_str()), Some("new"));
    }

    fn render_once(session: &mut Session, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, session))
            .expect("guide layout");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }
}
