//! Line parsing and dispatch, shared by the REPL and the TUI.
//!
//! Syntax is target-first: the target is the TUI's selected deck (its prefix is shown on the
//! command panel's divider line), and can be overridden per line with `deck0` / `0` / `master`. So
//! in the TUI `play` means "play the selected deck" while `deck1 play` or `master fx list` address
//! something else. The engine's [`Command`] protocol is unchanged — this is purely the front-end's
//! grammar.
//!
//! `load` decodes on a worker thread and hands the pipeline a ready source; `analyse` runs the
//! library analyser and publishes a compiled grid. Nothing here blocks the audio path.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use hypermixx_audio::fx::FxKind;
use hypermixx_audio::{AudioPipeline, Command, SAMPLE_RATE};
use hypermixx_media::ChunkedStreams;
use hypermixx_core::{
    Backend, CommandResponse, CueOp, FxChainId, FxSlotRef, KeylockMode, LiveConfig, LoopEditOp,
    LoopOp, LoopQuantum, NudgeOp, OutputOp, PhaseMode, Shared, Stem, StemOp, StemPreset, SyncOp,
    TrackAnalysis, VinylOp,
};

use crate::notices::{self, NoticeTx};
use crate::response::{fx_help_text, help_text};

/// Result of interpreting one input line.
#[derive(Debug)]
pub enum Action {
    Continue,
    Quit,
    /// Text to show the user (help).
    Message(String),
    /// A bad input line: the user's mistake, not an engine error.
    Failed(String),
}

/// Data the front-end wants that the engine does not carry.
pub enum UiEvent {
    /// A file was decoded and is about to be installed into `deck_id`.
    Loaded { deck_id: u8, path: String },
    /// A compiled analysis became available (bpm hint at load, or `analyse`).
    Grid { deck_id: u8, analysis: TrackAnalysis },
}

/// The slot kinds of every chain.
///
/// The front-end owns all chain mutations, so it can keep this accurate without asking the engine:
/// `FxAdded` appends, a dispatched `fx remove` drops locally, and `fx list` replaces. It is filled
/// once at startup (see [`Dispatcher::prime`]) so a slot can be named from the very first command.
pub struct SlotBook {
    slots: Mutex<HashMap<String, Vec<String>>>,
}

impl Default for SlotBook {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }
}

impl SlotBook {
    /// Updates from an engine response (`FxListed` replaces a chain, `FxAdded` appends).
    pub fn record(&self, response: &CommandResponse) {
        match response {
            CommandResponse::FxListed { chain, slots } => {
                if let Ok(mut book) = self.slots.lock() {
                    book.insert(
                        chain.label(),
                        slots.iter().map(|slot| slot.kind.clone()).collect(),
                    );
                }
            }
            CommandResponse::FxAdded {
                chain,
                index,
                kind,
            } => {
                if let Ok(mut book) = self.slots.lock() {
                    let entry = book.entry(chain.label()).or_default();
                    if *index <= entry.len() {
                        entry.insert(*index, kind.clone());
                    }
                }
            }
            _ => {}
        }
    }

    /// Resolves a slot argument: a bare index, or an effect name.
    pub fn resolve(&self, chain: &str, token: &str) -> Result<usize, String> {
        if let Ok(index) = token.parse::<usize>() {
            return Ok(index);
        }
        let book = self.slots.lock().map_err(|_| "slot book poisoned".to_owned())?;
        match book.get(chain) {
            Some(kinds) => kinds
                .iter()
                .position(|kind| kind.eq_ignore_ascii_case(token))
                .ok_or_else(|| {
                    format!(
                        "no `{token}` in {chain}; slots are {}",
                        list_kinds(kinds)
                    )
                }),
            None => Err(format!("{chain} has no known slots")),
        }
    }

    /// Drops a slot locally after a `fx remove` this front-end sent.
    pub fn remove(&self, chain: &str, index: usize) {
        if let Ok(mut book) = self.slots.lock() {
            if let Some(kinds) = book.get_mut(chain) {
                if index < kinds.len() {
                    kinds.remove(index);
                }
            }
        }
    }

    /// A copy of the slot kinds, for the completer (cheap: a handful of chains).
    pub fn snapshot(&self) -> HashMap<String, Vec<String>> {
        self.slots
            .lock()
            .map(|book| book.clone())
            .unwrap_or_default()
    }
}

/// Shared slot book handle.
pub type Slots = Arc<SlotBook>;

/// What a command line addresses when it does not start with a deck/chain word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Deck(u8),
    /// One stem of a deck: `deck0 vocals`. Everything a deck takes still applies to its deck (the
    /// next word decides), and the stem-scoped verbs (`level`, `mute`, `solo`) use the stem.
    Stem(u8, Stem),
    Master,
}

/// Everything a dispatch needs to act.
pub struct Dispatcher {
    pub command_tx: Sender<Command>,
    pub notices: NoticeTx,
    pub events: Option<Sender<UiEvent>>,
    /// Where `mixer reload` reads from when no path is given: `--config`'s file, or the shipped
    /// default under `./config/`. The front-end owns file IO, so the engine only ever sees text.
    topology: Arc<Mutex<Option<String>>>,
    /// Where `live reload` reads from: `--live`'s file, or the shipped default.
    live_path: Arc<Mutex<Option<String>>>,
    /// The profile this session started with, or `None` for the folder defaults. `profile show`
    /// reports it and `profile reload` replaces it.
    profile: Arc<Mutex<Option<String>>>,
    /// The machine's settings: the engine rate, the ring sizes, and the stem defaults.
    live: Arc<Mutex<LiveConfig>>,
    slots: Slots,
    /// In-flight separations, by deck, so `stem cancel` can reach one. Separation is the only job
    /// here long enough to be worth cancelling (~80 s), and the flag is observed between model
    /// windows — so a cancel lands within one window, not instantly.
    separations: Cancellations,
}

/// The separations a front-end has running. A plain `Mutex<HashMap>`: it is touched twice per job,
/// never on the audio path.
pub type Cancellations = Arc<Mutex<HashMap<u8, hypermixx_stems::Cancelled>>>;

