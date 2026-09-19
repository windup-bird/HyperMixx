//! Command line front-end for the Hypermixx audio engine.
//!
//! The CLI owns file IO and analysis: `load` decodes on a worker thread and hands the pipeline a
//! ready source; `analyse` runs the library analyser and publishes a compiled grid. The pipeline
//! only ever receives executable commands — including the `fx` family, which the producer thread
//! applies to the mixer's chains at a block boundary. A dedicated printer thread renders responses
//! as they arrive, so nothing here blocks on the engine.

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use crossbeam_channel::Sender;
use hypermixx_audio::fx::FxKind;
use hypermixx_audio::{
    reference_toml, simple_dj, AudioPipeline, Command, CommandResponse, MixerConfig, DECK_COUNT,
    SAMPLE_RATE,
};
use hypermixx_core::{Backend, FxChainId, FxSlotRef, Shared};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args = CliArgs::parse();
    if args.print_config {
        print!("{}", reference_toml());
        return;
    }
    let backend = args.backend;
    // `--config` overrides the built-in reference topology; a bad file is a clean exit, not a
    // half-alive engine (and the mixer's own validation — unknown FX, empty channels — runs
    // inside `start`, on the thread that will own the result).
    let cfg = match &args.config {
        Some(path) => match read_config(path) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!("error: {path}: {err}");
                std::process::exit(1);
            }
        },
        None => simple_dj(),
    };
    // The reference topology: two decks (EQ + filter each), a limiting master, main + headphones.
    // `start` builds the mixer on the producer thread (cpal streams are !Send) and reports a bad
    // config back synchronously, so a failure here is a clean exit, not a half-alive engine.
    let pipeline = match AudioPipeline::start(cfg) {
        Ok(pipeline) => pipeline,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    let command_tx = pipeline.command_tx();
    // A custom topology may name any number of channels, so the deck-id range comes from the
    // engine rather than the built-in constant.
    let decks = pipeline.channel_count().unwrap_or(DECK_COUNT).max(1);

    let config_note = args
        .config
        .as_deref()
        .map(|path| format!(", config {path}"))
        .unwrap_or_default();
    println!(
        "hypermixx {VERSION} — {decks} decks, {SAMPLE_RATE}Hz stereo, backend {backend:?}{config_note}. \
         `help` for commands, `quit` to exit."
    );

    // Printer thread: render every engine response as it lands, then end when the channel closes
    // (the producer thread dropping its reply sender is what ends a session). Scoped, because the
    // response channel belongs to the pipeline and cannot be moved out of it.
    std::thread::scope(|scope| {
        let printer = scope.spawn(|| {
            while let Ok(response) = pipeline.response_rx().recv() {
                print_response(&response);
            }
        });

        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        // `read_line` returns None on EOF / Ctrl-D, which ends the session.
        while let Some(line) = read_line(&mut stdin) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match dispatch(line, &command_tx, &pipeline, backend, decks) {
                Action::Continue => {}
                Action::Quit => break,
                Action::Bad(message) => eprintln!("error: {message}"),
            }
        }

        // However the loop ended, tell the engine to stop; its reply (and the channel closing)
        // is what lets the printer finish, and dropping the pipeline joins the producer.
        let _ = command_tx.send(Command::Quit);
        drop(command_tx);
        let _ = printer.join();
    });
}

/// Result of interpreting one input line.
enum Action {
    Continue,
    Quit,
    Bad(String),
}

