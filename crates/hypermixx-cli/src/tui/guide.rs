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
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::picker::{FilePicker, PortList};

const TICK: Duration = Duration::from_millis(33);
const WATCH_CAP: usize = 500;
const WATCH_PAGE: usize = 10;
const BLINK: Duration = Duration::from_millis(500);

/// Where the guide can send audio, and what this host offers.
///
/// The guide does not start an engine, so it cannot ask the mixer: the caller has already probed
/// cpal and told it which mixer file owns the `[[output]]` tables.
#[derive(Clone, Debug, Default)]
pub struct Outputs {
    pub mixer_path: String,
    pub devices: Vec<hypermixx_audio::DeviceInfo>,
    /// The engine's rate, shown against each device.
    pub engine_rate: u32,
    /// The `preference.toml` the guide's colours come from.
    pub preference: String,
    /// The controller this map belongs to, so its own audio interface is marked in the list.
    pub controller: Option<String>,
}

/// Runs the controller guide.
pub fn run(
    map_path: &str,
    port: Option<&str>,
    decks: usize,
    outputs: Outputs,
) -> Result<(), String> {
    let mut session = Session::new(map_path, port, decks, outputs)?;
    let mut terminal = ratatui::try_init().map_err(|err| format!("terminal: {err}"))?;
    let outcome = event_loop(&mut terminal, &mut session);
    ratatui::restore();
    outcome
}

struct Session {
    guide: Guide,
    /// `preference.toml`'s colours: the guide draws its modals through the same popup as the TUI.
    theme: crate::preference::Theme,
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
    /// The `outputs` group of the deck column: master/precue, then one row per destination.
    output_pane: OutputPane,
    /// What `Output` can bind, and where.
    outputs: Outputs,
    /// `(role, name)` of the outputs the mixer file declares, for the status line.
    output_names: Vec<String>,
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
    /// Level one of the device chooser: the hardware.
    Devices {
        selected: usize,
        target: DeviceTarget,
    },
    /// Level two: which channel pair of that hardware to use.
    Channels {
        device: usize,
        selected: usize,
        target: DeviceTarget,
    },
    Menu(Menu),
}

/// What a device choice is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceTarget {
    /// Rebind the `[[output]]` at this position in the mixer file.
    Output(usize),
    /// Create a new output with this role (`master` / `cue`).
    NewOutput(&'static str),
}

/// One `[[output]]` of the mixer file, as the output column shows it.
#[derive(Clone, Debug)]
struct OutputEntry {
    /// Its position in the `[[output]]` array, which is how it is addressed on write.
    index: usize,
    role: &'static str,
    name: String,
    device: Option<String>,
    channels: Option<(u16, u16)>,
}

/// The `outputs` group of the deck column.
///
/// The guide's three columns become `outputs › master|precue › destination`, the same Miller shape
/// the MIDI side uses: `←/→` move between the columns, `↑/↓` inside one, and `new` in the last column
/// opens a multi-level chooser (role, then device).
#[derive(Default)]
struct OutputPane {
    /// The deck column is on the `outputs` row.
    active: bool,
    /// Which of the three columns owns the arrows: 0 = deck, 1 = role, 2 = slot.
    focus: usize,
    role: usize,
    slot: usize,
    entries: Vec<OutputEntry>,
}

impl OutputPane {
    fn load(path: &str) -> Self {
        Self {
            active: false,
            focus: 1,
            role: 0,
            slot: 0,
            entries: read_outputs(path),
        }
    }

    /// The roles, in column order.
    const ROLES: [&'static str; 2] = ["master", "precue"];

    /// The mixer role a column row means (`precue` is `cue` in the file).
    fn file_role(row: usize) -> &'static str {
        match row {
            0 => "master",
            _ => "cue",
        }
    }

    /// The entries of the selected role, in file order.
    fn slots(&self) -> Vec<&OutputEntry> {
        let role = Self::file_role(self.role);
        self.entries
            .iter()
            .filter(|entry| entry.role == role)
            .collect()
    }

    fn clamp(&mut self) {
        // One extra row: `new`.
        let len = self.slots().len();
        self.slot = self.slot.min(len);
    }
}

