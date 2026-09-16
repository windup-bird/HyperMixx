//! 双 Deck 相位锁定回归测试。
//!
//! 两个 Deck 从同一份素材的 frame 0 同时起播，之后每隔 4 拍给 deck1 `beatjump +4`。
//! 每跳一次，相位差应当精确增加 4 拍（等待期间两个 Deck 走同样的量）。判据两条：
//! 1. 相位差的**增量**等于 4 拍的网格距离 —— 两 Deck 时钟不一致会逐轮累积漂移；
//! 2. 两个 Deck 的 `BeatGrid::phase()` 始终一致 —— 锚点或相位算术错就会停在拍缝里。

mod common;

use std::time::Duration;

use common::{ack, ask, decode_wav, load_both, seed_grids, session, state, states};
use crossbeam_channel::{Receiver, Sender};
use hypermixx_audio::{BeatGrid, Command, CommandResponse, SAMPLE_RATE};

const BPM: f32 = 122.0;
const BEATS: i64 = 4;
const ROUNDS: usize = 3;
/// Frames per beat, in float so the grid's per-beat rounding stays honest.
const BEAT_FRAMES: f64 = 44_100.0 * 60.0 / 122.0;
/// The distance one 4-beat jump must add.
const STEP_DELTA: i64 = (4.0 * BEAT_FRAMES).round() as i64;
/// Slack for the grid's per-beat frame rounding (±1) plus one block of switch quantization.
const TOLERANCE: i64 = 512;
/// How far apart the two decks may sit inside a beat, in beats (0.01 beat ≈ 4.9ms at 122 BPM).
const PHASE_TOLERANCE: f32 = 0.01;

fn beat_sleep(beats: i64) -> Duration {
    Duration::from_nanos((beats as f64 * BEAT_FRAMES * 1e9 / SAMPLE_RATE as f64) as u64)
}

/// Distance between two positions' beat phase, wrapped into [0, 0.5] beats.
fn phase_gap(grid: &BeatGrid, deck0: u64, deck1: u64) -> f32 {
    let gap = (grid.phase(deck1) - grid.phase(deck0)).abs();
    gap.min(1.0 - gap)
}

/// Both decks' positions, sampled inside the same production block.
fn pair(tx: &Sender<Command>, rx: &Receiver<CommandResponse>) -> (u64, u64) {
    let both = states(tx, rx);
    assert_eq!(both.len(), 2, "GetAllStates must answer for both decks");
    (both[0].current_frame, both[1].current_frame)
}

#[test]
fn repeated_beatjumps_add_exactly_four_beats_each_time() {
    let (tx, rx, _pipeline) = session();
    let total_frames = load_both(&tx, &rx, decode_wav("beatlock.wav", 14))[0].1;
    seed_grids(&tx, &rx, BPM, total_frames);
    let grid = BeatGrid::from_constant_bpm(BPM, 0, total_frames, SAMPLE_RATE);

    // Both play commands are queued before reading any answer, so the producer handles them inside
    // one drain pass and the decks start on the same block. Asked and answered one at a time, deck
    // 1 would begin a block late and carry that 256-frame offset forever.
    ask(&tx, Command::Play { deck_id: 0 });
    ask(&tx, Command::Play { deck_id: 1 });
    ack(&rx);
    ack(&rx);

    let (start0, start1) = pair(&tx, &rx);
    let baseline = start1 as i64 - start0 as i64;
    assert!(
        phase_gap(&grid, start0, start1) < PHASE_TOLERANCE,
        "both decks cue at frame 0, so they must share a phase: {start0} vs {start1}"
    );

    println!("round  deck0 frame  deck1 frame    delta  delta/beats    err  phase_gap");
    for round in 1..=ROUNDS {
        std::thread::sleep(beat_sleep(BEATS));
        ask(
            &tx,
            Command::BeatJump {
                deck_id: 1,
                beats: BEATS,
            },
        );
        ack(&rx);
        std::thread::sleep(Duration::from_millis(300)); // warm-up thread + block switch

        let (deck0, deck1) = pair(&tx, &rx);
        let delta = deck1 as i64 - deck0 as i64;
        let err = delta - (baseline + round as i64 * STEP_DELTA);
        let gap = phase_gap(&grid, deck0, deck1);
        println!(
            "{:>5}  {:>11}  {:>11}  {:>7}  {:>9.4}  {:>5}  {:>9.5}",
            round,
            deck0,
            deck1,
            delta,
            (delta - baseline) as f64 / BEAT_FRAMES,
            err,
            gap
        );

        assert!(
            err.abs() <= TOLERANCE,
            "round {round}: the gap must grow by exactly {STEP_DELTA} frames per jump, off by {err} \
             — a wrong bpm shifts every step by ~{} frames",
            ((4.0 * (130.0 - 122.0) / 122.0) * BEAT_FRAMES).round() as i64,
        );
        assert!(
            gap < PHASE_TOLERANCE,
            "round {round}: after a 4-beat jump both decks must share the beat phase, off by \
             {gap:.4} beats (deck0 {deck0}, deck1 {deck1})"
        );
    }

    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn undoing_a_beatjump_returns_to_the_same_frame() {
    let (tx, rx, _pipeline) = session();
    let total_frames = load_both(&tx, &rx, decode_wav("beatundo.wav", 6))[0].1;
    seed_grids(&tx, &rx, BPM, total_frames);
    ask(&tx, Command::Play { deck_id: 1 });
    ack(&rx);
    std::thread::sleep(Duration::from_millis(400));
    // Pause so the round trip is measured without playback drift.
    ask(&tx, Command::Pause { deck_id: 1 });
    ack(&rx);
    let before = state(&tx, &rx, 1).current_frame;

    ask(
        &tx,
        Command::BeatJump {
            deck_id: 1,
            beats: 8,
        },
    );
    ack(&rx);
    std::thread::sleep(Duration::from_millis(300));
    let forward = state(&tx, &rx, 1).current_frame;
    assert!(
        forward > before,
        "beatjump +8 did not move deck 1: {before} -> {forward}"
    );

    ask(
        &tx,
        Command::BeatJump {
            deck_id: 1,
            beats: -8,
        },
    );
    ack(&rx);
    std::thread::sleep(Duration::from_millis(300));
    let back = state(&tx, &rx, 1).current_frame;
    assert!(
        (back as i64 - before as i64).abs() <= 2 * 256,
        "+8 then -8 must land back on the same frame: {before} -> {forward} -> {back}"
    );

    ask(&tx, Command::Quit);
    ack(&rx);
}
