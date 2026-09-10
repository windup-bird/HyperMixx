//! Command line front-end for the Hypermixx audio engine.

use std::io::{self, BufRead, Write};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver};
use hypermixx_audio::{AudioPipeline, Command, CommandResponse, DECK_COUNT, SAMPLE_RATE};

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
            report(&response_rx, timeout);
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

/// Turns one input line into the commands to send. `state` fans out to every deck.
fn parse(line: &str) -> Result<Vec<Command>, String> {
    let mut words = line.split_whitespace();
    let commands = match words.next().unwrap_or_default() {
        "load" => {
            let deck_id = deck_id(words.next())?;
            let path = words.next().ok_or("usage: load <deck_id> <path> [bpm]")?;
            let bpm = words
                .next()
                .map(str::parse::<f32>)
                .transpose()
                .map_err(|_| "bpm must be a number")?;
            vec![Command::Load {
                deck_id,
                path: path.to_owned(),
                bpm,
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
        "state" => (0..DECK_COUNT)
            .map(|deck_id| Command::GetState { deck_id })
            .collect(),
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
  load <deck> <path> [bpm]   decode mp3/wav/flac into that deck (bpm defaults to the engine's)
  play <deck>                start that deck
  pause <deck>               stop it, keeping the position
  jump <deck> <frame>        seek to a frame (1 second = {SAMPLE_RATE} frames)
  beatjump <deck> <beats>    seek by whole beats, keeping the phase; negative goes back
  state                      show every deck
  quit                       exit",
        DECK_COUNT,
        DECK_COUNT - 1
    );
}

fn report(response_rx: &Receiver<CommandResponse>, timeout: Duration) {
    match response_rx.recv_timeout(timeout) {
        Ok(CommandResponse::Loaded {
            deck_id,
            total_frames,
            bpm,
        }) => println!(
            "deck{deck_id} loaded: {total_frames} frames ({}) @ {bpm:.1} BPM",
            format_time(total_frames)
        ),
        Ok(CommandResponse::State {
            deck_id,
            current_frame,
            playing,
            total_frames,
            bpm,
        }) => {
            let transport = if total_frames == 0 {
                "empty"
            } else if playing {
                "playing"
            } else {
                "paused"
            };
            let duration = if total_frames == 0 {
                "-".to_owned()
            } else {
                format_time(total_frames)
            };
            let tempo = if bpm > 0.0 {
                format!("{bpm:.1} BPM")
            } else {
                "no grid".into()
            };
            println!(
                "deck{deck_id}  {transport:<7} {} / {duration}  [{current_frame}/{total_frames}]  {tempo}",
                format_time(current_frame)
            );
        }
        Ok(CommandResponse::Ok) => {}
        Ok(CommandResponse::Error(message)) => eprintln!("error: {message}"),
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
            eprintln!("error: the engine did not answer in time")
        }
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
            eprintln!("error: the audio engine shut down")
        }
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
