//! Beat-sync + nudge: the acceptance criteria, driven through the same command channel the CLI uses.
//!
//! Every test ends with `Quit` — the harness drops the pipeline before it drops the command
//! sender, so a producer that is still waiting on that sender would never be joinable.

mod common;

use std::time::Duration;

use common::{ack, answer, ask, constant_grid, decode_wav, load_both, session, state, states};
use crossbeam_channel::{Receiver, Sender};
use hypermixx_audio::deck::wrap_phase;
use hypermixx_audio::{
    BeatGrid, Command, CommandResponse, DeckId, NudgeOp, PhaseMode, SyncOp, SAMPLE_RATE,
};

const BPM: f32 = 122.0;
/// How long a flow switch needs before its landing is readable.
const SWITCH_MS: u64 = 350;

fn seed(
    tx: &Sender<Command>,
    rx: &Receiver<CommandResponse>,
    deck_id: DeckId,
    bpm: f32,
    total: u64,
) {
    ask(
        tx,
        Command::SetAnalysis {
            deck_id,
            analysis: constant_grid(bpm, total),
        },
    );
    ack(rx);
}

fn expect_error(rx: &Receiver<CommandResponse>) -> String {
    match answer(rx) {
        CommandResponse::Error(message) => message,
        other => panic!("expected an error, got {other:?}"),
    }
}

/// leader phase − follower phase, folded into `[-0.5, 0.5]`.
fn phase_gap(grid: &BeatGrid, leader: u64, follower: u64) -> f64 {
    wrap_phase(f64::from(grid.phase(leader)) - f64::from(grid.phase(follower)))
}

fn sleep(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

// ---------------------------------------------------------------- sync tempo

#[test]
fn sync_tempo_puts_both_decks_on_one_beat_rate() {
    let (tx, rx, _pipeline) = session();
    let total = load_both(&tx, &rx, decode_wav("sync-tempo.wav", 14))[0].1;
    seed(&tx, &rx, 0, 122.0, total);
    seed(&tx, &rx, 1, 128.0, total);

    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::Tempo,
        },
    );
    ack(&rx);

    let follower = state(&tx, &rx, 1);
    let leader = state(&tx, &rx, 0);
    let expected = 122.0f64 / 128.0;
    assert!(
        (f64::from(follower.tempo) - expected).abs() < 1e-4,
        "follower tempo {} must be leader_bpm / own_bpm = {expected}",
        follower.tempo
    );
    assert!(!follower.lock, "a one-shot match must not lock anything");
    assert_eq!(follower.align, None, "tempo alone adds no phase correction");
    assert_eq!(follower.sync_mode, "free");

    // The point of the match: both decks now produce the same number of beats per second. Both
    // sides are measured with `bpm_at_frame` — the beat actually under the playhead — because
    // that is what the command matched, and `DeckState::bpm` reports the grid's *nominal* tempo
    // instead (which differs by the grid's per-beat frame rounding).
    let grid_a = BeatGrid::from_constant_bpm(122.0, 0, total, SAMPLE_RATE);
    let grid_b = BeatGrid::from_constant_bpm(128.0, 0, total, SAMPLE_RATE);
    let leader_beats = f64::from(grid_a.bpm_at_frame(leader.current_frame)) * f64::from(leader.tempo);
    let follower_beats =
        f64::from(grid_b.bpm_at_frame(follower.current_frame)) * f64::from(follower.tempo);
    assert!(
        (leader_beats - follower_beats).abs() < 1e-3,
        "beat rates must match: {leader_beats} vs {follower_beats}"
    );

    ask(&tx, Command::Quit);
    ack(&rx);
}

// ---------------------------------------------------------------- tempolock

#[test]
fn tempolock_shares_one_tempo_across_either_fader() {
    let (tx, rx, _pipeline) = session();
    let total = load_both(&tx, &rx, decode_wav("sync-lock.wav", 20))[0].1;
    seed(&tx, &rx, 0, BPM, total);
    seed(&tx, &rx, 1, BPM, total);

    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::TempoLock,
        },
    );
    ack(&rx);
    let locked = states(&tx, &rx);
    assert!(locked[0].lock && locked[1].lock, "tempolock drives both decks");
    assert_eq!(locked[0].sync_mode, "tempolock");
    assert!((locked[0].group_bpm - BPM).abs() < 0.01);
    assert!(
        (f64::from(locked[1].tempo) - 1.0).abs() < 1e-3,
        "locking must not nudge anything: {}",
        locked[1].tempo
    );

    // The leader's fader moves the group; the follower picks it up on the next block.
    ask(&tx, Command::SetRate { deck_id: 0, rate: 1.1 });
    ack(&rx);
    sleep(250);
    let moved = states(&tx, &rx);
    assert!((f64::from(moved[1].tempo) - 1.1).abs() < 1e-3, "the other side must follow: {}", moved[1].tempo);
    assert!((moved[0].group_bpm - 1.1 * 122.0).abs() < 0.1);

    // …and it is genuinely bidirectional: the follower's fader moves the leader.
    ask(&tx, Command::SetRate { deck_id: 1, rate: 0.95 });
    ack(&rx);
    sleep(250);
    let back = states(&tx, &rx);
    assert!((f64::from(back[0].tempo) - 0.95).abs() < 1e-3, "the lock is two-way: {}", back[0].tempo);
    assert!((f64::from(back[1].tempo) - 0.95).abs() < 1e-3);

    ask(&tx, Command::Quit);
    ack(&rx);
}

