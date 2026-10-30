//! MIDI input wiring for the CLI.
//!
//! Two threads sit between a controller and the engine:
//!
//! ```text
//! midir callback (owned by hypermixx_midi::ports)
//!   └─ Received ─► translator thread (this module) ─► Router ─► MergeBuffer ─► command_tx
//!                       ▲
//!                       └─ RouterCommand (mode changes from the UI)
//! ```
//!
//! The translator is where the pure [`hypermixx_midi`] router meets the live engine: it owns the
//! [`Router`], publishes mode snapshots for the TUI, and coalesces a fast sweep's intermediate
//! values before they reach the producer. Keeping this here means the midi crate stays
//! engine-agnostic (it only knows `Command`), and the CLI owns thread lifetime.
//!
//! FX bindings are resolved from *names* to slot indices before the translator starts, by asking
//! the engine with `ListFx`. A binding whose slot cannot be found is skipped rather than sent as a
//! bogus index.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use hypermixx_core::{BrowserAction, Command, CommandResponse, FxChainId};
use hypermixx_midi::map::Action;
use hypermixx_midi::{
    ports, Input, Map, MergeBuffer, Received, Router, RouterCommand, RouterState,
};

use crate::notices::{self, NoticeTx};

/// How long continuous commands are coalesced before they are flushed. The producer applies
/// commands once per ~5.8 ms block, so a 1 ms flush still lands a sweep's final value long before
/// the next block while shedding the intermediate ones.
const FLUSH_EVERY: Duration = Duration::from_millis(1);

/// How long to wait for each `ListFx` answer at startup. A dead engine must not hang the launch.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);

/// An open MIDI input and the thread translating it.
///
/// Dropping it closes the port, which ends the backend callback and closes the event channel; the
/// translator then sees its receive disconnect and finishes. The port is deliberately dropped
/// **before** the translator is joined, so the join can never wait on a live input.
pub struct MidiSession {
    input: Option<Input>,
    translator: Option<JoinHandle<()>>,
    control: Sender<RouterCommand>,
    state: Arc<Mutex<RouterState>>,
}

impl Drop for MidiSession {
    fn drop(&mut self) {
        self.input.take();
        if let Some(translator) = self.translator.take() {
            let _ = translator.join();
        }
    }
}

impl MidiSession {
    /// A sender for mode changes. Cloned by the dispatcher so `midi mode` can reach the router.
    pub fn control(&self) -> Sender<RouterCommand> {
        self.control.clone()
    }

    /// The latest mode snapshot. The TUI reads it on every draw; a missing lock just means a
    /// frame is skipped.
    pub fn state(&self) -> Arc<Mutex<RouterState>> {
        self.state.clone()
    }
}

/// Reads and parses a map file. FX-name resolution is the caller's job (see [`resolve_with`]).
pub fn read_map(map_path: &str) -> Result<Map, String> {
    let text = std::fs::read_to_string(map_path).map_err(|err| format!("{map_path}: {err}"))?;
    let base = Path::new(map_path)
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Map::from_toml_str_at(&text, base).map_err(|err| format!("{map_path}: {err}"))
}

/// Opens `port` and starts translating `map`.
///
/// The map must already have its FX names resolved (see [`resolve_with`]); an unresolved binding is
/// simply skipped by the translator, never sent as a bogus index.
///
/// A bad port or a failed thread spawn is a clean error: the caller exits without a half-wired MIDI
/// layer.
pub fn attach(
    port: &str,
    map: Map,
    command_tx: Sender<Command>,
    browser_tx: Option<Sender<BrowserAction>>,
) -> Result<MidiSession, String> {
    let (received_tx, received_rx) = crossbeam_channel::unbounded();
    let (control_tx, control_rx) = crossbeam_channel::unbounded();
    let router = Router::new(map);
    let state = Arc::new(Mutex::new(router.snapshot()));
    let input = ports::open(port, received_tx).map_err(|err| format!("midi: {err}"))?;
    let translator_command_tx = command_tx.clone();
    let translator_state = state.clone();
    let translator = std::thread::Builder::new()
        .name("midi-translate".to_owned())
        .spawn(move || {
            pump(
                received_rx,
                control_rx,
                router,
                translator_command_tx,
                browser_tx,
                translator_state,
            )
        })
        .map_err(|err| format!("midi thread: {err}"))?;
    Ok(MidiSession {
        input: Some(input),
        translator: Some(translator),
        control: control_tx,
        state,
    })
}