/// Turns a line into engine effects: an immediate command, or a background decode/analyse task.
fn dispatch(
    line: &str,
    command_tx: &Sender<Command>,
    pipeline: &AudioPipeline,
    backend: Backend,
    decks: usize,
) -> Action {
    let mut words = line.split_whitespace();
    let head = words.next().unwrap_or_default();
    let mut deck = || deck_id(words.next(), decks);
    match head {
        "load" => {
            let deck_id = match deck() {
                Ok(id) => id,
                Err(e) => return Action::Bad(e),
            };
            let Some(path) = words.next() else {
                return Action::Bad("usage: load <deck> <path> [bpm]".into());
            };
            let bpm = match words.next() {
                Some(raw) => match raw.parse::<f32>() {
                    Ok(v) => Some(v),
                    Err(_) => return Action::Bad("bpm must be a number".into()),
                },
                None => None,
            };
            spawn_load(deck_id, path.to_owned(), bpm, command_tx.clone());
            Action::Continue
        }
        "analyse" | "analyze" => {
            let deck_id = match deck() {
                Ok(id) => id,
                Err(e) => return Action::Bad(e),
            };
            let Some(source) = pipeline.deck_source(deck_id) else {
                return Action::Bad(format!("deck {deck_id} holds no track to analyse"));
            };
            spawn_analyse(deck_id, source, backend, command_tx.clone());
            Action::Continue
        }
        "play" => forward(deck().map(|d| Command::Play { deck_id: d }), command_tx),
        "pause" => forward(deck().map(|d| Command::Pause { deck_id: d }), command_tx),
        "jump" => {
            let deck_id = match deck() {
                Ok(id) => id,
                Err(e) => return Action::Bad(e),
            };
            match words.next().map(str::parse::<u64>) {
                Some(Ok(target_frame)) => send(
                    command_tx,
                    Command::Jump {
                        deck_id,
                        target_frame,
                    },
                ),
                _ => Action::Bad("usage: jump <deck> <frame>".into()),
            }
        }
        "beatjump" => {
            let deck_id = match deck() {
                Ok(id) => id,
                Err(e) => return Action::Bad(e),
            };
            match words.next().map(str::parse::<i64>) {
                Some(Ok(beats)) => send(command_tx, Command::BeatJump { deck_id, beats }),
                _ => Action::Bad("usage: beatjump <deck> <beats>".into()),
            }
        }
        "rate" => {
            let deck_id = match deck() {
                Ok(id) => id,
                Err(e) => return Action::Bad(e),
            };
            match words.next().map(str::parse::<f32>) {
                Some(Ok(rate)) => send(command_tx, Command::SetRate { deck_id, rate }),
                _ => Action::Bad("usage: rate <deck> <ratio>".into()),
            }
        }
        "profile" => {
            let deck_id = match deck() {
                Ok(id) => id,
                Err(e) => return Action::Bad(e),
            };
            match words.next() {
                Some(profile) => send(
                    command_tx,
                    Command::SetProfile {
                        deck_id,
                        profile: profile.to_owned(),
                    },
                ),
                None => Action::Bad("usage: profile <deck> <tape|keylock|wide>".into()),
            }
        }
        "state" => send(command_tx, Command::GetAllStates),
        "fx" => fx(&mut words, command_tx, decks),
        "help" | "h" | "?" => {
            print_help(decks);
            Action::Continue
        }
        "quit" | "exit" | "q" => Action::Quit,
        other => Action::Bad(format!("unknown command `{other}` — `help` lists them")),
    }
}

