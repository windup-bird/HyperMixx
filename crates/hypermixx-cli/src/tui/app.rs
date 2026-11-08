//! TUI application state.
//!
//! Deliberately plain data plus small mutators: rendering reads it, input mutates it, the event
//! loop feeds it. Nothing here draws or touches a device, which is what keeps the renderer and the
//! input mapping testable without a terminal.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use hypermixx_audio::{AudioPipeline, Meters};
use hypermixx_core::{
    BeatGrid, BrowserAction, Command, CommandResponse, DeckState, FxChainId, FxSlotStatus,
    KeyFormat, KeylockMode, StemStatus,
};
use hypermixx_library::{
    analyser::Backends, detect_source_kinds, ImportCalibration, LibraryDb, ScanProgress,
    ScanService, Source, SourceKind,
};
use hypermixx_midi::ModeEvent;

use crate::command::{Slots, UiEvent};
use crate::notices::NoticeTx;
use crate::preference::{LibrarySettings, Theme, WaveformSettings};
use crate::procmon::ProcessMonitor;
use crate::response::{self, LogLine};
use crate::tui::browser::Browser;
use crate::tui::completer::{self, Completion, Ctx};
use crate::tui::intent::UiIntent;
use crate::tui::picker::{FilePicker, PortList};
use crate::tui::waveform_view::WaveformView;

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
/// `Browser` is a target of its own: while it owns the command line a bare deck verb is refused
/// (the front-end passes `None` to `command::dispatch`), because falling back to the last focused
/// deck is how a stray `play` stops a track mid-browse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Deck(usize),
    Browser,
}

impl Target {
    /// The label shown on the command rule: `deck1`, `deck2`, … or `browser`.
    pub fn prefix(self) -> String {
        match self {
            Self::Deck(index) => hypermixx_core::deck_label(index as u8),
            Self::Browser => String::new(),
        }
    }

    /// Full tab label used by the command rule.
    pub fn label(self) -> String {
        match self {
            Self::Deck(index) => hypermixx_core::deck_label(index as u8),
            Self::Browser => "browser".to_owned(),
        }
    }

    /// Short tab label for a narrow command panel.
    pub fn compact_label(self) -> String {
        match self {
            Self::Deck(index) => format!("d{}", index + 1),
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
    /// Choosing a controller (MIDI input) port.
    Ports { list: PortList },
    /// Choosing a file: a track to load, or a controller map.
    Files {
        picker: FilePicker,
        purpose: FilePurpose,
    },
    /// The small deck chooser opened by double-clicking a track.
    LoadTarget { x: u16, y: u16, selected: usize },
    /// The settings screen: one column of files, one of fields.
    Settings(Box<crate::tui::settings::SettingsScreen>),
}

/// What the file picker's choice means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilePurpose {
    /// Decode and install into this deck.
    Load { deck: usize },
    /// Use as the controller mapping file (and connect if a port is known).
    MidiMap,
}

/// Which command set the eight pads currently fire. Front-end state: the engine only knows the
/// three commands, and a DJ switches between them per deck.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PadMode {
    /// Set on an empty pad, trigger on a filled one.
    #[default]
    HotCue,
    /// `1/4 1/2 1 2 4 8 16 32` beats.
    BeatLoop,
    /// `-16 -8 -4 -2 +2 +4 +8 +16` beats.
    BeatJump,
}

impl PadMode {
    pub const ALL: [Self; 3] = [Self::HotCue, Self::BeatLoop, Self::BeatJump];

    pub fn label(self) -> &'static str {
        match self {
            Self::HotCue => "cue",
            Self::BeatLoop => "loop",
            Self::BeatJump => "jump",
        }
    }

    /// The eight pad labels, in pad order.
    pub fn pads(self) -> [&'static str; 8] {
        match self {
            Self::HotCue => ["1", "2", "3", "4", "5", "6", "7", "8"],
            Self::BeatLoop => ["1/4", "1/2", "1", "2", "4", "8", "16", "32"],
            Self::BeatJump => ["-16", "-8", "-4", "-2", "+2", "+4", "+8", "+16"],
        }
    }

    /// Loop length per pad, in beats (only meaningful for [`PadMode::BeatLoop`]). Quarter and half
    /// beats are exact: `BeatLen` is a rational, so `1/4` is not 0.25 of a rounded frame count.
    pub fn loop_beats(self, pad: usize) -> Option<hypermixx_core::BeatLen> {
        // Written as exact fractions rather than a float: `1/4` is a rational beat length all the
        // way down, so a quarter-beat loop lands on the grid instead of on a rounded frame count.
        let table: [(u32, u32); 8] = [
            (1, 4),
            (1, 2),
            (1, 1),
            (2, 1),
            (4, 1),
            (8, 1),
            (16, 1),
            (32, 1),
        ];
        table
            .get(pad)
            .and_then(|(num, den)| hypermixx_core::BeatLen::fraction(*num, *den))
    }

    /// Jump size per pad, in beats (only meaningful for [`PadMode::BeatJump`]).
    pub fn jump_beats(self, pad: usize) -> Option<i64> {
        [-16, -8, -4, -2, 2, 4, 8, 16].get(pad).copied()
    }
}

/// What the middle of the beat block is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BeatMode {
    /// Tempo fader and bend.
    #[default]
    Tempo,
    /// Manual loop: in / toggle / out, with the loop length under the pads' `loop` mode.
    ManualLoop,
    /// Beat jump: backward / forward by the current step.
    ManualJump,
}

