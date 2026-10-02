//! Line parsing and dispatch, shared by the REPL and the TUI.
//!
//! Syntax is target-first: the target is the focused deck in the TUI (shown as the command box
//! prefix), and can be overridden per line with `deck0` / `0` / `master`. So in the TUI `play`
//! means "play the focused deck" while `deck1 play` or `master fx list` address something else.
//! The engine's [`Command`] protocol is unchanged — this is purely the front-end's grammar.
//!
//! `load` decodes on a worker thread and hands the pipeline a ready source; `analyse` runs the
//! library analyser and publishes a compiled grid. Nothing here blocks the audio path.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use hypermixx_audio::fx::FxKind;
use hypermixx_audio::{AudioPipeline, Command, SAMPLE_RATE};
use hypermixx_core::{
    Backend, CommandResponse, CueOp, FxChainId, FxSlotRef, KeylockMode, LoopEditOp, LoopOp,
    LoopQuantum, NudgeOp, PhaseMode, Shared, Stem, StemOp, StemPreset, SyncOp, TrackAnalysis,
};

use crate::notices::{self, NoticeTx};
use crate::response::{fx_help_text, help_text};

/// Result of interpreting one input line.
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
            slots: Arc::new(SlotBook::default()),
            separations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Also forwards front-end events (deck name, compiled grid) to `events`.
    pub fn tap_events(mut self, events: Sender<UiEvent>) -> Self {
        self.events = Some(events);
        self
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
                None => Action::Failed("usage: [deck] keylock on|off|wide".into()),
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
        "sync" => with_deck(target, "sync", |deck_id| {
            sync_command(&mut words, deck_id, command_tx)
        }),
        "nudge" => with_deck(target, "nudge", |deck_id| {
            nudge_command(&mut words, deck_id, command_tx)
        }),
        "fx" => fx(&mut words, dispatcher, target),
        "state" => send(command_tx, Command::GetAllStates),
        "midi" => midi_command(&mut words),
        "help" | "h" | "?" => Action::Message(help_text(decks)),
        "quit" | "exit" => Action::Quit,
        other => Action::Failed(format!("unknown command `{other}` — `help` lists them")),
    }
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
        None => CueOp::Smart,
        Some("play") => CueOp::Play,
        Some("back") => CueOp::Back,
        Some("set") => CueOp::Set,
        // The press edge of a held cue button (a MIDI button's note-on): cue the playhead if the
        // deck is paused, then play from the cue point.
        Some("hold") => CueOp::SmartPlay,
        Some(other) => {
            return Action::Failed(format!(
                "unknown cue subcommand `{other}` — play|back|set|hold (or bare `cue`)"
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
    let usage = "usage: [deck] loop in | out | <beats> | exit | cancel | halve | double | edit <len|move|in|out> <beats> | quantum <beat|half|quarter|eighth>";
    let Some(sub) = words.next() else {
        return Action::Message(usage.into());
    };
    let op = match sub {
        "in" => LoopOp::In,
        "out" => LoopOp::Out,
        "cancel" => LoopOp::Cancel,
        "exit" | "off" => LoopOp::Exit,
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
            let mut options = hypermixx_stems::SeparateOptions::default();
            while let Some(flag) = words.next() {
                match flag {
                    // Every option is refused rather than clamped: a silent clamp makes `--shifts 8`
                    // look like it did something it did not.
                    "--shifts" => match words.next().and_then(|v| v.parse::<usize>().ok()) {
                        Some(value) if value <= hypermixx_stems::MAX_SHIFTS => {
                            options.shifts = value
                        }
                        Some(value) => {
                            return Action::Failed(format!(
                                "--shifts {value} is past the useful range (0..={}); 0 and 1 are the \
                                 same single pass, 2 averages a second run for ~2× the time",
                                hypermixx_stems::MAX_SHIFTS
                            ))
                        }
                        None => return Action::Failed("--shifts needs a number".into()),
                    },
                    "--overlap" => match words.next().and_then(|v| v.parse::<f32>().ok()) {
                        Some(value)
                            if value.is_finite() && (0.0..=hypermixx_stems::MAX_OVERLAP).contains(&value) =>
                        {
                            options.overlap = value
                        }
                        Some(value) => {
                            return Action::Failed(format!(
                                "--overlap {value} is outside 0.0..={}; 0.0 is 24% faster than the \
                                 0.25 default and weights window edges more",
                                hypermixx_stems::MAX_OVERLAP
                            ))
                        }
                        None => return Action::Failed("--overlap needs a number".into()),
                    },
                    "--gpu" => options.provider = hypermixx_stems::Provider::Cuda,
                    "--provider" => match words.next().and_then(hypermixx_stems::Provider::parse) {
                        Some(provider) => options.provider = provider,
                        None => {
                            return Action::Failed(
                                "--provider needs cpu or cuda (--gpu is short for the latter)".into(),
                            )
                        }
                    },
                    other => {
                        return Action::Failed(format!(
                            "unknown stem separate option `{other}` \
                             (--shifts <n> | --overlap <x> | --gpu)"
                        ))
                    }
                }
            }
            // A build without the feature must say so *before* fetching a model and starting an
            // 80-second job, not after.
            if !hypermixx_stems::provider_available(options.provider) {
                return Action::Failed(format!(
                    "this build has no {} support (rebuild with --features cuda)",
                    options.provider.label()
                ));
            }
            let Some(source) = pipeline.deck_source(deck_id) else {
                return Action::Failed(format!("deck {deck_id} holds no track to separate"));
            };
            spawn_separate(deck_id, source, options, dispatcher);
            Action::Continue
        }
        "status" => send(command_tx, Command::GetStemState { deck_id }),
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
    source: Shared,
    options: hypermixx_stems::SeparateOptions,
    dispatcher: &Dispatcher,
) {
    let command_tx = dispatcher.command_tx.clone();
    let notices_tx = dispatcher.notices.clone();
    let cancellations = Arc::clone(&dispatcher.separations);
    std::thread::Builder::new()
        .name(format!("hypermixx-stem-{deck_id}"))
        .spawn(move || {
            let progress = {
                let notices_tx = notices_tx.clone();
                // Report in 10% steps: one notice per model window would flood the log.
                // An atomic rather than a captured `mut`: the sink must be `Fn`, and the model
                // reports from its own thread.
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
            notices::info(
                &notices_tx,
                format!("[stem] deck{deck_id}: preparing the separator (may fetch a model)..."),
            );
            let separator = match hypermixx_stems::default_separator(options, &progress) {
                Ok(separator) => separator,
                Err(err) => {
                    notices::error(&notices_tx, format!("[stem] deck{deck_id}: {err}"));
                    return;
                }
            };
            // The id names the model, shifts, overlap and provider — the four things that decide
            // which audio this is, and which cache entry it will land in.
            notices::info(
                &notices_tx,
                format!(
                    "[stem] deck{deck_id}: {} (shifts {}, overlap {}, {})",
                    separator.id(),
                    options.shifts,
                    options.overlap,
                    options.provider.label()
                ),
            );
            let cancelled = hypermixx_stems::Cancelled::new();
            if let Ok(mut running) = cancellations.lock() {
                running.insert(deck_id, cancelled.clone());
            }
            let started = std::time::Instant::now();
            match hypermixx_stems::separate_cached(
                separator.as_ref(),
                source.as_ref(),
                &progress,
                &cancelled,
            ) {
                Ok(stems) => {
                    let frames = stems.total_frames();
                    let seconds = started.elapsed().as_secs_f32();
                    notices::info(
                        &notices_tx,
                        format!(
                            "[stem] deck{deck_id}: {frames} frames in {seconds:.0}s — installing"
                        ),
                    );
                    let _ = command_tx.send(Command::SetStems { deck_id, stems });
                }
                Err(err) => {
                    if err == hypermixx_stems::StemError::Cancelled {
                        notices::info(&notices_tx, format!("[stem] deck{deck_id}: cancelled"));
                    } else {
                        notices::error(&notices_tx, format!("[stem] deck{deck_id}: {err}"));
                    }
                }
            }
            if let Ok(mut running) = cancellations.lock() {
                running.remove(&deck_id);
            }
        })
        .ok();
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
    // Decoding a long track takes seconds; say so immediately so the user does not think the
    // command was ignored.
    notices::info(&notices_tx, format!("[load] deck{deck_id}: decoding {path}..."));
    std::thread::Builder::new()
        .name(format!("hypermixx-load-{deck_id}"))
        .spawn(move || match hypermixx_media::decode_file(&path) {
            Ok(decoded) => {
                let total_frames = decoded.total_frames;
                let analysis = bpm.filter(|b| *b > 0.0).map(|bpm| {
                    TrackAnalysis::from_grid(
                        hypermixx_core::BeatGrid::from_constant_bpm(
                            bpm,
                            0,
                            total_frames,
                            SAMPLE_RATE,
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
    std::thread::Builder::new()
        .name(format!("hypermixx-analyse-{deck_id}"))
        .spawn(move || {
            notices::info(&notices_tx, format!("[analyse] deck{deck_id}: running {backend:?}..."));
            match hypermixx_library::analyser::analyze(source, SAMPLE_RATE, backend) {
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