/// Reads the mixer file's `[[output]]` tables.
fn read_outputs(path: &str) -> Vec<OutputEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    let Some(tables) = doc.get("output").and_then(|item| item.as_array_of_tables()) else {
        return Vec::new();
    };
    tables
        .iter()
        .enumerate()
        .map(|(index, table)| {
            let role = match table.get("role").and_then(|item| item.as_str()) {
                Some("cue") => "cue",
                _ => "master",
            };
            let name = table
                .get("name")
                .and_then(|item| item.as_str())
                .unwrap_or("output")
                .to_owned();
            let device = table
                .get("device")
                .and_then(|item| item.as_str())
                .map(str::to_owned);
            let channels = table
                .get("channels")
                .and_then(|item| item.as_array())
                .filter(|array| array.len() == 2)
                .and_then(|array| {
                    Some((
                        u16::try_from(array.get(0)?.as_integer()?).ok()?,
                        u16::try_from(array.get(1)?.as_integer()?).ok()?,
                    ))
                });
            OutputEntry {
                index,
                role,
                name,
                device,
                channels,
            }
        })
        .collect()
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
    /// The second level of the `new output` chooser: pick the device for this role.
    Device(DeviceTarget),
}

impl Session {
    fn new(
        open_hint: &str,
        port: Option<&str>,
        decks: usize,
        outputs: Outputs,
    ) -> Result<Self, String> {
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
            theme: crate::preference::load(&outputs.preference)
                .map(|(preference, _)| preference.theme)
                .unwrap_or_default(),
            overlay: Overlay::None,
            command_focus: 0,
            command_active: false,
            confirm_quit: false,
            blink: false,
            blink_at: Instant::now(),
            quit: false,
            output_pane: OutputPane::load(&outputs.mixer_path),
            output_names: output_labels(&outputs.mixer_path),
            outputs,
        };

        // The map (or the port) names the controller; that is what the device list marks.
        session.outputs.controller = session.guide.device().or_else(|| session.port.clone());

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
        let input = ports::open(name, tx).map_err(|err| format!("controller: {err}"))?;
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

    if session.output_pane.active {
        handle_output_keys(session, key);
        return;
    }

    // The deck column ends with `new`; one more Down lands on the `audio output` row, which is the
    // same Miller-column idiom rather than a separate mode or a command button.
    if key.code == KeyCode::Up
        && !session.command_active
        && session.guide.view().nav.focus == ColumnFocus::Deck
    {
        let view = session.guide.view();
        if view.nav.deck == 0 {
            session.output_pane.active = true;
            session.output_pane.focus = 0;
            session.output_pane.entries = read_outputs(&session.outputs.mixer_path);
            session.output_pane.clamp();
            return;
        }
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

/// Keys for the `outputs` group: three Miller columns, `←/→` between them, `↑/↓` inside one.
fn handle_output_keys(session: &mut Session, key: KeyEvent) {
    let slots = session.output_pane.slots().len();
    match key.code {
        KeyCode::Esc => session.output_pane.active = false,
        KeyCode::Left | KeyCode::BackTab => {
            session.output_pane.focus = session.output_pane.focus.saturating_sub(1);
        }
        KeyCode::Right | KeyCode::Tab => {
            session.output_pane.focus = (session.output_pane.focus + 1).min(2);
        }
        KeyCode::Down => match session.output_pane.focus {
            // Down from the deck column leaves the group, back onto `global`.
            0 => session.output_pane.active = false,
            1 => {
                session.output_pane.role = (session.output_pane.role + 1).min(1);
                session.output_pane.slot = 0;
            }
            _ => session.output_pane.slot = (session.output_pane.slot + 1).min(slots),
        },
        KeyCode::Up => match session.output_pane.focus {
            0 => {}
            1 => {
                session.output_pane.role = session.output_pane.role.saturating_sub(1);
                session.output_pane.slot = 0;
            }
            _ => session.output_pane.slot = session.output_pane.slot.saturating_sub(1),
        },
        KeyCode::Enter => match session.output_pane.focus {
            0 => session.output_pane.focus = 1,
            1 => session.output_pane.focus = 2,
            _ => activate_output(session),
        },
        KeyCode::Delete => delete_output(session),
        _ => {}
    }
}

/// Binds `device` (and optionally a channel pair) to the chooser's target and refreshes the columns.
fn apply_device(
    session: &mut Session,
    target: DeviceTarget,
    device: usize,
    channels: Option<(u16, u16)>,
) {
    let Some(info) = session.outputs.devices.get(device) else {
        session.warn("no output devices on this host");
        return;
    };
    let name = info.name.clone();
    match bind_device(&session.outputs.mixer_path, target, &name, channels) {
        Ok(role) => {
            let pair = channels
                .map(|(left, right)| format!(" ch {left},{right}"))
                .unwrap_or_default();
            session.ok(format!("{role} ← {name}{pair}"));
        }
        Err(err) => session.error(err),
    }
    session.output_pane.entries = read_outputs(&session.outputs.mixer_path);
    session.output_pane.clamp();
    session.output_names = output_labels(&session.outputs.mixer_path);
}

/// Level two of the chooser: the channel pairs of one interface.
/// Enter in the slot column: rebind an existing output, or start a new one.
fn activate_output(session: &mut Session) {
    let entries = session.output_pane.slots();
    match entries.get(session.output_pane.slot) {
        Some(entry) => {
            let index = entry.index;
            let selected = session
                .output_pane
                .entries
                .iter()
                .find(|entry| entry.index == index)
                .and_then(|entry| entry.device.as_deref())
                .and_then(|pattern| {
                    let needle = pattern.to_ascii_lowercase();
                    crate::tui::devices::hardware_rows(&session.outputs.devices)
                        .iter()
                        .position(|row| {
                            let device = &session.outputs.devices[row.device()];
                            !needle.is_empty() && device.name.to_ascii_lowercase().contains(&needle)
                        })
                })
                .unwrap_or(0);
            session.overlay = Overlay::Devices {
                selected,
                target: DeviceTarget::Output(index),
            };
        }
        // `new`: pick the role first, then the device — the multi-level chooser.
        None => {
            let items = vec![
                MenuItem {
                    label: "master (the room)".to_owned(),
                    action: MenuAction::Device(DeviceTarget::NewOutput("master")),
                },
                MenuItem {
                    label: "precue (the headphones)".to_owned(),
                    action: MenuAction::Device(DeviceTarget::NewOutput("cue")),
                },
            ];
            session.overlay = Overlay::Menu(Menu {
                title: " new output ".to_owned(),
                items,
                selected: 0,
            });
        }
    }
}

/// Removes the selected output from the mixer file.
fn delete_output(session: &mut Session) {
    let Some(entry) = session
        .output_pane
        .slots()
        .get(session.output_pane.slot)
        .map(|entry| entry.index)
    else {
        session.warn("select an output to delete");
        return;
    };
    match remove_output(&session.outputs.mixer_path, entry) {
        Ok(()) => {
            session.output_pane.entries = read_outputs(&session.outputs.mixer_path);
            session.output_pane.clamp();
            session.output_names = output_labels(&session.outputs.mixer_path);
            session.ok("output removed");
        }
        Err(err) => session.error(err),
    }
}

/// Drops `[[output]]` at `index`, keeping the rest of the file as it was written.
fn remove_output(path: &str, index: usize) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|err| format!("{path}: {err}"))?;
    let tables = doc
        .get_mut("output")
        .and_then(|item| item.as_array_of_tables_mut())
        .ok_or_else(|| format!("{path}: no [[output]] table"))?;
    if index >= tables.len() {
        return Err(format!("{path}: no [[output]] #{index}"));
    }
    tables.remove(index);
    std::fs::write(path, doc.to_string()).map_err(|err| format!("{path}: {err}"))
}