/// The REPL's convenience path: read, resolve against the engine, and attach.
///
/// Safe only because the REPL attaches **before** its printer thread exists, so this is the sole
/// reader of `responses`. The TUI resolves from its primed slot book instead (see [`resolve_with`]).
pub fn attach_file(
    port: &str,
    map_path: &str,
    decks: usize,
    command_tx: Sender<Command>,
    responses: &Receiver<CommandResponse>,
    notices: &NoticeTx,
) -> Result<MidiSession, String> {
    let mut map = read_map(map_path)?;
    let unresolved = resolve_via_engine(&mut map, decks, &command_tx, responses);
    report_unresolved(notices, unresolved);
    let session = attach(port, map, command_tx, None)?;
    notices::info(
        notices,
        format!("[midi] listening on `{port}` (map {map_path})"),
    );
    Ok(session)
}

/// Reports unresolved FX bindings as info lines (they are disabled, not fatal).
pub fn report_unresolved(notices: &NoticeTx, unresolved: Vec<String>) {
    for name in unresolved {
        notices::info(
            notices,
            format!("[midi] no FX slot `{name}` (binding disabled)"),
        );
    }
}

/// Drains events and front-end commands, forwarding translated commands on a 1 ms cadence.
fn pump(
    received: Receiver<Received>,
    control: Receiver<RouterCommand>,
    mut router: Router,
    command_tx: Sender<Command>,
    browser_tx: Option<Sender<BrowserAction>>,
    state: Arc<Mutex<RouterState>>,
) {
    let mut merge = MergeBuffer::new();
    let mut last_flush = Instant::now();
    loop {
        crossbeam_channel::select! {
            recv(received) -> message => match message {
                Ok(received) => {
                    let feed = router.feed(&received.event);
                    for command in feed.commands {
                        merge.push(command);
                    }
                    if let Some(browser_tx) = &browser_tx {
                        for action in feed.browser {
                            let _ = browser_tx.send(action);
                        }
                    }
                    if feed.state_changed {
                        publish(&state, &router);
                    }
                    if last_flush.elapsed() >= FLUSH_EVERY {
                        flush(&mut merge, &command_tx);
                        last_flush = Instant::now();
                    }
                }
                Err(_) => {
                    flush(&mut merge, &command_tx);
                    return;
                }
            },
            recv(control) -> message => match message {
                Ok(command) => {
                    let changed = router.command(command).unwrap_or(false);
                    if changed {
                        publish(&state, &router);
                    }
                    // A mode switch can change which target a held sweep belongs to; flush what
                    // was already valid before the new mode's events arrive.
                    flush(&mut merge, &command_tx);
                    last_flush = Instant::now();
                }
                // The dispatcher is gone (the front-end is shutting down). Leaving a disconnected
                // channel in the select would spin; stop the translator.
                Err(_) => {
                    flush(&mut merge, &command_tx);
                    return;
                }
            },
            default(FLUSH_EVERY) => {
                flush(&mut merge, &command_tx);
                last_flush = Instant::now();
            }
        }
    }
}

fn publish(state: &Arc<Mutex<RouterState>>, router: &Router) {
    if let Ok(mut slot) = state.lock() {
        *slot = router.snapshot();
    }
}

fn flush(merge: &mut MergeBuffer, command_tx: &Sender<Command>) {
    for command in merge.flush() {
        // A closed channel means the engine is already gone; the front-end is shutting down.
        if command_tx.send(command).is_err() {
            return;
        }
    }
}

/// Fills every `chain`+`fx` binding from a lookup (the CLI's slot book, a test table, ...).
///
/// Engine-free on purpose: the TUI calls this with its already-primed slot book, so attaching MIDI
/// mid-session never reads the engine's response channel and can never swallow a `Loaded`/`States`
/// answer meant for the UI. Returns the names that could not be resolved.
pub fn resolve_with(
    map: &mut Map,
    lookup: impl Fn(FxChainId, &str) -> Option<usize>,
) -> Vec<String> {
    let mut wanted: Vec<(FxChainId, String)> = Vec::new();
    for unit in &map.units {
        for mode in &unit.modes {
            for bind in mode.binds.iter().flatten() {
                for action in [&bind.actions.main, &bind.actions.press, &bind.actions.turn]
                    .into_iter()
                    .flatten()
                {
                    if let Action::Fx(fx_action) = action {
                        let pair = {
                            let (chain, fx) = fx_action.address();
                            (chain, fx.to_owned())
                        };
                        if !wanted.contains(&pair) {
                            wanted.push(pair);
                        }
                    }
                }
            }
        }
    }
    let mut unresolved = Vec::new();
    for (chain, name) in wanted {
        match lookup(chain, &name) {
            Some(index) => map.resolve_fx(chain, name, index),
            None => unresolved.push(format!("{}/{}", chain.label(), name)),
        }
    }
    unresolved
}

