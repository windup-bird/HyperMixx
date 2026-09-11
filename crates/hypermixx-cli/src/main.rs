//! Command line front-end for the Hypermixx audio engine.

use std::io::{self, BufRead, Write};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver};
use hypermixx_audio::{
    AudioPipeline, Command, CommandResponse, DeckState, DECK_COUNT, SAMPLE_RATE,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Decoding runs in the engine, so `load` may take a while on long files.
const LOAD_TIMEOUT: Duration = Duration::from_secs(300);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

fn main() {
    let (command_tx, command_rx) = unbounded();
    let (response_tx, response_rx) = unbounded();
    let pipeline = AudioPipeline::start(command_rx, response_tx);

    println!(
        "hypermixx {VERSION} — {DECK_COUNT} decks, 48kHz stereo. `help` for commands, `quit` to exit."
    );
    let mut stdin = io::stdin().lock();

    // `read_line` returns None on EOF or Ctrl-D, which ends the session.
    while let Some(line) = read_line(&mut stdin) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let commands = match parse(line) {
            Ok(commands) => commands,
            Err(message) => {
                eprintln!("error: {message}");
                continue;
            }
        };
        if commands.is_empty() {
            continue; // help, or a line that needs no engine round-trip
        }
        let quitting = commands
            .iter()
            .any(|command| matches!(command, Command::Quit));
        let loading = commands
            .iter()
            .any(|command| matches!(command, Command::Load { .. }));
        let timeout = if loading {
            LOAD_TIMEOUT
        } else {
            COMMAND_TIMEOUT
        };

        // Every command answers exactly once, so send them all and then collect the same number
        // of replies, in order.
        let expected = commands.len();
        for command in commands {
            if command_tx.send(command).is_err() {
                eprintln!("error: the audio engine is gone");
                return;
            }
        }
        if quitting {
            break;
        }
        for _ in 0..expected {
            if let Some(deck_id) = report(&response_rx, timeout) {
                spawn_analysis(deck_id, &pipeline);
            }
        }
    }

    // Ask nicely, then let the pipeline's Drop join its threads.
    let _ = command_tx.send(Command::Quit);
    drop(pipeline);
}

/// Reads one line, printing the prompt. `None` means end of input.
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

/// Turns one input line into the commands to send.
fn parse(line: &str) -> Result<Vec<Command>, String> {
    let mut words = line.split_whitespace();
    let commands = match words.next().unwrap_or_default() {
        "load" => {
            let deck_id = deck_id(words.next())?;
            let path = words.next().ok_or("usage: load <deck_id> <path>")?;
            vec![Command::Load {
                deck_id,
                path: path.to_owned(),
            }]
        }
        "play" => vec![Command::Play {
            deck_id: deck_id(words.next())?,
        }],
        "pause" => vec![Command::Pause {
            deck_id: deck_id(words.next())?,
        }],
        "jump" => {
            let deck_id = deck_id(words.next())?;
            let frame = words.next().ok_or("usage: jump <deck_id> <frame>")?;
            vec![Command::Jump {
                deck_id,
                target_frame: frame.parse::<u64>().map_err(|_| "frame must be a number")?,
            }]
        }
        "beatjump" => {
            let deck_id = deck_id(words.next())?;
            let beats = words.next().ok_or("usage: beatjump <deck_id> <beats>")?;
            vec![Command::BeatJump {
                deck_id,
                beats: beats
                    .parse::<i64>()
                    .map_err(|_| "beats must be a whole number")?,
            }]
        }
        "state" => vec![Command::GetAllStates],
        "rate" => {
            let deck_id = deck_id(words.next())?;
            let rate = words.next().ok_or("usage: rate <deck_id> <ratio>")?;
            vec![Command::SetRate {
                deck_id,
                rate: rate.parse::<f32>().map_err(|_| "rate must be a number")?,
            }]
        }
        "profile" => {
            let deck_id = deck_id(words.next())?;
            let profile = words
                .next()
                .ok_or("usage: profile <deck_id> <tape|keylock|wide>")?;
            vec![Command::SetProfile {
                deck_id,
                profile: profile.to_owned(),
            }]
        }
        "quit" | "exit" | "q" => vec![Command::Quit],
        "help" | "h" | "?" => {
            print_help();
            Vec::new()
        }
        other => return Err(format!("unknown command `{other}` — `help` lists them")),
    };
    Ok(commands)
}

