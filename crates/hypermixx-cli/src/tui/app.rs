//! TUI application state.
//!
//! Deliberately plain data plus small mutators: rendering reads it, input mutates it, the event
//! loop feeds it. Nothing here draws or touches a device, which is what keeps the renderer and the
//! input mapping testable without a terminal.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use hypermixx_audio::{AudioPipeline, Meters};
use hypermixx_core::{Backend, BeatGrid, CommandResponse, DeckState};

use crate::command::{Slots, UiEvent};
use crate::response::{self, LogLine};
use crate::tui::completer::{self, Completion, Ctx};

/// Waveform zoom bounds, in frames per braille dot. 256 frames is ~5.8ms, the finest bucket the
/// peak pyramid holds; 65536 is ~1.5s per dot (a very wide overview).
pub const MIN_FRAMES_PER_DOT: f64 = 256.0;
pub const MAX_FRAMES_PER_DOT: f64 = 65_536.0;
/// Default visible span: 1024 frames/dot over ~400 dots is roughly ten seconds.
pub const DEFAULT_FRAMES_PER_DOT: f64 = 1_024.0;
/// Keep the debug log bounded; this is a terminal, not a log file.
const LOG_CAP: usize = 500;
/// Candidates shown in the completion popup.
pub const COMPLETION_ROWS: usize = 8;

/// The open completion popup and which row is highlighted.
pub struct CompletionState {
    pub completion: Completion,
    pub selected: usize,
}

/// A deck's transport, as the TUI sees it. Mirrors [`DeckState`] plus front-end-only data.
pub struct App {
    pub version: &'static str,
    pub backend: Backend,
    pub sample_rate: u32,
    pub decks: usize,
    pub focused: usize,

    pub states: Vec<DeckState>,
    /// File name per deck (from the load tap), for the deck header.
    pub titles: Vec<Option<String>>,
    pub waveforms: Vec<Option<Arc<hypermixx_library::Waveform>>>,
    pub grids: Vec<Option<BeatGrid>>,
    /// CLI-side tempo mirror; refreshed from every state answer so the header tracks the engine
    /// (a lock or a fader on the other deck moves it without this front-end doing anything).
    pub rates: Vec<f32>,
    pub profiles: Vec<&'static str>,

    /// Shared zoom, in frames per braille dot. Both decks read this so they stay comparable.
    pub frames_per_dot: f64,
    /// Last waveform width in braille dots, so `fit` knows the viewport.
    pub wave_width_dots: usize,

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
    pub state_sent: Option<Instant>,
    pub state_latency: Duration,
    pub meters: Option<Meters>,
    pub ui_frame_ms: f32,
    pub quit: bool,

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
        Self {
            version: crate::VERSION,
            backend,
            sample_rate: hypermixx_core::SAMPLE_RATE,
            decks,
            focused: 0,
            states: (0..decks)
                .map(|index| DeckState {
                    deck_id: index as u8,
                    current_frame: 0,
                    playing: false,
                    total_frames: 0,
                    bpm: 0.0,
                    key: None,
                    virtual_frame: 0,
                    loop_range: None,
                    loop_in_armed: None,
                    tempo: 1.0,
                    nudgerate: 0.0,
                    playing_rate: 1.0,
                    lock: false,
                    align: None,
                    nudge: 0.0,
                    sync_leader: None,
                    sync_mode: "free".to_owned(),
                    group_bpm: 0.0,
                })
                .collect(),
            titles: vec![None; decks],
            waveforms: vec![None; decks],
            grids: vec![None; decks],
            rates: vec![1.0; decks],
            profiles: vec!["tape"; decks],
            frames_per_dot: DEFAULT_FRAMES_PER_DOT,
            wave_width_dots: 200,
            log: VecDeque::new(),
            log_offset: 0,
            command: Vec::new(),
            cursor: 0,
            completion: None,
            history: Vec::new(),
            history_pos: None,
            fx_slots,
            pending_states: 0,
            state_sent: None,
            state_latency: Duration::ZERO,
            meters: None,
            ui_frame_ms: 0.0,
            quit: false,
            notice_rx,
            event_rx,
            waveform_tx,
            waveform_rx,
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
        match response {
            CommandResponse::States(states) => {
                for state in states {
                    self.set_state(state);
                }
                self.pending_states = self.pending_states.saturating_sub(1);
                if let Some(sent) = self.state_sent.take() {
                    self.state_latency = sent.elapsed();
                }
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
                self.push_lines(response::format_response(response));
            }
            CommandResponse::FxListed { .. } => {
                self.fx_slots.record(response);
                self.push_lines(response::format_response(response));
            }
            CommandResponse::FxAdded { .. } => {
                self.fx_slots.record(response);
                self.push_lines(response::format_response(response));
            }
            _ => self.push_lines(response::format_response(response)),
        }
    }

    fn set_state(&mut self, state: &DeckState) {
        if let Some(slot) = self.states.get_mut(state.deck_id as usize) {
            *slot = state.clone();
        }
        if let Some(rate) = self.rates.get_mut(state.deck_id as usize) {
            *rate = state.tempo;
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
        std::thread::Builder::new()
            .name(format!("hypermixx-waveform-{deck_id}"))
            .spawn(move || {
                let waveform = Arc::new(hypermixx_library::Waveform::build(source.as_ref()));
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

    pub fn zoom_in(&mut self) {
        self.frames_per_dot = (self.frames_per_dot / 2.0).max(MIN_FRAMES_PER_DOT);
    }

    pub fn zoom_out(&mut self) {
        self.frames_per_dot = (self.frames_per_dot * 2.0).min(MAX_FRAMES_PER_DOT);
    }

    /// Fits the longest loaded track into the current viewport. A no-op until a track is loaded.
    pub fn fit(&mut self) {
        let longest = self
            .states
            .iter()
            .map(|state| state.total_frames)
            .max()
            .unwrap_or(0);
        if longest == 0 || self.wave_width_dots == 0 {
            return;
        }
        self.frames_per_dot = (longest as f64 / self.wave_width_dots as f64)
            .clamp(MIN_FRAMES_PER_DOT, MAX_FRAMES_PER_DOT);
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
            chain: format!("deck{}", self.focused),
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
