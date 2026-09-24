//! Command-channel sessions covering the acceptance criteria for both decks.

mod common;

use std::time::Duration;

use common::{
    ack, answer, ask, constant_grid, decode_wav, load, load_both, seed_grids, session, state,
    states,
};
use hypermixx_audio::{Command, CommandResponse, SAMPLE_RATE};
use hypermixx_core::{DeckId, FaderTarget};

/// Frames in one beat at 122 BPM.
const FRAMES_PER_BEAT: u64 = (44_100.0f64 * 60.0 / 122.0).round() as u64;
const DECK0: DeckId = 0;

#[test]
fn load_play_beatjump_and_pause_on_one_deck() {
    let (tx, rx, _pipeline) = session();
    let total_frames = load(&tx, &rx, DECK0, decode_wav("deck0.wav", 6));
    assert_eq!(total_frames, 6 * SAMPLE_RATE as u64);

    // Install a deterministic grid so beatjump has a reference.
    ask(
        &tx,
        Command::SetAnalysis {
            deck_id: DECK0,
            analysis: constant_grid(122.0, total_frames),
        },
    );
    ack(&rx);

    let cued = state(&tx, &rx, DECK0);
    assert_eq!(
        (cued.deck_id, cued.current_frame, cued.playing),
        (DECK0, 0, false)
    );
    assert_eq!(
        cued.total_frames,
        6 * SAMPLE_RATE as u64,
        "state must report the deck length"
    );

    // play -> the playhead advances roughly in real time.
    ask(&tx, Command::Play { deck_id: DECK0 });
    ack(&rx);
    std::thread::sleep(Duration::from_millis(500));
    let rolling = state(&tx, &rx, DECK0);
    assert!(rolling.playing, "play should report playing");
    let nominal = 500u64 * SAMPLE_RATE as u64 / 1000;
    assert!(
        rolling.current_frame > nominal / 2 && rolling.current_frame < nominal * 2,
        "playhead advanced at the wrong rate: {} after 500ms (expected ~{nominal})",
        rolling.current_frame
    );

    // Pause so the beatjump target is measurable without playback drift.
    ask(&tx, Command::Pause { deck_id: DECK0 });
    ack(&rx);
    let before = state(&tx, &rx, DECK0).current_frame;

    ask(
        &tx,
        Command::BeatJump {
            deck_id: DECK0,
            beats: 4,
        },
    );
    ack(&rx);
    std::thread::sleep(Duration::from_millis(250)); // warm-up thread + block switch
    let forward = state(&tx, &rx, DECK0).current_frame;
    assert!(
        (forward as i64 - (before as i64 + 4 * FRAMES_PER_BEAT as i64)).abs()
            < FRAMES_PER_BEAT as i64 / 2,
        "beatjump +4 from {before} landed at {forward}, expected ~{}",
        before + 4 * FRAMES_PER_BEAT
    );

    ask(
        &tx,
        Command::BeatJump {
            deck_id: DECK0,
            beats: -4,
        },
    );
    ack(&rx);
    std::thread::sleep(Duration::from_millis(250));
    let back = state(&tx, &rx, DECK0).current_frame;
    assert!(
        (back as i64 - before as i64).abs() < FRAMES_PER_BEAT as i64 / 2,
        "beatjump -4 must return to the starting phase: {before} -> {forward} -> {back}"
    );

    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn two_decks_run_independently() {
    let (tx, rx, _pipeline) = session();
    let loaded = load_both(&tx, &rx, decode_wav("dual0.wav", 6));
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
}

#[test]
fn bad_deck_ids_and_empty_decks_answer_with_errors() {
    let (tx, rx, _pipeline) = session();

    ask(&tx, Command::GetState { deck_id: 9 });
    match answer(&rx) {
        CommandResponse::Error(err) => assert!(err.contains("9"), "unexpected error: {err}"),
        other => panic!("expected Error, got {other:?}"),
    }

    // Deck 0 holds no track yet, so transport is rejected rather than silently no-op.
    ask(&tx, Command::Play { deck_id: 0 });
    match answer(&rx) {
        CommandResponse::Error(err) => assert!(err.contains("deck 0"), "unexpected: {err}"),
        other => panic!("expected Error for an empty deck, got {other:?}"),
    }

    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn set_fader_reaches_the_mixer_through_the_command_channel() {
    let (tx, rx, _pipeline) = session();

    // Every channel-scoped and bus-scoped target is accepted at a block boundary.
    for target in [
        FaderTarget::Flow(0),
        FaderTarget::Deck(1),
        FaderTarget::CueSend(0),
        FaderTarget::Crossfader,
        FaderTarget::Master,
        FaderTarget::Cue,
    ] {
        ask(&tx, Command::SetFader { target, value: 0.25 });
        ack(&rx);
    }

    // An unknown deck is the one reportable failure.
    ask(
        &tx,
        Command::SetFader { target: FaderTarget::Flow(9), value: 0.0 },
    );
    match answer(&rx) {
        CommandResponse::Error(err) => assert!(err.contains('9'), "unexpected error: {err}"),
        other => panic!("expected Error, got {other:?}"),
    }

    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn states_are_sampled_in_one_block() {
    let (tx, rx, _pipeline) = session();
    load_both(&tx, &rx, decode_wav("atomic.wav", 3));
    ask(&tx, Command::Play { deck_id: 0 });
    ack(&rx);
    ask(&tx, Command::Play { deck_id: 1 });
    ack(&rx);

    let both = states(&tx, &rx);
    assert_eq!(
        both.len(),
        2,
        "GetAllStates answers for every deck in one reply"
    );
    ask(&tx, Command::Quit);
    ack(&rx);
}

/// The `--tui` front-end reads `meters()` every few frames; the producer must answer with a real
/// snapshot (and the master meter must be live while a deck plays).
#[test]
fn meters_answer_with_a_live_snapshot() {
    let (tx, rx, pipeline) = session();
    load(&tx, &rx, DECK0, decode_wav("meters.wav", 3));
    ask(&tx, Command::Play { deck_id: DECK0 });
    ack(&rx);
    std::thread::sleep(Duration::from_millis(250));
    let meters = pipeline.meters().expect("the engine should answer meters");
    assert!(meters.master_peak > 0.0, "master meter stayed silent while playing");
    assert!(meters.cue_peak >= 0.0);
    assert_eq!(meters.overruns, 0, "headless outputs cannot overrun");

    // Tests that keep a command sender alive must still quit explicitly: `AudioPipeline::drop`
    // joins the producer, which only returns on `Quit` or a disconnected channel.
    ask(&tx, Command::Quit);
    ack(&rx);
}