impl BeatMode {
    pub const ALL: [Self; 3] = [Self::Tempo, Self::ManualLoop, Self::ManualJump];

    pub fn label(self) -> &'static str {
        match self {
            Self::Tempo => "bpm",
            Self::ManualLoop => "loop",
            Self::ManualJump => "jump",
        }
    }
}

/// Which of the two panels the pads slot is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PadPanel {
    #[default]
    Pads,
    Fx,
}

impl PadPanel {
    pub const ALL: [Self; 2] = [Self::Pads, Self::Fx];

    pub fn label(self) -> &'static str {
        match self {
            Self::Pads => "pads",
            Self::Fx => "fx",
        }
    }
}

/// Which of the two panels the eq slot is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EqPanel {
    #[default]
    Eq,
    Beat,
}

impl EqPanel {
    pub const ALL: [Self; 2] = [Self::Eq, Self::Beat];

    pub fn label(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Beat => "beat",
        }
    }
}

/// One clickable region of the drawn interface.
///
/// Rebuilt by the renderer on every frame and consumed by the mouse handler, which is the only way
/// a pointer can reach a control: the command line keeps the keyboard, exactly as before.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    /// Pad `index` (0-based) on `deck`.
    Pad {
        deck: usize,
        index: usize,
    },
    PadMode {
        deck: usize,
        mode: PadMode,
    },
    BeatMode {
        deck: usize,
        mode: BeatMode,
    },
    /// Switch the pads slot between the pad grid and the FX panel.
    PadPanel {
        deck: usize,
        panel: PadPanel,
    },
    /// Switch the eq slot between the EQ knobs and the beat controls.
    EqPanel {
        deck: usize,
        panel: EqPanel,
    },
    /// Select one flat FX slot in the deck chain.
    FxSelect {
        deck: usize,
        slot: usize,
    },
    /// Bypass/engage the selected FX slot.
    FxToggle {
        deck: usize,
        slot: usize,
    },
    /// Set one FX parameter to an absolute value (already mapped to its range).
    FxParam {
        deck: usize,
        slot: usize,
        param: usize,
        value: f32,
    },
    /// ×2 (`double = true`) or ÷2 the beat parameter of a slot.
    FxBeats {
        deck: usize,
        slot: usize,
        double: bool,
    },
    /// One step of the tempo fader (`direction` -1 / +1).
    TempoFader {
        deck: usize,
        direction: i8,
    },
    /// A bend press (`direction` -1 / +1) that releases itself.
    Bend {
        deck: usize,
        direction: i8,
    },
    /// Halve (`true`) or double (`false`) the loop length.
    LoopBeats {
        deck: usize,
        halve: bool,
    },
    /// Halve (`true`) or double (`false`) the jump length.
    JumpBeats {
        deck: usize,
        halve: bool,
    },
    /// Manual loop transport.
    Loop {
        deck: usize,
        op: LoopHit,
    },
    /// Beat jump backward / forward.
    Jump {
        deck: usize,
        direction: i8,
    },
    Transport {
        deck: usize,
        action: TransportHit,
    },
    /// A knob set to an absolute position, `-1.0..=1.0`.
    Knob {
        deck: usize,
        knob: Knob,
        value: f32,
    },
    /// The channel fader set to an absolute position, `-1.0..=1.0` (top = 0 dB). The band draws the
    /// channel fader (the one `[channel] fader` places); the per-stem levels a separated track
    /// exposes stay command-only, because one cell cannot mean two things.
    Fader {
        deck: usize,
        value: f32,
    },
    /// The crossfader, `-1.0..=1.0` (centre = 0).
    Crossfader {
        value: f32,
    },
    /// The header button that opens the settings screen.
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopHit {
    In,
    Out,
    Toggle,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportHit {
    Sync,
    Cue,
    Play,
    Keylock,
}

/// The five mixer knobs a channel has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Knob {
    Gain,
    High,
    Mid,
    Low,
    Filter,
}

impl Knob {
    pub const ALL: [Self; 5] = [Self::Gain, Self::High, Self::Mid, Self::Low, Self::Filter];

    pub fn label(self) -> &'static str {
        match self {
            Self::Gain => "gain",
            Self::High => "high",
            Self::Mid => "mid",
            Self::Low => "low",
            Self::Filter => "filt",
        }
    }
}

/// A deck's transport, as the TUI sees it. Mirrors [`DeckState`] plus front-end-only data.
pub struct App {
    pub version: &'static str,
    pub backends: Backends,
    pub sample_rate: u32,
    /// Output ring capacity in frames, from the live config (the sliders can change it).
    pub ring_frames: usize,
    pub decks: usize,
    pub focused: usize,
    /// The command-line target: one deck, or the (still prefix-less) browser.
    pub target: Target,
    /// The browser pane: Miller columns plus the current list's tracks.
    pub browser: Browser,
    /// Whether the deck's images are currently deleted because a modal is open. The transition, not
    /// the state, is what does the work.
    pub images_hidden: bool,
    /// Output devices probed before the terminal was taken over, for the settings screen.
    pub devices: Vec<hypermixx_audio::DeviceInfo>,
    /// The profile file this session was started from, for the settings screen.
    pub profile_path: Option<String>,
    /// Where `preference.toml` lives, for the settings screen.
    pub preference_path: String,
    /// Where the live settings came from, for the settings screen.
    pub live_path: Option<String>,
    /// Where the mixer topology came from, for the settings screen.
    pub mixer_path: Option<String>,
    /// How keys are rendered across the TUI.
    pub key_format: KeyFormat,
    /// The colour slots every panel draws from.
    pub theme: Theme,
    /// The library cache. `None` when startup could not open it; the browser then stays empty.
    pub library: Option<Arc<LibraryDb>>,
    /// Sources the configured preference categories allow. Everything else stays in the cache but
    /// is not shown or scanned.
    pub allowed_sources: HashSet<i64>,
    /// Shared with the command dispatcher: both front-ends and `browser refresh` use the same
    /// worker. Only `enqueue` and `poll_next` happen on the UI thread.
    pub scan: Arc<Mutex<ScanService>>,
    /// `None` probes each file; a fixed value overrides the decoder start trim.
    pub decoder_start_ms: Option<f64>,
    /// Extra imported-grid/cue correction, positive = later.
    pub grid_offset_ms: f64,