impl Dispatcher {
    pub fn new(command_tx: Sender<Command>, notices: NoticeTx) -> Self {
        Self {
            command_tx,
            notices,
            events: None,
            topology: Arc::new(Mutex::new(None)),
            live_path: Arc::new(Mutex::new(None)),
            profile: Arc::new(Mutex::new(None)),
            live: Arc::new(Mutex::new(LiveConfig::default())),
            slots: Arc::new(SlotBook::default()),
            separations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Also forwards front-end events (deck name, compiled grid) to `events`.
    pub fn tap_events(mut self, events: Sender<UiEvent>) -> Self {
        self.events = Some(events);
        self
    }

    /// Remembers the profile this session started with, so `profile show` can name it and
    /// `profile reload` can default to it.
    pub fn with_profile(&self, path: Option<String>) {
        if let Ok(mut slot) = self.profile.lock() {
            *slot = path;
        }
    }

    /// The profile in effect, if any.
    pub fn profile_path(&self) -> Option<String> {
        self.profile.lock().ok().and_then(|slot| slot.clone())
    }

    /// Installs the live settings and remembers where they came from, so `live reload` can re-read
    /// the same file.
    pub fn with_live(&self, live: LiveConfig, path: Option<String>) {
        if let Ok(mut slot) = self.live.lock() {
            *slot = live;
        }
        if let Ok(mut slot) = self.live_path.lock() {
            *slot = path;
        }
    }

    /// The live settings, as the workers read them (decode rate, stem defaults).
    pub fn live(&self) -> LiveConfig {
        self.live
            .lock()
            .map(|live| live.clone())
            .unwrap_or_default()
    }

    /// The live settings file: `--live`'s path, else the shipped default.
    fn live_path(&self) -> String {
        self.live_path
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or_else(|| DEFAULT_LIVE.to_owned())
    }

    /// Remembers which topology file `mixer reload` should fall back to (`--config`), and which one
    /// the session is actually running: a profile switch replaces it, so it is a setter rather than a
    /// builder step.
    pub fn with_topology(&self, path: Option<String>) {
        if let Ok(mut slot) = self.topology.lock() {
            *slot = path;
        }
    }

    /// The topology file to reload: the one from `--config`, else the shipped default.
    fn topology_path(&self) -> String {
        self.topology
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or_else(|| DEFAULT_TOPOLOGY.to_owned())
    }

    /// The shared slot book (the TUI holds a clone to update/read it).
    pub fn slots(&self) -> Slots {
        Arc::clone(&self.slots)
    }

    /// Fills the slot book for every chain, blocking until the answers arrive, so the first command
    /// the user types can already address a slot by name. Only called once at startup, before any
    /// other command is in flight.
    pub fn prime(&self, decks: usize, responses: &Receiver<CommandResponse>) {
        let chains = (0..decks as u8)
            .map(FxChainId::Deck)
            .chain(std::iter::once(FxChainId::Master));
        let mut expected = 0usize;
        for chain in chains {
            if self.command_tx.send(Command::ListFx { chain }).is_ok() {
                expected += 1;
            }
        }
        // The producer answers all of these in the same pass; a short timeout just guards against a
        // dead engine so startup cannot hang.
        for _ in 0..expected {
            match responses.recv_timeout(Duration::from_secs(2)) {
                Ok(response) => self.slots.record(&response),
                Err(_) => break,
            }
        }
    }
}

/// Turns a line into engine effects. `focused` is the deck a bare command targets.
pub fn dispatch(
    line: &str,
    dispatcher: &Dispatcher,
    pipeline: &AudioPipeline,
    backend: Backend,
    decks: usize,
    focused: u8,
) -> Action {
    let mut words = line.split_whitespace();
    let Some(first) = words.next() else {
        return Action::Continue;
    };
    // A leading deck/chain word overrides the focused deck; otherwise the verb is the first word.
    // A deck word may be followed by a stem name, which narrows the target to one stem:
    // `deck0 vocals mute`, `deck0 vocals fx add filter`.
    let (mut target, verb) = match parse_target(first, decks) {
        Some(target) => (target, words.next().unwrap_or_default()),
        None => (Target::Deck(focused), first),
    };
    if let Target::Deck(deck_id) = target {
        if let Some(stem) = Stem::parse(verb) {
            target = Target::Stem(deck_id, stem);
            return dispatch_stem(words, target, dispatcher, command_tx_of(dispatcher));
        }
    }
    let command_tx = &dispatcher.command_tx;

    match verb {
        "load" => {
            let deck_id = match deck_only(target) {
                Ok(id) => id,
                Err(e) => return Action::Failed(e),
            };
            let Some(path) = words.next() else {
                return Action::Failed("usage: [deck] load <path> [bpm]".into());
            };
            let bpm = match words.next() {
                Some(raw) => match raw.parse::<f32>() {
                    Ok(value) => Some(value),
                    Err(_) => return Action::Failed("bpm must be a number".into()),
                },
                None => None,
            };
            spawn_load(deck_id, path.to_owned(), bpm, dispatcher);
            Action::Continue
        }
        "analyse" | "analyze" => {
            let deck_id = match deck_only(target) {
                Ok(id) => id,
                Err(e) => return Action::Failed(e),
            };
            let Some(source) = pipeline.deck_source(deck_id) else {
                return Action::Failed(format!("deck {deck_id} holds no track to analyse"));
            };
            spawn_analyse(deck_id, source, backend, dispatcher);
            Action::Continue
        }
        "stem" => stem_family(&mut words, target, dispatcher, pipeline),
        "play" => forward(target, command_tx, |deck_id| Command::TogglePlay { deck_id }),
        "cue" => with_deck(target, "cue", |deck_id| {
            cue_command(&mut words, deck_id, command_tx)
        }),
        "jump" => with_deck(target, "jump", |deck_id| {
            match words.next().map(str::parse::<u64>) {
                Some(Ok(target_frame)) => send(
                    command_tx,
                    Command::Jump {
                        deck_id,
                        target_frame,
                    },
                ),
                _ => Action::Failed("usage: [deck] jump <frame>".into()),
            }
        }),
        "beatjump" => with_deck(target, "beatjump", |deck_id| {
            match words.next().map(str::parse::<i64>) {
                Some(Ok(beats)) => send(command_tx, Command::BeatJump { deck_id, beats }),
                _ => Action::Failed("usage: [deck] beatjump <beats>".into()),
            }
        }),
        "tempo" => with_deck(target, "tempo", |deck_id| {
            match words.next().map(str::parse::<f32>) {
                Some(Ok(tempo)) => send(command_tx, Command::SetTempo { deck_id, tempo }),
                _ => Action::Failed("usage: [deck] tempo <ratio>".into()),
            }
        }),
        "tempofader" => with_deck(target, "tempofader", |deck_id| {
            match words.next().map(str::parse::<f32>) {
                Some(Ok(position)) if position.is_finite() && (-1.0..=1.0).contains(&position) => {
                    send(command_tx, Command::SetTempoFader { deck_id, position })
                }
                _ => Action::Failed("usage: [deck] tempofader <-1..1>".into()),
            }
        }),
        "temporange" => with_deck(target, "temporange", |deck_id| {
            match words.next().map(str::parse::<f32>) {
                Some(Ok(range)) if range.is_finite() && range > 0.0 && range <= 1.0 => {
                    send(command_tx, Command::SetTempoRange { deck_id, range })
                }
                _ => Action::Failed("usage: [deck] temporange <0..1> (0.1 = ±10%)".into()),
            }
        }),
        "keylock" => with_deck(target, "keylock", |deck_id| {
            match words.next().and_then(KeylockMode::parse) {
                Some(mode) => send(command_tx, Command::SetKeylock { deck_id, mode }),
                None => Action::Failed("usage: [deck] keylock on|off".into()),
            }
        }),
        "key" => with_deck(target, "key", |deck_id| {
            match words.next().map(str::parse::<i32>) {
                Some(Ok(semitones)) => send(command_tx, Command::SetKey { deck_id, semitones }),
                _ => Action::Failed("usage: [deck] key <semitones>".into()),
            }
        }),
        "loop" => with_deck(target, "loop", |deck_id| {
            loop_command(&mut words, deck_id, command_tx)
        }),
        // The platter: touch pauses and is remembered, the wheel moves the playhead while held and
        // bends the tempo while not, release restores what the touch interrupted.
        "vinyl" => with_deck(target, "vinyl", |deck_id| {
            let op = match words.next() {
                None | Some("touch") => VinylOp::Touch,
                Some("release") => VinylOp::Release,
                Some("slip") => VinylOp::Slip,
                Some("turn") => match words.next().and_then(|value| value.parse::<i32>().ok()) {
                    Some(ticks) => VinylOp::Turn {
                        ticks,
                        frames_per_tick: hypermixx_midi::map::DEFAULT_VINYL_FRAMES_PER_TICK,
                    },
                    None => return Action::Failed("usage: [deck] vinyl turn <ticks>".into()),
                },
                Some(other) => {
                    return Action::Failed(format!(
                        "unknown vinyl subcommand `{other}` — touch|release|slip|turn <ticks>"
                    ))
                }
            };
            send(command_tx, Command::Vinyl { deck_id, op })
        }),
        "sync" => with_deck(target, "sync", |deck_id| {
            sync_command(&mut words, deck_id, command_tx)
        }),
        "fader" => fader_command(&mut words, target, command_tx),
        "nudge" => with_deck(target, "nudge", |deck_id| {
            nudge_command(&mut words, deck_id, command_tx)
        }),
        "fx" => fx(&mut words, dispatcher, target),
        "state" => send(command_tx, Command::GetAllStates),
        "output" | "outputs" => output_command(&mut words, command_tx),
        "mixer" => mixer_command(&mut words, dispatcher),
        "live" => live_command(&mut words, dispatcher),
        "profile" => profile_command(&mut words, dispatcher),
        "midi" => midi_command(&mut words),
        "help" | "h" | "?" => Action::Message(help_text(decks)),
        "quit" | "exit" => Action::Quit,
        other => Action::Failed(format!("unknown command `{other}` — `help` lists them")),
    }
}

/// The `fader` family: one verb for every level, so the CLI and the MIDI map name the same stages.
///
/// `flow | deck | gain | cuesend` address the target deck; `cross | master | cue` are the shared
/// controls and ignore the target. Positions are `-1.0 ..= 1.0` (0.0 = unity), except `cuesend`,
/// which is a linear `0..=1` send level.
fn fader_command<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    target: Target,
    command_tx: &Sender<Command>,
) -> Action {
    use hypermixx_core::FaderTarget;
    let usage = "usage: [deck] fader <flow|deck|gain|cuesend> <position> \
                 | fader <cross|master|cue> <position>";
    let Some(stage) = words.next() else {
        return Action::Message(usage.into());
    };
    let Some(raw) = words.next() else {
        return Action::Failed(usage.into());
    };
    let value = match raw.parse::<f32>() {
        Ok(value) if value.is_finite() => value,
        _ => return Action::Failed(format!("the position must be a number, got `{raw}`")),
    };
    let target_fader = match stage {
        "cross" | "crossfader" => FaderTarget::Crossfader,
        "master" => FaderTarget::Master,
        "cue" => FaderTarget::Cue,
        // Everything else names a stage inside the target channel.
        deck_stage => {
            let deck_id = match deck_only(target) {
                Ok(deck_id) => deck_id,
                Err(_) => return Action::Failed(format!("`fader {deck_stage}` needs a deck")),
            };
            match deck_stage {
                "flow" => FaderTarget::Flow(deck_id),
                "deck" => FaderTarget::Deck(deck_id),
                "gain" | "trim" => FaderTarget::Gain(deck_id),
                "cuesend" | "cue_send" => FaderTarget::CueSend(deck_id),
                other => {
                    return Action::Failed(format!(
                        "unknown fader `{other}` (flow|deck|gain|cuesend|cross|master|cue)"
                    ))
                }
            }
        }
    };
    send(command_tx, Command::SetFader { target: target_fader, value })
}

/// The shipped default topology, under `./profile/`. Also what a run without `--config` reads.
pub const DEFAULT_TOPOLOGY: &str = crate::profile::DEFAULT_MIXER;

/// The shipped live settings, under `./profile/`. Also what a run without `--live` reads.
pub const DEFAULT_LIVE: &str = crate::profile::DEFAULT_LIVE;

/// Downloads a model on a worker thread, reporting progress through the notice channel.
fn spawn_model_pull(
    deck_id: u8,
    spec: &'static hypermixx_stems::ModelSpec,
    dispatcher: &Dispatcher,
) -> Action {
    let notices_tx = dispatcher.notices.clone();
    std::thread::Builder::new()
        .name("hypermixx-model".into())
        .spawn(move || {
            let progress = {
                let notices_tx = notices_tx.clone();
                let last = std::sync::atomic::AtomicU32::new(0);
                std::sync::Arc::new(move |fraction: f32| {
                    let percent = (fraction * 100.0) as u32;
                    let previous = last.load(std::sync::atomic::Ordering::Relaxed);
                    if percent >= previous + 10 {
                        last.store(percent, std::sync::atomic::Ordering::Relaxed);
                        notices::info(&notices_tx, format!("[model] {}: {percent}%", spec.name));
                    }
                }) as hypermixx_stems::ProgressSink
            };
            notices::info(
                &notices_tx,
                format!(
                    "[model] {}: {:.0} MB ({})...",
                    spec.name,
                    spec.bytes as f32 / 1_000_000.0,
                    spec.repo_path
                ),
            );
            match hypermixx_stems::ensure_model(spec, None, &progress) {
                Ok(path) => notices::info(
                    &notices_tx,
                    format!("[model] {}: verified and cached at {}", spec.name, path.display()),
                ),
                Err(err) => notices::error(&notices_tx, format!("[model] {}: {err}", spec.name)),
            }
            let _ = deck_id;
        })
        .ok();
    Action::Continue
}

/// What `[stems] model` (or `--model`) names: a table entry, or a **path** to a local `.onnx`.
///
/// A path is recognised by shape (`.onnx`, or a separator in it) rather than by trying to open it,
/// so a typo in a table name is still reported as an unknown model instead of a missing file.
fn model_choice(
    token: &str,
) -> Result<(&'static hypermixx_stems::ModelSpec, Option<std::path::PathBuf>), String> {
    let looks_like_a_path = token.ends_with(".onnx") || token.contains('/');
    if looks_like_a_path {
        let path = std::path::PathBuf::from(token);
        // The spec is only a *description*: the shape comes from the default model, the name and size
        // from the file. Nothing is downloaded for a local model.
        let spec = hypermixx_stems::local_spec(&path, *hypermixx_stems::default_model());
        return Ok((Box::leak(Box::new(spec)), Some(path)));
    }
    hypermixx_stems::model_by_name(token)
        .map(|spec| (spec, None))
        .ok_or_else(|| {
            format!(
                "unknown model `{token}` (available: {}; a path to a local `.onnx` works too)",
                hypermixx_stems::model_names().join(", ")
            )
        })
}

/// Reads a live-settings file: TOML, then made sane. Returns the settings and whatever had to be
/// substituted, so the caller can say so once rather than silently changing a user's machine.
pub fn read_live_config(path: &str) -> Result<(LiveConfig, Vec<String>), String> {
    let text = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    let parsed: LiveConfig = toml::from_str(&text).map_err(|err| err.to_string())?;
    let live = parsed.clone().sanitised();
    let mut notes = Vec::new();
    if live != parsed {
        notes.push(format!(
            "{path}: some values were out of range and were clamped"
        ));
    }
    Ok((live, notes))
}

/// The `profile` family: which files this session is running with, and switching them.
///
/// A profile is read **once at startup** (`--profile <name|path>`, else `profile/profile.toml` when
/// it exists). This family is how a running session answers "which one am I on?" and how it switches
/// without a restart: everything a profile names can be re-read, except the MIDI input, which is
/// attached once at launch.
fn profile_command<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    dispatcher: &Dispatcher,
) -> Action {
    let Some(sub) = words.next() else {
        return Action::Message(profile_report(dispatcher));
    };
    match sub {
        "show" | "list" => Action::Message(profile_report(dispatcher)),
        "reload" | "use" => {
            let path = words
                .next()
                .map(crate::profile::resolve)
                .or_else(|| dispatcher.profile_path())
                .unwrap_or_else(|| crate::profile::AMBIENT.to_owned());
            match crate::profile::load(&path) {
                Ok((profile, notes)) => {
                    let mut applied = Vec::new();
                    // Reloading a profile is **exactly "start as if with `--profile X`"**, minus the
                    // MIDI input (attached once at launch): a key the profile does not name falls back
                    // to the folder default, rather than leaving whatever happened to be running.
                    let mixer = profile
                        .mixer
                        .clone()
                        .unwrap_or_else(|| crate::profile::DEFAULT_MIXER.to_owned());
                    match std::fs::read_to_string(&mixer) {
                        Ok(text) => {
                            let _ = dispatcher.command_tx.send(Command::MixerReload { text });
                            dispatcher.with_topology(Some(mixer.clone()));
                            applied.push(format!("topology <- {mixer}"));
                        }
                        Err(err) => applied.push(format!("topology: {mixer}: {err}")),
                    }
                    // The live settings: they are read from the dispatcher on every use, so a reload
                    // lands on the next separation.
                    let live = profile
                        .live
                        .clone()
                        .unwrap_or_else(|| crate::profile::DEFAULT_LIVE.to_owned());
                    match read_live_config(&live) {
                        Ok((config, live_notes)) => {
                            for note in &live_notes {
                                notices::info(&dispatcher.notices, format!("[live] {note}"));
                            }
                            applied.push(format!("live <- {live}"));
                            dispatcher.with_live(config, Some(live));
                        }
                        Err(err) => applied.push(format!("live: {err}")),
                    }
                    if profile.midi.device.is_some() || profile.midi.map.is_some() {
                        // Honest about the one thing that cannot change under a running session.
                        applied.push(format!(
                            "midi: {} — takes effect at the next launch",
                            profile.midi.device.as_deref().unwrap_or("(no device)")
                        ));
                    }
                    dispatcher.with_profile(Some(path.clone()));
                    for note in notes {
                        applied.push(note);
                    }
                    Action::Message(format!(
                        "profile {path}:{}\n  (live reload applies now; MIDI only at launch)",
                        applied
                            .iter()
                            .map(|line| format!("\n  {line}"))
                            .collect::<String>()
                    ))
                }
                Err(err) => Action::Failed(err),
            }
        }
        other => Action::Failed(format!("unknown profile subcommand `{other}` (show|reload)")),
    }
}

