//! The `--midi-guide` front-end: a tree editor for a unit map.
//!
//! There is no wizard and no stage. The left pane is the map tree (decks → units → controls /
//! modes / actions / selectors); the right pane is the selected node plus the raw MIDI monitor,
//! split in half. The bottom bar is a cfdisk-style row of small, centred buttons: each stage's
//! context actions followed by the four persistent ones (`Connect`, `Open`, `Save`, `Exit`).
//!
//! Nothing is loaded or connected at startup. The user chooses a file with `Open` and a port with
//! `Connect`; the selected port is written into `[meta].device` in memory and persisted by `Save`.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use hypermixx_midi::guide::{TreeButton, TreeNodeKind, TreeView};
use hypermixx_midi::map::{Edge, EventKind, RawSelectorBehavior};
use hypermixx_midi::{ports, Guide, Input, Received};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::picker::{self, FilePicker, PortList};

const TICK: Duration = Duration::from_millis(33);
const MONITOR_CAP: usize = 500;
const MONITOR_PAGE: usize = 10;

pub fn run(map_path: &str, _port: Option<&str>, decks: usize) -> Result<(), String> {
    // The path and port are hints only. The guide starts empty; `Open`/`Connect` do the real work.
    let mut session = Session::new(map_path, decks)?;
    let mut terminal = ratatui::try_init().map_err(|err| format!("terminal: {err}"))?;
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let outcome = event_loop(&mut terminal, &mut session);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    outcome
}

struct Session {
    guide: Guide,
    decks: usize,
    /// Where the `Open` picker starts; `--midi-guide <file>` sets it but does not load the file.
    open_hint: String,
    port: Option<String>,
    input: Option<Input>,
    received: Option<Receiver<Received>>,
    monitor: VecDeque<MonitorLine>,
    monitor_offset: usize,
    overlay: Overlay,
    button_focus: usize,
    button_rects: Vec<(Rect, TreeButton)>,
    list_rect: Rect,
    list_offset: usize,
    confirm_quit: bool,
    quit: bool,
}

struct MonitorLine {
    text: String,
    key: Option<(EventKind, u8, u8)>,
}

enum Overlay {
    None,
    Ports(PortList),
    Files(FilePicker),
    Menu(Menu),
    Text(TextPrompt),
}

struct Menu {
    title: String,
    items: Vec<String>,
    selected: usize,
    action: MenuAction,
}

#[derive(Clone, Debug)]
enum MenuAction {
    NewUnitPreset,
    NewUnitDeck { preset: String },
    CloneDeck,
    NewSelector,
    SelectorTarget,
    ActionPick,
    SelectorBehavior,
}

struct TextPrompt {
    title: String,
    buffer: String,
    action: TextAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextAction {
    NewMode,
    ActionLine,
    SaveAs,
}

impl Session {
    fn new(open_hint: &str, decks: usize) -> Result<Self, String> {
        let guide = Guide::empty(decks)?;
        Ok(Self {
            guide,
            decks: decks.max(1),
            open_hint: open_hint.to_owned(),
            port: None,
            input: None,
            received: None,
            monitor: VecDeque::new(),
            monitor_offset: 0,
            overlay: Overlay::None,
            button_focus: 0,
            button_rects: Vec::new(),
            list_rect: Rect::default(),
            list_offset: 0,
            confirm_quit: false,
            quit: false,
        })
    }

    fn open_port(&mut self, name: &str) -> Result<(), String> {
        let (tx, rx): (Sender<Received>, Receiver<Received>) = crossbeam_channel::unbounded();
        let input = ports::open(name, tx).map_err(|err| format!("midi: {err}"))?;
        self.input = Some(input);
        self.received = Some(rx);
        self.port = Some(name.to_owned());
        self.guide.set_device(Some(name.to_owned()));
        self.overlay = Overlay::None;
        self.guide.note(format!("connected `{name}`; Save writes it to [meta].device"));
        Ok(())
    }