    pub states: Vec<DeckState>,
    /// File name per deck (from the load tap), for the deck header.
    pub titles: Vec<Option<String>>,
    /// Embedded artwork per deck, read off the UI thread when a track loads.
    pub covers: Vec<Option<Arc<hypermixx_media::Cover>>>,
    /// Which command set each deck's pads show.
    pub pad_mode: Vec<PadMode>,
    /// Which panel each deck's pads slot shows.
    pub pad_panel: Vec<PadPanel>,
    /// What each deck's beat block is showing.
    pub beat_mode: Vec<BeatMode>,
    /// Which panel each deck's eq slot shows.
    pub eq_panel: Vec<EqPanel>,
    /// The selected flat FX slot per deck (`FxSlotStatus::index`); `0` until the first poll.
    pub fx_slot: Vec<usize>,
    /// Whether CLI/MIDI commands and mode triggers may switch panels on their own.
    pub follow_commands: bool,
    /// The latest intent produced by the command fan-out.
    pub intent: Arc<Mutex<Option<UiIntent>>>,
    /// Live FX slot state per deck, polled with [`Command::GetFxState`] so the knobs show the
    /// engine's values rather than a local guess.
    pub deck_fx: Vec<Vec<FxSlotStatus>>,
    /// Outstanding `GetFxState` requests, so a stalled engine cannot build a backlog.
    pub fx_pending: usize,
    /// Every clickable region of the last frame, in draw order.
    pub hits: Vec<(ratatui::layout::Rect, Hit)>,
    pub waveforms: Vec<Option<Arc<hypermixx_library::Waveform>>>,
    pub grids: Vec<Option<BeatGrid>>,

    /// Shared zoom, in frames per terminal cell. Both decks read this so they stay comparable.
    pub frames_per_cell: f64,
    /// Last waveform width in cells, so `fit` knows the viewport.
    pub wave_width_cells: usize,
    /// Waveform preferences (renderer, colours, image refresh target).
    pub waveform: WaveformSettings,
    /// The selected waveform transport (braille / kitty) and its per-deck image state.
    pub waveform_view: WaveformView,
    /// The still images (cover, overview) each deck shows through the same image backend.
    pub deck_images: Vec<crate::tui::image_slot::DeckImages>,
    /// The overview playhead column drawn last frame, so the image path can erase its old glyph.
    pub overview_playhead: Vec<Option<u16>>,

    pub log: VecDeque<LogLine>,
    /// Lines scrolled up from the bottom (0 = following).
    pub log_offset: usize,

    pub command: Vec<char>,
    pub cursor: usize,
    pub completion: Option<CompletionState>,
    pub history: Vec<String>,
    pub history_pos: Option<usize>,

    /// Slot kinds per chain label (`"deck1"`, `"master"`), shared with the dispatcher; feeds
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

    /// The last terminal area, for mouse hit testing of popups.
    pub screen: ratatui::layout::Rect,
    /// A modal picker, if one is open.
    pub overlay: Option<Overlay>,
    /// The live MIDI inputs, one per `[[controller]]`; dropping the app closes their ports.
    pub midi: Vec<crate::midi::MidiSession>,
    /// The chosen (or flag-supplied) port, remembered across reconnects.
    pub midi_port: Option<String>,
    /// The chosen (or flag-supplied) map path.
    pub midi_map: Option<String>,
    /// Controller face → physical deck table, used to compile a controller's normal and mirror
    /// unit maps when it connects.
    pub deck_routes: hypermixx_midi::map::DeckRoutes,
    /// One entry per `[[controller]]` instance; `attach_midi` connects all of them.
    pub controller_sessions: Vec<crate::profile::ControllerSession>,

    notice_rx: Receiver<LogLine>,
    event_rx: Receiver<UiEvent>,
    /// Selector presses from the MIDI router, including re-selecting the current mode.
    mode_tx: Sender<ModeEvent>,
    mode_rx: Receiver<ModeEvent>,
    /// Browser actions from MIDI (up/down/open/exit/load/refresh).
    pub browser_tx: Sender<BrowserAction>,
    browser_rx: Receiver<BrowserAction>,
    waveform_tx: Sender<(usize, Arc<hypermixx_library::Waveform>)>,
    waveform_rx: Receiver<(usize, Arc<hypermixx_library::Waveform>)>,
    cover_tx: Sender<(usize, Option<Arc<hypermixx_media::Cover>>)>,
    cover_rx: Receiver<(usize, Option<Arc<hypermixx_media::Cover>>)>,
}