/// What this session is running with: the profile, and the files it resolved to.
fn profile_report(dispatcher: &Dispatcher) -> String {
    let mut text = match dispatcher.profile_path() {
        Some(path) => format!("profile: {path}"),
        None => "profile: none (folder defaults; `--profile <name>` reads profile/<name>.toml)"
            .to_owned(),
    };
    text.push_str(&format!(
        "\n  topology: {}",
        dispatcher.topology_path()
    ));
    text.push_str(&format!("\n  live:     {}", dispatcher.live_path()));
    let live = dispatcher.live();
    text.push_str(&format!(
        "\n  stems:    model {}  provider {}  overlap {:.2}  shifts {}  threads {}  memory {}",
        live.stems.model,
        live.stems.provider,
        live.stems.overlap,
        live.stems.shifts,
        if live.stems.threads == 0 {
            "auto".to_owned()
        } else {
            live.stems.threads.to_string()
        },
        if live.stems.low_memory { "low" } else { "max-speed" },
    ));
    text.push_str("\n  MIDI is attached at launch, so a switch takes effect on the next one.");
    text
}

/// The `live` family: what this machine is set to.
///
/// These are the settings a *restart* applies (the engine rate, the ring sizes); `live reload`
/// re-reads the file and says so rather than pretending a running engine can change its clock. The
/// stem defaults it carries do apply to the next separation, which is why a reload is worth doing
/// even mid-session.
fn live_command<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    dispatcher: &Dispatcher,
) -> Action {
    let Some(sub) = words.next() else {
        return Action::Message(live_report(&dispatcher.live(), None));
    };
    match sub {
        "show" | "list" => Action::Message(live_report(&dispatcher.live(), None)),
        "reload" | "load" => {
            let path = words
                .next()
                .map(str::to_owned)
                .unwrap_or_else(|| dispatcher.live_path());
            match read_live_config(&path) {
                Ok((live, notes)) => {
                    let previous = dispatcher.live();
                    let restart = live.audio != previous.audio || live.keylock != previous.keylock;
                    let mut text = live_report(&live, Some(&path));
                    for note in notes {
                        text.push_str(&format!("\n  {note}"));
                    }
                    if restart {
                        text.push_str(
                            "\n  audio/keylock settings changed: they apply on the next launch \
                             (the engine's clock, rings and keylock window are set when it starts)",
                        );
                    }
                    dispatcher.with_live(live, Some(path));
                    Action::Message(text)
                }
                Err(err) => Action::Failed(format!("{path}: {err}")),
            }
        }
        other => Action::Failed(format!("unknown live subcommand `{other}` (show|reload)")),
    }
}

/// The live settings, one line per group, with the file they came from when it is known.
fn live_report(live: &LiveConfig, path: Option<&str>) -> String {
    let mut text = match path {
        Some(path) => format!("live settings ({path}):"),
        None => "live settings:".to_owned(),
    };
    for line in live.report() {
        text.push_str("\n  ");
        text.push_str(&line);
    }
    text
}

/// The `mixer` family: reloading the topology while the engine runs.
///
/// The file is read here, on the front-end, and its **text** is sent to the engine — file IO stays
/// out of the audio crate, and the engine never needs to know where a user keeps configs. Undefined
/// keys take their defaults, exactly as at startup, so a partial file is a partial topology rather
/// than a patch over the running one.
fn mixer_command<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    dispatcher: &Dispatcher,
) -> Action {
    let Some(sub) = words.next() else {
        return Action::Message("usage: mixer reload [<file>]".into());
    };
    match sub {
        "reload" | "load" => {
            let path = words
                .next()
                .map(str::to_owned)
                .unwrap_or_else(|| dispatcher.topology_path());
            match std::fs::read_to_string(&path) {
                Ok(text) => send(
                    &dispatcher.command_tx,
                    Command::MixerReload { text },
                ),
                Err(err) => Action::Failed(format!("{path}: {err}")),
            }
        }
        other => Action::Failed(format!("unknown mixer subcommand `{other}` (reload)")),
    }
}

/// The `output` family: which physical destination each logical output is bound to.
///
/// `output list` reports every destination with its state (`live` / `sink` / `missing` /
/// `disabled`); `output refresh` re-binds them all, which is what to run after plugging a device in.
fn output_command<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    command_tx: &Sender<Command>,
) -> Action {
    let op = match words.next() {
        Some("list") | Some("ls") | None => OutputOp::List,
        Some("refresh") | Some("reload") | Some("reopen") => OutputOp::Refresh,
        Some(other) => {
            return Action::Failed(format!("unknown output subcommand `{other}` (list|refresh)"))
        }
    };
    send(command_tx, Command::Output { op })
}

/// The `midi` family. Only port listing lands here: opening an input happens at launch
/// (`--midi`), because a mapping is wired to the running engine once, not toggled mid-session.
fn midi_command<'a>(words: &mut impl Iterator<Item = &'a str>) -> Action {
    match words.next() {
        Some("ports") | Some("list") => match hypermixx_midi::list_ports() {
            Ok(ports) if ports.is_empty() => Action::Message("midi: no input ports".into()),
            Ok(ports) => {
                let mut text = String::from("midi input ports:");
                for port in ports {
                    text.push_str(&format!("\n  {}  {}", port.index, port.name));
                }
                Action::Message(text)
            }
            Err(err) => Action::Failed(format!("midi: {err}")),
        },
        _ => Action::Message("usage: midi ports".into()),
    }
}

/// The `sync` family. Parsing mistakes come back as `Failed` immediately; refusals from the engine
/// (no grid, a reversed lock, a deck that is already the leader) arrive later as `Error` responses.
///
/// `phase` includes `tempo`, `phaselock` includes `tempolock` — the subcommand says how far the
/// deck ends up committed, not which arithmetic it runs first.
fn sync_command<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    deck_id: u8,
    command_tx: &Sender<Command>,
) -> Action {
    let usage = "usage: [deck] sync tempo | phase <mode> [seconds] | tempolock | phaselock <mode> [seconds] | set-leader | unlock";
    let Some(sub) = words.next() else {
        return Action::Message(usage.into());
    };
    let op = match sub {
        "tempo" => SyncOp::Tempo,
        "phase" => {
            let (mode, t_seconds) = match phase_mode(words, "phase") {
                Ok(parsed) => parsed,
                Err(message) => return Action::Failed(message),
            };
            SyncOp::Phase { mode, t_seconds }
        }
        "tempolock" | "lock" => SyncOp::TempoLock,
        "phaselock" => {
            let (mode, t_seconds) = match phase_mode(words, "phaselock") {
                Ok(parsed) => parsed,
                Err(message) => return Action::Failed(message),
            };
            SyncOp::PhaseLock { mode, t_seconds }
        }
        // Target-first grammar: the deck named before `sync` *is* the leader, so no argument.
        "set-leader" | "leader" => SyncOp::SetLeader,
        "unlock" | "off" => SyncOp::Unlock,
        other => {
            return Action::Failed(format!(
                "unknown sync subcommand `{other}` — tempo|phase|tempolock|phaselock|set-leader|unlock"
            ));
        }
    };
    send(command_tx, Command::Sync { deck_id, op })
}