// ---------------------------------------------------------------- phaselock

#[test]
fn phaselock_ignores_the_followers_fader_and_follows_the_leader() {
    let (tx, rx, _pipeline) = session();
    let total = load_both(&tx, &rx, decode_wav("sync-phase-lock.wav", 20))[0].1;
    seed(&tx, &rx, 0, BPM, total);
    seed(&tx, &rx, 1, BPM, total);

    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::PhaseLock {
                mode: PhaseMode::Pid,
                t_seconds: None,
            },
        },
    );
    ack(&rx);
    let locked = states(&tx, &rx);
    assert_eq!(locked[1].sync_mode, "phaselock");
    assert!(locked[1].lock, "the follower is group-driven");
    assert!(
        !locked[0].lock,
        "the leader is the source of truth, not a group member"
    );
    assert_eq!(locked[1].align.as_deref(), Some("pid"));
    assert_eq!(locked[1].sync_leader, Some(0), "the follower reports who it tracks");
    assert_eq!(locked[0].sync_leader, None, "the leader tracks nobody");

    // One-way: the follower's own fader is overridden.
    ask(&tx, Command::SetRate { deck_id: 1, rate: 0.9 });
    ack(&rx);
    sleep(250);
    let ignored = states(&tx, &rx);
    assert!(
        (f64::from(ignored[1].tempo) - 1.0).abs() < 1e-3,
        "a phaselocked follower's fader must be ignored, got {}",
        ignored[1].tempo
    );

    // The leader's fader *is* the group's tempo, so the follower tracks it.
    ask(&tx, Command::SetRate { deck_id: 0, rate: 1.05 });
    ack(&rx);
    sleep(250);
    let followed = states(&tx, &rx);
    assert!((f64::from(followed[0].tempo) - 1.05).abs() < 1e-3);
    assert!(
        (f64::from(followed[1].tempo) - 1.05).abs() < 1e-3,
        "the follower must track the leader: {}",
        followed[1].tempo
    );

    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn a_nudge_still_works_on_a_phaselocked_follower() {
    let (tx, rx, _pipeline) = session();
    let total = load_both(&tx, &rx, decode_wav("sync-nudge-lock.wav", 20))[0].1;
    seed(&tx, &rx, 0, BPM, total);
    seed(&tx, &rx, 1, BPM, total);

    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::PhaseLock {
                mode: PhaseMode::Pid,
                t_seconds: None,
            },
        },
    );
    ack(&rx);

    // The bend rides `nudgerate`, which the lock does not own — the DJ can still hand-correct.
    ask(
        &tx,
        Command::Nudge {
            deck_id: 1,
            op: NudgeOp::Start {
                delta: 0.05,
                seconds: None,
            },
        },
    );
    ack(&rx);
    sleep(400);
    let bent = states(&tx, &rx);
    assert!(bent[1].nudge > 0.03, "the follower must still be able to bend: {}", bent[1].nudge);
    assert!(
        (f64::from(bent[1].tempo) - 1.0).abs() < 1e-3,
        "a nudge must not disturb the locked tempo: {}",
        bent[1].tempo
    );

    ask(
        &tx,
        Command::Nudge {
            deck_id: 1,
            op: NudgeOp::Stop,
        },
    );
    ack(&rx);
    sleep(500);
    let released = states(&tx, &rx);
    assert!(released[1].nudge.abs() < 1e-4, "still bent: {}", released[1].nudge);

    ask(&tx, Command::Quit);
    ack(&rx);
}

// ---------------------------------------------------------------- phase correction