impl App {
    pub fn new(
        decks: usize,
        backends: Backends,
        notice_rx: Receiver<LogLine>,
        event_rx: Receiver<UiEvent>,
        fx_slots: Slots,
        scan: Arc<Mutex<ScanService>>,
    ) -> Self {
        let decks = decks.max(1);
        let (waveform_tx, waveform_rx) = crossbeam_channel::unbounded();
        let (cover_tx, cover_rx) = crossbeam_channel::unbounded();
        let (browser_tx, browser_rx) = crossbeam_channel::unbounded();
        let (mode_tx, mode_rx) = crossbeam_channel::unbounded();
        let intent = Arc::new(Mutex::new(None));
        let browser = Browser::new();
        Self {
            version: crate::VERSION,
            backends,
            sample_rate: hypermixx_core::SAMPLE_RATE,
            ring_frames: hypermixx_audio::OUTPUT_RING_CAPACITY,
            decks,
            focused: 0,
            // A session starts in the library. The browser takes the arrow keys *and* the command
            // line: a bare deck verb is refused until `Tab` picks a deck (or the line names one).
            target: Target::Browser,
            browser,
            images_hidden: false,
            devices: Vec::new(),
            profile_path: None,
            preference_path: crate::preference::DEFAULT_PATH.to_owned(),
            live_path: None,
            mixer_path: None,
            key_format: KeyFormat::Camelot,
            theme: Theme::default(),
            library: None,
            allowed_sources: HashSet::new(),
            scan,
            decoder_start_ms: None,
            grid_offset_ms: 0.0,
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
                    jump_beats: 8,
                    loop_beats: hypermixx_core::BeatLen::beats(8),
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
                    hot_cues: [None; hypermixx_core::HOT_CUE_SLOTS],
                    vinyl: false,
                    keylock: KeylockMode::On,
                    key_shift: 0,
                    stems: StemStatus::default(),
                })
                .collect(),
            titles: vec![None; decks],
            covers: vec![None; decks],
            pad_mode: vec![PadMode::default(); decks],
            pad_panel: vec![PadPanel::default(); decks],
            beat_mode: vec![BeatMode::default(); decks],
            eq_panel: vec![EqPanel::default(); decks],
            fx_slot: vec![0; decks],
            follow_commands: true,
            intent,
            deck_fx: vec![Vec::new(); decks],
            fx_pending: 0,
            hits: Vec::new(),
            waveforms: vec![None; decks],
            grids: vec![None; decks],
            frames_per_cell: DEFAULT_FRAMES_PER_CELL,
            wave_width_cells: 200,
            waveform: WaveformSettings::default(),
            deck_images: (0..decks)
                .map(|_| crate::tui::image_slot::DeckImages::new())
                .collect(),
            overview_playhead: vec![None; decks],
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
            screen: ratatui::layout::Rect::default(),
            overlay: None,
            midi: Vec::new(),
            midi_port: None,
            midi_map: None,
            deck_routes: Default::default(),
            controller_sessions: Vec::new(),
            notice_rx,
            event_rx,
            mode_tx,
            mode_rx,
            browser_tx,
            browser_rx,
            waveform_tx,
            waveform_rx,
            cover_tx,
            cover_rx,
        }
    }

    /// Opens the remembered port with the remembered map, replacing any current input.
    ///
    /// FX names are resolved from the **already-primed slot book**, not by asking the engine: it
    /// runs inside the UI thread, and reading the shared response channel here would swallow a
    /// `Loaded`/`States`/`Error` answer meant for the UI. On failure the reason is logged and the
    /// matching picker reopens so another choice can be made.
    pub fn connect_midi(
        &mut self,
        command_tx: Sender<Command>,
        notices: &NoticeTx,
        dispatcher: &crate::command::Dispatcher,
    ) {
        let specs: Vec<crate::profile::ControllerSession> = if self.controller_sessions.is_empty() {
            // No explicit controller list: the F9-style single connection.
            let Some(port) = self.midi_port.clone() else {
                return;
            };
            vec![crate::profile::ControllerSession {
                id: "default".to_owned(),
                port: Some(port),
                map: self
                    .midi_map
                    .clone()
                    .unwrap_or_else(|| crate::profile::default_map_path().to_owned()),
                routes: self.deck_routes.clone(),
            }]
        } else {
            self.controller_sessions.clone()
        };
        // Dropping the old sessions closes their ports before the new ones open.
        self.midi.clear();
        let slots = self.fx_slots.clone();
        let mode_tx = self.mode_sender();
        let mut first = true;
        for spec in specs {
            let Some(port) = spec.port.clone() else {
                continue;
            };
            let (mut map, mut mirror_map) =
                match crate::midi::read_map_pair(&spec.map, &spec.routes) {
                    Ok(pair) => pair,
                    Err(err) => {
                        self.push_log(LogLine::error(format!("controller `{}`: {err}", spec.id)));
                        self.overlay = Some(Overlay::Files {
                            picker: FilePicker::new(
                                Some(std::path::Path::new(&spec.map)),
                                &["toml"],
                                "controller map",
                            ),
                            purpose: FilePurpose::MidiMap,
                        });
                        continue;
                    }
                };
            let unresolved = crate::midi::resolve_with(&mut map, |chain, fx| {
                slots.resolve(&chain.label(), fx).ok()
            });
            crate::midi::report_unresolved(notices, unresolved);
            mirror_map.copy_fx_slots(&map);
            match crate::midi::attach(
                &port,
                map,
                mirror_map,
                command_tx.clone(),
                Some(self.browser_tx.clone()),
                Some(mode_tx.clone()),
            ) {
                Ok(session) => {
                    // The dispatcher's single control/state pair follows the first controller.
                    if first {
                        dispatcher.set_midi(session.control(), session.state());
                        first = false;
                    }
                    self.push_log(LogLine::info(format!(
                        "[controller] `{}` connected `{port}` ({})",
                        spec.id, spec.map
                    )));
                    self.midi.push(session);
                }
                Err(err) => {
                    self.push_log(LogLine::error(format!("controller `{}`: {err}", spec.id)));
                    self.overlay = Some(Overlay::Ports {
                        list: PortList::refresh(),
                    });
                }
            }
        }
    }

    /// The shared slot the command fan-out writes intents into.
    pub fn intent_slot(&self) -> Arc<Mutex<Option<UiIntent>>> {
        self.intent.clone()
    }

    /// A sender for selector presses from the MIDI router.
    pub fn mode_sender(&self) -> Sender<ModeEvent> {
        self.mode_tx.clone()
    }

    /// Every FX slot the panel may show: the deck chain minus the fixed `eq`/`filter` stages.
    pub fn fx_panel_slots(&self, deck: usize) -> Vec<&FxSlotStatus> {
        self.deck_fx
            .get(deck)
            .into_iter()
            .flatten()
            .filter(|slot| !matches!(slot.role.as_str(), "eq" | "filter"))
            .collect()
    }

    /// The selected FX slot, falling back to the first one when the saved index disappeared.
    pub fn selected_fx(&self, deck: usize) -> Option<&FxSlotStatus> {
        let slots = self.fx_panel_slots(deck);
        let wanted = self.fx_slot.get(deck).copied().unwrap_or(0);
        slots
            .iter()
            .copied()
            .find(|slot| slot.index == wanted)
            .or_else(|| slots.first().copied())
    }

    /// Keeps `fx_slot` pointing at a mounted slot after a `fx remove`/`mixer reload`.
    pub fn clamp_fx_slot(&mut self, deck: usize) {
        let indices: Vec<usize> = self
            .fx_panel_slots(deck)
            .iter()
            .map(|slot| slot.index)
            .collect();
        if let Some(selected) = self.fx_slot.get_mut(deck) {
            if !indices.contains(&*selected) {
                *selected = indices.first().copied().unwrap_or(0);
            }
        }
    }

    /// Applies a command/mode intent. The preference gate lives here so both sources agree.
    pub fn apply_intent(&mut self, intent: UiIntent) {
        if !self.follow_commands {
            return;
        }
        match intent {
            UiIntent::Pads { deck, mode } => {
                if let Some(panel) = self.pad_panel.get_mut(deck) {
                    *panel = PadPanel::Pads;
                }
                if let Some(slot) = self.pad_mode.get_mut(deck) {
                    *slot = mode;
                }
            }
            UiIntent::Beat { deck, mode } => {
                if let Some(panel) = self.eq_panel.get_mut(deck) {
                    *panel = EqPanel::Beat;
                }
                if let Some(slot) = self.beat_mode.get_mut(deck) {
                    *slot = mode;
                }
            }
            UiIntent::Eq { deck } => {
                if let Some(panel) = self.eq_panel.get_mut(deck) {
                    *panel = EqPanel::Eq;
                }
            }
            UiIntent::FxPanel { deck } => {
                if let Some(panel) = self.pad_panel.get_mut(deck) {
                    *panel = PadPanel::Fx;
                }
            }
            UiIntent::FxSlot { deck, slot } => {
                if let Some(panel) = self.pad_panel.get_mut(deck) {
                    *panel = PadPanel::Fx;
                }
                if let Some(selected) = self.fx_slot.get_mut(deck) {
                    *selected = slot;
                }
                self.clamp_fx_slot(deck);
            }
            UiIntent::FxOrdinal { deck, ordinal } => {
                if let Some(panel) = self.pad_panel.get_mut(deck) {
                    *panel = PadPanel::Fx;
                }
                let index = self
                    .fx_panel_slots(deck)
                    .get(ordinal.saturating_sub(1))
                    .map(|slot| slot.index);
                if let (Some(index), Some(selected)) = (index, self.fx_slot.get_mut(deck)) {
                    *selected = index;
                }
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

    /// Rebuilds the browser from the cache after a scan or a source change.
    pub fn reload_browser(&mut self) {
        let Some(db) = self.library.clone() else {
            self.browser = Browser::new();
            return;
        };
        match db.sources() {
            Ok(sources) => {
                let visible: Vec<Source> = sources
                    .into_iter()
                    .filter(|source| {
                        self.allowed_sources.contains(&source.id) && source.root.is_dir()
                    })
                    .collect();
                // The browser opens its first source, so put ~/Music there when it is configured.
                let preferred = music_dir().and_then(|path| std::fs::canonicalize(path).ok());
                let visible = prefer_source(visible, preferred.as_deref());
                match Browser::load_sources(&db, visible) {
                    Ok(mut browser) => {
                        browser.key_format = self.key_format;
                        self.browser = browser;
                    }
                    Err(err) => self.push_log(LogLine::error(format!("library: {err}"))),
                }
            }
            Err(err) => self.push_log(LogLine::error(format!("library: {err}"))),
        }
    }

    /// Drains browser actions queued by MIDI. The caller executes them with the dispatcher.
    pub fn drain_browser_actions(&mut self) -> Vec<BrowserAction> {
        let mut actions = Vec::new();
        while let Ok(action) = self.browser_rx.try_recv() {
            actions.push(action);
        }
        actions
    }

    /// Applies the configured source categories: explicit folders, local user libraries, and the
    /// watcher for USB media. Sources outside these categories are neither scanned nor displayed.
    pub fn sync_library(&mut self, settings: &LibrarySettings) {
        let Some(db) = self.library.clone() else {
            return;
        };
        self.allowed_sources.clear();
        self.decoder_start_ms = settings.decoder_start_ms;
        self.grid_offset_ms = settings.grid_offset_ms;
        db.set_import_calibration(ImportCalibration {
            rkb_mp3_offset_ms: settings.rkb_mp3_offset_ms,
            serato_offset_ms: settings.serato_offset_ms,
            vdj_offset_ms: settings.vdj_offset_ms,
        });

        let mut pending = Vec::new();
        for folder in &settings.folders {
            let root = expand_home(folder);
            match db.add_folder_source(&root, None) {
                Ok(source) => {
                    self.allowed_sources.insert(source.id);
                    pending.push(source);
                }
                Err(err) => self.push_log(LogLine::error(format!("[library] {folder}: {err}"))),
            }
        }
        for kind in settings.local_kinds() {
            for root in local_roots(kind) {
                if !detect_source_kinds(&root).contains(&kind) {
                    continue;
                }
                match db.add_source(kind, &root, None) {
                    Ok(source) => {
                        self.allowed_sources.insert(source.id);
                        pending.push(source);
                    }
                    Err(err) => self.push_log(LogLine::error(format!(
                        "[library] {}: {err}",
                        root.display()
                    ))),
                }
            }
        }
        // The library owns the removable-media watcher and the scan worker; this front-end only
        // configures them and enqueues the configured sources. Nothing here touches the disk.
        let mut scan = self.scan.lock().expect("scan service lock");
        scan.configure(&db, settings.usb_kinds());
        let count = pending.len();
        let queued = scan.enqueue(pending);
        drop(scan);
        // Show the cached tree before the worker fills it in: the first scan of a large folder can
        // take a while, and blocking here would keep the terminal on the normal screen.
        self.reload_browser();
        if queued {
            self.push_log(LogLine::info(format!(
                "[library] scanning {count} source(s) in background"
            )));
        }
    }

    /// Applies an engine response. Deck states update silently (they are on every poll); everything
    /// else is echoed to the log, exactly as the REPL would print it.
    pub fn on_response(&mut self, response: &CommandResponse, pipeline: &AudioPipeline) {
        let sample_rate = self.sample_rate;
        if matches!(response, CommandResponse::Error(_)) {
            // An engine error may be the answer to a knob poll (an FX chain that does not exist on
            // a custom topology). Drop the in-flight count so the poll keeps going instead of
            // stalling behind a response that will never come.
            self.fx_pending = 0;
        }
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
            CommandResponse::FxState { chain, slots } => {
                // A poll, not a command: record it silently, or a 10Hz knob read would flood the
                // response log.
                if let FxChainId::Deck(deck_id) = chain {
                    if let Some(slot) = self.deck_fx.get_mut(*deck_id as usize) {
                        *slot = slots.clone();
                    }
                    self.clamp_fx_slot(*deck_id as usize);
                }
                self.fx_pending = self.fx_pending.saturating_sub(1);
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

    /// Reads the embedded cover off the UI thread. Missing artwork is a normal answer (`None`), not
    /// an error: most WAV files have none, and the header falls back to a colour block.
    fn spawn_cover(&mut self, deck: usize, path: String) {
        if let Some(slot) = self.covers.get_mut(deck) {
            *slot = None;
        }
        let tx = self.cover_tx.clone();
        std::thread::Builder::new()
            .name(format!("hypermixx-cover-{deck}"))
            .spawn(move || {
                let cover = hypermixx_media::read_cover(&path).map(Arc::new);
                let _ = tx.send((deck, cover));
            })
            .ok();
    }

    /// The value a knob currently shows, or `None` when the deck's chain has no such slot (a custom
    /// topologies may omit `eq`/`filter` entirely).
    pub fn knob_value(&self, deck: usize, knob: Knob) -> Option<f32> {
        if knob == Knob::Gain {
            return self
                .meters
                .as_ref()
                .and_then(|meters| meters.channels.get(deck))
                .map(|levels| levels.gain);
        }
        let (role, param) = match knob {
            Knob::Gain => unreachable!("handled above"),
            Knob::High => ("eq", "high"),
            Knob::Mid => ("eq", "mid"),
            Knob::Low => ("eq", "low"),
            // Matched by **role**, not kind: the filter slot can hold `moog` instead of `filter`
            // without the knob losing its home.
            Knob::Filter => ("filter", "value"),
        };
        self.deck_fx
            .get(deck)?
            .iter()
            .find(|slot| slot.role.eq_ignore_ascii_case(role))
            .and_then(|slot| {
                slot.params
                    .iter()
                    .find(|param_status| param_status.name == param)
                    .map(|param_status| param_status.value)
            })
    }

    /// The slot index of a knob's effect in the deck chain, for [`Command::SetFxParam`].
    ///
    /// Addressed by role so a swapped effect (`filter` → `moog`) stays under the same knob.
    pub fn knob_slot(&self, deck: usize, knob: Knob) -> Option<usize> {
        let role = match knob {
            Knob::Gain => return None,
            Knob::High | Knob::Mid | Knob::Low => "eq",
            Knob::Filter => "filter",
        };
        self.deck_fx
            .get(deck)?
            .iter()
            .find(|slot| slot.role.eq_ignore_ascii_case(role))
            .map(|slot| slot.index)
    }

    /// The mixer's position for a deck's channel fader.
    pub fn fader_value(&self, deck: usize) -> Option<f32> {
        self.meters
            .as_ref()?
            .channels
            .get(deck)
            .map(|levels| levels.deck)
    }

    /// Master peak and cue peak, linear.
    pub fn peaks(&self) -> (f32, f32, Vec<f32>) {
        match self.meters.as_ref() {
            Some(meters) => (
                meters.master_peak,
                meters.cue_peak,
                meters.deck_peaks.clone(),
            ),
            None => (0.0, 0.0, Vec::new()),
        }
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
                    self.spawn_cover(deck_id as usize, path);
                }
                UiEvent::Grid { deck_id, analysis } => {
                    if let Some(grid) = self.grids.get_mut(deck_id as usize) {
                        *grid = Some(analysis.beatgrid);
                    }
                }
            }
            changed = true;
        }
        while let Ok((index, cover)) = self.cover_rx.try_recv() {
            if let Some(slot) = self.covers.get_mut(index) {
                *slot = cover;
            }
            changed = true;
        }
        while let Ok((index, waveform)) = self.waveform_rx.try_recv() {
            if let Some(slot) = self.waveforms.get_mut(index) {
                *slot = Some(waveform);
            }
            changed = true;
        }
        // Scan progress comes from the library's own worker. The front-end only maps it onto the
        // browser and the log; it never scans, watches removable media, or owns a db write here.
        // Pull the events under the lock, then drop it before touching `self`: the dispatcher may
        // be waiting to enqueue a `browser refresh`, and `reload_browser` reads the db itself.
        let events = {
            let scan = self.scan.lock().expect("scan service lock");
            let mut events = Vec::new();
            while let Some(event) = scan.poll_next() {
                events.push(event);
            }
            events
        };
        let scan_changed = !events.is_empty();
        // Reload only when what the browser *shows* can have changed. A rebuild resets navigation
        // state, so a refresh on every event would make the browser flicker after each scan.
        let mut reload = false;
        for event in events {
            match event {
                ScanProgress::SourceRegistered { source_id, .. } => {
                    self.allowed_sources.insert(source_id);
                    // Redraw now so a freshly mounted device shows all of its sources together,
                    // each still carrying its cached tracks until its own pass reports.
                    reload = true;
                }
                ScanProgress::SourcesRemoved { source_ids } => {
                    for id in source_ids {
                        self.allowed_sources.remove(&id);
                    }
                    reload = true;
                }
                ScanProgress::ListsReady { name, .. } => {
                    reload = true;
                    self.push_log(LogLine::info(format!(
                        "[library] {name}: lists ready, reading track metadata"
                    )));
                }
                ScanProgress::SourceDone {
                    name, kind, stats, ..
                } => {
                    // A scan that only re-confirmed what the cache already had produces the very
                    // same rows; rebuilding the browser for it is what made it flash.
                    if stats.files_added > 0 || stats.files_removed > 0 {
                        reload = true;
                    }
                    self.push_log(LogLine::info(format!(
                        "[library] {name} [{kind}]: {}",
                        stats.summary()
                    )));
                }
                ScanProgress::Failed { name, error, .. } => {
                    self.push_log(LogLine::error(format!("[library] {name}: {error}")))
                }
            }
        }
        if scan_changed {
            changed = true;
        }
        if reload {
            self.reload_browser();
        }
        // The command fan-out and the MIDI router both leave one lightweight intent; apply the
        // newest one after the other side channels so a mode/panel change wins over stale data.
        let intent = self.intent.lock().ok().and_then(|mut slot| slot.take());
        if let Some(intent) = intent {
            self.apply_intent(intent);
            changed = true;
        }
        while let Ok(event) = self.mode_rx.try_recv() {
            if let Some(intent) = crate::tui::intent::from_mode_event(&event, self.decks) {
                self.apply_intent(intent);
                changed = true;
            }
        }
        changed
    }

    /// The deck a *UI affordance* acts on when it needs one (the bare-`load` picker, a browser
    /// load): the targeted deck, or the last focused one while the browser is the target. Deck
    /// commands do not use this — they need a real deck target (`command::Target::Unset`).
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
        // The response pane wraps lines, so its real height is known only at render time. Keep the
        // requested visual-line offset here; the renderer clamps it to the wrapped total.
        self.log_offset = (self.log_offset as isize + delta).max(0) as usize;
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
            // The chain label the engine reports, which is 1-based.
            chain: hypermixx_core::deck_label(self.default_deck() as u8),
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

fn home_dir() -> Option<PathBuf> {
    // Which variable wins is a platform question, not a matter of taste: on Windows `HOME` is
    // whatever the *shell* thinks (MSYS/Git Bash write `/c/Users/me`, WSL `/mnt/c/...`), and a native
    // process cannot open those spellings, while `USERPROFILE` is the real profile directory.
    let names: &[&str] = if cfg!(windows) {
        &["USERPROFILE", "HOME"]
    } else {
        &["HOME", "USERPROFILE"]
    };
    names
        .iter()
        .find_map(|name| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        // The last resort on a Windows session that has been stripped down to a login shell.
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            let mut home = PathBuf::from(drive);
            home.push(path);
            Some(home)
        })
}

fn music_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join("Music"))
}

