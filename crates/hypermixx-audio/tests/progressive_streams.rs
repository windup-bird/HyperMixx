//! A growing source, end to end at the deck: the audio is the fallback until a block lands, and a
//! block that lands is audible **through the handles the deck already holds** — no reinstall, no
//! restart, no gap.
//!
//! Nothing here is stem-specific: `ChunkedStreams` grows *n* streams and the deck plays them. Stems
//! are simply the first user (four streams whose fallback is the mix); a progressive load, a
//! streamed file or any future producer of blocks gets the same behaviour for free. The stem-facing
//! view of it is `StemStatus::separated`; what is checked here is the audio: the sum of the streams,
//! which is what a channel at unity sends to the master.

use std::sync::Arc;

use hypermixx_audio::{Deck, BLOCK_SIZE, SAMPLE_RATE};
use hypermixx_core::{Source, CHANNELS};
use hypermixx_media::{ChunkedStreams, DecodedAudio, PcmPool, DEFAULT_FADE_FRAMES};

const FRAMES: u64 = 200_000;

/// A constant DC mix: the sum of the streams is then a plain number, so "which audio is playing"
/// has an exact answer.
fn mix(value: f32) -> Arc<dyn Source> {
    Arc::new(PcmPool::from_decoded(DecodedAudio {
        pcm: (0..FRAMES as usize)
            .flat_map(|_| [value, value])
            .collect(),
        total_frames: FRAMES,
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
    }))
}

/// Renders one block and returns the (left, right) sample of its first frame.
fn render(deck: &mut Deck) -> (f32, f32) {
    let mut out = vec![0.0f32; BLOCK_SIZE * CHANNELS];
    deck.process_block(&mut out);
    (out[0], out[1])
}

/// Renders until the deck has warmed its new flow into `streams` streams (the switch happens on the
/// warm-up thread, a few blocks after `set_sources`).
fn settle(deck: &mut Deck, streams: usize) {
    for _ in 0..500 {
        render(deck);
        if deck.stream_count() == streams {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("the new flow never landed (stream_count {})", deck.stream_count());
}

/// Renders until the sum reaches `want`, or gives up after `limit` blocks (the engine's read-ahead
/// means a change is heard a few blocks after it is made, not on the next one).
fn render_until(deck: &mut Deck, want: f32, limit: usize) -> bool {
    for _ in 0..limit {
        if (render(deck).0 - want).abs() < 1e-5 {
            return true;
        }
    }
    false
}

/// One block: `frames` frames per stream, stream `i` carrying `values[i]`.
fn block(frames: usize, values: [f32; 4]) -> Vec<Arc<Vec<f32>>> {
    values
        .iter()
        .map(|value| Arc::new(vec![*value; frames * CHANNELS]))
        .collect()
}

#[test]
fn a_block_lands_in_a_deck_that_is_already_playing() {
    let original = mix(0.5);
    let growing = ChunkedStreams::new(4, FRAMES, Some(Arc::clone(&original)), DEFAULT_FADE_FRAMES);
    let mut deck = Deck::new(Arc::clone(&original));
    deck.play();
    // The install: four streams, none of them separated yet.
    deck.set_sources(growing.streams());
    settle(&mut deck, 4);
    assert_eq!(deck.stream_count(), 4, "the set is installed immediately");
    assert_eq!(growing.available_frames(), 0);
    // The track — what analysis and the waveform read — is still the mix, not stream 0.
    assert_eq!(deck.source().total_frames(), FRAMES);

    // The new engine needs one block to settle after the source swap; after that the mix is exact.
    render(&mut deck);

    // Nothing separated: the deck plays the mix through those four streams.
    for _ in 0..5 {
        let (l, r) = render(&mut deck);
        assert!((l - 0.5).abs() < 1e-5 && (r - 0.5).abs() < 1e-5, "the mix, not stems: {l}/{r}");
    }

    // A block whose four streams sum to 0.4 — *different* from the mix, so if it is audible the sum
    // changes and the test can tell.
    let at = deck.current_frame();
    let len = 100_000usize;
    assert!(at < len as u64);
    growing.append(0, block(len, [0.1, 0.1, 0.1, 0.1])).unwrap();
    assert_eq!(growing.available_frames(), len as u64);

    // The chunk becomes audible within the engine's read-ahead (the ring already holds a few
    // milliseconds of mix), and nothing else has to happen: no reinstall, no flow restart.
    let (first, _) = render(&mut deck);
    assert!((first - 0.5).abs() < 1e-5, "the ring still held the mix, got {first}");
    assert!(
        render_until(&mut deck, 0.4, 60),
        "the appended block never became audible (sum stays at the mix)"
    );

    // And past the frontier it is the mix again, so a jump into the un-separated tail still sounds
    // like the track.
    let far = len as u64 + 50_000;
    deck.jump(far);
    assert!(
        render_until(&mut deck, 0.5, 400),
        "past the frontier the mix must play (position {:?})",
        deck.current_frame()
    );
}

/// Blocks arriving while the deck renders: the reader never blocks and never sees a half-block.
#[test]
fn blocks_can_arrive_while_the_audio_thread_reads() {
    let original = mix(0.5);
    let growing = ChunkedStreams::new(4, FRAMES, Some(Arc::clone(&original)), DEFAULT_FADE_FRAMES);
    let handle = Arc::new(growing);
    let mut deck = Deck::new(original);
    deck.play();
    deck.set_sources(handle.streams());
    settle(&mut deck, 4);

    let writer = {
        let handle = Arc::clone(&handle);
        std::thread::spawn(move || {
            let len = FRAMES as usize / 8;
            for index in 0..8u64 {
                handle
                    .append(index * len as u64, block(len, [0.1, 0.1, 0.1, 0.1]))
                    .unwrap();
                std::thread::yield_now();
            }
        })
    };
    // Render throughout: every block is a legal sum (the mix, the stems' sum, or a fade between
    // them), never a torn read and never silence.
    let mut worst = 0.0f32;
    while !writer.is_finished() {
        let (l, _) = render(&mut deck);
        worst = worst.max((l - 0.5).abs());
        assert!(
            (l - 0.4).abs() < 0.2 && (l - 0.5).abs() < 0.2,
            "a block landed outside both legal levels: {l}"
        );
    }
    writer.join().unwrap();
    assert_eq!(handle.available_frames(), FRAMES, "every block landed");
    assert!(worst > 0.0, "the blocks were actually audible while arriving");
}