    fn open_map(&mut self, path: &Path) -> Result<(), String> {
        let path = path.display().to_string();
        self.guide.open(&path)?;
        self.open_hint = path.clone();
        self.overlay = Overlay::None;
        self.guide.note(format!("opened {path}"));
        Ok(())
    }

    fn drain_events(&mut self) {
        let Some(received) = &self.received else {
            return;
        };
        while let Ok(received) = received.try_recv() {
            let key = event_key(&received.event);
            self.monitor.push_back(MonitorLine {
                text: format!("{}  {}", clock(received.at), event_label(&received.event)),
                key,
            });
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
                match event::read().map_err(|err| format!("input: {err}"))? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if handle_key(session, key) {
                            quit = true;
                        }
                    }
                    Event::Mouse(mouse) => handle_mouse(session, mouse),
                    _ => {}
                }
                if session.quit {
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

fn handle_key(session: &mut Session, key: KeyEvent) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    if !matches!(session.overlay, Overlay::None) {
        handle_overlay(session, key);
        return false;
    }
    match key.code {
        KeyCode::Up => session.guide.move_tree_selection(-1),
        KeyCode::Down => session.guide.move_tree_selection(1),
        KeyCode::Tab | KeyCode::Right => move_button(session, 1),
        KeyCode::Left => move_button(session, -1),
        KeyCode::Enter => {
            let view = session.guide.tree_view();
            if let Some(button) = view
                .buttons
                .get(session.button_focus.min(view.buttons.len().saturating_sub(1)))
                .copied()
            {
                activate_button(session, button);
            }
        }
        KeyCode::PageUp => session.scroll_monitor(MONITOR_PAGE as isize),
        KeyCode::PageDown => session.scroll_monitor(-(MONITOR_PAGE as isize)),
        KeyCode::Esc => {
            if session.guide.is_capturing() {
                session.guide.cancel_learn();
            }
        }
        _ => {}
    }
    false
}

fn move_button(session: &mut Session, delta: isize) {
    let len = session.guide.tree_view().buttons.len();
    if len == 0 {
        return;
    }
    session.button_focus =
        (session.button_focus as isize + delta).rem_euclid(len as isize) as usize;
}

fn activate_button(session: &mut Session, button: TreeButton) {
    if button != TreeButton::Exit {
        session.confirm_quit = false;
    }
    let result = match button {
        TreeButton::Learn => session.guide.tree_begin_learn(),
        TreeButton::NewUnit => {
            let items = session.guide.preset_names();
            if items.is_empty() {
                Err("no presets available".to_owned())
            } else {
                session.overlay = Overlay::Menu(Menu {
                    title: "new unit — preset".to_owned(),
                    items,
                    selected: 0,
                    action: MenuAction::NewUnitPreset,
                });
                Ok(())
            }
        }
        TreeButton::Clone => {
            let deck = session.guide.tree_selected_unit_deck();
            let Some(current) = deck else {
                return session
                    .guide
                    .note("select a unit to clone".to_owned());
            };
            let items: Vec<String> = (0..session.decks)
                .filter(|deck| *deck as u8 != current)
                .map(|deck| format!("deck {}", deck + 1))
                .collect();
            if items.is_empty() {
                session.guide.note("no other deck to clone to".to_owned());
            } else {
                session.overlay = Overlay::Menu(Menu {
                    title: "clone to deck".to_owned(),
                    items,
                    selected: 0,
                    action: MenuAction::CloneDeck,
                });
            }
            Ok(())
        }
        TreeButton::NewMode => {
            session.overlay = Overlay::Text(TextPrompt {
                title: "new mode name".to_owned(),
                buffer: String::new(),
                action: TextAction::NewMode,
            });
            Ok(())
        }
        TreeButton::NewSelector => {
            session.overlay = Overlay::Menu(Menu {
                title: "new selector — behavior".to_owned(),
                items: vec!["Trigger".to_owned(), "Cycle".to_owned()],
                selected: 0,
                action: MenuAction::NewSelector,
            });
            Ok(())
        }
        TreeButton::Delete => session.guide.tree_delete(),
        TreeButton::Up => session.guide.tree_move_mode(-1),
        TreeButton::Down => session.guide.tree_move_mode(1),
        TreeButton::SetStart => session.guide.tree_set_start(),
        TreeButton::Edit => match session.guide.selected_tree_kind() {
            Some(TreeNodeKind::Action { .. }) => {
                let mut items = vec!["<type a line>".to_owned()];
                items.extend(
                    hypermixx_midi::guide::action_names()
                        .into_iter()
                        .map(str::to_owned),
                );
                session.overlay = Overlay::Menu(Menu {
                    title: "action line".to_owned(),
                    items,
                    selected: 0,
                    action: MenuAction::ActionPick,
                });
                Ok(())
            }
            Some(TreeNodeKind::Selector { .. }) => {
                let modes = session.guide.mode_names();
                let mut items = vec!["cycle".to_owned()];
                items.extend(modes.into_iter().map(|mode| format!("trigger {mode}")));
                session.overlay = Overlay::Menu(Menu {
                    title: "selector behavior".to_owned(),
                    items,
                    selected: 0,
                    action: MenuAction::SelectorBehavior,
                });
                Ok(())
            }
            _ => Err("select an action or selector first".to_owned()),
        },
        TreeButton::On => {
            let next = match session.guide.tree_on() {
                Some(_) => None,
                None => Some(Edge::Release),
            };
            session.guide.tree_set_on(next)
        }
        TreeButton::Confirm => session.guide.tree_confirm_learn(),
        TreeButton::Cancel => {
            session.guide.cancel_learn();
            Ok(())
        }
        TreeButton::Mode => {
            session.guide.cycle_candidate_mode();
            Ok(())
        }
        TreeButton::Connect => {
            session.overlay = Overlay::Ports(PortList::refresh());
            Ok(())
        }
        TreeButton::Open => {
            let start = session
                .guide
                .path()
                .map(Path::new)
                .unwrap_or_else(|| Path::new(&session.open_hint));
            session.overlay = Overlay::Files(FilePicker::new(
                Some(start),
                &["toml"],
                "map file",
            ));
            Ok(())
        }
        TreeButton::Save => save(session),
        TreeButton::Exit => {
            if session.guide.dirty() && !session.confirm_quit {
                session.confirm_quit = true;
                session
                    .guide
                    .note("unsaved changes — press Exit again to discard, or Save");
            } else {
                session.quit = true;
            }
            Ok(())
        }
    };
    if let Err(err) = result {
        session.guide.note(err);
    }
}

fn save(session: &mut Session) -> Result<(), String> {
    if session.guide.path().is_some() {
        session.guide.save()
    } else {
        session.overlay = Overlay::Text(TextPrompt {
            title: "save as".to_owned(),
            buffer: session.open_hint.clone(),
            action: TextAction::SaveAs,
        });
        Ok(())
    }
}

fn handle_mouse(session: &mut Session, mouse: MouseEvent) {
    if !matches!(session.overlay, Overlay::None) {
        return;
    }
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }
    for (rect, button) in session.button_rects.clone() {
        if mouse.column >= rect.x
            && mouse.column < rect.x + rect.width
            && mouse.row == rect.y
        {
            if let Some(index) = session
                .guide
                .tree_view()
                .buttons
                .iter()
                .position(|candidate| *candidate == button)
            {
                session.button_focus = index;
            }
            activate_button(session, button);
            return;
        }
    }
    let inner = Block::bordered().inner(session.list_rect);
    if mouse.column >= inner.x
        && mouse.column < inner.x + inner.width
        && mouse.row >= inner.y
        && mouse.row < inner.y + inner.height
    {
        let row = (mouse.row - inner.y) as usize + session.list_offset;
        let len = session.guide.tree_view().nodes.len();
        if row < len {
            session.guide.select_tree_row(row);
        }
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
                    if session.open_port(&port.name).is_err() {
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
                Some(path) => match session.open_map(&path) {
                    Ok(()) => {}
                    Err(err) => {
                        session.guide.note(err);
                        session.overlay = Overlay::Files(picker);
                    }
                },
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
            KeyCode::Enter => menu_enter(session, menu),
            _ => session.overlay = Overlay::Menu(menu),
        },
        Overlay::Text(mut prompt) => match key.code {
            KeyCode::Esc => {}
            KeyCode::Backspace => {
                prompt.buffer.pop();
                session.overlay = Overlay::Text(prompt);
            }
            KeyCode::Enter => text_enter(session, prompt),
            KeyCode::Char(ch) => {
                prompt.buffer.push(ch);
                session.overlay = Overlay::Text(prompt);
            }
            _ => session.overlay = Overlay::Text(prompt),
        },
    }
}

fn menu_enter(session: &mut Session, menu: Menu) {
    let Some(selected) = menu.items.get(menu.selected).cloned() else {
        return;
    };
    match menu.action {
        MenuAction::NewUnitPreset => {
            if session.guide.preset_is_global(&selected) {
                if let Err(err) = session.guide.tree_new_global_unit(&selected, None) {
                    session.guide.note(err);
                }
                return;
            }
            let preferred = session.guide.tree_selected_unit_deck();
            let max = session
                .decks
                .max(preferred.map_or(0, |deck| deck as usize + 1));
            let items: Vec<String> = (0..max)
                .map(|deck| format!("deck {}", deck + 1))
                .collect();
            let selected_deck = preferred
                .and_then(|deck| items.iter().position(|item| parse_deck(item) == deck))
                .unwrap_or(0);
            session.overlay = Overlay::Menu(Menu {
                title: format!("deck for {selected}"),
                items,
                selected: selected_deck,
                action: MenuAction::NewUnitDeck { preset: selected },
            });
        }
        MenuAction::NewUnitDeck { preset } => {
            let deck = parse_deck(&selected);
            if let Err(err) = session.guide.tree_new_unit(&preset, deck, None) {
                session.guide.note(err);
            }
        }
        MenuAction::CloneDeck => {
            let deck = parse_deck(&selected);
            if let Err(err) = session.guide.tree_clone(deck) {
                session.guide.note(err);
            }
        }
        MenuAction::NewSelector => {
            if selected == "Cycle" {
                if let Err(err) = session
                    .guide
                    .tree_begin_selector(RawSelectorBehavior::Cycle, None)
                {
                    session.guide.note(err);
                }
            } else {
                let items = session.guide.mode_names();
                session.overlay = Overlay::Menu(Menu {
                    title: "trigger selector — target mode".to_owned(),
                    items,
                    selected: 0,
                    action: MenuAction::SelectorTarget,
                });
            }
        }
        MenuAction::SelectorTarget => {
            if let Err(err) = session
                .guide
                .tree_begin_selector(RawSelectorBehavior::Trigger, Some(selected))
            {
                session.guide.note(err);
            }
        }
        MenuAction::ActionPick => {
            let buffer = if selected == "<type a line>" {
                session.guide.tree_action_text().unwrap_or_default()
            } else {
                format!("{selected} ")
            };
            session.overlay = Overlay::Text(TextPrompt {
                title: "action line".to_owned(),
                buffer,
                action: TextAction::ActionLine,
            });
        }
        MenuAction::SelectorBehavior => {
            let (behavior, target) = if selected == "cycle" {
                (RawSelectorBehavior::Cycle, None)
            } else if let Some(mode) = selected.strip_prefix("trigger ") {
                (RawSelectorBehavior::Trigger, Some(mode.to_owned()))
            } else {
                session.guide.note(format!("unknown selector behavior `{selected}`"));
                return;
            };
            if let Err(err) = session.guide.tree_set_selector_behavior(behavior, target) {
                session.guide.note(err);
            }
        }
    }
}

fn text_enter(session: &mut Session, prompt: TextPrompt) {
    let result = match prompt.action {
        TextAction::NewMode => session.guide.tree_add_mode(prompt.buffer),
        TextAction::ActionLine => session.guide.tree_set_action(prompt.buffer),
        TextAction::SaveAs => session.guide.save_as(prompt.buffer.trim()),
    };
    if let Err(err) = result {
        session.guide.note(err);
    }
}

fn parse_deck(label: &str) -> u8 {
    label
        .rsplit(' ')
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn render(frame: &mut Frame, session: &mut Session) {
    let view = session.guide.tree_view();
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(2),
        Constraint::Length(1), // separator
        Constraint::Length(1), // cfdisk-style button row
    ])
    .split(area);