fn handle_overlay(session: &mut Session, key: KeyEvent) {
    let overlay = std::mem::replace(&mut session.overlay, Overlay::None);
    match overlay {
        Overlay::None => {}
        Overlay::Ports(mut list) => match key.code {
            KeyCode::Esc | KeyCode::Left => {}
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
        Overlay::Devices {
            mut selected,
            target,
        } => {
            // Level one is hardware only: a device's channel pairs belong to level two, so the list
            // never becomes a tree.
            let rows = crate::tui::devices::hardware_rows(&session.outputs.devices);
            match key.code {
                KeyCode::Esc | KeyCode::Left => {}
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    session.overlay = Overlay::Devices { selected, target };
                }
                KeyCode::Down => {
                    selected = (selected + 1).min(rows.len().saturating_sub(1));
                    session.overlay = Overlay::Devices { selected, target };
                }
                KeyCode::Right => {
                    // Level two lives to the right: `→` enters it, `←` comes back.
                    let Some(row) = rows.get(selected).copied() else {
                        session.overlay = Overlay::Devices { selected, target };
                        return;
                    };
                    let pairs = crate::tui::devices::pairs(
                        session
                            .outputs
                            .devices
                            .get(row.device())
                            .map_or(0, |device| device.channels),
                    );
                    if pairs.len() > 1 {
                        session.overlay = Overlay::Channels {
                            device: row.device(),
                            selected: 0,
                            target,
                        };
                    } else {
                        apply_device(session, target, row.device(), pairs.first().copied());
                    }
                }
                KeyCode::Enter => {
                    let Some(row) = rows.get(selected).copied() else {
                        session.warn("no output devices on this host");
                        session.overlay = Overlay::Devices { selected, target };
                        return;
                    };
                    let device = row.device();
                    let channels = match row {
                        crate::tui::devices::Row::Pair { channels, .. } => Some(channels),
                        crate::tui::devices::Row::Device { .. } => None,
                    };
                    // Level two: an interface with several stereo pairs asks which one this output
                    // is. A plain stereo device is already answered.
                    let pairs = crate::tui::devices::pairs(
                        session
                            .outputs
                            .devices
                            .get(device)
                            .map_or(0, |d| d.channels),
                    );
                    if channels.is_none() && pairs.len() > 1 {
                        session.overlay = Overlay::Channels {
                            device,
                            selected: 0,
                            target,
                        };
                    } else {
                        apply_device(session, target, device, channels);
                    }
                }
                _ => session.overlay = Overlay::Devices { selected, target },
            }
        }
        Overlay::Channels {
            device,
            mut selected,
            target,
        } => {
            let pairs = crate::tui::devices::pairs(
                session
                    .outputs
                    .devices
                    .get(device)
                    .map_or(0, |device| device.channels),
            );
            match key.code {
                KeyCode::Esc => {}
                // Back to the hardware level, which is where the cursor came from.
                KeyCode::Left => {
                    let rows = crate::tui::devices::hardware_rows(&session.outputs.devices);
                    let selected = rows
                        .iter()
                        .position(|row| *row == crate::tui::devices::Row::Device { index: device })
                        .unwrap_or(0);
                    session.overlay = Overlay::Devices { selected, target };
                }
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    session.overlay = Overlay::Channels {
                        device,
                        selected,
                        target,
                    };
                }
                KeyCode::Down => {
                    selected = (selected + 1).min(pairs.len().saturating_sub(1));
                    session.overlay = Overlay::Channels {
                        device,
                        selected,
                        target,
                    };
                }
                KeyCode::Enter | KeyCode::Right => {
                    if let Some(pair) = pairs.get(selected).copied() {
                        apply_device(session, target, device, Some(pair));
                    }
                }
                _ => {
                    session.overlay = Overlay::Channels {
                        device,
                        selected,
                        target,
                    }
                }
            }
        }
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
            KeyCode::Delete | KeyCode::PageUp => {
                picker.up();
                session.overlay = Overlay::Files(picker);
            }
            // `←` backs out of the picker, the same rule as every other level.
            KeyCode::Left => {}
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
            KeyCode::Delete | KeyCode::PageUp => {
                picker.up();
                session.overlay = Overlay::Files(picker);
            }
            KeyCode::Left => {}
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
            KeyCode::Esc | KeyCode::Left => {}
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
                        MenuAction::Device(target) => {
                            session.overlay = Overlay::Devices {
                                selected: 0,
                                target,
                            };
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
    // The `outputs` group reuses the same three columns: outputs › master|precue › destination.
    let outputs = session.output_pane.active && !session.command_active;
    let row = |label: String| Row {
        label,
        muted: false,
    };
    // `audio output` is the first row of the deck column: it is a *group* like `global`, and the
    // decks follow it.
    let mut deck_rows: Vec<Row> = Vec::with_capacity(view.decks.len() + 1);
    deck_rows.push(row("audio output".to_owned()));
    deck_rows.extend(view.decks.iter().cloned());
    let role_rows: Vec<Row> = OutputPane::ROLES
        .iter()
        .map(|role| row((*role).to_owned()))
        .collect();
    let mut slot_rows: Vec<Row> = if outputs {
        session
            .output_pane
            .slots()
            .iter()
            .map(|entry| {
                let device = entry
                    .device
                    .clone()
                    .unwrap_or_else(|| "<host default>".to_owned());
                let pair = entry
                    .channels
                    .map(|(left, right)| format!(" ch {left},{right}"))
                    .unwrap_or_default();
                row(format!("{} {device}{pair}", entry.name))
            })
            .collect()
    } else {
        view.slots.clone()
    };
    if outputs {
        slot_rows.push(row("new".to_owned()));
    }

    let (deck_sel, deck_rows_ref) = if outputs {
        (0, &deck_rows[..])
    } else {
        (view.nav.deck + 1, &deck_rows[..])
    };
    let (unit_sel, unit_rows) = if outputs {
        (session.output_pane.role, &role_rows[..])
    } else {
        (view.nav.unit, &view.units[..])
    };
    let (slot_sel, slot_rows) = if outputs {
        (session.output_pane.slot, &slot_rows[..])
    } else {
        (view.nav.slot, &view.slots[..])
    };

    let focus = if session.command_active {
        None
    } else if outputs {
        Some(match session.output_pane.focus {
            0 => ColumnFocus::Deck,
            1 => ColumnFocus::Unit,
            _ => ColumnFocus::Slot,
        })
    } else {
        Some(view.nav.focus)
    };
    render_column(
        frame,
        columns[0],
        " deck ",
        deck_rows_ref,
        deck_sel,
        focus == Some(ColumnFocus::Deck),
        Column::Deck,
        session,
    );
    render_column(
        frame,
        columns[1],
        if outputs { " role " } else { " unit " },
        unit_rows,
        unit_sel,
        focus == Some(ColumnFocus::Unit),
        Column::Unit,
        session,
    );
    render_column(
        frame,
        columns[2],
        if outputs { " output " } else { " slot " },
        slot_rows,
        slot_sel,
        focus == Some(ColumnFocus::Slot),
        Column::Slot,
        session,
    );

    let lower =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Fill(1)]).split(chunks[2]);
    render_info(frame, lower[0], &view);
    render_watch(frame, lower[1], session, &view);
    render_hint(frame, chunks[3], session, &view);
    render_commands(frame, chunks[4], session, &view);

    // Every modal is the same single-column list; drawing happens last, over everything else.
    if let Some(view) = overlay_view(session) {
        crate::tui::popup::render(frame, area, &session.theme, &view);
    }
}

