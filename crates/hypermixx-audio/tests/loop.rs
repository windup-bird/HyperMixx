//! Loop integration tests: the `Command::Loop` family through the producer thread.
//!
//! Warm-up hops happen on another thread here (unlike the deck unit tests), so every engagement is
//! polled rather than assumed — the protocol's own observability (`DeckState.loop_range`,
//! `loop_in_armed`) is what the polls read, which doubles as a check that the state snapshot
//! carries the loop.
//!
//! The last test is the loop-exit regression: one deck laps a beat loop, checks its *virtual*
//! clock stayed locked to an untouched sibling while looping (proof that `virtual_pos` kept
//! counting), and then exits — the stream must resume **exactly where it stopped, inside the
//! loop**, play the lap out and flow past `out` without a seam. Exiting onto the slipped clock
//! (or skipping straight to `out`) is not this command.

mod common;

use std::time::Duration;

use common::{ack, answer, ask, decode_wav, load, load_both, seed_grids, session, state, states};
use crossbeam_channel::{Receiver, Sender};
use hypermixx_audio::{Command, CommandResponse, SAMPLE_RATE};
use hypermixx_core::{DeckState, LoopEditOp, LoopOp, LoopQuantum};

const BPM: f32 = 122.0;
/// One beat at the engine rate, as the grid lays it out (float — per-beat rounding stays honest).
const BEAT: f64 = SAMPLE_RATE as f64 * 60.0 / BPM as f64;