/// `<mode> [seconds]` for `sync phase` / `sync phaselock`. Only `linear` takes a duration — a
/// duration on `pid` would be a typo the user should see rather than an argument silently ignored.
fn phase_mode<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    what: &str,
) -> Result<(PhaseMode, Option<f64>), String> {
    let usage = format!("usage: [deck] sync {what} <instant|linear [seconds]|pid>");
    let Some(token) = words.next() else {
        return Err(usage);
    };
    let Some(mode) = PhaseMode::parse(token) else {
        return Err(format!("unknown phase mode `{token}` — instant|linear|pid\n{usage}"));
    };
    let t_seconds = match words.next() {
        Some(raw) => match raw.parse::<f64>() {
            Ok(seconds) => {
                if mode != PhaseMode::Linear {
                    return Err(format!(
                        "`{}` does not take a duration — only `linear` does (e.g. `sync {what} linear 2.0`)",
                        mode.label()
                    ));
                }
                Some(seconds)
            }
            Err(_) => return Err(format!("duration must be a number of seconds, got `{raw}`")),
        },
        None => None,
    };
    Ok((mode, t_seconds))
}

/// The `cue` family. A bare `cue` is the smart gesture — the deck decides between "back to the
/// cue point and pause" and "set the cue point here" from its own transport, so the front-end
/// never has to guess. The explicit subcommands are for a held button (MIDI press/release).
fn cue_command<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    deck_id: u8,
    command_tx: &Sender<Command>,
) -> Action {
    let op = match words.next() {
        // Bare `cue` is the *button*, not one edge of it: the first press is the press edge (cue the
        // playhead and play, or back-cue while playing), the next is the release (return to the cue
        // and pause). The deck owns that state.
        None => CueOp::Button,
        Some("play") => CueOp::Play,
        Some("back") => CueOp::Back,
        Some("set") => CueOp::Set,
        // The decision without the preview: back-cue while playing, place the cue point while paused.
        Some("smart") => CueOp::Smart,
        Some(other) => {
            return Action::Failed(format!(
                "unknown cue subcommand `{other}` — play|back|set|smart (or bare `cue`)"
            ));
        }
    };
    send(command_tx, Command::Cue { deck_id, op })
}

/// The `nudge` family: a temporary rate bend that changes phase while it runs and leaves the
/// tempo alone when it ends. The duration is mandatory — a bend with no end is a tempo change the
/// user did not ask for (hold-to-bend lives in MIDI's momentary binding).
fn nudge_command<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    deck_id: u8,
    command_tx: &Sender<Command>,
) -> Action {
    let usage = "usage: [deck] nudge <delta> <seconds> | [deck] nudge off";
    let Some(raw) = words.next() else {
        return Action::Message(usage.into());
    };
    let op = match raw {
        "off" | "stop" | "release" | "reset" => NudgeOp::Stop,
        other => {
            let delta = match other.parse::<f32>() {
                Ok(delta) => delta,
                Err(_) => {
                    return Action::Failed(format!(
                        "nudge needs a rate (0.04 = 4% fast), got `{other}`\n{usage}"
                    ));
                }
            };
            let seconds = match words.next() {
                Some(raw_seconds) => match raw_seconds.parse::<f64>() {
                    Ok(seconds) => seconds,
                    Err(_) => {
                        return Action::Failed(format!(
                            "nudge duration must be a number of seconds, got `{raw_seconds}`"
                        ));
                    }
                },
                None => return Action::Failed(format!("nudge needs a duration\n{usage}")),
            };
            NudgeOp::Start {
                delta,
                seconds: Some(seconds),
            }
        }
    };
    send(command_tx, Command::Nudge { deck_id, op })
}

/// The `loop` family. Parsing mistakes come back as `Failed` immediately; refusals from the engine
/// (no grid, `out` without `in`) arrive later as `Error` responses, printed like any other.
fn loop_command<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    deck_id: u8,
    command_tx: &Sender<Command>,
) -> Action {
    let usage = "usage: [deck] loop in | out | <beats> | slip in | slip out | slip <beats> | slip roll <beats> | exit | cancel | halve | double | edit <len|move|in|out> <beats> | quantum <beat|half|quarter|eighth>";
    let Some(sub) = words.next() else {
        return Action::Message(usage.into());
    };
    let op = match sub {
        "in" => LoopOp::In,
        "out" => LoopOp::Out,
        "cancel" => LoopOp::Cancel,
        "exit" | "off" => LoopOp::Exit,
        "slip" => match words.next() {
            Some("in") => LoopOp::SlipIn,
            Some("out") => LoopOp::SlipOut,
            Some("roll") => match words.next().and_then(|token| token.parse::<u64>().ok()) {
                Some(beats) if beats > 0 => LoopOp::SlipRoll(beats),
                _ => return Action::Failed("usage: [deck] loop slip roll <beats>".into()),
            },
            Some(raw) => match raw.parse::<u64>() {
                Ok(beats) if beats > 0 => LoopOp::SlipBeats(beats),
                _ => {
                    return Action::Failed(format!(
                        "unknown slip loop subcommand `{raw}` — in|out|roll <beats>|<beats>"
                    ));
                }
            },
            None => {
                return Action::Failed("usage: [deck] loop slip in|out|<beats>|roll <beats>".into())
            }
        },
        "quantum" => match words.next() {
            Some(token) => match LoopQuantum::parse(token) {
                Some(quantum) => LoopOp::SetQuantum(quantum),
                None => {
                    return Action::Failed(format!(
                        "unknown quantum `{token}` — beat|half|quarter|eighth"
                    ));
                }
            },
            None => {
                return Action::Failed(
                    "usage: [deck] loop quantum <beat|half|quarter|eighth>".into(),
                );
            }
        },
        "edit" => match loop_edit_op(words) {
            Ok(op) => LoopOp::Edit(op),
            Err(message) => return Action::Failed(message),
        },
        // The dedicated ÷2/×2 length keys — relative to whatever the loop runs right now,
        // clamped to 1/32..=64 beats by the engine.
        "halve" | "/2" | "÷2" => LoopOp::Edit(LoopEditOp::Halve),
        "double" | "x2" | "*2" | "×2" => LoopOp::Edit(LoopEditOp::Double),
        raw => match raw.parse::<u64>() {
            Ok(beats) if beats > 0 => LoopOp::Beats(beats),
            Ok(_) => return Action::Failed("a loop needs at least 1 beat".into()),
            Err(_) => {
                return Action::Failed(format!("unknown loop subcommand `{raw}`\n{usage}"));
            }
        },
    };
    send(command_tx, Command::Loop { deck_id, op })
}

/// One `loop edit` argument pair: `<len|move|in|out> <beats>`.
fn loop_edit_op<'a>(words: &mut impl Iterator<Item = &'a str>) -> Result<LoopEditOp, String> {
    let usage = "usage: [deck] loop edit <len|move|in|out> <beats>";
    let what = words.next().ok_or_else(|| usage.to_owned())?;
    let raw = words.next().ok_or_else(|| usage.to_owned())?;
    let number = |raw: &str| format!("beats must be a number, got `{raw}`");
    match what {
        "len" | "length" | "size" => raw
            .parse::<f64>()
            .map(|beats| LoopEditOp::Length { beats })
            .map_err(|_| number(raw)),
        "move" | "shift" => raw
            .parse::<i64>()
            .map(|beats| LoopEditOp::Move { beats })
            .map_err(|_| number(raw)),
        "in" => raw
            .parse::<i64>()
            .map(|beats| LoopEditOp::In { beats })
            .map_err(|_| number(raw)),
        "out" => raw
            .parse::<i64>()
            .map(|beats| LoopEditOp::Out { beats })
            .map_err(|_| number(raw)),
        other => Err(format!("unknown edit `{other}` — len|move|in|out\n{usage}")),
    }
}

/// The `fx` family. The chain comes from the line's target, not from an argument:
/// `fx add eq`, `deck1 fx list`, `deck0 vocals fx add filter`, `master fx add limiter`.
fn fx<'a>(
    mut words: impl Iterator<Item = &'a str>,
    dispatcher: &Dispatcher,
    target: Target,
) -> Action {
    let chain = match target {
        Target::Deck(deck_id) => FxChainId::Deck(deck_id),
        Target::Stem(deck, stem) => FxChainId::Stem { deck, stem },
        Target::Master => FxChainId::Master,
    };
    let command_tx = &dispatcher.command_tx;
    let Some(sub) = words.next() else {
        return Action::Message(fx_help_text());
    };
    match sub {
        "add" => {
            let Some(kind) = words.next() else {
                return Action::Failed("usage: [deck|master] fx add <kind>".into());
            };
            if let Err(err) = FxKind::parse(kind) {
                return Action::Failed(err.message());
            }
            send(
                command_tx,
                Command::AddFx {
                    chain,
                    kind: kind.to_owned(),
                },
            )
        }
        "remove" | "rm" => {
            let index = match resolve_slot(words.next(), chain, dispatcher) {
                Ok(index) => index,
                Err(e) => return Action::Failed(e),
            };
            let action = send(command_tx, Command::RemoveFx { chain, index });
            // We sent the removal, so we can mirror it locally without a round-trip.
            dispatcher.slots().remove(&chain.label(), index);
            action
        }
        "list" | "ls" => send(command_tx, Command::ListFx { chain }),
        // The engine owns a slot's a/b state, so a toggle is a command rather than a front-end flip.
        "toggle" | "tgl" => {
            let index = match resolve_slot(words.next(), chain, dispatcher) {
                Ok(index) => index,
                Err(e) => return Action::Failed(e),
            };
            send(command_tx, Command::ToggleFx { slot: FxSlotRef { chain, index } })
        }
        "set" => {
            let index = match resolve_slot(words.next(), chain, dispatcher) {
                Ok(index) => index,
                Err(e) => return Action::Failed(e),
            };
            let Some(param) = words.next() else {
                return Action::Failed("usage: [deck|master] fx set <slot> <param> <value>".into());
            };
            let Some(raw_value) = words.next() else {
                return Action::Failed("usage: [deck|master] fx set <slot> <param> <value>".into());
            };
            let value = match parse_param_value(param, raw_value) {
                Ok(value) => value,
                Err(e) => return Action::Failed(e),
            };
            send(
                command_tx,
                Command::SetFxParam {
                    slot: FxSlotRef { chain, index },
                    name: param.to_owned(),
                    value,
                },
            )
        }
        "on" | "off" => {
            let enabled = sub == "on";
            match resolve_slot(words.next(), chain, dispatcher) {
                Ok(index) => send(
                    command_tx,
                    Command::SetFxEnabled {
                        slot: FxSlotRef { chain, index },
                        enabled,
                    },
                ),
                Err(e) => Action::Failed(e),
            }
        }
        "trigger" => match resolve_slot(words.next(), chain, dispatcher) {
            Ok(index) => send(
                command_tx,
                Command::FxTrigger {
                    slot: FxSlotRef { chain, index },
                },
            ),
            Err(e) => Action::Failed(e),
        },
        "pad" => {
            let index = match resolve_slot(words.next(), chain, dispatcher) {
                Ok(index) => index,
                Err(e) => return Action::Failed(e),
            };
            match words.next() {
                Some("press" | "on" | "down") => send(
                    command_tx,
                    Command::PadPress {
                        slot: FxSlotRef { chain, index },
                    },
                ),
                Some("release" | "off" | "up") => send(
                    command_tx,
                    Command::PadRelease {
                        slot: FxSlotRef { chain, index },
                    },
                ),
                _ => Action::Failed("usage: [deck|master] fx pad <slot> <press|release>".into()),
            }
        }
        "help" | "h" => Action::Message(fx_help_text()),
        other => Action::Failed(format!(
            "unknown fx subcommand `{other}` — try `fx help`"
        )),
    }
}

