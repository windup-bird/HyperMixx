//! Command line front-end for the Hypermixx audio engine.

use std::io::{self, BufRead, Write};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver};
use hypermixx_audio::{AudioPipeline, Command, CommandResponse, SAMPLE_RATE};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Decoding runs in the engine, so `load` may take a while on long files.
const LOAD_TIMEOUT: Duration = Duration::from_secs(300);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

fn main() {
    let (command_tx, command_rx) = unbounded();
    let (response_tx, response_rx) = unbounded();
    let pipeline = AudioPipeline::start(command_rx, response_tx);

    println!(
        "hypermixx {VERSION} — single deck, 48kHz stereo. `help` for commands, `quit` to exit."
    );
    let mut total_frames = 0u64;
    let mut stdin = io::stdin().lock();

    loop {
        match read_line(&mut stdin) {
            None => break, // EOF or Ctrl-D
            Some(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Some(command) = parse(line) else { continue };
                let quitting = matches!(command, Command::Quit);
                let timeout = if matches!(command, Command::Load { .. }) {
                    LOAD_TIMEOUT
                } else {
                    COMMAND_TIMEOUT
                };
                if command_tx.send(command).is_err() {
                    eprintln!("error: the audio engine is gone");
                    break;
                }
                if quitting {
                    break;
                }
                report(&response_rx, timeout, &mut total_frames);
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

fn parse(line: &str) -> Option<Command> {
    let mut words = line.split_whitespace();
    match words.next().unwrap_or_default() {
        "load" => match words.next() {
            Some(path) => Some(Command::Load {
                path: path.to_owned(),
            }),
            None => {
                eprintln!("usage: load <path>");
                None
            }
        },
        "play" => Some(Command::Play),
        "pause" => Some(Command::Pause),
        "jump" => match words.next().map(str::parse::<u64>) {
            Some(Ok(frame)) => Some(Command::Jump {
                target_frame: frame,
            }),
            Some(Err(err)) => {
                eprintln!("usage: jump <frame> ({err})");
                None
            }
            None => {
                eprintln!("usage: jump <frame>");
                None
            }
        },
        "state" => Some(Command::GetState),
        "help" | "h" | "?" => {
            print_help();
            None
        }
        "quit" | "exit" | "q" => Some(Command::Quit),
        other => {
            eprintln!("unknown command `{other}` — `help` lists them");
            None
        }
    }
}

fn print_help() {
    println!(
        "commands:
  load <path>       decode an audio file (mp3/wav/flac) into the deck
  play              start playback
  pause             stop playback, keeping the position
  jump <frame>      seek to a frame at 48kHz (1 second = {SAMPLE_RATE} frames)
  state             show position and transport state
  quit              exit"
    );
}

fn report(response_rx: &Receiver<CommandResponse>, timeout: Duration, total_frames: &mut u64) {
    match response_rx.recv_timeout(timeout) {
        Ok(CommandResponse::Loaded {
            total_frames: frames,
        }) => {
            *total_frames = frames;
            println!(
                "loaded: {} frames ({})",
                frames,
                format_time(frames, SAMPLE_RATE)
            );
        }
        Ok(CommandResponse::State {
            current_frame,
            playing,
        }) => {
            println!(
                "{}  {current_frame} / {} frames  ({})",
                if playing { "playing" } else { "paused" },
                *total_frames,
                format_time(current_frame, SAMPLE_RATE)
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

/// Frames -> `m:ss.mmm`.
fn format_time(frames: u64, sample_rate: u32) -> String {
    let millis = frames as f64 * 1000.0 / sample_rate as f64;
    let minutes = (millis / 60_000.0) as u64;
    let seconds = (millis / 1000.0) as u64 % 60;
    let remainder = (millis % 1000.0) as u32;
    format!("{minutes}:{seconds:02}.{remainder:03}")
}