    render_header(frame, chunks[0], session, &view);
    let body = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);
    session.list_rect = body[0];
    render_tree(frame, body[0], session, &view);
    let right = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body[1]);
    render_detail(frame, right[0], &view);
    render_monitor(frame, right[1], session, &view);
    render_status(frame, chunks[2], session, &view);
    render_separator(frame, chunks[3], area.width);
    render_buttons(frame, chunks[4], session, &view);

    match &session.overlay {
        Overlay::None => {}
        Overlay::Ports(list) => {
            picker::render_port_list(frame, area, list, "MIDI device (Enter)")
        }
        Overlay::Files(picker) => picker::render_file_picker(frame, area, picker),
        Overlay::Menu(menu) => render_menu(frame, area, menu),
        Overlay::Text(prompt) => render_text(frame, area, prompt),
    }
}

/// Header: only the file and the controller, as the mapping editor's two pieces of context.
fn render_header(frame: &mut Frame, area: Rect, session: &Session, view: &TreeView) {
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

fn render_tree(frame: &mut Frame, area: Rect, session: &mut Session, view: &TreeView) {
    let items: Vec<ListItem> = view
        .nodes
        .iter()
        .map(|node| {
            let indent = "  ".repeat(node.depth);
            let (style, marker) = match node.kind {
                TreeNodeKind::Deck { .. } => (Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD), "▾ "),
                TreeNodeKind::AddUnit { .. } => (Style::default().fg(Color::Green), "+ "),
                TreeNodeKind::Unit { .. } => (Style::default().fg(Color::White).add_modifier(Modifier::BOLD), ""),
                TreeNodeKind::Control { .. } => (Style::default().fg(Color::White), ""),
                TreeNodeKind::ControlPart { part, .. } => (
                    Style::default().fg(Color::Gray),
                    match part {
                        hypermixx_midi::guide::ControlPart::Press => "◉ ",
                        hypermixx_midi::guide::ControlPart::Turn => "↻ ",
                    },
                ),
                TreeNodeKind::Mode { .. } => (Style::default().fg(Color::Cyan), "• "),
                TreeNodeKind::Action { .. } => (Style::default().fg(Color::Gray), ""),
                TreeNodeKind::Selector { .. } => (Style::default().fg(Color::Magenta), ""),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{indent}{marker}{}", node.label), style),
                Span::raw("  "),
                Span::styled(node.hint.clone(), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" map "))
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(Some(view.selected));
    frame.render_stateful_widget(list, area, &mut state);
    session.list_offset = state.offset();
}

fn render_detail(frame: &mut Frame, area: Rect, view: &TreeView) {
    let mut lines: Vec<Line> = view
        .details
        .iter()
        .map(|(key, value)| {
            Line::from(vec![
                Span::styled(format!("{key}: "), Style::default().fg(Color::DarkGray)),
                Span::raw(value.clone()),
            ])
        })
        .collect();
    if view.capturing {
        if let Some(candidate) = &view.candidate {
            lines.push(Line::from(vec![
                Span::styled("detected: ", Style::default().fg(Color::Cyan)),
                Span::styled(candidate.label(), Style::default().fg(Color::Cyan)),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                "waiting for a control…",
                Style::default().fg(Color::Cyan),
            )));
        }
    }
    if let Some(error) = &view.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" selected "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_monitor(frame: &mut Frame, area: Rect, session: &Session, view: &TreeView) {
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
        .map(|line| {
            let style = if line.key.is_some_and(|key| view.endpoints.contains(&key)) {
                Style::default().fg(Color::Black).bg(Color::Gray)
            } else {
                Style::default().fg(Color::White)
            };
            Line::styled(line.text.clone(), style)
        })
        .collect();
    let title = if session.monitor_offset > 0 {
        format!(" raw midi [↑{}] ", session.monitor_offset)
    } else {
        " raw midi ".to_owned()
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered().title(title)),
        area,
    );
}

fn render_separator(frame: &mut Frame, area: Rect, width: u16) {
    let line = "─".repeat(width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            line,
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

fn render_status(frame: &mut Frame, area: Rect, session: &Session, view: &TreeView) {
    let first = view
        .message
        .clone()
        .or_else(|| view.error.clone())
        .unwrap_or_else(|| {
            if view.path.is_none() {
                "Open a map, or add a unit and Save".to_owned()
            } else if view.capturing {
                "press the control, then Confirm".to_owned()
            } else {
                "select a tree row; its actions appear in the bottom bar".to_owned()
            }
        });
    let style = if view.error.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Cyan)
    };
    let candidate = view
        .candidate
        .as_ref()
        .map(|candidate| format!("detected: {}", candidate.label()))
        .unwrap_or_else(|| "detected: —".to_owned());
    let hints = if session.confirm_quit {
        "Exit again = discard · Save = write".to_owned()
    } else {
        let focused = view
            .buttons
            .get(session.button_focus)
            .map(|button| button.hint())
            .unwrap_or("select a row");
        format!("{focused} · ↑↓ tree · ←→ button · Enter activate · PgUp/PgDn raw")
    };
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(Span::styled(first, style)),
            Line::from(vec![
                Span::styled(candidate, Style::default().fg(Color::DarkGray)),
                Span::raw("   "),
                Span::styled(hints, Style::default().fg(Color::DarkGray)),
            ]),
        ])),
        area,
    );
}

