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
use hypermixx_midi::guide::{Column, ColumnFocus, Entered, Row, TreeButton, View};
use hypermixx_midi::map::{Edge, EventKind, SlotRole};
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
    /// Tab moves focus between the columns and the command row; while the row has focus the columns
    /// keep only their grey selection.
    buttons_focused: bool,
    button_rects: Vec<(Rect, TreeButton)>,
    /// `(rect, column, row index)` for every rendered column row, so a click can select it.
    row_rects: Vec<(Rect, Column, usize)>,
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
    /// The channel offset a clone applies to every endpoint.
    CloneOffset { deck: usize },
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
            buttons_focused: false,
            button_rects: Vec::new(),
            row_rects: Vec::new(),
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
            let at_ms = received
                .at
                .duration_since(UNIX_EPOCH)
                .map(|delta| delta.as_millis() as u64)
                .ok();
            self.guide.feed_at(&received.event, at_ms);
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
    if session.buttons_focused {
        match key.code {
            KeyCode::Tab | KeyCode::Right => move_button(session, 1),
            KeyCode::BackTab | KeyCode::Left => move_button(session, -1),
            KeyCode::Enter => {
                let view = session.guide.view();
                if let Some(button) = view
                    .buttons
                    .get(session.button_focus.min(view.buttons.len().saturating_sub(1)))
                    .copied()
                {
                    activate_button(session, button);
                }
            }
            KeyCode::Esc | KeyCode::Up | KeyCode::Down => session.buttons_focused = false,
            KeyCode::PageUp => session.scroll_monitor(MONITOR_PAGE as isize),
            KeyCode::PageDown => session.scroll_monitor(-(MONITOR_PAGE as isize)),
            _ => {}
        }
        return false;
    }
    match key.code {
        KeyCode::Up => session.guide.move_cursor(-1),
        KeyCode::Down => session.guide.move_cursor(1),
        KeyCode::Left => session.guide.move_column(-1),
        KeyCode::Right => session.guide.move_column(1),
        KeyCode::Char('[') => session.guide.step_mode(-1),
        KeyCode::Char(']') => session.guide.step_mode(1),
        KeyCode::Tab => {
            session.buttons_focused = true;
            session.button_focus = 0;
        }
        KeyCode::BackTab => {
            session.buttons_focused = true;
            session.button_focus = session.guide.view().buttons.len().saturating_sub(1);
        }
        KeyCode::Enter => match session.guide.enter() {
            Ok(Entered::AddUnit) => open_new_unit_menu(session),
            Ok(Entered::Moved) | Ok(Entered::Learning) => {}
            Err(err) => session.guide.note(err),
        },
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
    let len = session.guide.view().buttons.len();
    if len == 0 {
        return;
    }
    session.button_focus =
        (session.button_focus as isize + delta).rem_euclid(len as isize) as usize;
}

/// The preset menu the `add unit` row opens.
fn open_new_unit_menu(session: &mut Session) {
    let items = session.guide.preset_names();
    if items.is_empty() {
        session.guide.note("no presets available".to_owned());
        return;
    }
    session.overlay = Overlay::Menu(Menu {
        title: "new unit — preset".to_owned(),
        items,
        selected: 0,
        action: MenuAction::NewUnitPreset,
    });
}