/// The modal for the current overlay, or `None` when nothing is open. `←`/`→` move between levels,
/// which is why there is never a second window: a deeper level replaces the same list.
fn overlay_view(session: &Session) -> Option<crate::tui::popup::View> {
    use crate::tui::popup::{Row, View};
    match &session.overlay {
        Overlay::None => None,
        Overlay::Ports(list) => Some(list.view("controller › device")),
        Overlay::Files(picker) => {
            let view = picker.view();
            Some(View {
                title: format!("guide › {}", picker.title()),
                ..view
            })
        }
        Overlay::Devices { selected, target } => {
            let bound = match target {
                DeviceTarget::Output(index) => bound_device(&session.outputs.mixer_path, *index),
                DeviceTarget::NewOutput(_) => None,
            }
            .and_then(|(pattern, channels)| {
                device_row(&session.outputs.devices, &pattern, channels)
            });
            let rows = crate::tui::devices::hardware_rows(&session.outputs.devices)
                .iter()
                .enumerate()
                .map(|(index, row)| {
                    let device = &session.outputs.devices[row.device()];
                    let controller =
                        session
                            .outputs
                            .controller
                            .as_deref()
                            .is_some_and(|controller| {
                                crate::tui::devices::matches_controller(device, controller)
                            });
                    let mark = match (bound == Some(index), controller) {
                        (true, _) => '●',
                        (false, true) => '◆',
                        (false, false) => ' ',
                    };
                    let rate = match device.accepts {
                        Some(true) => "✓",
                        Some(false) => "✗ rate",
                        None => "",
                    };
                    Row::new(crate::tui::devices::label(&session.outputs.devices, *row))
                        .mark(mark)
                        .note(format!("{rate} {}", device.rates))
                })
                .collect::<Vec<_>>();
            Some(
                View::new(format!("{} › device", target_title(*target)), rows)
                    .cursor(*selected)
                    .hint(format!(
                        "engine {}Hz · ↑↓ move · →/Enter channels · ← back · Esc close",
                        session.outputs.engine_rate
                    )),
            )
        }
        Overlay::Channels {
            device,
            selected,
            target,
        } => {
            let rows = crate::tui::devices::pairs(
                session
                    .outputs
                    .devices
                    .get(*device)
                    .map_or(0, |device| device.channels),
            )
            .into_iter()
            .enumerate()
            .map(|(index, (left, right))| {
                let row = Row::new(format!("channels {left},{right}"));
                if index == 0 {
                    row.note("the usual stereo pair")
                } else {
                    row
                }
            })
            .collect::<Vec<_>>();
            Some(
                View::new(format!("{} › channels", target_title(*target)), rows)
                    .cursor(*selected)
                    .hint("↑↓ move · Enter bind · ← back · Esc close"),
            )
        }
        Overlay::Menu(menu) => Some(
            View::new(
                menu.title.trim().to_owned(),
                menu.items
                    .iter()
                    .map(|item| Row::new(item.label.clone()))
                    .collect::<Vec<_>>(),
            )
            .cursor(menu.selected)
            .hint("↑↓ move · Enter pick · Esc close"),
        ),
    }
}

