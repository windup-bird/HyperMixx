//! Command-channel sessions covering the README's acceptance criteria for both decks.

mod common;

use std::fs;
use std::time::Duration;

use common::{ack, answer, ask, load_both, seed_grids, session, state};
use hypermixx_audio::{Command, CommandResponse, SAMPLE_RATE};

/// Frames in one beat at 122 BPM.
const FRAMES_PER_BEAT: u64 = (48_000.0f64 * 60.0 / 122.0).round() as u64;

#[test]
fn load_play_beatjump_and_pause_on_one_deck() {
    let (path, tx, rx, _pipeline) = session("deck0.wav", 6);

    ask(
        &tx,
        Command::Load {
            deck_id: 0,
            path: path.clone(),
        },
    );
    match answer(&rx) {
        CommandResponse::Loaded {
            deck_id,
            total_frames,
        } => {
            assert_eq!(deck_id, 0);
            assert_eq!(total_frames, 6 * SAMPLE_RATE as u64);
        }
        CommandResponse::Error(err) => panic!("load failed: {err}"),
        other => panic!("expected Loaded, got {other:?}"),
    }
    seed_grids(&tx, &rx, 122.0, 6 * SAMPLE_RATE as u64);

    let cued = state(&tx, &rx, 0);
    assert_eq!(
        (cued.deck_id, cued.current_frame, cued.playing),
        (0, 0, false)
    );
    assert_eq!(
        cued.total_frames,
        6 * SAMPLE_RATE as u64,
        "state must report the deck length"
    );

    // play -> the playhead advances roughly in real time.
    ask(&tx, Command::Play { deck_id: 0 });
    ack(&rx);
    std::thread::sleep(Duration::from_millis(500));
    let rolling = state(&tx, &rx, 0);
    assert!(rolling.playing, "play should report playing");
    let nominal = 500u64 * SAMPLE_RATE as u64 / 1000;
    assert!(
        rolling.current_frame > nominal / 2 && rolling.current_frame < nominal * 2,
        "playhead advanced at the wrong rate: {} after 500ms (expected ~{nominal})",
        rolling.current_frame
    );

    // Pause so the beatjump target is measurable without playback drift.
    ask(&tx, Command::Pause { deck_id: 0 });
    ack(&rx);
    let before = state(&tx, &rx, 0).current_frame;

    ask(
        &tx,
        Command::BeatJump {
            deck_id: 0,
            beats: 4,
        },
    );
    ack(&rx);
    std::thread::sleep(Duration::from_millis(250)); // warm-up thread + block switch
    let forward = state(&tx, &rx, 0).current_frame;
    assert!(
        (forward as i64 - (before as i64 + 4 * FRAMES_PER_BEAT as i64)).abs()
            < FRAMES_PER_BEAT as i64 / 2,
        "beatjump +4 from {before} landed at {forward}, expected ~{}",
        before + 4 * FRAMES_PER_BEAT
    );

    ask(
        &tx,
        Command::BeatJump {
            deck_id: 0,
            beats: -4,
        },
    );
    ack(&rx);
    std::thread::sleep(Duration::from_millis(250));
    let back = state(&tx, &rx, 0).current_frame;
    assert!(
        (back as i64 - before as i64).abs() < FRAMES_PER_BEAT as i64 / 2,
        "beatjump -4 must return to the starting phase: {before} -> {forward} -> {back}"
    );

    ask(&tx, Command::Quit);
    ack(&rx);
    let _ = fs::remove_file(path);
}

#[test]
fn two_decks_run_independently() {
    let (path, tx, rx, _pipeline) = session("dual0.wav", 6);
    let loaded = load_both(&tx, &rx, &path);
    assert_eq!(
        loaded,
        vec![(0, 6 * SAMPLE_RATE as u64), (1, 6 * SAMPLE_RATE as u64)]
    );
    seed_grids(&tx, &rx, 122.0, 6 * SAMPLE_RATE as u64);

    ask(&tx, Command::Play { deck_id: 0 });
    ack(&rx);
    std::thread::sleep(Duration::from_millis(300));
    ask(&tx, Command::Play { deck_id: 1 });
    ack(&rx);
    std::thread::sleep(Duration::from_millis(300));

    let deck0 = state(&tx, &rx, 0);
    let deck1 = state(&tx, &rx, 1);
    assert!(
        deck0.playing && deck1.playing,
        "both decks should be playing"
    );
    assert!(
        deck0.current_frame > deck1.current_frame,
        "deck 0 started earlier and must be ahead: {} vs {}",
        deck0.current_frame,
        deck1.current_frame
    );

    // Jumping one deck leaves the other on its own timeline.
    ask(
        &tx,
        Command::Jump {
            deck_id: 1,
            target_frame: 5 * SAMPLE_RATE as u64,
        },
    );
    ack(&rx);
    std::thread::sleep(Duration::from_millis(200));
    let deck1 = state(&tx, &rx, 1);
    assert!(
        deck1.current_frame >= 5 * SAMPLE_RATE as u64,
        "deck 1 jump lost: {deck1:?}"
    );
    let still_moving = state(&tx, &rx, 0).current_frame;
    assert!(
        still_moving > deck0.current_frame && still_moving < 2 * SAMPLE_RATE as u64,
        "deck 0 must be untouched by deck 1's jump, at {still_moving}"
    );

    // Pausing one deck does not pause the other.
    ask(&tx, Command::Pause { deck_id: 0 });
    ack(&rx);
    let held = state(&tx, &rx, 0);
    assert!(!held.playing);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        state(&tx, &rx, 0).current_frame,
        held.current_frame,
        "a paused deck must not drift"
    );
    assert!(
        state(&tx, &rx, 1).playing,
        "the other deck must keep playing"
    );

    ask(&tx, Command::Quit);
    ack(&rx);
    let _ = fs::remove_file(path);
}

#[test]
fn bad_deck_ids_and_missing_files_answer_with_errors() {
    let (_path, tx, rx, _pipeline) = session("errors.wav", 1);

    ask(&tx, Command::GetState { deck_id: 9 });
    match answer(&rx) {
        CommandResponse::Error(err) => assert!(err.contains("9"), "unexpected error: {err}"),
        other => panic!("expected Error, got {other:?}"),
    }

    ask(&tx, Command::Play { deck_id: 0 });
    match answer(&rx) {
        CommandResponse::Error(err) => assert!(err.contains("deck 0"), "unexpected: {err}"),
        other => panic!("expected Error for an empty deck, got {other:?}"),
    }

    ask(
        &tx,
        Command::Load {
            deck_id: 0,
            path: "/nope/nothing.wav".into(),
        },
    );
    match answer(&rx) {
        CommandResponse::Error(err) => {
            assert!(err.contains("nothing.wav"), "unexpected error: {err}")
        }
        other => panic!("expected Error for a missing file, got {other:?}"),
    }

    ask(&tx, Command::Quit);
    ack(&rx);
}
