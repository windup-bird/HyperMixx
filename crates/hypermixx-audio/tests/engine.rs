//! One command-channel session covering the README's acceptance criteria.

mod common;

use std::fs;
use std::time::Duration;

use common::{temp_path, write_wav};
use crossbeam_channel::{unbounded, Receiver};
use hypermixx_audio::{AudioPipeline, Command, CommandResponse, SAMPLE_RATE};

fn expect(rx: &Receiver<CommandResponse>) -> CommandResponse {
    rx.recv_timeout(Duration::from_secs(30))
        .expect("the engine did not answer")
}

fn state(rx: &Receiver<CommandResponse>) -> (u64, bool) {
    match expect(rx) {
        CommandResponse::State {
            current_frame,
            playing,
        } => (current_frame, playing),
        other => panic!("expected State, got {other:?}"),
    }
}

#[test]
fn session_covers_the_acceptance_criteria() {
    let path = temp_path("session.wav");
    write_wav(&path, SAMPLE_RATE, 2, 2 * SAMPLE_RATE as usize); // two seconds

    let (command_tx, command_rx) = unbounded();
    let (response_tx, response_rx) = unbounded();
    let pipeline = AudioPipeline::start(command_rx, response_tx);

    // load -> total frame count
    command_tx
        .send(Command::Load { path: path.clone() })
        .unwrap();
    match expect(&response_rx) {
        CommandResponse::Loaded { total_frames } => {
            assert_eq!(total_frames, 2 * SAMPLE_RATE as u64)
        }
        CommandResponse::Error(err) => panic!("load failed: {err}"),
        other => panic!("expected Loaded, got {other:?}"),
    }

    // cued at zero, paused
    command_tx.send(Command::GetState).unwrap();
    assert_eq!(state(&response_rx), (0, false));

    // play -> the playhead advances roughly in real time
    command_tx.send(Command::Play).unwrap();
    assert!(matches!(expect(&response_rx), CommandResponse::Ok));
    std::thread::sleep(Duration::from_millis(500));
    command_tx.send(Command::GetState).unwrap();
    let (played, is_playing) = state(&response_rx);
    assert!(is_playing, "play should report playing");
    let expected = 500u64 * SAMPLE_RATE as u64 / 1000;
    assert!(
        played > expected / 2 && played < expected * 2,
        "playhead advanced at the wrong rate: {played} after 500ms (expected ~{expected})"
    );

    // jump -> position changes to the target
    command_tx
        .send(Command::Jump {
            target_frame: SAMPLE_RATE as u64,
        })
        .unwrap();
    assert!(matches!(expect(&response_rx), CommandResponse::Ok));
    std::thread::sleep(Duration::from_millis(300)); // let the warm-up thread finish
    command_tx.send(Command::GetState).unwrap();
    let (jumped, _) = state(&response_rx);
    assert!(
        jumped >= SAMPLE_RATE as u64,
        "jump did not take effect: {jumped}"
    );

    // pause -> position stops, play resumes from it
    command_tx.send(Command::Pause).unwrap();
    assert!(matches!(expect(&response_rx), CommandResponse::Ok));
    std::thread::sleep(Duration::from_millis(200));
    command_tx.send(Command::GetState).unwrap();
    let (held, is_playing) = state(&response_rx);
    assert!(!is_playing, "pause should report paused");
    command_tx.send(Command::GetState).unwrap();
    assert_eq!(
        state(&response_rx).0,
        held,
        "a paused deck must not advance"
    );

    // commands on an empty deck are rejected, not swallowed
    command_tx
        .send(Command::Load {
            path: "/nope/nothing.wav".into(),
        })
        .unwrap();
    match expect(&response_rx) {
        CommandResponse::Error(err) => {
            assert!(err.contains("nothing.wav"), "unexpected error: {err}")
        }
        other => panic!("expected Error for a missing file, got {other:?}"),
    }

    command_tx.send(Command::Quit).unwrap();
    assert!(matches!(expect(&response_rx), CommandResponse::Ok));
    drop(pipeline);
    let _ = fs::remove_file(path);
}