fn deck_id(word: Option<&str>) -> Result<usize, String> {
    let last = DECK_COUNT - 1;
    let raw = word.ok_or(format!("missing deck id, expected 0..{last}"))?;
    let id = raw
        .parse::<usize>()
        .map_err(|_| format!("deck id must be a number, got `{raw}`"))?;
    if id < DECK_COUNT {
        Ok(id)
    } else {
        Err(format!("unknown deck {id}, valid ids are 0..{last}"))
    }
}

fn print_help() {
    println!(
        "commands ({} decks, ids 0..{}):
  load <deck> <path>         decode mp3/wav/flac into that deck
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

fn report(response_rx: &Receiver<CommandResponse>, timeout: Duration) -> Option<usize> {
    match response_rx.recv_timeout(timeout) {
        Ok(CommandResponse::Loaded {
            deck_id,
            total_frames,
        }) => {
            println!(
                "deck{deck_id} loaded: {total_frames} frames ({})",
                format_time(total_frames)
            );
            Some(deck_id)
        }
        Ok(CommandResponse::State(state)) => {
            print_state(&state);
            None
        }
        // One answer holding every deck: all rows come from the same production block.
        Ok(CommandResponse::States(states)) => {
            for state in states {
                print_state(&state);
            }
            None
        }
        Ok(CommandResponse::Ok) => None,
        Ok(CommandResponse::Error(message)) => {
            eprintln!("error: {message}");
            None
        }
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
            eprintln!("error: the engine did not answer in time");
            None
        }
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
            eprintln!("error: the audio engine shut down");
            None
        }
    }
}

fn print_state(state: &DeckState) {
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
    let key_label = state.key.as_deref().unwrap_or("--");
    println!(
        "deck{}  {transport:<7} {} / {duration}  [{}/{}]  {tempo}  {key_label}",
        state.deck_id,
        format_time(state.current_frame),
        state.current_frame,
        state.total_frames,
    );
}

/// Spawns a background thread that reads PCM from the deck, runs beat/key analysis, and publishes
/// the result via `Deck::set_analysis` (ArcSwap, lock-free). The next `state` query shows it.
fn spawn_analysis(deck_id: usize, pipeline: &AudioPipeline) {
    let Some(deck) = pipeline.deck(deck_id).cloned() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name(format!("hypermixx-analysis-{deck_id}"))
        .spawn(move || {
            let (source, total_frames) = {
                let guard = deck.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                (guard.source(), guard.total_frames())
            };
            if total_frames == 0 {
                return;
            }
            eprintln!("[analysis] deck{deck_id}: analyzing {total_frames} frames...");
            let mono = hypermixx_analysis::downmix_to_mono(source.as_ref(), total_frames, 2);
            match hypermixx_analysis::analyze(&mono, hypermixx_audio::SAMPLE_RATE) {
                Ok(analysis) => {
                    let bpm = analysis.bpm.unwrap_or(0.0);
                    let key = analysis.key.as_ref().map(|k| k.name()).unwrap_or_default();
                    deck.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .set_analysis(analysis);
                    eprintln!("[analysis] deck{deck_id}: {bpm:.1} BPM, {key}");
                }
                Err(err) => {
                    eprintln!("[analysis] deck{deck_id}: {err}");
                }
            }
        });
    if let Err(err) = spawned {
        eprintln!("[analysis] could not start thread: {err}");
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
