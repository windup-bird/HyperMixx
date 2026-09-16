//! Command line front-end for the Hypermixx audio engine.
//!
//! The CLI owns file IO and analysis: `load` decodes on a worker thread and hands the pipeline a
//! ready source; `analyse` runs the library analyser and publishes a compiled grid. The pipeline
//! only ever receives executable commands. A dedicated printer thread renders responses as they
//! arrive, so nothing here blocks on the engine.

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use crossbeam_channel::{unbounded, Sender};
use hypermixx_audio::{AudioPipeline, Command, CommandResponse, DECK_COUNT, SAMPLE_RATE};
use hypermixx_core::Backend;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let backend = parse_backend();
    let (command_tx, command_rx) = unbounded();
    let (response_tx, response_rx) = unbounded();
    let pipeline = AudioPipeline::start(command_rx, response_tx);

    println!(
        "hypermixx {VERSION} — {DECK_COUNT} decks, {SAMPLE_RATE}Hz stereo, backend {backend:?}. \
         `help` for commands, `quit` to exit."
    );

    // Printer thread: render every engine response as it lands, then end when the channel closes.
    let printer = std::thread::spawn(move || {
        while let Ok(response) = response_rx.recv() {
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
        match dispatch(line, &command_tx, &pipeline, backend) {
            Action::Continue => {}
            Action::Quit => break,
            Action::Bad(message) => eprintln!("error: {message}"),
        }
    }

    let _ = command_tx.send(Command::Quit);
    drop(command_tx);
    drop(pipeline);
    let _ = printer.join();
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
) -> Action {
    let mut words = line.split_whitespace();
    let head = words.next().unwrap_or_default();
    let mut deck = || deck_id(words.next());

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
            let Some(source) = deck_source(pipeline, deck_id) else {
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
        "help" | "h" | "?" => {
            print_help();
            Action::Continue
        }
        "quit" | "exit" | "q" => Action::Quit,
        other => Action::Bad(format!("unknown command `{other}` — `help` lists them")),
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

fn deck_id(word: Option<&str>) -> Result<u8, String> {
    let last = (DECK_COUNT - 1) as u8;
    let raw = word.ok_or_else(|| format!("missing deck id, expected 0..{last}"))?;
    let id: u8 = raw
        .parse()
        .map_err(|_| format!("deck id must be a number in 0..{last}, got `{raw}`"))?;
    if (id as usize) < DECK_COUNT {
        Ok(id)
    } else {
        Err(format!("unknown deck {id}, valid ids are 0..={last}"))
    }
}

/// Clones the deck's current PCM source out from behind its mutex.
fn deck_source(pipeline: &AudioPipeline, deck_id: u8) -> Option<Arc<dyn hypermixx_core::Source>> {
    let deck = pipeline.deck(deck_id)?;
    let guard = deck.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.total_frames() == 0 {
        return None;
    }
    Some(guard.source())
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
                let source: hypermixx_core::Shared =
                    Arc::new(hypermixx_media::PcmPool::from_decoded(decoded));
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
fn spawn_analyse(
    deck_id: u8,
    source: Arc<dyn hypermixx_core::Source>,
    backend: Backend,
    command_tx: Sender<Command>,
) {
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

/// Reads `--backend auto|stratum|timestretch` (default `auto`).
fn parse_backend() -> Backend {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--backend" {
            return match args.next().as_deref() {
                Some("stratum") => Backend::Stratum,
                Some("timestretch") => Backend::Timestretch,
                _ => Backend::Auto,
            };
        }
    }
    Backend::Auto
}

fn print_help() {
    println!(
        "commands ({} decks, ids 0..{}):
  load <deck> <path> [bpm]   decode a file; a given bpm builds a fixed grid, skipping analysis
  analyse <deck>             run the analyser on the deck's track and publish its grid
  play <deck>                start that deck
  pause <deck>               stop it, keeping the position
  jump <deck> <frame>        seek to a frame (1 second = {SAMPLE_RATE} frames)
  beatjump <deck> <beats>    seek by whole beats, keeping the phase
  rate <deck> <ratio>        set tempo rate (1.0 = unity, 0.5 = half speed)
  profile <deck> <name>      tape / keylock / wide (default: tape)
  state                      show every deck
  quit                       exit",
        DECK_COUNT,
        DECK_COUNT - 1
    );
}

/// Frames -> `m:ss.mmm` at the engine rate.
fn format_time(frames: u64) -> String {
    let millis = frames as f64 * 1000.0 / SAMPLE_RATE as f64;
    let minutes = (millis / 60_000.0) as u64;
    let seconds = (millis / 1000.0) as u64 % 60;
    let remainder = (millis % 1000.0) as u32;
    format!("{minutes}:{seconds:02}.{remainder:03}")
}