/// The `fx` family: `add / remove / list / set / on / off / trigger / pad / help`.
fn fx<'a>(
    mut words: impl Iterator<Item = &'a str>,
    command_tx: &Sender<Command>,
    decks: usize,
) -> Action {
    let usage = "usage: fx <add|remove|list|set|on|off|trigger|pad|help> ... (`fx help` for details)";
    let Some(sub) = words.next() else {
        print_fx_help();
        return Action::Continue;
    };
    match sub {
        "add" => {
            let chain = match next_chain(&mut words, decks) {
                Ok(c) => c,
                Err(e) => return Action::Bad(e),
            };
            let Some(kind) = words.next() else {
                return Action::Bad("usage: fx add <chain> <kind>".into());
            };
            // Validate the kind here so `fx help` can list what exists; unknown names get an error
            // now rather than a round-trip to the engine and back.
            if let Err(err) = FxKind::parse(kind) {
                return Action::Bad(err.message());
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
            let chain = match next_chain(&mut words, decks) {
                Ok(c) => c,
                Err(e) => return Action::Bad(e),
            };
            match next_index(&mut words) {
                Ok(index) => send(command_tx, Command::RemoveFx { chain, index }),
                Err(e) => Action::Bad(e),
            }
        }
        "list" | "ls" => match next_chain(&mut words, decks) {
            Ok(chain) => send(command_tx, Command::ListFx { chain }),
            Err(e) => Action::Bad(e),
        },
        "set" => {
            let chain = match next_chain(&mut words, decks) {
                Ok(c) => c,
                Err(e) => return Action::Bad(e),
            };
            let index = match next_index(&mut words) {
                Ok(i) => i,
                Err(e) => return Action::Bad(e),
            };
            let Some(param) = words.next() else {
                return Action::Bad("usage: fx set <chain> <index> <param> <value>".into());
            };
            let value = match words.next().map(str::parse::<f32>) {
                Some(Ok(v)) => v,
                _ => return Action::Bad("value must be a number".into()),
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
            let chain = match next_chain(&mut words, decks) {
                Ok(c) => c,
                Err(e) => return Action::Bad(e),
            };
            match next_index(&mut words) {
                Ok(index) => send(
                    command_tx,
                    Command::SetFxEnabled {
                        slot: FxSlotRef { chain, index },
                        enabled,
                    },
                ),
                Err(e) => Action::Bad(e),
            }
        }
        "trigger" => {
            let chain = match next_chain(&mut words, decks) {
                Ok(c) => c,
                Err(e) => return Action::Bad(e),
            };
            match next_index(&mut words) {
                Ok(index) => send(
                    command_tx,
                    Command::FxTrigger {
                        slot: FxSlotRef { chain, index },
                    },
                ),
                Err(e) => Action::Bad(e),
            }
        }
        "pad" => {
            let chain = match next_chain(&mut words, decks) {
                Ok(c) => c,
                Err(e) => return Action::Bad(e),
            };
            let index = match next_index(&mut words) {
                Ok(i) => i,
                Err(e) => return Action::Bad(e),
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
                _ => Action::Bad("usage: fx pad <chain> <index> <press|release>".into()),
            }
        }
        "help" | "h" => {
            print_fx_help();
            Action::Continue
        }
        other => Action::Bad(format!("unknown fx subcommand `{other}`; {usage}")),
    }
}

fn forward(built: Result<Command, String>, command_tx: &Sender<Command>) -> Action {
    match built {
        Ok(command) => send(command_tx, command),
        Err(message) => Action::Bad(message),
    }
}

fn send(command_tx: &Sender<Command>, command: Command) -> Action {
    if command_tx.send(command).is_err() {
        Action::Bad("the audio engine is gone".into())
    } else {
        Action::Continue
    }
}

fn deck_id(word: Option<&str>, decks: usize) -> Result<u8, String> {
    let last = (decks - 1) as u8;
    let raw = word.ok_or_else(|| format!("missing deck id, expected 0..{last}"))?;
    let id: u8 = raw
        .parse()
        .map_err(|_| format!("deck id must be a number in 0..{last}, got `{raw}`"))?;
    if (id as usize) < decks {
        Ok(id)
    } else {
        Err(format!("unknown deck {id}, valid ids are 0..={last}"))
    }
}

/// Parses an FX chain address: `master`, or a deck (`deck0`, `d0`, or a bare `0`).
fn fx_chain(word: Option<&str>, decks: usize) -> Result<FxChainId, String> {
    let raw = word.ok_or_else(|| "missing chain, use `master` or `deck<N>`".to_owned())?;
    let lower = raw.to_ascii_lowercase();
    if lower == "master" || lower == "m" {
        return Ok(FxChainId::Master);
    }
    let digits = lower
        .strip_prefix("deck")
        .or_else(|| lower.strip_prefix('d'))
        .unwrap_or(&lower);
    let id: u8 = digits
        .parse()
        .map_err(|_| format!("bad chain `{raw}`, use `master` or `deck<N>`"))?;
    let last = (decks - 1) as u8;
    if id <= last {
        Ok(FxChainId::Deck(id))
    } else {
        Err(format!("unknown deck {id}, valid ids are 0..={last}"))
    }
}

/// The next word as a slot index.
fn next_index<'a>(words: &mut impl Iterator<Item = &'a str>) -> Result<usize, String> {
    words
        .next()
        .ok_or_else(|| "missing slot index".to_owned())
        .and_then(|raw| {
            raw.parse::<usize>()
                .map_err(|_| format!("slot index must be a number, got `{raw}`"))
        })
}

/// The next word as a chain address.
fn next_chain<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    decks: usize,
) -> Result<FxChainId, String> {
    fx_chain(words.next(), decks)
}

