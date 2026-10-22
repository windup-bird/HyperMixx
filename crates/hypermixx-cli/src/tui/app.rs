//! TUI application state.
//!
//! Deliberately plain data plus small mutators: rendering reads it, input mutates it, the event
//! loop feeds it. Nothing here draws or touches a device, which is what keeps the renderer and the
//! input mapping testable without a terminal.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use hypermixx_audio::{AudioPipeline, Meters};
use hypermixx_core::{Backend, BeatGrid, Command, CommandResponse, DeckState, KeylockMode, StemStatus};

use crate::command::{Slots, UiEvent};
use crate::notices::NoticeTx;
use crate::response::{self, LogLine};
use crate::preference::WaveformSettings;
use crate::procmon::ProcessMonitor;
use crate::tui::browser::Browser;
use crate::tui::waveform_view::WaveformView;
use crate::tui::completer::{self, Completion, Ctx};
use crate::tui::picker::{FilePicker, PortList};

/// Waveform zoom bounds, in frames per terminal cell. 512 frames is ~11.6ms per cell (the finest
/// the peak pyramid resolves), 131072 is ~3s per cell (a very wide overview). Keeping the unit
/// backend-independent means switching braille ↔ image keeps the same visible span.
pub const MIN_FRAMES_PER_CELL: f64 = 512.0;
pub const MAX_FRAMES_PER_CELL: f64 = 131_072.0;
/// Default visible span: 2048 frames/cell over ~120 cells is roughly five seconds.
pub const DEFAULT_FRAMES_PER_CELL: f64 = 2_048.0;
/// Keep the debug log bounded; this is a terminal, not a log file.
const LOG_CAP: usize = 500;
/// Candidates shown in the completion popup.
pub const COMPLETION_ROWS: usize = 8;

/// Which prefix the command line currently speaks for.
///
/// `Browser` has no CLI prefix yet, so a submitted line falls back to the last focused deck; when
/// the browser grows its own command family it will route there instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Deck(usize),
    Browser,
}

impl Target {
    /// The prefix shown on the command rule: `deck0`, `deck1`, … or empty for the browser.
    pub fn prefix(self) -> String {
        match self {
            Self::Deck(index) => format!("deck{index}"),
            Self::Browser => String::new(),
        }
    }

    /// Full tab label used by the command rule.
    pub fn label(self) -> String {
        match self {
            Self::Deck(index) => format!("deck{index}"),
            Self::Browser => "browser".to_owned(),
        }
    }

    /// Short tab label for a narrow command panel.
    pub fn compact_label(self) -> String {
        match self {
            Self::Deck(index) => format!("d{index}"),
            Self::Browser => "br".to_owned(),
        }
    }
}

/// The open completion popup and which row is highlighted.
pub struct CompletionState {
    pub completion: Completion,
    pub selected: usize,
}

/// A modal picker drawn over the UI. While set, the keyboard belongs to it.
pub enum Overlay {
    /// Choosing a MIDI input port.
    Ports { list: PortList },
    /// Choosing a file: a track to load, or a MIDI map.
    Files {
        picker: FilePicker,
        purpose: FilePurpose,
    },
}

/// What the file picker's choice means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilePurpose {
    /// Decode and install into this deck.
    Load { deck: usize },
    /// Use as the MIDI mapping file (and connect if a port is known).
    MidiMap,
}

/// A deck's transport, as the TUI sees it. Mirrors [`DeckState`] plus front-end-only data.
pub struct App {
    pub version: &'static str,
    pub backend: Backend,
    pub sample_rate: u32,
    /// Output ring capacity in frames, from the live config (the sliders can change it).
    pub ring_frames: usize,
    pub decks: usize,
    pub focused: usize,
    /// The command-line target: one deck, or the (still prefix-less) browser.
    pub target: Target,
    /// The browser pane: layout/state only until folder scanning lands.
    pub browser: Browser,

