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

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};
use hypermixx_audio::{
    AudioPipeline, BeatGrid, Command, CommandResponse, DeckId, DeckState, TrackAnalysis,
};
use hypermixx_core::Shared;
use hypermixx_media::{decode_file, PcmPool};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// Writes a generated wav, decodes it (as the CLI would), and returns the ready source.
pub fn decode_wav(name: &str, seconds: u64) -> Shared {
    let path = temp_path(name);
    write_wav(
        &path,
        hypermixx_audio::SAMPLE_RATE,
        2,
        (seconds as u32 * hypermixx_audio::SAMPLE_RATE) as usize,
    );
    let decoded = decode_file(&path).expect("generated wav should decode");
    let _ = std::fs::remove_file(&path);
    Arc::new(PcmPool::from_decoded(decoded))
}

/// Starts an engine and hands back a command line plus its answers.
pub fn session() -> (Sender<Command>, Receiver<CommandResponse>, AudioPipeline) {
    let (command_tx, command_rx) = unbounded();
    let (response_tx, response_rx) = unbounded();
    let pipeline = AudioPipeline::start(command_rx, response_tx);
    (command_tx, response_rx, pipeline)
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

pub fn state(tx: &Sender<Command>, rx: &Receiver<CommandResponse>, deck_id: DeckId) -> DeckState {
    ask(tx, Command::GetState { deck_id });
    match answer(rx) {
        CommandResponse::State(state) => state,
        other => panic!("expected State, got {other:?}"),
    }
}

/// Every deck at once, sampled inside the same production block. Use this when comparing decks:
/// two `GetState` commands are answered a block apart and so differ by ~256 frames on their own.
pub fn states(tx: &Sender<Command>, rx: &Receiver<CommandResponse>) -> Vec<DeckState> {
    ask(tx, Command::GetAllStates);
    match answer(rx) {
        CommandResponse::States(states) => states,
        other => panic!("expected States, got {other:?}"),
    }
}

/// Loads the same decoded source into a deck and returns its length from the `Loaded` reply.
pub fn load(
    tx: &Sender<Command>,
    rx: &Receiver<CommandResponse>,
    deck_id: DeckId,
    source: Shared,
) -> u64 {
    ask(
        tx,
        Command::Load {
            deck_id,
            source,
            analysis: None,
        },
    );
    match answer(rx) {
        CommandResponse::Loaded {
            deck_id: d,
            total_frames,
        } => {
            assert_eq!(d, deck_id);
            total_frames
        }
        CommandResponse::Error(err) => panic!("load failed: {err}"),
        other => panic!("expected Loaded, got {other:?}"),
    }
}

/// Loads the same source into both decks, returning `(deck_id, total_frames)` for each.
pub fn load_both(
    tx: &Sender<Command>,
    rx: &Receiver<CommandResponse>,
    source: Shared,
) -> Vec<(DeckId, u64)> {
    let mut loaded = Vec::new();
    for deck_id in 0..2 {
        ask(
            tx,
            Command::Load {
                deck_id,
                source: Arc::clone(&source),
                analysis: None,
            },
        );
    }
    for _ in 0..2 {
        match answer(rx) {
            CommandResponse::Loaded {
                deck_id,
                total_frames,
            } => loaded.push((deck_id, total_frames)),
            CommandResponse::Error(err) => panic!("load failed: {err}"),
            other => panic!("expected Loaded, got {other:?}"),
        }
    }
    loaded.sort_unstable();
    loaded
}

/// A deterministic constant-BPM analysis the caller can install alongside a load.
pub fn constant_grid(bpm: f32, total_frames: u64) -> TrackAnalysis {
    TrackAnalysis {
        beatgrid: BeatGrid::from_constant_bpm(bpm, 0, total_frames, hypermixx_audio::SAMPLE_RATE),
        key: None,
        bpm: Some(bpm),
    }
}

/// Sends a constant-BPM grid to both decks so beatjump has something to work with.
pub fn seed_grids(
    tx: &Sender<Command>,
    rx: &Receiver<CommandResponse>,
    bpm: f32,
    total_frames: u64,
) {
    for deck_id in 0..2 {
        ask(
            tx,
            Command::SetAnalysis {
                deck_id,
                analysis: constant_grid(bpm, total_frames),
            },
        );
        ack(rx);
    }
}