fn activate_button(session: &mut Session, button: TreeButton) {
    if button != TreeButton::Exit {
        session.confirm_quit = false;
    }
    let result = match button {
        TreeButton::Learn => match session.guide.enter() {
            Ok(Entered::Learning) | Ok(Entered::Moved) => Ok(()),
            Ok(Entered::AddUnit) => Err("select a slot first".to_owned()),
            Err(err) => Err(err),
        },
        TreeButton::NewUnit => {
            open_new_unit_menu(session);
            Ok(())
        }
        TreeButton::Clone => {
            let deck = session.guide.tree_selected_unit_deck();
            let Some(current) = deck else {
                return session
                    .guide
                    .note("select a unit to clone".to_owned());
            };
            let items: Vec<String> = (0..session.decks)
                .filter(|deck| *deck != current)
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
        TreeButton::Delete => session.guide.delete_selected(),
        TreeButton::Up => session.guide.move_mode(-1),
        TreeButton::Down => session.guide.move_mode(1),
        TreeButton::SetStart => session.guide.set_start(),
        TreeButton::Edit => match session.guide.current_slot_role() {
            Some(SlotRole::Action) if session.guide.is_capturing() => Ok(()),
            Some(SlotRole::Action) => {
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
            Some(SlotRole::ModeTrigger) | Some(SlotRole::ModeCycle) => {
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
            None => Err("select a slot first".to_owned()),
        },
        TreeButton::On => {
            let next = match session.guide.on() {
                Some(_) => None,
                None => Some(Edge::Release),
            };
            session.guide.set_on(next)
        }
        TreeButton::Confirm => session.guide.confirm_learn(),
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
                .view()
                .buttons
                .iter()
                .position(|candidate| *candidate == button)
            {
                session.button_focus = index;
            }
            session.buttons_focused = true;
            activate_button(session, button);
            return;
        }
    }
    for (rect, column, index) in session.row_rects.clone() {
        if mouse.row == rect.y && mouse.column >= rect.x && mouse.column < rect.x + rect.width {
            while session.guide.column() != column {
                let forward = matches!(
                    (session.guide.column(), column),
                    (Column::Deck, Column::Unit) | (Column::Unit, Column::Slot)
                );
                session.guide.move_column(if forward { 1 } else { -1 });
            }
            let current = match column {
                Column::Deck => session.guide.nav().deck,
                Column::Unit => session.guide.nav().unit,
                Column::Slot => session.guide.nav().slot,
            };
            session.buttons_focused = false;
            session
                .guide
                .move_cursor(index as isize - current as isize);
            return;
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
            if !session.guide.preset_scope(&selected).takes_deck() {
                if let Err(err) = session.guide.new_unit(&selected, None, None) {
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
            if let Err(err) = session.guide.new_unit(&preset, Some(deck), None) {
                session.guide.note(err);
            }
        }
        MenuAction::CloneDeck => {
            // The channel shift is device-specific (a 4-deck controller may put a deck on
            // channels 0, 2, 4 …), so it is asked for rather than assumed.
            let deck = parse_deck(&selected);
            session.overlay = Overlay::Text(TextPrompt {
                title: format!("channel offset for deck {}", deck + 1),
                buffer: "1".to_owned(),
                action: TextAction::CloneOffset { deck },
            });
        }
        MenuAction::NewSelector => {
            if selected == "Cycle" {
                if let Err(err) = session.guide.begin_mode_key(false, None) {
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
            if let Err(err) = session.guide.begin_mode_key(true, Some(selected)) {
                session.guide.note(err);
            }
        }
        MenuAction::ActionPick => {
            let buffer = if selected == "<type a line>" {
                session.guide.action_text().unwrap_or_default()
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
            let (trigger, target) = if selected == "cycle" {
                (false, None)
            } else if let Some(mode) = selected.strip_prefix("trigger ") {
                (true, Some(mode.to_owned()))
            } else {
                session.guide.note(format!("unknown mode key behavior `{selected}`"));
                return;
            };
            if let Err(err) = session.guide.set_mode_key(trigger, target) {
                session.guide.note(err);
            }
        }
    }
}

fn text_enter(session: &mut Session, prompt: TextPrompt) {
    let result = match prompt.action {
        TextAction::NewMode => session.guide.add_mode(prompt.buffer),
        TextAction::ActionLine => session.guide.set_action(prompt.buffer),
        TextAction::SaveAs => session.guide.save_as(prompt.buffer.trim()),
        TextAction::CloneOffset { deck } => match prompt.buffer.trim().parse::<i16>() {
            Ok(offset) => session.guide.clone_unit(deck, offset),
            Err(_) => Err(format!("`{}` is not a channel offset", prompt.buffer.trim())),
        },
    };
    if let Err(err) = result {
        session.guide.note(err);
    }
}

fn parse_deck(label: &str) -> usize {
    label
        .rsplit(' ')
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|number| number.saturating_sub(1))
        .unwrap_or(0)
}

fn render(frame: &mut Frame, session: &mut Session) {
    let view = session.guide.view();
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(8),    // the three columns
        Constraint::Fill(1),   // info
        Constraint::Fill(1),   // raw midi / watchdog
        Constraint::Length(2), // status
        Constraint::Length(1), // button row
    ])
    .split(area);

    render_header(frame, chunks[0], session, &view);
    let columns = Layout::horizontal([
        Constraint::Length(18),
        Constraint::Percentage(38),
        Constraint::Fill(1),
    ])
    .split(chunks[1]);
    session.row_rects.clear();
    let focused_column = if session.buttons_focused {
        None
    } else {
        Some(view.nav.focus)
    };
    render_column(
        frame,
        columns[0],
        " deck ",
        &view.decks,
        (view.nav.deck, focused_column == Some(ColumnFocus::Deck)),
        Column::Deck,
        session,
    );
    render_column(
        frame,
        columns[1],
        " unit ",
        &view.units,
        (view.nav.unit, focused_column == Some(ColumnFocus::Unit)),
        Column::Unit,
        session,
    );
    render_column(
        frame,
        columns[2],
        " slot ",
        &view.slots,
        (view.nav.slot, focused_column == Some(ColumnFocus::Slot)),
        Column::Slot,
        session,
    );
    render_detail(frame, chunks[2], &view);
    render_monitor(frame, chunks[3], session, &view);
    render_status(frame, chunks[4], session, &view);
    render_buttons(frame, chunks[5], session, &view);

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

/// Header: only the file and the controller, the mapping editor's two pieces of context.
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

/// One bordered column, styled like the main TUI: the *border* colour carries focus (cyan when the
/// column owns the cursor, grey otherwise), and the selected row is the grey `List` highlight.
fn render_column(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    rows: &[Row],
    (selected, focused): (usize, bool),
    column: Column,
    session: &mut Session,
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
        // Same highlight as the browser's list: grey background, white text.
        let style = if active {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        } else if row.muted {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        let marker = if active { "› " } else { "  " };
        let text = if active && !row.hint.is_empty() {
            format!("{marker}{}  {}", row.label, row.hint)
        } else {
            format!("{marker}{}", row.label)
        };
        session.row_rects.push((
            Rect::new(inner.x, inner.y + (index - start) as u16, inner.width, 1),
            column,
            index,
        ));
        lines.push(Line::from(Span::styled(text, style)));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_detail(frame: &mut Frame, area: Rect, view: &View) {
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
                Span::styled(" detected: ", Style::default().fg(Color::Cyan)),
                Span::styled(candidate.label(), Style::default().fg(Color::Cyan)),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                " waiting for a control…",
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
    let title = match &view.mode {
        Some((index, count, name)) => format!(" info · mode {}/{count} {name} ", index + 1),
        None => " info ".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_monitor(frame: &mut Frame, area: Rect, session: &Session, view: &View) {
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
        " raw midi / watchdog ".to_owned()
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered().title(title)),
        area,
    );
}

fn render_status(frame: &mut Frame, area: Rect, session: &Session, view: &View) {
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
                format!(
                    "{} column · Enter to drill in · [ ] mode",
                    view.nav.focus.column().label()
                )
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
        format!("{focused} · ↑↓ row · ←→ column · Tab button · Enter")
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

fn render_buttons(frame: &mut Frame, area: Rect, session: &mut Session, view: &View) {
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
        let style = if *index == session.button_focus && session.buttons_focused {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if *index == session.button_focus {
            // The row does not own the cursor, so the focused button is only outlined by colour.
            Style::default().fg(Color::Cyan)
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
        // The menu is 1-based; the stored deck id is 0-based.
        assert_eq!(parse_deck("deck 1"), 0);
        assert_eq!(parse_deck("deck 2"), 1);
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

    /// The whole flow the guide exists for: pick a preset, learn an endpoint, save, reopen.
    #[test]
    fn a_whole_edit_round_trips_and_renders() {
        let dir = std::env::temp_dir().join(format!("hypermixx-cli-guide-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("map.toml");
        let path_text = path.display().to_string();
        let mut session = Session::new(&path_text, 2).unwrap();

        session
            .guide
            .new_unit("transport", Some(0), None)
            .unwrap();
        session.guide.select_slot(0, "play");
        session.guide.begin_learn().unwrap();
        session.guide.feed(&hypermixx_midi::Event::NoteOn {
            channel: 0,
            key: 16,
            velocity: 100,
        });
        session.guide.confirm_learn().unwrap();
        render_once(&mut session);
        session.guide.save_as(&path_text).unwrap();
        assert!(path.exists());

        let reopened = Session::new(&path_text, 2).unwrap();
        assert!(reopened.guide.view().error.is_none());
        assert!(!reopened.guide.view().units.is_empty());
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    fn render_once(session: &mut Session) {
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, session))
            .expect("guide layout");
    }

    #[test]
    fn the_shipped_map_renders_its_units_and_slots() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = root.join("profile/midi-map.toml");
        let mut session = Session::new(&path.display().to_string(), 2).unwrap();
        session.open_map(&path).unwrap();
        let view = session.guide.view();
        assert!(view.error.is_none(), "{:?}", view.error);
        let decks: Vec<&str> = view.decks.iter().map(|row| row.label.as_str()).collect();
        assert!(decks.contains(&"deck1") && decks.contains(&"global"), "{decks:?}");
        let units: Vec<&str> = view.units.iter().map(|row| row.label.as_str()).collect();
        assert!(units.contains(&"deck1.pad"), "{units:?}");

        // The slot column lists the jog's two halves.
        let jog = session.guide.unit_index("deck1.jog").expect("deck1.jog");
        session.guide.select_slot(jog, "jog");
        let slots: Vec<String> = session
            .guide
            .view()
            .slots
            .into_iter()
            .map(|row| row.label)
            .collect();
        assert!(slots.contains(&"press".to_owned()), "{slots:?}");
        assert!(slots.contains(&"turn".to_owned()), "{slots:?}");
        render_once(&mut session);
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
    fn the_column_context_switches_the_bottom_buttons() {
        let mut session = test_session();
        session
            .guide
            .new_unit("pad", Some(0), None)
            .expect("unit from preset");

        // Unit column: a deck unit can be cloned, and nothing else but the persistent four.
        let view = session.guide.view();
        assert_eq!(
            view.buttons,
            vec![
                TreeButton::Clone,
                TreeButton::Connect,
                TreeButton::Open,
                TreeButton::Save,
                TreeButton::Exit
            ]
        );

        // A global unit has no deck to clone to.
        session
            .guide
            .new_unit("output", None, None)
            .expect("global unit");
        let view = session.guide.view();
        assert_eq!(
            view.buttons,
            vec![
                TreeButton::Connect,
                TreeButton::Open,
                TreeButton::Save,
                TreeButton::Exit
            ]
        );

        // Slot column: learning is the only extra.
        let pad = session.guide.unit_index("deck1.pad").expect("pad");
        session.guide.select_slot(pad, "pad1");
        let view = session.guide.view();
        assert_eq!(
            view.buttons,
            vec![
                TreeButton::Learn,
                TreeButton::Connect,
                TreeButton::Open,
                TreeButton::Save,
                TreeButton::Exit
            ]
        );
    }

    #[test]
    fn tab_then_enter_activates_the_button_row_not_the_columns() {
        let mut session = test_session();
        session
            .guide
            .new_unit("transport", Some(0), None)
            .unwrap();
        let before = session.guide.nav().focus;

        handle_key(&mut session, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert!(session.buttons_focused, "Tab moves the cursor to the buttons");
        // The columns keep their selection but no longer look focused.
        assert_eq!(session.guide.nav().focus, before);

        // Walk to `Open` and press Enter: the file picker must open, not a drill-down.
        let view = session.guide.view();
        let open = view
            .buttons
            .iter()
            .position(|button| *button == TreeButton::Open)
            .expect("open button");
        session.button_focus = open;
        handle_key(&mut session, KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert!(matches!(session.overlay, Overlay::Files(_)), "{:?}", std::mem::discriminant(&session.overlay));
        assert_eq!(session.guide.column(), before.column());
    }

    #[test]
    fn clicking_a_column_row_moves_the_cursor() {
        let mut session = test_session();
        session
            .guide
            .new_unit("transport", Some(0), None)
            .unwrap();
        render_once(&mut session);
        let (rect, column, index) = session
            .row_rects
            .iter()
            .find(|(_, column, index)| *column == Column::Slot && *index == 0)
            .copied()
            .expect("a slot row");
        handle_mouse(
            &mut session,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 1,
                row: rect.y,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert_eq!(session.guide.column(), column);
        assert_eq!(session.guide.nav().slot, index);
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