    pub states: Vec<DeckState>,
    /// File name per deck (from the load tap), for the deck header.
    pub titles: Vec<Option<String>>,
    pub waveforms: Vec<Option<Arc<hypermixx_library::Waveform>>>,
    pub grids: Vec<Option<BeatGrid>>,

    /// Shared zoom, in frames per terminal cell. Both decks read this so they stay comparable.
    pub frames_per_cell: f64,
    /// Last waveform width in cells, so `fit` knows the viewport.
    pub wave_width_cells: usize,
    /// Waveform preferences (renderer, colours, image refresh target).
    pub waveform: WaveformSettings,
    /// The selected waveform transport (braille / kitty / sixel) and its per-deck image state.
    pub waveform_view: WaveformView,

    pub log: VecDeque<LogLine>,
    /// Lines scrolled up from the bottom (0 = following).
    pub log_offset: usize,

    pub command: Vec<char>,
    pub cursor: usize,
    pub completion: Option<CompletionState>,
    pub history: Vec<String>,
    pub history_pos: Option<usize>,

    /// Slot kinds per chain label (`"deck0"`, `"master"`), shared with the dispatcher; feeds
    /// name resolution and param completion.
    pub fx_slots: Slots,

    pub pending_states: usize,
    pub meters: Option<Meters>,
    /// Exponential moving averages for the header: UI draw cost and measured frame rate.
    pub ui_draw_ms: f32,
    pub ui_fps: f32,
    /// Start of the previous draw, for the frame-rate average.
    pub last_draw_at: Option<Instant>,
    /// This process's CPU/RSS, sampled once a second.
    pub process: ProcessMonitor,
    pub quit: bool,

    /// A modal picker, if one is open.
    pub overlay: Option<Overlay>,
    /// The live MIDI input, when connected; dropping the app closes the port.
    pub midi: Option<crate::midi::MidiSession>,
    /// The chosen (or flag-supplied) port, remembered across reconnects.
    pub midi_port: Option<String>,
    /// The chosen (or flag-supplied) map path.
    pub midi_map: Option<String>,

    notice_rx: Receiver<LogLine>,
    event_rx: Receiver<UiEvent>,
    waveform_tx: Sender<(usize, Arc<hypermixx_library::Waveform>)>,
    waveform_rx: Receiver<(usize, Arc<hypermixx_library::Waveform>)>,
}

impl App {
    pub fn new(
        decks: usize,
        backend: Backend,
        notice_rx: Receiver<LogLine>,
        event_rx: Receiver<UiEvent>,
        fx_slots: Slots,
    ) -> Self {
        let decks = decks.max(1);
        let (waveform_tx, waveform_rx) = crossbeam_channel::unbounded();
        let browser = Browser::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        Self {
            version: crate::VERSION,
            backend,
            sample_rate: hypermixx_core::SAMPLE_RATE,
            ring_frames: hypermixx_audio::OUTPUT_RING_CAPACITY,
            decks,
            focused: 0,
            target: Target::Deck(0),
            browser,
            states: (0..decks)
                .map(|index| DeckState {
                    deck_id: index as u8,
                    current_frame: 0,
                    playing: false,
                    total_frames: 0,
                    bpm: 0.0,
                    bpm_at_frame: 0.0,
                    key: None,
                    virtual_frame: 0,
                    loop_range: None,
                    loop_in_armed: None,
                    tempo: 1.0,
                    tempo_fader: 0.0,
                    tempo_range: 0.1,
                    nudgerate: 0.0,
                    playing_rate: 1.0,
                    lock: false,
                    align: None,
                    nudge: 0.0,
                    sync_leader: None,
                    sync_mode: "free".to_owned(),
                    group_bpm: 0.0,
                    cue_frame: 0,
                    vinyl: false,
                    keylock: KeylockMode::On,
                    key_shift: 0,
                    stems: StemStatus::default(),
                })
                .collect(),
            titles: vec![None; decks],
            waveforms: vec![None; decks],
            grids: vec![None; decks],
            frames_per_cell: DEFAULT_FRAMES_PER_CELL,
            wave_width_cells: 200,
            waveform: WaveformSettings::default(),
            waveform_view: WaveformView::new(WaveformSettings::default(), decks),
            log: VecDeque::new(),
            log_offset: 0,
            command: Vec::new(),
            cursor: 0,
            completion: None,
            history: Vec::new(),
            history_pos: None,
            fx_slots,
            pending_states: 0,
            meters: None,
            ui_draw_ms: 0.0,
            ui_fps: 60.0,
            last_draw_at: None,
            process: ProcessMonitor::new(),
            quit: false,
            overlay: None,
            midi: None,
            midi_port: None,
            midi_map: None,
            notice_rx,
            event_rx,
            waveform_tx,
            waveform_rx,
        }
    }