/// Resolves an fx slot argument: a bare index, or an effect name from the slot book.
fn resolve_slot(raw: Option<&str>, chain: FxChainId, dispatcher: &Dispatcher) -> Result<usize, String> {
    let Some(raw) = raw else {
        return Err("missing slot: an index or an effect name (see `fx list`)".into());
    };
    dispatcher.slots().resolve(&chain.label(), raw)
}

/// Parses a parameter value. Everything the engine exposes is numeric; a non-number is a typo the
/// user should see before the engine does.
fn parse_param_value(_param: &str, raw: &str) -> Result<f32, String> {
    raw.parse::<f32>()
        .map_err(|_| format!("value must be a number, got `{raw}`"))
}

fn list_kinds(kinds: &[String]) -> String {
    kinds
        .iter()
        .enumerate()
        .map(|(index, kind)| format!("{index} {kind}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The default target for a bare command. `Master` only accepts `fx`.
fn deck_only(target: Target) -> Result<u8, String> {
    match target {
        Target::Deck(deck_id) => Ok(deck_id),
        // `deck0 vocals play` is unambiguous — the stem only narrows *stem* commands — so it plays
        // deck 0 rather than making the user retype the line.
        Target::Stem(deck_id, _) => Ok(deck_id),
        Target::Master => Err("`master` only takes `fx` commands".into()),
    }
}

/// Runs `apply` for a deck-targeted command, rejecting `master`.
fn with_deck(target: Target, verb: &str, apply: impl FnOnce(u8) -> Action) -> Action {
    match deck_only(target) {
        Ok(deck_id) => apply(deck_id),
        Err(_) => Action::Failed(format!("`{verb}` needs a deck, not `master`")),
    }
}

/// Builds and sends a no-argument deck command.
fn forward(target: Target, command_tx: &Sender<Command>, build: impl FnOnce(u8) -> Command) -> Action {
    match deck_only(target) {
        Ok(deck_id) => send(command_tx, build(deck_id)),
        Err(_) => Action::Failed("that command needs a deck, not `master`".into()),
    }
}

fn send(command_tx: &Sender<Command>, command: Command) -> Action {
    if command_tx.send(command).is_err() {
        Action::Failed("the audio engine is gone".into())
    } else {
        Action::Continue
    }
}

/// Parses a leading deck/chain token: `master`/`m`, or `deck0`/`d0`/`0`.
fn parse_target(token: &str, decks: usize) -> Option<Target> {
    let lower = token.to_ascii_lowercase();
    if lower == "master" || lower == "m" {
        return Some(Target::Master);
    }
    let digits = lower
        .strip_prefix("deck")
        .or_else(|| lower.strip_prefix('d'))
        .unwrap_or(&lower);
    let id: u8 = digits.parse().ok()?;
    ((id as usize) < decks).then_some(Target::Deck(id))
}

/// The `stem` family on a deck: `stem separate`, `stem status`, `stem acapella`, …
fn stem_family<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    target: Target,
    dispatcher: &Dispatcher,
    pipeline: &AudioPipeline,
) -> Action {
    let Ok(deck_id) = deck_only(target) else {
        return Action::Failed("`stem` needs a deck, not `master`".into());
    };
    let command_tx = &dispatcher.command_tx;
    let Some(sub) = words.next() else {
        return Action::Message(stem_help_text());
    };
    match sub {
        "separate" | "split" => {
            // One read of the live config for the whole arm: the rate guard, the model choice and
            // every separator parameter come from it (`live reload` applies an edit immediately).
            let live = dispatcher.live();
            // The bundled model is a 44.1 kHz export and the cache is keyed on that rate, so a
            // 48k engine would feed it material it was not trained on. Refuse rather than resample
            // silently in both directions — that is a job for a model whose spec says 48k.
            if live.audio.sample_rate != SAMPLE_RATE {
                return Action::Failed(format!(
                    "separation needs the engine at {}Hz (the model is a {}Hz export); this run is \
                     at {}Hz — set `[audio] sample_rate` in profile/live.toml and launch it",
                    SAMPLE_RATE,
                    SAMPLE_RATE,
                    live.audio.sample_rate
                ));
            }
            // Everything the separator is asked for comes from `[stems]` in live.toml: the model, the
            // provider, the window overlap, the shift ensemble, the thread budget and the memory
            // trade. A hand-typed flag would be a second place to keep them in step — and these are
            // properties of *this machine*, which is exactly what live.toml is for. `live reload`
            // picks up an edit immediately.
            let (mut spec, mut local_file) = match model_choice(&live.stems.model) {
                Ok(choice) => choice,
                Err(err) => return Action::Failed(err),
            };
            match words.next() {
                // The one thing a run may override: try another model (a table name, or the path to a
                // local file) without editing the config first.
                Some("--model") => match words.next() {
                    Some(token) => match model_choice(token) {
                        Ok((named, path)) => {
                            spec = named;
                            local_file = path;
                        }
                        Err(err) => return Action::Failed(err),
                    },
                    None => return Action::Failed("--model needs a name or a path".into()),
                },
                Some(other) => {
                    return Action::Failed(format!(
                        "`stem separate` takes no options (got `{other}`): the model, provider, \
                         overlap, shifts, threads and memory are `[stems]` keys in live.toml, and \
                         `live reload` applies an edit. `--model <name|path>` is the one-off override."
                    ))
                }
                None => {}
            }
            let options = hypermixx_stems::SeparateOptions {
                shifts: live.stems.shifts,
                overlap: live.stems.overlap,
                provider: hypermixx_stems::Provider::parse(&live.stems.provider)
                    .unwrap_or_default(),
            };
            let runtime = hypermixx_stems::RuntimeOptions {
                threads: hypermixx_stems::RuntimeOptions::threads_from(live.stems.threads),
                low_memory: live.stems.low_memory,
            };
            // A build without the feature must say so *before* fetching a model and starting a job
            // that takes minutes.
            if !hypermixx_stems::provider_available(options.provider) {
                return Action::Failed(format!(
                    "this build has no {} support (rebuild with --features cuda, or set \
                     `[stems] provider` in live.toml)",
                    options.provider.label()
                ));
            }
            let Some(source) = pipeline.deck_source(deck_id) else {
                return Action::Failed(format!("deck {deck_id} holds no track to separate"));
            };
            spawn_separate(deck_id, source, options, runtime, spec, local_file, dispatcher);
            Action::Continue
        }
        "status" => send(command_tx, Command::GetStemState { deck_id }),
        "model" => match words.next() {
            // What a model is (name, size, source) is data in the table, so this cannot drift from
            // what `--model` accepts.
            None | Some("list") | Some("ls") => {
                let mut text = String::from("stem models:");
                for spec in hypermixx_stems::MODELS {
                    let cached = hypermixx_stems::model_path(spec).exists();
                    text.push_str(&format!(
                        "\n  {:<14} {:>6.0} MB  {}  {}{}",
                        spec.name,
                        spec.bytes as f32 / 1_000_000.0,
                        spec.repo_path.split('/').next().unwrap_or("-"),
                        // Presence, not validity: `ensure_model` verifies the hash when the file is
                        // actually used, and a table entry that is missing is fetched then.
                        if cached { "in .model" } else { "not fetched" },
                        if spec.name == hypermixx_stems::default_model().name {
                            "  (default)"
                        } else {
                            ""
                        }
                    ));
                }
                text.push_str("\n  `stem separate --model <name>` picks one, and it is part of the cache key.");
                Action::Message(text)
            }
            Some("pull") => {
                let Some(name) = words.next() else {
                    return Action::Failed("usage: stem model pull <name>".into());
                };
                match hypermixx_stems::model_by_name(name) {
                    Some(spec) => spawn_model_pull(deck_id, spec, dispatcher),
                    None => Action::Failed(format!(
                        "unknown model `{name}` (available: {})",
                        hypermixx_stems::model_names().join(", ")
                    )),
                }
            }
            Some(other) => Action::Failed(format!("unknown stem model subcommand `{other}` (list|pull)")),
        },
        "cache" => {
            // `stem cache prune [--keep <n>]` / `stem cache clear`; a bare `stem cache` reports.
            match words.next() {
                None => {
                    let (bytes, entries) = hypermixx_stems::cache_usage();
                    Action::Message(format!(
                        "stem cache: {entries} track(s), {:.1} MB in {}",
                        bytes as f64 / (1024.0 * 1024.0),
                        hypermixx_stems::stems_root().display()
                    ))
                }
                Some("clear") => cache_prune(0),
                Some("prune") => {
                    let mut keep = 4usize;
                    if let Some(flag) = words.next() {
                        if flag != "--keep" {
                            return Action::Failed(format!(
                                "usage: stem cache prune [--keep <n>], got `{flag}`"
                            ));
                        }
                        match words.next().and_then(|v| v.parse::<usize>().ok()) {
                            Some(value) => keep = value,
                            None => return Action::Failed("--keep needs a number".into()),
                        }
                    }
                    cache_prune(keep)
                }
                Some(other) => Action::Failed(format!(
                    "unknown stem cache command `{other}` (prune [--keep <n>] | clear)"
                )),
            }
        }
        "cancel" => {
            let running = dispatcher
                .separations
                .lock()
                .ok()
                .and_then(|running| running.get(&deck_id).cloned());
            match running {
                Some(cancelled) => {
                    cancelled.cancel();
                    Action::Message(format!(
                        "deck{deck_id}: cancelling the separation (it stops at the end of the \
                         current model window)"
                    ))
                }
                None => Action::Message(format!("deck{deck_id}: no separation is running")),
            }
        }
        "clear" => send(
            command_tx,
            Command::Stem {
                deck_id,
                op: StemOp::Clear,
            },
        ),
        other => match StemPreset::parse(other) {
            Some(preset) => send(
                command_tx,
                Command::Stem {
                    deck_id,
                    op: StemOp::Preset(preset),
                },
            ),
            None => Action::Failed(format!("unknown stem command `{other}`\n{}", stem_help_text())),
        },
    }
}

/// The verbs a stem sub-target takes: `deck0 vocals mute on`, `deck0 vocals level -0.5`, …
fn dispatch_stem<'a>(
    mut words: impl Iterator<Item = &'a str>,
    target: Target,
    dispatcher: &Dispatcher,
    command_tx: Sender<Command>,
) -> Action {
    let Target::Stem(deck_id, stem) = target else {
        return Action::Continue;
    };
    let Some(verb) = words.next() else {
        return Action::Failed(format!(
            "`deck{deck_id} {stem}` needs a verb (level|mute|solo|unsolo|fx)"
        ));
    };
    let on_off = |raw: Option<&str>| -> Result<bool, String> {
        match raw.map(str::to_ascii_lowercase).as_deref() {
            // A bare `mute` means "mute it": the common case should not need a second word.
            None | Some("on") | Some("1") | Some("true") => Ok(true),
            Some("off") | Some("0") | Some("false") => Ok(false),
            Some(other) => Err(format!("expected on|off, got `{other}`")),
        }
    };
    let op = match verb {
        "level" | "fader" => {
            let Some(raw) = words.next() else {
                return Action::Failed(format!("usage: deck{deck_id} {stem} level <position>"));
            };
            match raw.parse::<f32>() {
                Ok(position) if position.is_finite() => StemOp::Level { stem, position },
                _ => return Action::Failed(format!("level must be a number, got `{raw}`")),
            }
        }
        "mute" => match on_off(words.next()) {
            Ok(on) => StemOp::Mute { stem, on },
            Err(err) => return Action::Failed(err),
        },
        "solo" => match on_off(words.next()) {
            Ok(on) => StemOp::Solo { stem, on },
            Err(err) => return Action::Failed(err),
        },
        "unsolo" => StemOp::Solo { stem, on: false },
        "fx" => return fx(words, dispatcher, Target::Stem(deck_id, stem)),
        other => {
            return Action::Failed(format!(
                "unknown stem verb `{other}` (level|mute|solo|unsolo|fx)"
            ))
        }
    };
    send(&command_tx, Command::Stem { deck_id, op })
}

fn command_tx_of(dispatcher: &Dispatcher) -> Sender<Command> {
    dispatcher.command_tx.clone()
}

/// Separates a deck's loaded track on a worker thread and installs the result.
///
/// The pipeline needs the *mix* to separate, and `deck_source` is exactly that (the deck's track,
/// which it keeps even after stems replace stream 0). The four streams come back through
/// `Command::SetStems`, which is a seamless source swap
/// — so the user can keep playing, looping and beatmatching while this runs.
pub(crate) fn spawn_separate(
    deck_id: u8,
    mix: Shared,
    options: hypermixx_stems::SeparateOptions,
    runtime: hypermixx_stems::RuntimeOptions,
    spec: &'static hypermixx_stems::ModelSpec,
    local_file: Option<std::path::PathBuf>,
    dispatcher: &Dispatcher,
) {
    let command_tx = dispatcher.command_tx.clone();
    let notices_tx = dispatcher.notices.clone();
    let cancellations = Arc::clone(&dispatcher.separations);
    std::thread::Builder::new()
        .name(format!("hypermixx-stem-{deck_id}"))
        .spawn(move || {
            let cancelled = hypermixx_stems::Cancelled::new();
            if let Ok(mut running) = cancellations.lock() {
                running.insert(deck_id, cancelled.clone());
            }
            let out = separate_into_deck(SeparateJob {
                deck_id,
                mix: &mix,
                options,
                runtime,
                table_spec: spec,
                local_file,
                command_tx: &command_tx,
                notices_tx: &notices_tx,
                cancelled: &cancelled,
            });
            if let Err(err) = out {
                if err == hypermixx_stems::StemError::Cancelled {
                    notices::info(&notices_tx, format!("[stem] deck{deck_id}: cancelled"));
                } else {
                    notices::error(&notices_tx, format!("[stem] deck{deck_id}: {err}"));
                }
            }
            if let Ok(mut running) = cancellations.lock() {
                running.remove(&deck_id);
            }
        })
        .ok();
}

/// The job body: install the growing streams, then separate **block by block into them**.
///
/// The install happens before the first block, which is the whole point: the deck immediately renders
/// four streams, all of which are the mix (stream 0), so the listener keeps hearing the track — and
/// every block that arrives replaces its own span of that mix with real stems. Nothing is restarted,
/// nothing is re-installed, and the hand-over at the frontier is a 50 ms fade the growing source
/// owns.
///
/// The block length is **the model's own**: the runtime windows the track, and each block is emitted
/// once no later window can change it, so there is no chunk size to choose and no second segmentation
/// to keep aligned with the first.
/// One separation job's inputs. A struct because the job has grown past what an argument list reads
/// well as, and because a front-end's own worker may want to fill it in pieces.
struct SeparateJob<'a> {
    deck_id: u8,
    /// The **mix**: what the deck is playing, which is also what falls back in before a block lands.
    mix: &'a Shared,
    options: hypermixx_stems::SeparateOptions,
    runtime: hypermixx_stems::RuntimeOptions,
    /// The table entry (what gets downloaded, and what describes the architecture).
    table_spec: &'static hypermixx_stems::ModelSpec,
    /// A model file to run instead of the table entry: the user's own export.
    local_file: Option<std::path::PathBuf>,
    command_tx: &'a Sender<Command>,
    notices_tx: &'a NoticeTx,
    cancelled: &'a hypermixx_stems::Cancelled,
}