#[test]
fn phase_pid_closes_a_staggered_start_and_parks() {
    let (tx, rx, _pipeline) = session();
    let total = load_both(&tx, &rx, decode_wav("sync-phase.wav", 60))[0].1;
    seed(&tx, &rx, 0, BPM, total);
    seed(&tx, &rx, 1, BPM, total);
    let grid = BeatGrid::from_constant_bpm(BPM, 0, total, SAMPLE_RATE);

    // Start them one after the other, which is exactly how a beat gap appears in a set.
    ask(&tx, Command::Play { deck_id: 0 });
    ack(&rx);
    sleep(500);
    ask(&tx, Command::Play { deck_id: 1 });
    ack(&rx);
    sleep(250);

    let started = states(&tx, &rx);
    let initial = phase_gap(&grid, started[0].current_frame, started[1].current_frame);
    assert!(initial.abs() > 0.05, "a staggered start must leave a real gap: {initial} beats");

    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::Phase {
                mode: PhaseMode::Pid,
                t_seconds: None,
            },
        },
    );
    ack(&rx);
    // Read the tempo the *command* set, so the assertion below is about the controller rather
    // than about `sync tempo`'s own arithmetic: the match reads the beat under each playhead, and
    // a constant grid's per-beat widths alternate 21688/21689 frames by rounding, so an identical
    // grid pair does not match to a bit-exact 1.0.
    let synced = state(&tx, &rx, 1);
    let synced_tempo = f64::from(synced.tempo);
    assert_eq!(synced.align.as_deref(), Some("pid"));

    let mut settled = 0u32;
    for _ in 0..90 {
        sleep(250);
        let rolling = states(&tx, &rx);
        let here = phase_gap(&grid, rolling[0].current_frame, rolling[1].current_frame);
        if here.abs() < 0.01 && rolling[1].nudgerate.abs() < 0.005 {
            settled += 1;
            if settled >= 3 {
                break;
            }
        } else {
            settled = 0;
        }
    }

    let finished = states(&tx, &rx);
    let gap = phase_gap(&grid, finished[0].current_frame, finished[1].current_frame);
    assert!(
        gap.abs() < 0.01,
        "the phase gap must close (started at {initial} beats): {gap} beats"
    );
    assert!(
        finished[1].nudgerate.abs() < 0.005,
        "a converged controller must park at zero, still {}",
        finished[1].nudgerate
    );
    assert!(
        (f64::from(finished[1].tempo) - synced_tempo).abs() < 1e-6,
        "phase correction must never touch the tempo: {} vs {synced_tempo}",
        finished[1].tempo
    );
    assert_eq!(finished[1].align.as_deref(), Some("pid"));

    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn an_instant_phase_sync_is_a_jump_and_leaves_no_controller() {
    let (tx, rx, _pipeline) = session();
    let total = load_both(&tx, &rx, decode_wav("sync-instant.wav", 20))[0].1;
    seed(&tx, &rx, 0, BPM, total);
    seed(&tx, &rx, 1, BPM, total);
    let grid = BeatGrid::from_constant_bpm(BPM, 0, total, SAMPLE_RATE);

    // Park the follower 0.35 into a beat while the leader sits on beat 0. Paused, so this is
    // exact: no warm-up compensation, no drift between the two reads.
    let beat = grid.beat_width(6);
    let target = grid.frame_at_beat(6) + (0.35 * beat as f64).round() as u64;
    ask(&tx, Command::Jump { deck_id: 1, target_frame: target });
    ack(&rx);
    sleep(SWITCH_MS);

    let offset = states(&tx, &rx);
    let initial = phase_gap(&grid, offset[0].current_frame, offset[1].current_frame);
    assert!(
        (initial + 0.35).abs() < 0.01,
        "the follower should sit 0.35 beats behind, got {initial}"
    );

    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::Phase {
                mode: PhaseMode::Instant,
                t_seconds: None,
            },
        },
    );
    ack(&rx);
    sleep(SWITCH_MS);

    let landed = states(&tx, &rx);
    let gap = phase_gap(&grid, landed[0].current_frame, landed[1].current_frame);
    assert!(gap.abs() < 0.02, "instant must land on the leader's phase: {gap} beats");
    assert_eq!(
        landed[1].align, None,
        "instant is a jump, so no controller may be left running"
    );
    assert!(
        landed[1].nudgerate.abs() < 1e-4,
        "nothing may still be bending: {}",
        landed[1].nudgerate
    );

    ask(&tx, Command::Quit);
    ack(&rx);
}

// ---------------------------------------------------------------- unlock