/// What a device choice is for, as the popup title says it.
fn target_title(target: DeviceTarget) -> &'static str {
    match target {
        DeviceTarget::Output(_) => "output",
        DeviceTarget::NewOutput("cue") => "new precue output",
        DeviceTarget::NewOutput(_) => "new master output",
    }
}

/// The row of `devices` a bound `device`/`channels` pair points at.
fn device_row(
    devices: &[hypermixx_audio::DeviceInfo],
    pattern: &str,
    channels: Option<(u16, u16)>,
) -> Option<usize> {
    let needle = pattern.to_ascii_lowercase();
    let device = devices.iter().position(|device| {
        !needle.is_empty() && device.name.to_ascii_lowercase().contains(&needle)
    })?;
    let wanted = match channels {
        Some(pair) => crate::tui::devices::Row::Pair {
            index: device,
            channels: pair,
        },
        None => crate::tui::devices::Row::Device { index: device },
    };
    crate::tui::devices::hardware_rows(devices)
        .iter()
        .position(|row| *row == wanted)
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
            " CONTROLLER ",
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
    } else if session.output_pane.active {
        "audio output · ↑↓ pick · ←→ column · Enter choose device / new · Del remove · ↑ to leave"
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

/// The host's output devices for the mixer's master output, with the bound one marked.
/// `[role] name` of every `[[output]]` the mixer file declares, for the status line.
fn output_labels(path: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(tables) = doc.get("output").and_then(|item| item.as_array_of_tables()) {
        for table in tables.iter() {
            let name = table
                .get("name")
                .and_then(|item| item.as_str())
                .unwrap_or("output");
            let role = table
                .get("role")
                .and_then(|item| item.as_str())
                .unwrap_or("master");
            out.push(format!("{role}:{name}"));
        }
    }
    out
}

/// One output's `(device pattern, channel pair)`, as written in the mixer file.
fn bound_device(path: &str, index: usize) -> Option<(String, Option<(u16, u16)>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc = text.parse::<toml_edit::DocumentMut>().ok()?;
    let table = doc.get("output")?.as_array_of_tables()?.get(index)?;
    let device = table
        .get("device")
        .and_then(|item| item.as_str())
        .map(str::to_owned)?;
    let channels = table
        .get("channels")
        .and_then(|item| item.as_array())
        .filter(|array| array.len() == 2)
        .and_then(|array| {
            Some((
                u16::try_from(array.get(0)?.as_integer()?).ok()?,
                u16::try_from(array.get(1)?.as_integer()?).ok()?,
            ))
        });
    Some((device, channels))
}

/// Writes a device into one output, or appends a new output for a role.
///
/// `toml_edit` again: the guide is not allowed to reformat a hand-written topology, and the mixer
/// file is a second document from the map — the `Save` button belongs to the map, these edits land
/// immediately.
fn bind_device(
    path: &str,
    target: DeviceTarget,
    device: &str,
    channels: Option<(u16, u16)>,
) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|err| format!("{path}: {err}"))?;
    if doc.get("output").is_none() {
        doc.insert(
            "output",
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }
    let tables = doc
        .get_mut("output")
        .and_then(|item| item.as_array_of_tables_mut())
        .ok_or_else(|| format!("{path}: `output` is not an array of tables"))?;
    let index = match target {
        DeviceTarget::Output(index) => index,
        DeviceTarget::NewOutput(role) => {
            let mut table = toml_edit::Table::new();
            let name = format!(
                "{}{}",
                if role == "cue" { "phones" } else { "main" },
                tables.len()
            );
            table.insert("name", toml_edit::Item::Value(name.clone().into()));
            table.insert("role", toml_edit::Item::Value(role.into()));
            let mut decor = toml_edit::Decor::default();
            decor.set_prefix("\n");
            table.decor_mut().clone_from(&decor);
            tables.push(table);
            tables.len() - 1
        }
    };
    let table = tables
        .get_mut(index)
        .ok_or_else(|| format!("{path}: no [[output]] #{index}"))?;
    let role = table
        .get("role")
        .and_then(|item| item.as_str())
        .unwrap_or("master")
        .to_owned();
    let name = table
        .get("name")
        .and_then(|item| item.as_str())
        .unwrap_or("output")
        .to_owned();
    // Keep the decor (comments, blank lines) around the value.
    if let Some(toml_edit::Item::Value(existing)) = table.get_mut("device") {
        let decor = existing.decor().clone();
        *existing = device.into();
        *existing.decor_mut() = decor;
    } else {
        table.insert("device", toml_edit::Item::Value(device.into()));
    }
    if let Some((left, right)) = channels {
        let mut array = toml_edit::Array::new();
        array.push(i64::from(left));
        array.push(i64::from(right));
        match table.get_mut("channels") {
            Some(toml_edit::Item::Value(existing)) => {
                let decor = existing.decor().clone();
                *existing = toml_edit::Value::Array(array);
                *existing.decor_mut() = decor;
            }
            _ => {
                table.insert(
                    "channels",
                    toml_edit::Item::Value(toml_edit::Value::Array(array)),
                );
            }
        }
    }
    std::fs::write(path, doc.to_string()).map_err(|err| format!("{path}: {err}"))?;
    Ok(format!("{role}:{name}"))
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
        Session::new("does-not-exist.toml", None, 2, Outputs::default()).unwrap()
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
    fn the_output_button_binds_the_master_device_and_keeps_the_comment() {
        let dir = std::env::temp_dir().join(format!(
            "hypermixx-guide-output-{}-{}",
            std::process::id(),
            hypermixx_library::library::db::unix_now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mixer = dir.join("mixer.toml");
        std::fs::write(
            &mixer,
            "# my rig\n[[output]]\nname = \"main\"\nrole = \"master\"\n\n\
             [[output]]\nname = \"phones\"\nrole = \"cue\"\n",
        )
        .unwrap();
        let path = mixer.display().to_string();
        assert!(output_labels(&path).contains(&"master:main".to_owned()));
        assert_eq!(bound_device(&path, 0), None, "no device yet");

        let role = bind_device(
            &path,
            DeviceTarget::Output(0),
            "USB Interface",
            Some((2, 3)),
        )
        .expect("bound");
        assert_eq!(role, "master:main");
        let written = std::fs::read_to_string(&mixer).unwrap();
        assert!(written.contains("# my rig"), "{written}");
        assert!(written.contains("device = \"USB Interface\""), "{written}");
        assert!(written.contains("channels = [2, 3]"), "{written}");
        assert_eq!(
            bound_device(&path, 0),
            Some(("USB Interface".to_owned(), Some((2, 3)))),
            "the channel pair is written with the device"
        );
    }

    /// One key press through the guide's real router.
    fn press(session: &mut Session, code: KeyCode) {
        handle_key(session, KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn the_audio_output_row_is_reached_from_the_deck_column() {
        let mut session = test_session();
        session.guide.select_deck(0);
        // Start from the deck column (the guide opens on `unit`).
        for _ in 0..3 {
            if session.guide.view().nav.focus == ColumnFocus::Deck {
                break;
            }
            press(&mut session, KeyCode::Left);
        }
        assert_eq!(session.guide.view().nav.focus, ColumnFocus::Deck);
        // `audio output` is the row *above* `global`: walk up to `global`, then once more to enter.
        for _ in 0..4 {
            if session.guide.view().nav.deck == 0 {
                break;
            }
            press(&mut session, KeyCode::Up);
        }
        assert_eq!(
            session.guide.view().nav.deck,
            0,
            "the cursor is on `global`"
        );
        press(&mut session, KeyCode::Up);
        assert!(
            session.output_pane.active,
            "Up from `global` lands on the `audio output` row"
        );
        assert_eq!(
            session.output_pane.focus, 0,
            "the deck column keeps the cursor"
        );
        // Down leaves it again, back onto `global`.
        press(&mut session, KeyCode::Down);
        assert!(!session.output_pane.active);
        assert_eq!(session.guide.view().nav.deck, 0);
    }

    #[test]
    fn the_outputs_group_is_a_three_column_browser() {
        let dir = std::env::temp_dir().join(format!(
            "hypermixx-guide-outputs-{}-{}",
            std::process::id(),
            hypermixx_library::library::db::unix_now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mixer = dir.join("mixer.toml");
        std::fs::write(
            &mixer,
            "[[output]]\nname = \"main\"\nrole = \"master\"\n\n\
             [[output]]\nname = \"phones\"\nrole = \"cue\"\n",
        )
        .unwrap();
        let mut session = test_session();
        session.outputs.mixer_path = mixer.display().to_string();
        session.output_pane = OutputPane::load(&session.outputs.mixer_path);
        session.output_pane.active = true;

        // Role column: master first, cue second — each with its own destinations.
        assert_eq!(OutputPane::file_role(0), "master");
        assert_eq!(OutputPane::file_role(1), "cue");
        assert_eq!(session.output_pane.slots().len(), 1);
        assert_eq!(session.output_pane.slots()[0].name, "main");
        session.output_pane.role = 1;
        assert_eq!(session.output_pane.slots()[0].name, "phones");

        // The slot column ends with `new`, and Enter on it opens the role chooser.
        session.output_pane.focus = 2;
        session.output_pane.slot = 0;
        press(&mut session, KeyCode::Down);
        assert_eq!(
            session.output_pane.slot, 1,
            "one row past the last output is `new`"
        );
        press(&mut session, KeyCode::Enter);
        match &session.overlay {
            Overlay::Menu(menu) => assert_eq!(menu.items.len(), 2, "master or precue"),
            _ => panic!("`new` opens the multi-level chooser"),
        }

        // A 4-channel interface asks for the channel pair as a second level.
        session.overlay = Overlay::None;
        session.output_pane.slot = 0;
        session.outputs.devices = vec![hypermixx_audio::DeviceInfo {
            name: "Quad Interface".to_owned(),
            description: String::new(),
            is_default: true,
            rates: "44100".to_owned(),
            accepts: Some(true),
            channels: 4,
        }];
        press(&mut session, KeyCode::Enter); // -> hardware list
        assert!(
            matches!(session.overlay, Overlay::Devices { .. }),
            "Enter on an output opens the hardware level"
        );
        press(&mut session, KeyCode::Enter); // pick the only hardware
        assert!(
            matches!(session.overlay, Overlay::Channels { .. }),
            "the hardware level is followed by the channel level"
        );
        press(&mut session, KeyCode::Down); // channels 2,3
        press(&mut session, KeyCode::Enter);
        let written = std::fs::read_to_string(&session.outputs.mixer_path).unwrap();
        assert!(written.contains("channels = [2, 3]"), "{written}");
        assert!(written.contains("device = \"Quad Interface\""), "{written}");

        // Deleting writes the file back without that output.
        session.overlay = Overlay::None;
        session.output_pane.slot = 0;
        press(&mut session, KeyCode::Delete);
        let written = std::fs::read_to_string(&session.outputs.mixer_path).unwrap();
        assert!(!written.contains("phones"), "{written}");
        assert!(written.contains("main"), "{written}");
    }

    #[test]
    fn the_output_overlay_moves_with_the_cursor() {
        let mut session = test_session();
        session.outputs = Outputs {
            mixer_path: "profile/mixer.toml".to_owned(),
            devices: vec![
                hypermixx_audio::DeviceInfo {
                    name: "A".to_owned(),
                    description: String::new(),
                    is_default: true,
                    rates: "44100".to_owned(),
                    accepts: Some(true),
                    channels: 2,
                },
                hypermixx_audio::DeviceInfo {
                    name: "B".to_owned(),
                    description: String::new(),
                    is_default: false,
                    rates: "44100".to_owned(),
                    accepts: Some(true),
                    channels: 2,
                },
            ],
            engine_rate: 44_100,
            preference: crate::preference::DEFAULT_PATH.to_owned(),
            controller: None,
        };
        session.overlay = Overlay::Devices {
            selected: 0,
            target: DeviceTarget::Output(0),
        };
        let selected = |session: &Session| match &session.overlay {
            Overlay::Devices { selected, .. } => Some(*selected),
            _ => None,
        };
        let press = |session: &mut Session, code| {
            handle_key(session, KeyEvent::new(code, KeyModifiers::NONE));
        };
        press(&mut session, KeyCode::Down);
        assert_eq!(selected(&session), Some(1), "Down moves the device cursor");
        press(&mut session, KeyCode::Up);
        assert_eq!(selected(&session), Some(0), "Up moves it back");
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
    fn every_overlay_is_the_same_flat_popup() {
        let mut session = test_session();
        session.outputs = Outputs {
            mixer_path: "/nonexistent/mixer.toml".to_owned(),
            devices: vec![hypermixx_audio::DeviceInfo {
                name: "sysdefault:CARD=III".to_owned(),
                description: "Numark Party Mix III".to_owned(),
                is_default: true,
                rates: "44100".to_owned(),
                accepts: Some(true),
                channels: 2,
            }],
            engine_rate: 44_100,
            preference: crate::preference::DEFAULT_PATH.to_owned(),
            controller: Some("Numark Party Mix III".to_owned()),
        };
        // The device chooser: a plain title, one column, and a hint line that says where `←`/`→` go.
        session.overlay = Overlay::Devices {
            selected: 0,
            target: DeviceTarget::Output(0),
        };
        let text = render_once(&mut session, 100, 30);
        assert!(text.contains("output › device"), "{text}");
        assert!(
            text.contains("◆"),
            "the controller's own interface is marked"
        );
        assert!(text.contains("engine 44100Hz"), "{text}");

        // The menu uses the same widget, so it has the same title and hint treatment.
        session.overlay = Overlay::Menu(Menu {
            title: " new output ".to_owned(),
            items: vec![MenuItem {
                label: "master (the room)".to_owned(),
                action: MenuAction::Device(DeviceTarget::NewOutput("master")),
            }],
            selected: 0,
        });
        let text = render_once(&mut session, 100, 30);
        assert!(text.contains(" new output "), "{text}");
        assert!(text.contains("Enter pick"), "{text}");
    }

    #[test]
    fn the_shipped_map_renders() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = root.join("profile/midi-map.toml");
        let mut session =
            Session::new(&path.display().to_string(), None, 2, Outputs::default()).unwrap();
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
