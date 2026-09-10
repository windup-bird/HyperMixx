//! Helpers shared by the integration tests.
//! Not every test binary uses every helper.
#![allow(dead_code)]

use std::f32::consts::PI;
use std::fs;
use std::io::{BufWriter, Write};

/// Minimal 16-bit PCM WAV writer, just enough to hand symphonia a real file.
pub fn write_wav(path: &str, sample_rate: u32, channels: u16, frames: usize) {
    let block_align = channels * 2;
    let data_bytes = (frames * block_align as usize) as u32;

    let mut pcm = Vec::with_capacity(data_bytes as usize);
    for frame in 0..frames {
        let value = (frame as f32 / sample_rate as f32 * 440.0 * 2.0 * PI).sin();
        for _ in 0..channels {
            pcm.extend_from_slice(&((value * i16::MAX as f32) as i16).to_le_bytes());
        }
    }

    let mut out = BufWriter::new(fs::File::create(path).unwrap());
    out.write_all(b"RIFF").unwrap();
    out.write_all(&(36 + data_bytes).to_le_bytes()).unwrap();
    out.write_all(b"WAVE").unwrap();
    out.write_all(b"fmt ").unwrap();
    out.write_all(&16u32.to_le_bytes()).unwrap();
    out.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    out.write_all(&channels.to_le_bytes()).unwrap();
    out.write_all(&sample_rate.to_le_bytes()).unwrap();
    out.write_all(&(sample_rate * block_align as u32).to_le_bytes())
        .unwrap();
    out.write_all(&block_align.to_le_bytes()).unwrap();
    out.write_all(&16u16.to_le_bytes()).unwrap();
    out.write_all(b"data").unwrap();
    out.write_all(&data_bytes.to_le_bytes()).unwrap();
    out.write_all(&pcm).unwrap();
    out.flush().unwrap();
}

/// A unique path under the system temp dir.
pub fn temp_path(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("hypermixx-it-{name}"))
        .to_string_lossy()
        .into_owned()
}

// Command-channel harness, shared by the engine-level tests.

use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};
use hypermixx_audio::{AudioPipeline, Command, CommandResponse, SAMPLE_RATE};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// Starts an engine over a generated wav and hands back a command line plus its answers.
pub fn session(
    name: &str,
    seconds: u64,
) -> (
    String,
    Sender<Command>,
    Receiver<CommandResponse>,
    AudioPipeline,
) {
    let path = temp_path(name);
    write_wav(
        &path,
        SAMPLE_RATE,
        2,
        (seconds as u32 * SAMPLE_RATE) as usize,
    );
    let (command_tx, command_rx) = unbounded();
    let (response_tx, response_rx) = unbounded();
    let pipeline = AudioPipeline::start(command_rx, response_tx);
    (path, command_tx, response_rx, pipeline)
}

pub fn ask(tx: &Sender<Command>, command: Command) {
    tx.send(command)
        .expect("the engine dropped its command channel");
}

pub fn answer(rx: &Receiver<CommandResponse>) -> CommandResponse {
    rx.recv_timeout(ANSWER_TIMEOUT)
        .expect("the engine did not answer")
}

pub fn ack(rx: &Receiver<CommandResponse>) {
    match answer(rx) {
        CommandResponse::Ok => {}
        CommandResponse::Error(err) => panic!("command failed: {err}"),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[derive(Debug)]
pub struct Snapshot {
    pub deck_id: usize,
    pub current_frame: u64,
    pub playing: bool,
    pub total_frames: u64,
    pub bpm: f32,
}

pub fn state(tx: &Sender<Command>, rx: &Receiver<CommandResponse>, deck_id: usize) -> Snapshot {
    ask(tx, Command::GetState { deck_id });
    match answer(rx) {
        CommandResponse::State {
            deck_id,
            current_frame,
            playing,
            total_frames,
            bpm,
        } => Snapshot {
            deck_id,
            current_frame,
            playing,
            total_frames,
            bpm,
        },
        other => panic!("expected State, got {other:?}"),
    }
}

/// Loads the same file into both decks and waits for both decode threads to answer.
pub fn load_both(
    tx: &Sender<Command>,
    rx: &Receiver<CommandResponse>,
    path: &str,
    bpm: f32,
) -> Vec<(usize, u64)> {
    for deck_id in 0..2 {
        ask(
            tx,
            Command::Load {
                deck_id,
                path: path.to_owned(),
                bpm: Some(bpm),
            },
        );
    }
    let mut loaded = Vec::new();
    for _ in 0..2 {
        match answer(rx) {
            CommandResponse::Loaded {
                deck_id,
                total_frames,
                ..
            } => loaded.push((deck_id, total_frames)),
            CommandResponse::Error(err) => panic!("load failed: {err}"),
            other => panic!("expected Loaded, got {other:?}"),
        }
    }
    loaded.sort_unstable();
    loaded
}