    /// Opens the remembered port with the remembered map, replacing any current input.
    ///
    /// FX names are resolved from the **already-primed slot book**, not by asking the engine: it
    /// runs inside the UI thread, and reading the shared response channel here would swallow a
    /// `Loaded`/`States`/`Error` answer meant for the UI. On failure the reason is logged and the
    /// matching picker reopens so another choice can be made.
    pub fn connect_midi(&mut self, command_tx: Sender<Command>, notices: &NoticeTx) {
        let Some(port) = self.midi_port.clone() else {
            return;
        };
        let map_path = self
            .midi_map
            .clone()
            .unwrap_or_else(|| "midi-map.toml".to_owned());
        let mut map = match crate::midi::read_map(&map_path) {
            Ok(map) => map,
            Err(err) => {
                self.push_log(LogLine::error(format!("midi: {err}")));
                self.overlay = Some(Overlay::Files {
                    picker: FilePicker::new(
                        Some(std::path::Path::new(&map_path)),
                        &["toml"],
                        "midi map",
                    ),
                    purpose: FilePurpose::MidiMap,
                });
                return;
            }
        };
        let slots = self.fx_slots.clone();
        let unresolved = crate::midi::resolve_with(&mut map, |chain, fx| {
            slots.resolve(&chain.label(), fx).ok()
        });
        crate::midi::report_unresolved(notices, unresolved);
        match crate::midi::attach(&port, map, command_tx) {
            Ok(session) => {
                // Replacing the old session closes its port before the new one is held.
                self.midi = Some(session);
                self.push_log(LogLine::info(format!(
                    "[midi] connected `{port}` ({map_path})"
                )));
            }
            Err(err) => {
                self.push_log(LogLine::error(format!("midi: {err}")));
                self.overlay = Some(Overlay::Ports {
                    list: PortList::refresh(),
                });
            }
        }
    }

    /// Appends one line and re-follows the tail.
    pub fn push_log(&mut self, line: LogLine) {
        self.log.push_back(line);
        while self.log.len() > LOG_CAP {
            self.log.pop_front();
        }
        self.log_offset = 0;
    }

    pub fn push_lines(&mut self, lines: impl IntoIterator<Item = LogLine>) {
        for line in lines {
            self.push_log(line);
        }
    }

    /// Applies an engine response. Deck states update silently (they are on every poll); everything
    /// else is echoed to the log, exactly as the REPL would print it.
    pub fn on_response(&mut self, response: &CommandResponse, pipeline: &AudioPipeline) {
        let sample_rate = self.sample_rate;
        match response {
            CommandResponse::States(states) => {
                for state in states {
                    self.set_state(state);
                }
                self.pending_states = self.pending_states.saturating_sub(1);
            }
            CommandResponse::State(state) => self.set_state(state),
            CommandResponse::Loaded {
                deck_id,
                total_frames,
            } => {
                if let Some(state) = self.states.get_mut(*deck_id as usize) {
                    state.total_frames = *total_frames;
                }
                self.spawn_waveform(*deck_id, pipeline);
                self.push_lines(response::format_response(response, sample_rate));
            }
            CommandResponse::FxListed { .. } => {
                self.fx_slots.record(response);
                self.push_lines(response::format_response(response, sample_rate));
            }
            CommandResponse::FxAdded { .. } => {
                self.fx_slots.record(response);
                self.push_lines(response::format_response(response, sample_rate));
            }
            _ => self.push_lines(response::format_response(response, sample_rate)),
        }
    }