/// Decodes on a worker thread, then hands the pipeline a ready source (+ optional constant grid).
fn spawn_load(deck_id: u8, path: String, bpm: Option<f32>, command_tx: Sender<Command>) {
    std::thread::Builder::new()
        .name(format!("hypermixx-load-{deck_id}"))
        .spawn(move || match hypermixx_media::decode_file(&path) {
            Ok(decoded) => {
                let total_frames = decoded.total_frames;
                let analysis = bpm.filter(|b| *b > 0.0).map(|bpm| {
                    hypermixx_core::TrackAnalysis::from_grid(
                        hypermixx_core::BeatGrid::from_constant_bpm(
                            bpm,
                            0,
                            total_frames,
                            SAMPLE_RATE,
                        ),
                        bpm,
                    )
                });
                let source: Shared = Arc::new(hypermixx_media::PcmPool::from_decoded(decoded));
                let _ = command_tx.send(Command::Load {
                    deck_id,
                    source,
                    analysis,
                });
            }
            Err(err) => eprintln!("error: deck {deck_id}, {path}: {err}"),
        })
        .ok();
}

/// Runs the library analyser on a worker thread, then publishes a compiled grid.
fn spawn_analyse(deck_id: u8, source: Shared, backend: Backend, command_tx: Sender<Command>) {
    std::thread::Builder::new()
        .name(format!("hypermixx-analyse-{deck_id}"))
        .spawn(move || {
            eprintln!("[analyse] deck{deck_id}: running {backend:?}...");
            match hypermixx_library::analyser::analyze(source, SAMPLE_RATE, backend) {
                Ok(analysis) => {
                    let bpm = analysis.bpm();
                    let key = analysis
                        .key
                        .map(|k| k.traditional())
                        .unwrap_or_else(|| "--".into());
                    eprintln!("[analyse] deck{deck_id}: {bpm:.1} BPM, {key}");
                    let _ = command_tx.send(Command::SetAnalysis { deck_id, analysis });
                }
                Err(err) => eprintln!("[analyse] deck{deck_id}: {err}"),
            }
        })
        .ok();
}

fn print_response(response: &CommandResponse) {
    match response {
        CommandResponse::Loaded {
            deck_id,
            total_frames,
        } => println!(
            "deck{deck_id} loaded: {total_frames} frames ({})",
            format_time(*total_frames)
        ),
        CommandResponse::State(state) => print_state(state),
        CommandResponse::States(states) => {
            for state in states {
                print_state(state);
            }
        }
        CommandResponse::FxAdded {
            chain,
            index,
            kind,
        } => println!("{} fx[{index}] {kind} added", chain.label()),
        CommandResponse::FxListed { chain, slots } => {
            if slots.is_empty() {
                println!("{}: no effects", chain.label());
            }
            for slot in slots {
                let state = if slot.enabled { "on" } else { "off" };
                println!("{} fx[{}] {} [{state}]", chain.label(), slot.index, slot.kind);
                for (name, value) in &slot.params {
                    println!("    {name} = {value:.4}");
                }
            }
        }
        CommandResponse::Ok => {}
        CommandResponse::Error(message) => eprintln!("error: {message}"),
    }
}

fn print_state(state: &hypermixx_core::DeckState) {
    let transport = if state.total_frames == 0 {
        "empty"
    } else if state.playing {
        "playing"
    } else {
        "paused"
    };
    let duration = if state.total_frames == 0 {
        "-".to_owned()
    } else {
        format_time(state.total_frames)
    };
    let tempo = if state.bpm > 0.0 {
        format!("{:.1} BPM", state.bpm)
    } else {
        "no grid".into()
    };
    let key = state.key.as_deref().unwrap_or("--");
    println!(
        "deck{}  {transport:<7} {} / {duration}  [{}/{}]  {tempo}  {key}",
        state.deck_id,
        format_time(state.current_frame),
        state.current_frame,
        state.total_frames,
    );
}