/// Polls a deck's state until `label` holds, or panics after ~3 s of warm-up slack.
fn wait_for(
    tx: &Sender<Command>,
    rx: &Receiver<CommandResponse>,
    deck_id: u8,
    label: &str,
    pred: impl Fn(&DeckState) -> bool,
) -> DeckState {
    for _ in 0..600 {
        let snapshot = state(tx, rx, deck_id);
        if pred(&snapshot) {
            return snapshot;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {label}");
}

fn ask_loop(tx: &Sender<Command>, deck_id: u8, op: LoopOp) {
    ask(tx, Command::Loop { deck_id, op });
}

/// A refused loop command must come back as an engine `Error`, not a silent ack.
fn expect_error(rx: &Receiver<CommandResponse>, label: &str) {
    match answer(rx) {
        CommandResponse::Error(_) => {}
        CommandResponse::Ok => panic!("{label}: the engine accepted a command it should refuse"),
        other => panic!("{label}: expected an error, got {other:?}"),
    }
}

#[test]
fn beat_loop_engages_edits_and_exits_through_the_protocol() {
    let (tx, rx, _pipeline) = session();
    let total = load(&tx, &rx, 0, decode_wav("loop-protocol.wav", 30));
    seed_grids(&tx, &rx, BPM, total);
    ask(&tx, Command::Play { deck_id: 0 });
    ack(&rx);

    // Engage a 4-beat loop; the warm-up hop is another thread, so poll the state for it.
    ask_loop(&tx, 0, LoopOp::Beats(4));
    ack(&rx);
    let engaged = wait_for(&tx, &rx, 0, "a 4-beat loop", |s| s.loop_range.is_some());
    let (in_frame, out_frame) = engaged.loop_range.expect("engaged");
    assert!(
        ((out_frame - in_frame) as f64 - 4.0 * BEAT).abs() <= 2.0,
        "four grid beats, got {}",
        out_frame - in_frame
    );

    // While the loop runs, the slip clock keeps counting: two samples a lap-fraction apart must
    // show `virtual` moving even though the audible position stays under `out`.
    let first = state(&tx, &rx, 0);
    std::thread::sleep(Duration::from_millis(400));
    let second = state(&tx, &rx, 0);
    assert!(
        second.virtual_frame > first.virtual_frame,
        "the slip clock must keep running: {} -> {}",
        first.virtual_frame,
        second.virtual_frame
    );
    assert!(
        second.current_frame <= out_frame && first.current_frame <= out_frame,
        "the audible position must stay under `out`: {} / {}",
        first.current_frame,
        second.current_frame
    );

    // In-place edit: shift two beats (ack + the range really moved — no flow change needed).
    ask_loop(&tx, 0, LoopOp::Edit(LoopEditOp::Move { beats: 2 }));
    ack(&rx);
    let moved = wait_for(&tx, &rx, 0, "the moved range", |s| {
        s.loop_range
            .is_some_and(|(start, _)| (start as i64 - in_frame as i64 - (2.0 * BEAT) as i64).abs() <= 2)
    });
    let moved_range = moved.loop_range.expect("still looping");
    assert_eq!(
        moved_range.1 - moved_range.0,
        out_frame - in_frame,
        "a move preserves the length"
    );

    // Halve through the beat-loop verb (decision 5): `in` stays, `out` comes in.
    ask_loop(&tx, 0, LoopOp::Beats(2));
    ack(&rx);
    let moved_in = moved_range.0;
    let halved = wait_for(&tx, &rx, 0, "the halved range", |s| {
        s.loop_range.is_some_and(|(start, end)| {
            start == moved_in && ((end - start) as f64 - 2.0 * BEAT).abs() <= 2.0
        })
    });
    let halved_len = halved.loop_range.expect("halved").1 - moved_in;

    // The dedicated ÷2/×2 keys through the protocol: exact frame scaling, `in` pinned.
    ask_loop(&tx, 0, LoopOp::Edit(LoopEditOp::Double));
    ack(&rx);
    wait_for(&tx, &rx, 0, "the doubled range", |s| {
        s.loop_range.is_some_and(|(start, end)| {
            start == moved_in && end - start == 2 * halved_len
        })
    });
    ask_loop(&tx, 0, LoopOp::Edit(LoopEditOp::Halve));
    ack(&rx);
    wait_for(&tx, &rx, 0, "halved back to the same length", |s| {
        s.loop_range.is_some_and(|(start, end)| {
            start == moved_in && end - start == halved_len
        })
    });

    // Quantum is a deck setting: it acks without disturbing the range.
    ask_loop(&tx, 0, LoopOp::SetQuantum(LoopQuantum::Quarter));
    ack(&rx);

    // Exit: the range dies with its flow, and the two clocks agree again (slip ≡ virtual, no map).
    ask_loop(&tx, 0, LoopOp::Exit);
    ack(&rx);
    let left = wait_for(&tx, &rx, 0, "the exit", |s| s.loop_range.is_none());
    assert_eq!(
        left.current_frame, left.virtual_frame,
        "after exit there is no mapping left"
    );

    // The producer only stops on `Quit` (the channel outlives the pipeline in these tests).
    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn manual_loop_arms_immediately_and_engages_on_out() {
    let (tx, rx, _pipeline) = session();
    let total = load(&tx, &rx, 0, decode_wav("loop-manual.wav", 30));
    seed_grids(&tx, &rx, BPM, total);
    ask(&tx, Command::Play { deck_id: 0 });
    ack(&rx);

    // `in` is immediate state: the in point is quantized and reported before any warm-up finishes.
    ask_loop(&tx, 0, LoopOp::In);
    ack(&rx);
    let armed = state(&tx, &rx, 0);
    let p_in = armed
        .loop_in_armed
        .expect("`loop in` must report the armed in point");
    let nearest_head = (p_in as f64 / BEAT).round() * BEAT;
    assert!(
        (p_in as f64 - nearest_head).abs() <= 1.0,
        "the in point sits on a beat head, got {p_in}"
    );
    assert!(armed.loop_range.is_none(), "arming alone must not engage");

    // `out` engages — whether by promoting the LoopFlow or (if it is still warming) by falling
    // back to a fresh ranged flow; either way the range appears without a clock jump backwards.
    let before = state(&tx, &rx, 0).virtual_frame;
    ask_loop(&tx, 0, LoopOp::Out);
    ack(&rx);
    let engaged = wait_for(&tx, &rx, 0, "the manual loop", |s| s.loop_range.is_some());
    let (start, end) = engaged.loop_range.expect("engaged");
    assert_eq!(start, p_in, "the loop starts at the armed in point");
    assert!(end > start);
    assert!(
        engaged.virtual_frame >= before,
        "engaging must never move the slip clock: {before} -> {}",
        engaged.virtual_frame
    );

    // A second `out` is refused: nothing is armed any more.
    ask_loop(&tx, 0, LoopOp::Out);
    expect_error(&rx, "out without an armed in");

    // The producer only stops on `Quit` (the channel outlives the pipeline in these tests).
    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn loop_commands_refuse_without_a_beat_grid() {
    let (tx, rx, _pipeline) = session();
    let total = load(&tx, &rx, 0, decode_wav("loop-nogrid.wav", 10));
    assert!(total > 0);
    ask(&tx, Command::Play { deck_id: 0 });
    ack(&rx);

    for (label, op) in [
        ("in", LoopOp::In),
        ("out", LoopOp::Out),
        ("beat loop", LoopOp::Beats(4)),
        ("edit", LoopOp::Edit(LoopEditOp::Move { beats: 1 })),
    ] {
        ask_loop(&tx, 0, op);
        expect_error(&rx, label);
    }

    // The idempotent pair works regardless: cancelling an unarmed arm is a no-op, not an error.
    ask_loop(&tx, 0, LoopOp::Cancel);
    ack(&rx);
    assert!(state(&tx, &rx, 0).loop_in_armed.is_none());

    // The producer only stops on `Quit` (the channel outlives the pipeline in these tests).
    ask(&tx, Command::Quit);
    ack(&rx);
}

#[test]
fn loop_exit_is_seamless_while_the_slip_clock_keeps_pace_with_a_sibling() {
    // The exit regression (and, while looping, the slip-clock one): deck0 plays untouched, deck1
    // laps a 4-beat loop and exits. Both decks started in the same production block, so while the
    // loop runs deck1's *virtual* must equal deck0's position — the clock never stopped counting.
    // The exit then resumes **exactly where the looped flow stopped, inside the loop**: it plays
    // the lap out and flows past `out` with no jump — not a skip to `out`, not the slipped clock.
    let (tx, rx, _pipeline) = session();
    let total_frames = load_both(&tx, &rx, decode_wav("loop-slip.wav", 30))[0].1;
    seed_grids(&tx, &rx, BPM, total_frames);

    // Both plays queued before either ack, so the decks start on the same block (beatlock rule).
    ask(&tx, Command::Play { deck_id: 0 });
    ask(&tx, Command::Play { deck_id: 1 });
    ack(&rx);
    ack(&rx);

    ask_loop(&tx, 1, LoopOp::Beats(4));
    ack(&rx);
    let engaged = wait_for(&tx, &rx, 1, "deck1's loop", |s| s.loop_range.is_some());
    let (in_frame, out_frame) = engaged.loop_range.expect("engaged");

    // Let it lap two loop lengths' worth of real time, plus margin for the warm-up hop.
    let beats = |n: f64| Duration::from_secs_f64(n * BEAT / f64::from(SAMPLE_RATE));
    std::thread::sleep(beats(8.0) + Duration::from_millis(400));
    let lapped = wait_for(&tx, &rx, 1, "two laps past `out`", |s| {
        s.virtual_frame as f64 > out_frame as f64 + 4.0 * BEAT
    });
    assert!(
        lapped.current_frame < out_frame && lapped.current_frame >= in_frame,
        "deck1 must be audibly inside the loop: {}",
        lapped.current_frame
    );
    // While looping, the slip clock and the untouched sibling's position are the same number:
    // both decks started together and neither clock stopped.
    let pair = states(&tx, &rx);
    assert_eq!(pair.len(), 2);
    assert!(
        (pair[1].virtual_frame as i64 - pair[0].current_frame as i64).abs() <= 4,
        "the slipped clock must track the sibling: virtual {} vs {}",
        pair[1].virtual_frame,
        pair[0].current_frame
    );

    // Exit: resume *where the looped flow's output stopped* — inside the loop, so the listener
    // plays the lap out and flows past `out` without ever hearing a jump (let alone an instant
    // skip to `out`, or the slipped clock's whole laps).
    let pre_exit = pair[1].current_frame;
    ask_loop(&tx, 1, LoopOp::Exit);
    ack(&rx);
    let left = wait_for(&tx, &rx, 1, "the exit", |s| s.loop_range.is_none());
    assert!(
        left.current_frame >= in_frame && left.current_frame < out_frame,
        "exit resumes inside the loop: {} (loop {in_frame}-{out_frame})",
        left.current_frame
    );
    // Only the exit flow's warm-up may pass between the press and the switch, wrapped into the
    // loop's own coordinates.
    let len = (out_frame - in_frame) as i64;
    let advanced = (left.current_frame as i64 - pre_exit as i64).rem_euclid(len);
    assert!(
        advanced <= SAMPLE_RATE as i64 / 2,
        "only the warm-up may pass between press and switch: {advanced} frames"
    );
    assert_eq!(
        left.current_frame, left.virtual_frame,
        "no mapping left after the exit"
    );
    // …and with nothing wrapping it, that stream plays the rest of the lap out and continues
    // past the loop's end on its own.
    wait_for(
        &tx,
        &rx,
        1,
        "playing past the loop's end",
        |s| s.current_frame > out_frame,
    );

    // The producer only stops on `Quit` (the channel outlives the pipeline in these tests).
    ask(&tx, Command::Quit);
    ack(&rx);
}