    fn set_state(&mut self, state: &DeckState) {
        if let Some(slot) = self.states.get_mut(state.deck_id as usize) {
            *slot = state.clone();
        }
    }

    /// Builds the peak pyramid off the UI thread. `deck_source` is a cheap producer query, so the
    /// brief round-trip happens here; the O(track) scan does not.
    fn spawn_waveform(&self, deck_id: u8, pipeline: &AudioPipeline) {
        let Some(source) = pipeline.deck_source(deck_id) else {
            return;
        };
        let tx = self.waveform_tx.clone();
        let index = deck_id as usize;
        let low_hz = f32::from(self.waveform.low_hz);
        let high_hz = f32::from(self.waveform.high_hz);
        let sample_rate = self.sample_rate;
        std::thread::Builder::new()
            .name(format!("hypermixx-waveform-{deck_id}"))
            .spawn(move || {
                let waveform = Arc::new(hypermixx_library::Waveform::build_with_crossovers_at(
                    source.as_ref(),
                    low_hz,
                    high_hz,
                    sample_rate,
                ));
                let _ = tx.send((index, waveform));
            })
            .ok();
    }

    /// Drains every side channel without blocking. Returns true if anything changed.
    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(line) = self.notice_rx.try_recv() {
            self.push_log(line);
            changed = true;
        }
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                UiEvent::Loaded { deck_id, path } => {
                    if let Some(title) = self.titles.get_mut(deck_id as usize) {
                        *title = std::path::Path::new(&path)
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned());
                    }
                }
                UiEvent::Grid { deck_id, analysis } => {
                    if let Some(grid) = self.grids.get_mut(deck_id as usize) {
                        *grid = Some(analysis.beatgrid);
                    }
                }
            }
            changed = true;
        }
        while let Ok((index, waveform)) = self.waveform_rx.try_recv() {
            if let Some(slot) = self.waveforms.get_mut(index) {
                *slot = Some(waveform);
            }
            changed = true;
        }
        changed
    }

    /// The deck a command with no explicit target acts on. The browser has no commands of its
    /// own yet, so it keeps pointing at the last focused deck instead of silently disabling input.
    pub fn default_deck(&self) -> usize {
        match self.target {
            Target::Deck(index) => index,
            Target::Browser => self.focused,
        }
    }

    /// Cycles the command target: deck0 → deck1 → … → browser → deck0. The browser has no CLI
    /// prefix yet, but it is already a stop on the ring.
    pub fn cycle_target(&mut self, delta: isize) {
        let slots = self.decks as isize + 1;
        let current = match self.target {
            Target::Deck(index) => index as isize,
            Target::Browser => self.decks as isize,
        };
        let next = (current + delta).rem_euclid(slots) as usize;
        self.target = if next == self.decks {
            Target::Browser
        } else {
            Target::Deck(next)
        };
        if let Target::Deck(index) = self.target {
            self.focused = index;
        }
        self.refresh_completion();
    }

    pub fn zoom_in(&mut self) {
        self.frames_per_cell = (self.frames_per_cell / 2.0).max(MIN_FRAMES_PER_CELL);
    }

    pub fn zoom_out(&mut self) {
        self.frames_per_cell = (self.frames_per_cell * 2.0).min(MAX_FRAMES_PER_CELL);
    }

    /// Fits the longest loaded track into the current viewport. A no-op until a track is loaded.
    pub fn fit(&mut self) {
        let longest = self
            .states
            .iter()
            .map(|state| state.total_frames)
            .max()
            .unwrap_or(0);
        if longest == 0 || self.wave_width_cells == 0 {
            return;
        }
        self.frames_per_cell = (longest as f64 / self.wave_width_cells as f64)
            .clamp(MIN_FRAMES_PER_CELL, MAX_FRAMES_PER_CELL);
    }

    pub fn log_scroll(&mut self, delta: isize) {
        let max = self.log.len().saturating_sub(1) as isize;
        self.log_offset = (self.log_offset as isize + delta).clamp(0, max) as usize;
    }

    /// Clears the command line and closes any popup. The input box is always focused, so this is
    /// what `Esc` and a submitted command both do.
    pub fn clear_command(&mut self) {
        self.command.clear();
        self.cursor = 0;
        self.completion = None;
        self.history_pos = None;
    }

    // ---- command-line editing -------------------------------------------------

    pub fn insert_char(&mut self, ch: char) {
        self.command.insert(self.cursor, ch);
        self.cursor += 1;
        self.refresh_completion();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.command.remove(self.cursor - 1);
            self.cursor -= 1;
        }
        self.refresh_completion();
    }

    pub fn delete(&mut self) {
        if self.cursor < self.command.len() {
            self.command.remove(self.cursor);
        }
        self.refresh_completion();
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let max = self.command.len() as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
        self.refresh_completion();
    }

    pub fn command_string(&self) -> String {
        self.command.iter().collect()
    }

    /// Recomputes the popup from the current text and cursor. An empty (or blank) box has nothing
    /// to complete.
    pub fn refresh_completion(&mut self) {
        if self.command.iter().all(|ch| ch.is_whitespace()) {
            self.completion = None;
            return;
        }
        let slots = self.fx_slots.snapshot();
        let ctx = Ctx {
            decks: self.decks,
            chain: format!("deck{}", self.default_deck()),
            slots: &slots,
        };
        self.completion = completer::complete(&self.command, self.cursor, &ctx).map(|completion| {
            CompletionState {
                completion,
                selected: 0,
            }
        });
    }

    pub fn completion_move(&mut self, delta: isize) {
        if let Some(state) = &mut self.completion {
            let len = state.completion.items.len() as isize;
            if len > 0 {
                state.selected = (state.selected as isize + delta).rem_euclid(len) as usize;
            }
        }
    }

    /// Replaces the completed token with the highlighted candidate. A space is appended so the next
    /// argument starts cleanly, unless the candidate is a directory (`foo/`).
    pub fn completion_accept(&mut self) {
        let Some(state) = self.completion.take() else {
            return;
        };
        let Some(candidate) = state.completion.items.get(state.selected) else {
            return;
        };
        let range = state.completion.replace.clone();
        self.command.splice(range.clone(), candidate.text.chars());
        self.cursor = range.start + candidate.text.chars().count();
        if !candidate.text.ends_with('/') {
            self.command.insert(self.cursor, ' ');
            self.cursor += 1;
        }
        self.refresh_completion();
    }

    /// True when the highlighted candidate differs from the typed token, i.e. applying it would
    /// change the buffer. An exact match means `Enter` should submit rather than "complete" to the
    /// same word (which would otherwise need a second keypress).
    pub fn completion_changes_token(&self) -> bool {
        let Some(state) = &self.completion else {
            return false;
        };
        let Some(candidate) = state.completion.items.get(state.selected) else {
            return false;
        };
        let typed: String = self.command[state.completion.replace.clone()]
            .iter()
            .collect();
        candidate.text != typed
    }

    /// Browses submitted commands. Up from the prompt loads the newest.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_pos {
            Some(0) => 0,
            Some(index) => index - 1,
            None => self.history.len() - 1,
        };
        self.history_pos = Some(next);
        self.command = self.history[next].chars().collect();
        self.cursor = self.command.len();
        self.completion = None;
    }

    pub fn history_next(&mut self) {
        let Some(index) = self.history_pos else {
            return;
        };
        if index + 1 >= self.history.len() {
            self.history_pos = None;
            self.command.clear();
            self.cursor = 0;
        } else {
            self.history_pos = Some(index + 1);
            self.command = self.history[index + 1].chars().collect();
            self.cursor = self.command.len();
        }
        self.completion = None;
    }
}