fn separate_into_deck(job: SeparateJob<'_>) -> Result<(), hypermixx_stems::StemError> {
    let SeparateJob {
        deck_id,
        mix,
        options,
        runtime,
        table_spec,
        local_file,
        command_tx,
        notices_tx,
        cancelled,
    } = job;
    let total = mix.total_frames();
    let growing = ChunkedStreams::new(
        Stem::COUNT,
        total,
        Some(Arc::clone(mix)),
        hypermixx_media::DEFAULT_FADE_FRAMES,
    );
    let streams: [Shared; Stem::COUNT] = match growing.streams().try_into() {
        Ok(streams) => streams,
        Err(_) => {
            return Err(hypermixx_stems::StemError::Backend(
                "a growing set must hand out one stream per stem".into(),
            ))
        }
    };
    let _ = command_tx.send(Command::SetStems {
        deck_id,
        stems: hypermixx_core::StemSet::new(streams),
    });

    // Say what is about to happen before it happens: fetching 316 MB from a mirror with no message
    // reads as a hang, and the fetch happens *before* the first progress report.
    // A local file is described by a spec built from the table entry's *shape* (segment length and
    // sample rate are properties of the architecture, not of the file) plus whatever the file says
    // about itself. The table entry is kept as well: it is the one that knows how to be downloaded.
    let spec: hypermixx_stems::ModelSpec = match &local_file {
        Some(path) => hypermixx_stems::local_spec(path, *table_spec),
        None => *table_spec,
    };
    let model = &spec;
    // The block length is the model's own window stride, so it follows `--overlap`:
    // `(1 - overlap) × segment`. There is no separate chunk size to set — see `docs/stem-info.md`.
    let block_frames = hypermixx_stems::stride_frames(model.segment_frames, options.overlap);
    notices::info(
        notices_tx,
        format!(
            "[stem] deck{deck_id}: {} blocks of {:.1}s as the model finishes them — the deck plays \
             the mix until the first one lands",
            total.div_ceil(block_frames.max(1)),
            block_frames as f32 / SAMPLE_RATE as f32,
        ),
    );
    notices::info(
        notices_tx,
        if hypermixx_stems::model_path(model).exists() {
            // The file is there; verifying it is a size check.
            format!("[stem] deck{deck_id}: checking the {} model...", model.name)
        } else {
            format!(
                "[stem] deck{deck_id}: fetching the {} model ({:.0} MB) — this happens once...",
                model.name,
                model.bytes as f32 / 1_000_000.0
            )
        },
    );

    // Progress in 10% steps: one notice per block would flood the log on a short window.
    let progress = {
        let notices_tx = notices_tx.clone();
        let last = std::sync::atomic::AtomicU32::new(0);
        std::sync::Arc::new(move |fraction: f32| {
            let percent = (fraction * 100.0) as u32;
            let previous = last.load(std::sync::atomic::Ordering::Relaxed);
            if percent >= previous + 10 {
                last.store(percent, std::sync::atomic::Ordering::Relaxed);
                notices::info(&notices_tx, format!("[stem] deck{deck_id}: {percent}%"));
            }
        }) as hypermixx_stems::ProgressSink
    };
    let separator = match &local_file {
        Some(path) => hypermixx_stems::local_separator(path, spec, options, runtime)?,
        None => hypermixx_stems::default_separator(table_spec, options, runtime, &progress)?,
    };
    notices::info(
        notices_tx,
        format!(
            "[stem] deck{deck_id}: {} (shifts {}, overlap {}, {}, threads {})",
            separator.id(),
            options.shifts,
            options.overlap,
            options.provider.label(),
            match runtime.threads {
                Some(threads) => threads.to_string(),
                None => "auto".to_owned(),
            },
        ),
    );

    let started = std::time::Instant::now();
    let mut blocks = 0u64;
    let mut sink = DeckSink {
        growing: &growing,
        deck_id,
        notices_tx,
        started,
        blocks: &mut blocks,
    };
    let out = hypermixx_stems::stream_cached(
        separator.as_ref(),
        mix.as_ref(),
        &mut sink,
        &progress,
        cancelled,
    );
    let separated = growing.available_frames();
    match &out {
        Ok(()) => notices::info(
            notices_tx,
            format!(
                "[stem] deck{deck_id}: {separated}/{total} frames separated in {:.0}s ({:.0}%), \
                 {blocks} blocks{}",
                started.elapsed().as_secs_f32(),
                separated as f32 / total.max(1) as f32 * 100.0,
                if separated >= total {
                    ""
                } else {
                    " — the rest is still the mix"
                },
            ),
        ),
        // A failure is reported by the caller; the watermark is still worth saying, because the deck
        // keeps playing what did arrive.
        Err(_) => notices::info(
            notices_tx,
            format!(
                "[stem] deck{deck_id}: stopping at {separated}/{total} frames after {:.0}s",
                started.elapsed().as_secs_f32()
            ),
        ),
    }
    out
}