#[test]
fn unlock_drops_the_lock_and_keeps_the_tempo() {
    let (tx, rx, _pipeline) = session();
    let total = load_both(&tx, &rx, decode_wav("sync-unlock.wav", 20))[0].1;
    seed(&tx, &rx, 0, BPM, total);
    seed(&tx, &rx, 1, BPM, total);

    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::TempoLock,
        },
    );
    ack(&rx);
    ask(&tx, Command::SetRate { deck_id: 0, rate: 1.1 });
    ack(&rx);
    sleep(250);
    let locked = states(&tx, &rx);
    assert!((f64::from(locked[1].tempo) - 1.1).abs() < 1e-3);

    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::Unlock,
        },
    );
    ack(&rx);
    sleep(250);
    let free = states(&tx, &rx);

    for deck in &free {
        assert!(
            (f64::from(deck.tempo) - f64::from(locked[deck.deck_id as usize].tempo)).abs() < 1e-6,
            "unlock keeps the speed ({} vs {})",
            deck.tempo,
            locked[deck.deck_id as usize].tempo
        );
        assert!(!deck.lock, "deck{} must be unlocked", deck.deck_id);
        assert_eq!(deck.align, None);
        assert!(deck.nudgerate.abs() < 1e-6, "still bending: {}", deck.nudgerate);
        assert_eq!(deck.sync_mode, "free");
        assert_eq!(deck.group_bpm, 0.0);
    }

    // The pair is genuinely independent again: one side's fader no longer moves the other.
    ask(&tx, Command::SetRate { deck_id: 0, rate: 0.8 });
    ack(&rx);
    sleep(250);
    let separate = states(&tx, &rx);
    assert!((f64::from(separate[0].tempo) - 0.8).abs() < 1e-3);
    assert!(
        (f64::from(separate[1].tempo) - 1.1).abs() < 1e-3,
        "after unlock the follower must stand alone: {}",
        separate[1].tempo
    );

    ask(&tx, Command::Quit);
    ack(&rx);
}

// ---------------------------------------------------------------- nudge

#[test]
fn nudge_bends_the_rate_then_returns_without_touching_the_tempo() {
    let (tx, rx, _pipeline) = session();
    let total = load_both(&tx, &rx, decode_wav("nudge.wav", 20))[0].1;
    seed(&tx, &rx, 0, BPM, total);
    seed(&tx, &rx, 1, BPM, total);

    ask(&tx, Command::Play { deck_id: 0 });
    ack(&rx);
    sleep(200);
    let before = state(&tx, &rx, 0);

    ask(
        &tx,
        Command::Nudge {
            deck_id: 0,
            op: NudgeOp::Start {
                delta: 0.06,
                seconds: None,
            },
        },
    );
    ack(&rx);
    sleep(400);
    let bent = state(&tx, &rx, 0);
    assert!((f64::from(bent.nudge) - 0.06).abs() < 0.01, "bend must reach its target: {}", bent.nudge);
    assert!(
        (f64::from(bent.tempo) - f64::from(before.tempo)).abs() < 1e-6,
        "a nudge must never write the tempo: {} vs {}",
        bent.tempo,
        before.tempo
    );
    assert!(
        (f64::from(bent.playing_rate) - (f64::from(bent.tempo) + f64::from(bent.nudgerate))).abs()
            < 1e-5,
        "playing rate must be tempo + nudgerate"
    );

    // A timed release runs on its own; no second command needed.
    ask(
        &tx,
        Command::Nudge {
            deck_id: 0,
            op: NudgeOp::Start {
                delta: -0.05,
                seconds: Some(0.3),
            },
        },
    );
    ack(&rx);
    sleep(1_500);
    let timed = state(&tx, &rx, 0);
    assert!(timed.nudge.abs() < 1e-4, "a timed bend must release itself: {}", timed.nudge);
    assert!(
        (f64::from(timed.tempo) - f64::from(before.tempo)).abs() < 1e-6,
        "tempo survives the bend: {} vs {}",
        timed.tempo,
        before.tempo
    );

    ask(&tx, Command::Quit);
    ack(&rx);
}

// ---------------------------------------------------------------- refusals