/// Resolves FX names by asking the engine, one `ListFx` per chain.
///
/// **The caller must be the only reader of `responses`** (startup, before any front-end loop or
/// printer exists): a response meant for someone else would be discarded here. This is why the TUI
/// uses [`resolve_with`] against its slot book rather than this function.
pub fn resolve_via_engine(
    map: &mut Map,
    decks: usize,
    command_tx: &Sender<Command>,
    responses: &Receiver<CommandResponse>,
) -> Vec<String> {
    let chains: Vec<FxChainId> = (0..decks as u8)
        .map(FxChainId::Deck)
        .chain(std::iter::once(FxChainId::Master))
        .collect();
    let mut table: Vec<(FxChainId, Vec<String>)> = Vec::new();
    for chain in chains {
        if command_tx.send(Command::ListFx { chain }).is_err() {
            break;
        }
        if let Ok(CommandResponse::FxListed { slots, .. }) = responses.recv_timeout(RESOLVE_TIMEOUT)
        {
            table.push((chain, slots.into_iter().map(|slot| slot.kind).collect()));
        }
    }
    resolve_with(map, |chain, name| {
        table
            .iter()
            .find(|(candidate, _)| *candidate == chain)
            .and_then(|(_, kinds)| {
                kinds
                    .iter()
                    .position(|kind| kind.eq_ignore_ascii_case(name))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_midi::{Event, Received};
    use std::time::SystemTime;

    /// The CLI's pump, fed synthetic events, must turn a bound pad into an engine command. This is
    /// the seam the unit tests in `hypermixx-midi` cannot cover: the translator's thread, its
    /// coalescing cadence and the command channel.
    #[test]
    fn the_translator_forwards_a_bound_pad_as_a_command() {
        let map = Map::from_toml_str(
            r#"
[[unit]]
id = "deck1.pad"
deck = 1

[[unit.control]]
slot = "pad"
type = "Button"
endpoint = { midi = "note", channel = 0, id = 48 }

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "pad"
action = "play"
"#,
        )
        .unwrap();
        let (received_tx, received_rx) = crossbeam_channel::unbounded();
        let (_control_tx, control_rx) = crossbeam_channel::unbounded();
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let router = Router::new(map);
        let state = Arc::new(Mutex::new(router.snapshot()));
        let translator = std::thread::spawn(move || {
            pump(received_rx, control_rx, router, command_tx, None, state)
        });

        let at = SystemTime::now();
        received_tx
            .send(Received {
                at,
                event: Event::NoteOn {
                    channel: 0,
                    key: 48,
                    velocity: 100,
                },
            })
            .unwrap();

        let command = command_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the translator should flush a command");
        assert!(matches!(command, Command::TogglePlay { deck_id: 0 }));

        // Dropping the event sender closes the channel; the translator must exit rather than hang.
        drop(received_tx);
        translator.join().unwrap();
    }

    /// `resolve_with` is the engine-free path the TUI uses; it must walk every mode's bindings, fill
    /// names from a lookup and report the ones it could not find.
    #[test]
    fn resolve_with_fills_names_and_reports_misses() {
        let mut map = Map::from_toml_str(
            r#"
[[unit]]
id = "deck1.mixer"
deck = 1

[[unit.control]]
slot = "low"
type = "Knob"
endpoint = { midi = "cc", channel = 0, id = 20 }

[[unit.control]]
slot = "mid"
type = "Knob"
endpoint = { midi = "cc", channel = 0, id = 21 }

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "low"
action = "fx param value"
fx = "filter"
[[unit.mode.bind]]
slot = "mid"
action = "fx param value"
fx = "missing"
"#,
        )
        .unwrap();
        let chain = hypermixx_core::FxChainId::Deck(0);
        let unresolved = resolve_with(&mut map, |candidate, name| {
            (candidate == chain && name == "filter").then_some(2)
        });
        assert_eq!(map.fx_slot(chain, "filter"), Some(2));
        assert_eq!(unresolved, vec!["deck1/missing".to_owned()]);
    }
}