fn render_buttons(frame: &mut Frame, area: Rect, session: &mut Session, view: &TreeView) {
    session.button_rects.clear();
    if view.buttons.is_empty() {
        return;
    }
    session.button_focus = session.button_focus.min(view.buttons.len() - 1);
    let labels: Vec<(usize, TreeButton, String, u16)> = view
        .buttons
        .iter()
        .enumerate()
        .map(|(index, button)| {
            let label = format!(" {} ", button.label());
            let width = label.chars().count() as u16;
            (index, *button, label, width)
        })
        .collect();
    let total: u16 = labels.iter().map(|(_, _, _, width)| width + 1).sum();
    let mut start = 0usize;
    if total > area.width {
        while start < session.button_focus {
            let used: u16 = labels[start..=session.button_focus]
                .iter()
                .map(|(_, _, _, width)| width + 1)
                .sum();
            if used <= area.width {
                break;
            }
            start += 1;
        }
    }
    let mut x = if total <= area.width {
        area.x + (area.width - total) / 2
    } else {
        area.x
    };
    let mut spans: Vec<Span> = Vec::new();
    if start > 0 {
        spans.push(Span::styled("‹", Style::default().fg(Color::DarkGray)));
        x += 1;
    }
    for (index, button, label, width) in labels.iter().skip(start) {
        if x + width > area.x + area.width {
            break;
        }
        let style = if *index == session.button_focus {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        session
            .button_rects
            .push((Rect::new(x, area.y, *width, 1), *button));
        spans.push(Span::styled(label.clone(), style));
        spans.push(Span::raw(" "));
        x += width + 1;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_menu(frame: &mut Frame, area: Rect, menu: &Menu) {
    let height = (menu.items.len() as u16 + 2).clamp(3, (area.height.saturating_sub(4)).min(16));
    let width = (area.width * 3 / 4).clamp(24, area.width.saturating_sub(2));
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = menu
        .items
        .iter()
        .map(|item| ListItem::new(item.clone()))
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(menu.title.clone()))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(Some(menu.selected));
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_text(frame: &mut Frame, area: Rect, prompt: &TextPrompt) {
    let width = (area.width * 2 / 3).clamp(30, area.width.saturating_sub(2));
    let popup = centered(area, width, 3);
    frame.render_widget(Clear, popup);
    let text = Line::from(vec![
        Span::raw(prompt.buffer.clone()),
        Span::styled("▌", Style::default().fg(Color::Cyan)),
    ]);
    frame.render_widget(
        Paragraph::new(Text::from(text))
            .block(Block::bordered().title(prompt.title.clone()))
            .wrap(Wrap { trim: false }),
        popup,
    );
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
        Session::new("midi-map.toml", 2).unwrap()
    }

    #[test]
    fn the_clock_formats_within_a_day() {
        let at = UNIX_EPOCH + Duration::from_secs(3600 + 2 * 60 + 3);
        assert_eq!(clock(at), "01:02:03");
    }

    #[test]
    fn a_deck_label_parses_back_to_its_number() {
        assert_eq!(parse_deck("deck 0"), 0);
        assert_eq!(parse_deck("deck 1"), 1);
    }

    #[test]
    fn monitor_scrolling_clamps_to_the_buffered_lines() {
        let mut session = test_session();
        session.monitor = (0..5)
            .map(|index| MonitorLine {
                text: index.to_string(),
                key: None,
            })
            .collect();
        session.scroll_monitor(3);
        assert_eq!(session.monitor_offset, 3);
        session.scroll_monitor(100);
        assert_eq!(session.monitor_offset, 5);
        session.scroll_monitor(-100);
        assert_eq!(session.monitor_offset, 0);
    }

    #[test]
    fn the_layout_renders_at_normal_and_tiny_sizes() {
        let mut session = test_session();
        for (width, height) in [(80, 24), (20, 8)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| render(frame, &mut session))
                .expect("guide layout");
        }
    }

    #[test]
    fn the_tree_context_switches_the_bottom_buttons() {
        let mut session = test_session();
        session
            .guide
            .tree_new_unit("touchpad", 0, None)
            .expect("unit from preset");
        let tree = session.guide.tree();
        let control = tree
            .iter()
            .position(|node| matches!(node.kind, TreeNodeKind::Control { control: 0, .. }))
            .expect("pad1");
        session.guide.select_tree_row(control);
        let view = session.guide.tree_view();
        assert!(view.buttons.contains(&TreeButton::Learn));
        assert!(!view.buttons.contains(&TreeButton::Edit));
        let action = tree
            .iter()
            .position(|node| matches!(node.kind, TreeNodeKind::Action { mode: 0, control: 0, .. }))
            .expect("hotcue/pad1 action");
        session.guide.select_tree_row(action);
        let view = session.guide.tree_view();
        assert!(view.buttons.contains(&TreeButton::Edit));
        assert!(view.buttons.contains(&TreeButton::On));
        assert!(view.nodes.iter().any(|node| node.label.contains("pad1")));
    }

    #[test]
    fn the_persistent_buttons_are_visible_and_clickable() {
        let mut session = test_session();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &mut session))
            .expect("guide layout");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        for label in ["Connect", "Open", "Save", "Exit"] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
        let (rect, _) = session
            .button_rects
            .iter()
            .find(|(_, button)| *button == TreeButton::Connect)
            .copied()
            .expect("connect button rect");
        handle_mouse(
            &mut session,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 1,
                row: rect.y,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert!(matches!(session.overlay, Overlay::Ports(_)));
    }
}