/// The sink the separation writes into: append to the growing streams, then say so.
struct DeckSink<'a> {
    growing: &'a ChunkedStreams,
    deck_id: u8,
    notices_tx: &'a NoticeTx,
    started: std::time::Instant,
    blocks: &'a mut u64,
}

impl hypermixx_stems::StemBlockSink for DeckSink<'_> {
    fn block(&mut self, start_frame: u64, blocks: &[Vec<f32>]) -> bool {
        if blocks.len() != Stem::COUNT {
            notices::error(
                self.notices_tx,
                format!(
                    "[stem] deck{}: a block carried {} streams, expected {}; stopping",
                    self.deck_id,
                    blocks.len(),
                    Stem::COUNT
                ),
            );
            return false;
        }
        let buffers: Vec<Arc<Vec<f32>>> = blocks.iter().map(|block| Arc::new(block.clone())).collect();
        if let Err(err) = self.growing.append(start_frame, buffers) {
            notices::error(
                self.notices_tx,
                format!("[stem] deck{}: {err}; stopping", self.deck_id),
            );
            return false;
        }
        *self.blocks += 1;
        let total = self.growing.total_frames().max(1);
        let separated = self.growing.available_frames();
        // A block is short (a few seconds of audio), so the log line goes out every few blocks rather
        // than every one: the percentage is what a user reads.
        if *self.blocks % 4 == 1 || separated >= total {
            notices::info(
                self.notices_tx,
                format!(
                    "[stem] deck{}: block {} — {:.0}% separated ({:.0}s)",
                    self.deck_id,
                    self.blocks,
                    separated as f32 / total as f32 * 100.0,
                    self.started.elapsed().as_secs_f32()
                ),
            );
        }
        true
    }
}


/// Prunes the stem cache and reports what went, in the same shape as `stem cache`.
fn cache_prune(keep: usize) -> Action {
    match hypermixx_stems::prune_cache(keep) {
        Ok((removed, freed)) => {
            let (bytes, entries) = hypermixx_stems::cache_usage();
            Action::Message(format!(
                "stem cache: removed {removed} track(s), freed {:.1} MB; {entries} left, {:.1} MB in {}",
                freed as f64 / (1024.0 * 1024.0),
                bytes as f64 / (1024.0 * 1024.0),
                hypermixx_stems::stems_root().display()
            ))
        }
        Err(err) => Action::Failed(format!("stem cache: {err}")),
    }
}

/// The reference to the always-available help text.
fn stem_help_text() -> String {
    crate::response::stem_help_text()
}

/// Decodes on a worker thread, then hands the pipeline a ready source (+ optional constant grid).
pub(crate) fn spawn_load(deck_id: u8, path: String, bpm: Option<f32>, dispatcher: &Dispatcher) {
    let command_tx = dispatcher.command_tx.clone();
    let notices_tx = dispatcher.notices.clone();
    let events = dispatcher.events.clone();
    // The decoder resamples to the engine's rate, which is a live setting rather than a constant:
    // after this call nothing downstream has a sample-rate concept left.
    let sample_rate = dispatcher.live().audio.sample_rate;
    // Decoding a long track takes seconds; say so immediately so the user does not think the
    // command was ignored.
    notices::info(&notices_tx, format!("[load] deck{deck_id}: decoding {path}..."));
    std::thread::Builder::new()
        .name(format!("hypermixx-load-{deck_id}"))
        .spawn(move || match hypermixx_media::decode_file_at(&path, sample_rate) {
            Ok(decoded) => {
                let total_frames = decoded.total_frames;
                let analysis = bpm.filter(|b| *b > 0.0).map(|bpm| {
                    TrackAnalysis::from_grid(
                        hypermixx_core::BeatGrid::from_constant_bpm(
                            bpm,
                            0,
                            total_frames,
                            sample_rate,
                        ),
                        bpm,
                    )
                });
                if let Some(tap) = &events {
                    let _ = tap.send(UiEvent::Loaded {
                        deck_id,
                        path: path.clone(),
                    });
                    if let Some(analysis) = &analysis {
                        let _ = tap.send(UiEvent::Grid {
                            deck_id,
                            analysis: analysis.clone(),
                        });
                    }
                }
                let source: Shared = Arc::new(hypermixx_media::PcmPool::from_decoded(decoded));
                let _ = command_tx.send(Command::Load {
                    deck_id,
                    source,
                    analysis,
                });
            }
            Err(err) => notices::error(&notices_tx, format!("deck {deck_id}, {path}: {err}")),
        })
        .ok();
}