fn read_line(stdin: &mut impl BufRead) -> Option<String> {
    print!("hypermixx> ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match stdin.read_line(&mut line) {
        Ok(0) => {
            println!();
            None
        }
        Ok(_) => Some(line),
        Err(err) => {
            eprintln!("error: stdin ({err})");
            None
        }
    }
}

/// Reads `--config <path>` (topology file), `--print-config` (emit the reference TOML and exit),
/// and `--backend auto|stratum|timestretch` (default `auto`). Unknown flags are errors rather
/// than silently ignored: a typo'd `--conifg` that launches the default topology instead is the
/// kind of surprise nobody wants mid-set.
#[derive(Debug, Default)]
struct CliArgs {
    config: Option<String>,
    print_config: bool,
    backend: Backend,
}

impl CliArgs {
    fn parse() -> Self {
        let mut args = Self::default();
        let mut raw = std::env::args().skip(1);
        while let Some(arg) = raw.next() {
            match arg.as_str() {
                "--print-config" => args.print_config = true,
                "--backend" => {
                    args.backend = match raw.next().as_deref() {
                        Some("stratum") => Backend::Stratum,
                        Some("timestretch") => Backend::Timestretch,
                        Some("auto") | None => Backend::Auto,
                        Some(other) => {
                            eprintln!("error: unknown backend `{other}` (auto|stratum|timestretch)");
                            std::process::exit(1);
                        }
                    };
                }
                "--config" => {
                    args.config = raw.next().or_else(|| {
                        eprintln!("error: --config needs a path");
                        std::process::exit(1);
                    });
                }
                other => {
                    eprintln!("error: unknown argument `{other}` (--config, --print-config, --backend)");
                    std::process::exit(1);
                }
            }
        }
        args
    }
}

/// Reads a topology file. Parse errors carry the TOML positions, so a typo points at its line.
fn read_config(path: &str) -> Result<MixerConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    MixerConfig::from_toml_str(&text).map_err(|err| err.message())
}

fn print_help(decks: usize) {
    println!(
        "commands ({decks} decks, ids 0..{}):
  load <deck> <path> [bpm]   decode a file; a given bpm builds a fixed grid, skipping analysis
  analyse <deck>             run the analyser on the deck's track and publish its grid
  play <deck>                start that deck
  pause <deck>               stop it, keeping the position
  jump <deck> <frame>        seek to a frame (1 second = {SAMPLE_RATE} frames)
  beatjump <deck> <beats>    seek by whole beats, keeping the phase
  rate <deck> <ratio>        set tempo rate (1.0 = unity, 0.5 = half speed)
  profile <deck> <name>      tape / keylock / wide (default: tape)
  fx ...                     effects on a chain — `fx help` for the subcommands
  state                      show every deck
  quit                       exit

startup flags: --config <file> (custom topology), --print-config (reference TOML),
               --backend auto|stratum|timestretch",
        decks - 1
    );
}

/// The `fx` subcommands, plus the effect kinds and their parameters (straight from the registry,
/// so this text can never disagree with what the engine actually builds).
fn print_fx_help() {
    println!(
        "fx commands (chain is `master` or `deck<N>`, e.g. deck0):
  fx add <chain> <kind>                  append an effect to the chain
  fx remove <chain> <index>              drop a slot (later slots shift down)
  fx list <chain>                        show every slot and its parameters
  fx set <chain> <index> <param> <val>   set one parameter (value is smoothed, no clicks)
  fx on|off <chain> <index>              engage / bypass a slot
  fx trigger <chain> <index>             fire the effect's one-shot hook
  fx pad <chain> <index> press|release   hold a slot engaged while the pad is down

effect kinds and their parameters:"
    );
    for kind in FxKind::ALL {
        println!("  {:<8} {}", kind.name(), kind.param_names().join(", "));
    }
}

/// Frames -> `m:ss.mmm` at the engine rate.
fn format_time(frames: u64) -> String {
    let millis = frames as f64 * 1000.0 / SAMPLE_RATE as f64;
    let minutes = (millis / 60_000.0) as u64;
    let seconds = (millis / 1000.0) as u64 % 60;
    let remainder = (millis % 1000.0) as u32;
    format!("{minutes}:{seconds:02}.{remainder:03}")
}