#[test]
fn sync_refuses_a_deck_that_cannot_be_compared() {
    let (tx, rx, _pipeline) = session();
    load_both(&tx, &rx, decode_wav("sync-refuse.wav", 8));

    // Neither deck has a grid: there is nothing to measure a tempo or a phase against.
    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::Tempo,
        },
    );
    let message = expect_error(&rx);
    assert!(message.contains("beat grid"), "unhelpful refusal: {message}");

    // A leader without a grid is its own failure: matching to a silent zero would be worse.
    seed(&tx, &rx, 1, BPM, u64::from(8 * SAMPLE_RATE));
    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::Tempo,
        },
    );
    let message = expect_error(&rx);
    assert!(
        message.contains("leader deck0") && message.contains("grid"),
        "unhelpful refusal: {message}"
    );

    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn two_decks_may_not_sync_to_each_other() {
    let (tx, rx, _pipeline) = session();
    let total = load_both(&tx, &rx, decode_wav("sync-mutual.wav", 14))[0].1;
    seed(&tx, &rx, 0, BPM, total);
    seed(&tx, &rx, 1, BPM, total);

    // Naming a leader, then syncing the other deck to it, is the legal direction.
    ask(
        &tx,
        Command::Sync {
            deck_id: 0,
            op: SyncOp::SetLeader,
        },
    );
    ack(&rx);
    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::Tempo,
        },
    );
    ack(&rx);

    // Turning around and syncing the leader to its own follower closes the loop: refused.
    ask(
        &tx,
        Command::Sync {
            deck_id: 0,
            op: SyncOp::Tempo,
        },
    );
    let message = expect_error(&rx);
    assert!(message.contains("follow each other"), "unhelpful refusal: {message}");

    // An explicitly named leader also refuses to sync to itself.
    ask(
        &tx,
        Command::Sync {
            deck_id: 0,
            op: SyncOp::SetLeader,
        },
    );
    ack(&rx);
    ask(
        &tx,
        Command::Sync {
            deck_id: 0,
            op: SyncOp::Tempo,
        },
    );
    let message = expect_error(&rx);
    assert!(message.contains("is the leader"), "unhelpful refusal: {message}");

    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn a_one_shot_tempo_match_is_refused_once_the_pair_is_locked() {
    let (tx, rx, _pipeline) = session();
    let total = load_both(&tx, &rx, decode_wav("sync-locked-tempo.wav", 14))[0].1;
    seed(&tx, &rx, 0, 122.0, total);
    seed(&tx, &rx, 1, 128.0, total);

    ask(&tx, Command::Sync { deck_id: 1, op: SyncOp::Tempo });
    ack(&rx);
    ask(&tx, Command::Sync { deck_id: 1, op: SyncOp::TempoLock });
    ack(&rx);
    let locked = states(&tx, &rx);
    let locked_tempo = f64::from(locked[1].tempo);

    // Under a lock the one-shot match would be overwritten by the group recompute on the next
    // block — answering `Ok` while doing nothing is worse than refusing.
    ask(&tx, Command::Sync { deck_id: 1, op: SyncOp::Tempo });
    let message = expect_error(&rx);
    assert!(message.contains("already locked"), "{message}");

    sleep(250);
    let unchanged = states(&tx, &rx);
    assert!(
        (f64::from(unchanged[1].tempo) - locked_tempo).abs() < 1e-6,
        "a refused match must leave the tempo alone: {} vs {locked_tempo}",
        unchanged[1].tempo
    );
    assert_eq!(unchanged[1].sync_mode, "tempolock");

    // Changing the phase mode *while* locked stays legal: the group owns the tempo already, so
    // only the correction is installed — and the lock must survive it.
    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::Phase {
                mode: PhaseMode::Pid,
                t_seconds: None,
            },
        },
    );
    ack(&rx);
    sleep(250);
    let corrected = states(&tx, &rx);
    assert_eq!(corrected[1].align.as_deref(), Some("pid"));
    assert!(corrected[1].lock, "adding a correction must not drop the lock");
    assert_eq!(corrected[1].sync_mode, "tempolock");
    assert!(
        (f64::from(corrected[1].tempo) - locked_tempo).abs() < 1e-6,
        "the group still owns the tempo: {} vs {locked_tempo}",
        corrected[1].tempo
    );

    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn a_linear_phase_sync_rejects_a_nonsense_duration() {
    let (tx, rx, _pipeline) = session();
    let total = load_both(&tx, &rx, decode_wav("sync-linear.wav", 14))[0].1;
    seed(&tx, &rx, 0, BPM, total);
    seed(&tx, &rx, 1, BPM, total);

    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::Phase {
                mode: PhaseMode::Linear,
                t_seconds: Some(0.0),
            },
        },
    );
    let message = expect_error(&rx);
    assert!(message.contains("positive duration"), "unhelpful refusal: {message}");

    // …and a good one applies: tempo matched, controller installed, still free of any lock.
    ask(
        &tx,
        Command::Sync {
            deck_id: 1,
            op: SyncOp::Phase {
                mode: PhaseMode::Linear,
                t_seconds: Some(2.0),
            },
        },
    );
    ack(&rx);
    let applied = state(&tx, &rx, 1);
    assert_eq!(applied.align.as_deref(), Some("linear"));
    assert_eq!(applied.sync_mode, "free");
    assert!(!applied.lock);

    ask(&tx, Command::Quit);
    ack(&rx);
}