/// Runs the library analyser on a worker thread, then publishes a compiled grid.
fn spawn_analyse(deck_id: u8, source: Shared, backend: Backend, dispatcher: &Dispatcher) {
    let command_tx = dispatcher.command_tx.clone();
    let notices_tx = dispatcher.notices.clone();
    let events = dispatcher.events.clone();
    // The analyser's beat grid is in frames, so it needs the same rate the material was resampled
    // to — the live setting, not the default.
    let sample_rate = dispatcher.live().audio.sample_rate;
    std::thread::Builder::new()
        .name(format!("hypermixx-analyse-{deck_id}"))
        .spawn(move || {
            notices::info(&notices_tx, format!("[analyse] deck{deck_id}: running {backend:?}..."));
            match hypermixx_library::analyser::analyze(source, sample_rate, backend) {
                Ok(analysis) => {
                    let bpm = analysis.bpm();
                    let key = analysis
                        .key
                        .map(|k| k.traditional())
                        .unwrap_or_else(|| "--".into());
                    notices::info(&notices_tx, format!("[analyse] deck{deck_id}: {bpm:.1} BPM, {key}"));
                    if let Some(tap) = &events {
                        let _ = tap.send(UiEvent::Grid {
                            deck_id,
                            analysis: analysis.clone(),
                        });
                    }
                    let _ = command_tx.send(Command::SetAnalysis { deck_id, analysis });
                }
                Err(err) => notices::error(&notices_tx, format!("[analyse] deck{deck_id}: {err}")),
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use hypermixx_core::FaderTarget;

    /// The `fader` verb is a pure parse into `Command::SetFader`, so it can be tested without an
    /// engine: every stage must resolve, and a bad line must fail before it reaches the engine.
    #[test]
    fn the_fader_verb_addresses_every_stage() {
        let (tx, rx) = unbounded();
        let parse = |line: &str, target: Target| {
            let mut words = line.split_whitespace();
            fader_command(&mut words, target, &tx)
        };
        let expect = |expected: FaderTarget, value: f32| match rx.recv().unwrap() {
            Command::SetFader { target, value: got } => {
                assert_eq!(target, expected);
                assert_eq!(got, value);
            }
            other => panic!("expected SetFader, got {other:?}"),
        };

        assert!(matches!(parse("flow -0.5", Target::Deck(1)), Action::Continue));
        expect(FaderTarget::Flow(1), -0.5);
        assert!(matches!(parse("deck 0.25", Target::Deck(1)), Action::Continue));
        expect(FaderTarget::Deck(1), 0.25);
        assert!(matches!(parse("gain 1.0", Target::Deck(0)), Action::Continue));
        expect(FaderTarget::Gain(0), 1.0);
        assert!(matches!(parse("cuesend 0.9", Target::Deck(0)), Action::Continue));
        expect(FaderTarget::CueSend(0), 0.9);
        // The shared stages ignore the target: they are one physical control.
        for (line, expected) in [
            ("cross -1.0", FaderTarget::Crossfader),
            ("master 0.1", FaderTarget::Master),
            ("cue -0.2", FaderTarget::Cue),
        ] {
            assert!(matches!(parse(line, Target::Master), Action::Continue), "{line}");
            expect(expected, line.split_whitespace().nth(1).unwrap().parse().unwrap());
        }

        // A stage that needs a channel refuses `master` rather than silently addressing deck 0.
        assert!(matches!(parse("flow 0.1", Target::Master), Action::Failed(_)));
        // A mistyped stage or value is the user's mistake, reported before the engine sees it.
        assert!(matches!(parse("volume 0.1", Target::Deck(0)), Action::Failed(_)));
        assert!(matches!(parse("gain loud", Target::Deck(0)), Action::Failed(_)));
        assert!(matches!(parse("gain", Target::Deck(0)), Action::Failed(_)));
    }

    /// The live file is TOML with this machine's settings; a typo is an error, a wild value is
    /// clamped, and the shipped file parses.
    #[test]
    fn live_settings_parse_and_report() {
        let path = std::env::temp_dir().join("hypermixx-live-test.toml");
        std::fs::write(&path, "[audio]\nsample_rate = 48000\nring_frames = 99\n").unwrap();
        let (live, notes) = read_live_config(&path.to_string_lossy()).unwrap();
        assert_eq!(live.audio.sample_rate, 48_000, "a named value is honoured");
        assert_eq!(live.audio.ring_frames, hypermixx_core::live::MIN_RING_FRAMES, "clamped");
        assert!(!notes.is_empty(), "and the clamp is reported rather than silent");
        let _ = std::fs::remove_file(&path);

        // A typo is a failure, not a silent default: running at another rate than the file says is
        // exactly the surprise this file exists to prevent.
        std::fs::write(&path, "[audio]\nsample_rat = 48000\n").unwrap();
        assert!(read_live_config(&path.to_string_lossy()).is_err());
        let _ = std::fs::remove_file(&path);
        // A missing file is the caller's to handle (the CLI falls back to the built-ins).
        assert!(read_live_config("/nonexistent/live.toml").is_err());

        // The shipped file parses and is sane.
        let shipped = format!("{}/../../profile/live.toml", env!("CARGO_MANIFEST_DIR"));
        let (live, notes) = read_live_config(&shipped).expect("the shipped live file parses");
        assert!(notes.is_empty(), "the shipped file needs no clamping: {notes:?}");
        assert_eq!(live.audio.sample_rate, hypermixx_core::SAMPLE_RATE);
        assert!(
            matches!(live.stems.provider.as_str(), "cpu" | "cuda" | "gpu"),
            "the shipped provider is a known EP: {}",
            live.stems.provider
        );
    }

    /// `live` reports what is in effect, and a reload says when a setting needs a restart.
    #[test]
    fn the_live_verb_shows_and_reloads() {
        let (tx, _rx) = unbounded();
        let (notice_tx, _notice_rx) = unbounded();
        let dispatcher = Dispatcher::new(tx, notice_tx);
        dispatcher.with_live(
            hypermixx_core::LiveConfig::default(),
            Some("/tmp/x.toml".to_owned()),
        );
        // A bare `live` and `live show` both report.
        for line in ["", "show"] {
            match live_command(&mut line.split_whitespace(), &dispatcher) {
                Action::Message(text) => assert!(text.contains("44100Hz"), "{text}"),
                other => panic!("expected a report, got {other:?}"),
            }
        }
        // A path that is not there is a failure; a mistyped subcommand too.
        assert!(matches!(
            live_command(&mut "reload /nonexistent/live.toml".split_whitespace(), &dispatcher),
            Action::Failed(_)
        ));
        assert!(matches!(
            live_command(&mut "lst".split_whitespace(), &dispatcher),
            Action::Failed(_)
        ));
    }

    /// The grammars are one: every action a MIDI map can bind is also a session verb.
    ///
    /// The table below is the *counterpart* line for each registry action — a mapping that only one
    /// side understood would be a binding that silently does nothing, and this is what catches it.
    /// A new action forces a decision here rather than passing unnoticed.
    #[test]
    fn every_registry_action_is_a_session_verb() {
        // The verb, plus whatever a session line has to spell out that a controller supplies
        // (a value, a slot, a duration).
        let counterpart = |name: &str| -> Option<&'static str> {
            Some(match name {
                "play" => "play",
                "cue" => "cue",
                "vinyl" => "vinyl",
                "beatjump" => "beatjump 4",
                "fader flow" => "fader flow -0.5",
                "vocals fader" => "vocals fader -0.5",
                "vocals mute" => "vocals mute on",
                "vocals solo" => "vocals solo on",
                "fader deck" => "fader deck -0.5",
                "fader gain" => "fader gain 1.0",
                "fader cuesend" => "fader cuesend 0.5",
                "fader cross" => "fader cross -1.0",
                "fader master" => "fader master 0",
                "fader cue" => "fader cue 0",
                "tempofader" => "tempofader 0.5",
                "nudge" => "nudge 0.04 0.5",
                "loop in" => "loop in",
                "loop out" => "loop out",
                "loop exit" => "loop exit",
                "loop cancel" => "loop cancel",
                "loop halve" => "loop halve",
                "loop double" => "loop double",
                "loop" => "loop 4",
                "sync tempo" => "sync tempo",
                "sync phase" => "sync phase pid",
                "sync tempolock" => "sync tempolock",
                "sync phaselock" => "sync phaselock pid",
                "sync set-leader" => "sync set-leader",
                "sync unlock" => "sync unlock",
                "fx on" => "fx on eq",
                "fx off" => "fx off eq",
                "fx toggle" => "fx toggle eq",
                "fx pad" => "fx pad eq press",
                "fx trigger" => "fx trigger eq",
                // The one place the grammars differ in *shape*: a session line carries the value
                // (`fx set <slot> <param> <value>`) while a controller's value is the message, so the
                // action names the parameter only. Same addressing, same parameter name.
                "fx param" => "fx set eq low 0.5",
                _ => return None,
            })
        };

        // The reference topology with no outputs: a test must not open a sound card, and `eq`/`filter`
        // exist so the `fx` verbs have slots to name.
        let cfg = hypermixx_audio::MixerConfig {
            outputs: vec![],
            ..hypermixx_audio::simple_dj()
        };
        let pipeline = hypermixx_audio::AudioPipeline::start(cfg).expect("a headless engine starts");
        let (notice_tx, _notice_rx) = unbounded();
        let dispatcher = Dispatcher::new(pipeline.command_tx(), notice_tx);
        // `fx …` addresses slots by *name*, which needs the chain lists the front-end normally
        // primes at startup.
        dispatcher.prime(1, pipeline.response_rx());
        for spec in hypermixx_core::ACTIONS {
            let Some(line) = counterpart(spec.name) else {
                panic!("no session counterpart for `{}`", spec.name);
            };
            let full = format!("deck0 {line}");
            if let Action::Failed(message) = dispatch(&full, &dispatcher, &pipeline, Backend::Auto, 2, 0)
            {
                panic!("`{full}` failed: {message}");
            }
        }
    }

    /// `profile show` names what this session runs with, and `profile reload` switches the parts of
    /// it that can change under a running engine (the topology and the live settings — MIDI is
    /// attached at launch, and the reply says so).
    #[test]
    fn the_profile_family_reports_and_switches() {
        let (tx, _rx) = unbounded();
        let dispatcher = Dispatcher::new(tx, crossbeam_channel::unbounded().0);
        dispatcher.with_topology(Some("profile/mixer.toml".to_owned()));
        dispatcher.with_live(LiveConfig::default(), Some("profile/live.toml".to_owned()));
        dispatcher.with_profile(Some("profile/dj.toml".to_owned()));

        // `show` (and a bare `profile`) reports the profile and the files it resolved to.
        for line in ["", "show"] {
            match profile_command(&mut line.split_whitespace(), &dispatcher) {
                Action::Message(text) => {
                    assert!(text.contains("profile/dj.toml"), "{text}");
                    assert!(text.contains("profile/mixer.toml"), "{text}");
                    assert!(text.contains("profile/live.toml"), "{text}");
                    assert!(text.contains("model htdemucs"), "{text}");
                    assert!(text.to_lowercase().contains("midi"), "{text}");
                }
                other => panic!("expected a report, got {other:?}"),
            }
        }
        // With no profile the report says so rather than looking broken.
        let bare = Dispatcher::new(crossbeam_channel::unbounded().0, crossbeam_channel::unbounded().0);
        match profile_command(&mut "".split_whitespace(), &bare) {
            Action::Message(text) => assert!(text.contains("none"), "{text}"),
            other => panic!("expected a report, got {other:?}"),
        }

        // A profile that names a topology: reload sends the file's *text* to the engine, and the
        // profile becomes the current one.
        let dir = std::env::temp_dir().join("hypermixx-profile-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("club.toml"), "[[channel]]\nside = \"center\"\n").unwrap();
        let profile_file = dir.join("dj.toml");
        std::fs::write(&profile_file, "mixer = \"club.toml\"\n").unwrap();
        let (tx, rx) = unbounded();
        let dispatcher = Dispatcher::new(tx, crossbeam_channel::unbounded().0);
        let line = format!("reload {}", profile_file.display());
        match profile_command(&mut line.split_whitespace(), &dispatcher) {
            Action::Message(text) => assert!(text.contains("club.toml"), "{text}"),
            other => panic!("expected a report, got {other:?}"),
        }
        match rx.recv().unwrap() {
            Command::MixerReload { text } => assert!(text.contains("center"), "{text}"),
            other => panic!("expected MixerReload, got {other:?}"),
        }
        assert_eq!(dispatcher.profile_path().as_deref(), Some(profile_file.to_string_lossy().as_ref()));
        // The report follows the switch: the topology slot is the one the session is *running*, not
        // the one it started with (a stale slot would make `profile show` lie).
        assert_eq!(
            dispatcher.topology_path(),
            dir.join("club.toml").to_string_lossy(),
            "the reload must record the topology it just loaded"
        );
        // A key the profile does not name falls back to the folder default — a reload is "start as
        // if with `--profile X`", not a patch over whatever was running.
        let bare_profile = dir.join("bare.toml");
        std::fs::write(&bare_profile, "[preference]\n").unwrap();
        let line = format!("reload {}", bare_profile.display());
        dispatcher.with_topology(Some("stale/other.toml".to_owned()));
        match profile_command(&mut line.split_whitespace(), &dispatcher) {
            Action::Message(text) => {
                assert!(text.contains(crate::profile::DEFAULT_MIXER), "{text}");
                assert!(text.contains("topology:"), "a failure is reported, not hidden: {text}");
            }
            other => panic!("expected a report, got {other:?}"),
        }
        // The default file is not there in this test's working directory, so nothing was loaded —
        // and the slot says exactly that rather than naming a file the session is not running.
        assert_eq!(
            dispatcher.topology_path(),
            "stale/other.toml",
            "a failed load must not be recorded as the running topology"
        );

        // A profile that is not there is a failure, and a mistyped subcommand too.
        assert!(matches!(
            profile_command(&mut "reload /nonexistent/p.toml".split_whitespace(), &dispatcher),
            Action::Failed(_)
        ));
        assert!(matches!(
            profile_command(&mut "swtich".split_whitespace(), &dispatcher),
            Action::Failed(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `mixer reload` reads a file here and sends its *text*: file IO stays out of the engine, and
    /// an unreadable file never reaches it.
    #[test]
    fn the_mixer_verb_sends_the_files_text() {
        let (tx, _rx) = unbounded();
        let dispatcher = Dispatcher::new(tx, crossbeam_channel::unbounded().0);
        // No subcommand is a usage message, not a silent no-op.
        assert!(matches!(mixer_command(&mut "".split_whitespace(), &dispatcher), Action::Message(_)));
        assert!(matches!(
            mixer_command(&mut "nope".split_whitespace(), &dispatcher),
            Action::Failed(_)
        ));
        // A path that does not exist is the front-end's failure, reported as such.
        assert!(matches!(
            mixer_command(&mut "reload /nonexistent/mixer.toml".split_whitespace(), &dispatcher),
            Action::Failed(_)
        ));
        // And the file's text travels as-is.
        let path = std::env::temp_dir().join("hypermixx-cli-reload-test.toml");
        std::fs::write(&path, "[[channel]]\nside = \"left\"\n").unwrap();
        let (tx, rx) = unbounded();
        let dispatcher = Dispatcher::new(tx, crossbeam_channel::unbounded().0);
        let line = format!("reload {}", path.display());
        assert!(matches!(
            mixer_command(&mut line.split_whitespace(), &dispatcher),
            Action::Continue
        ));
        match rx.recv().unwrap() {
            Command::MixerReload { text } => assert!(text.contains("side = \"left\"")),
            other => panic!("expected MixerReload, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// `output` is a global verb: `list` (the default) and `refresh`.
    #[test]
    fn the_output_verb_lists_and_refreshes() {
        let (tx, rx) = unbounded();
        for (line, expected) in [("list", OutputOp::List), ("refresh", OutputOp::Refresh)] {
            let mut words = line.split_whitespace();
            assert!(matches!(output_command(&mut words, &tx), Action::Continue), "{line}");
            match rx.recv().unwrap() {
                Command::Output { op } => assert_eq!(op, expected),
                other => panic!("expected Output, got {other:?}"),
            }
        }
        // Bare `output` lists, like `state` with no argument prints everything.
        let mut words = "".split_whitespace();
        assert!(matches!(output_command(&mut words, &tx), Action::Continue));
        match rx.recv().unwrap() {
            Command::Output { op: OutputOp::List } => {}
            other => panic!("expected a list, got {other:?}"),
        }
        // A mistyped subcommand is the user's mistake, reported before the engine sees it.
        let mut words = "opne".split_whitespace();
        assert!(matches!(output_command(&mut words, &tx), Action::Failed(_)));
    }
}