fn expand_tilde(path: &str, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home {
        if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
            return home.join(rest);
        }
        if path == "~" {
            return home.to_path_buf();
        }
    }
    PathBuf::from(path)
}

fn expand_home(path: &str) -> PathBuf {
    expand_tilde(path, home_dir().as_deref())
}

/// Moves the preferred root to the front, so `Browser::load_sources` opens it first.
fn prefer_source(mut sources: Vec<Source>, preferred: Option<&Path>) -> Vec<Source> {
    if let Some(index) =
        preferred.and_then(|root| sources.iter().position(|source| source.root == root))
    {
        sources.rotate_left(index);
    }
    sources
}

fn local_roots(kind: SourceKind) -> Vec<PathBuf> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let mut roots = match kind {
        SourceKind::Rekordbox => vec![
            home.join(".config/rekordbox"),
            home.join("rekordbox"),
            home.join("Music"),
        ],
        SourceKind::Serato => vec![home.join("Music"), home.clone()],
        _ => vec![
            home.join(".VirtualDJ"),
            home.join("VirtualDJ"),
            home.join("Music"),
            home.clone(),
        ],
    };
    roots.dedup();
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preference::LibrarySettings;
    use hypermixx_library::SourceOrigin;

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hypermixx-app-{}-{}-{}",
            label,
            std::process::id(),
            hypermixx_library::library::db::unix_now()
        ))
    }

    fn wav() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36u32 + 400).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8_000u32.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&400u32.to_le_bytes());
        bytes.resize(44 + 400, 0);
        bytes
    }

    fn source(name: &str, root: impl Into<PathBuf>) -> Source {
        Source {
            id: 0,
            kind: SourceKind::Folder,
            origin: SourceOrigin::Folder,
            name: name.to_owned(),
            root: root.into(),
            last_scan_at: None,
        }
    }

    #[test]
    fn the_browser_starts_on_the_music_folder() {
        let music = Path::new("home").join("me").join("Music");
        let sources = vec![
            source("mp3", "mp3"),
            source("Music", &music),
            source("USB", "run/media/stick"),
        ];
        let ordered = prefer_source(sources, Some(&music));
        assert_eq!(ordered[0].name, "Music");

        // Without a Music source the configured order is left alone.
        let ordered = prefer_source(
            vec![source("mp3", "mp3"), source("USB", "run/media/stick")],
            Some(&music),
        );
        assert_eq!(ordered[0].name, "mp3");
    }

    #[test]
    fn a_tilde_folder_resolves_against_the_home_directory() {
        let home = Path::new("home").join("me");
        assert_eq!(
            expand_tilde("~/Music", Some(home.as_path())),
            home.join("Music")
        );
        assert_eq!(
            expand_tilde("~\\Music", Some(home.as_path())),
            home.join("Music")
        );
        assert_eq!(expand_tilde("~", Some(home.as_path())), home);
        assert_eq!(
            expand_tilde("./mp3", Some(home.as_path())),
            PathBuf::from("./mp3")
        );
        assert_eq!(
            expand_tilde("~/Music", None),
            PathBuf::from("~/Music"),
            "no home directory means the literal path is tried"
        );
    }

    #[test]
    fn only_configured_sources_reach_the_browser() {
        let dir = temp_dir("configured");
        let _ = std::fs::remove_dir_all(&dir);
        let configured = dir.join("configured");
        let hidden = dir.join("hidden");
        std::fs::create_dir_all(&configured).unwrap();
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(configured.join("a.wav"), wav()).unwrap();
        std::fs::write(hidden.join("b.wav"), wav()).unwrap();

        let db = Arc::new(LibraryDb::open(dir.join("library.sqlite3")).unwrap());
        db.add_folder_source(&hidden, Some("Hidden")).unwrap();
        let (_notice_tx, notice_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut app = App::new(
            1,
            Backends::default(),
            notice_rx,
            event_rx,
            Default::default(),
            Default::default(),
        );
        app.library = Some(db);
        let settings = LibrarySettings {
            folders: vec![configured.display().to_string()],
            ..Default::default()
        };
        app.sync_library(&settings);
        assert_eq!(app.browser.columns[0].entries.len(), 1);
        assert_eq!(
            app.browser.columns[0].entries[0].label(),
            configured.file_name().unwrap().to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn intents_switch_panels_only_while_following() {
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
        app.apply_intent(UiIntent::Beat {
            deck: 1,
            mode: BeatMode::ManualLoop,
        });
        assert_eq!(app.eq_panel[1], EqPanel::Beat);
        assert_eq!(app.beat_mode[1], BeatMode::ManualLoop);

        app.follow_commands = false;
        app.apply_intent(UiIntent::Pads {
            deck: 0,
            mode: PadMode::BeatJump,
        });
        assert_eq!(app.pad_panel[0], PadPanel::Pads);
        assert_eq!(app.pad_mode[0], PadMode::HotCue);
    }
}
