//! The real model, end to end — opt-in, because it needs a 302 MB model file and tens of seconds.
//!
//! ```text
//! HYPERMIXX_TEST_STEMS=1 cargo test -p hypermixx-stems -- --ignored real_model
//! HYPERMIXX_TEST_STEMS=1 cargo test -p hypermixx-stems --features cuda -- --ignored real_model
//! ```
//!
//! These are the P0 measurements turned into a regression test: the four streams must be
//! **frame-exact**, and their sum must land back on the input to within the ~−30 dB the model
//! achieves. That pair catches the two failure modes that matter — a length that drifts (which
//! would make the engine's four streams desynchronise) and a preprocessing change that quietly
//! stops reconstructing.

use std::sync::Arc;

use hypermixx_core::{Source, Stem, CHANNELS, SAMPLE_RATE};
use hypermixx_media::{DecodedAudio, PcmPool};
use hypermixx_stems::{
    ensure_model, no_progress, Cancelled, CharonSeparator, Provider, SeparateOptions,
    StemSeparator, HTDEMUCS,
};

/// A deterministic, bounded test signal: enough structure for the model to have something to split,
/// short enough for three windows (~25 s).
fn excerpt(frames: u64) -> Arc<dyn Source> {
    let mut seed = 0x1234_5678u32;
    let pcm = (0..frames as usize)
        .flat_map(|i| {
            let mut sample = |scale: f32, step: f32| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = ((seed >> 8) as f32 / 16_777_216.0 - 0.5) * 0.1;
                let tone = (i as f32 * step).sin() * 0.4;
                (tone + noise) * scale
            };
            let left = sample(1.0, 0.01);
            let right = sample(0.9, 0.013);
            [left, right]
        })
        .collect();
    Arc::new(PcmPool::from_decoded(DecodedAudio {
        total_frames: frames,
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
        pcm,
    }))
}

fn read_all(source: &dyn Source) -> Vec<f32> {
    let mut out = vec![0.0f32; source.total_frames() as usize * CHANNELS];
    let n = source.read_frames(0, &mut out);
    out.truncate(n * CHANNELS);
    out
}

fn rms(samples: &[f32]) -> f64 {
    (samples.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / samples.len().max(1) as f64)
        .sqrt()
}

/// Separates the excerpt on `provider` and checks the contract the engine depends on.
fn check(provider: Provider) {
    let model = ensure_model(&HTDEMUCS, None, &no_progress())
        .expect("HTDemucs model (302 MB, fetched from a mirror and SHA-256 checked)");
    let separator = CharonSeparator::from_options(
        model,
        SeparateOptions {
            provider,
            ..Default::default()
        },
    );
    assert_eq!(
        separator.provider(),
        provider,
        "the separator must report the provider it was asked for"
    );

    let frames = 25 * SAMPLE_RATE as u64;
    let mix = excerpt(frames);
    let started = std::time::Instant::now();
    let set = separator
        .separate(mix.as_ref(), &no_progress(), &Cancelled::new())
        .unwrap_or_else(|err| panic!("{provider:?} separation failed: {err}"));
    eprintln!(
        "{provider:?}: {frames} frames in {:.1}s",
        started.elapsed().as_secs_f64()
    );

    // Frame-exact, or the deck's four streams would share a clock they cannot honour.
    assert!(set.is_frame_aligned(), "the four streams must be the same length");
    assert_eq!(set.total_frames(), frames);
    for stem in Stem::ALL {
        assert_eq!(set.get(stem).total_frames(), frames, "{stem}");
    }

    // And they must still add up: a preprocessing regression (window, weights, normalisation) shows
    // up here as a much larger residual, long before it is audible as an artefact.
    let mut sum = vec![0.0f32; frames as usize * CHANNELS];
    for stem in Stem::ALL {
        for (acc, sample) in sum.iter_mut().zip(read_all(set.get(stem).as_ref())) {
            *acc += sample;
        }
    }
    let original = read_all(mix.as_ref());
    let residual: Vec<f32> = sum.iter().zip(&original).map(|(a, b)| a - b).collect();
    let relative_db = 20.0 * (rms(&residual) / rms(&original).max(1e-12)).log10();
    eprintln!("{provider:?}: Σstems − input = {relative_db:.1} dB");
    assert!(
        relative_db < -20.0,
        "the four stems should reconstruct the mix to well under -20 dB, got {relative_db:.1} dB"
    );
}

#[test]
#[ignore = "needs the 302 MB model; run with --ignored"]
fn real_model_separates_frame_exactly_on_cpu() {
    check(Provider::Cpu);
}

/// The **streaming** path with the real model: blocks arrive in order, cover the track exactly once,
/// and stitch back to **bit-identical** audio — the property the deck relies on when it plays them as
/// they land, and the reason there is no chunk length to configure.
///
/// The comparison is against a one-piece run of the same excerpt, sample for sample. If a backend
/// change ever makes the streaming path disagree with the whole-track one, this fails here rather
/// than as a seam somewhere in a set.
#[test]
#[ignore = "needs the 302 MB model; run with --ignored"]
fn real_model_streams_blocks_identical_to_a_whole_track_run() {
    struct Stitch {
        blocks: Vec<(u64, usize)>,
        streams: Vec<Vec<f32>>,
    }
    impl hypermixx_stems::StemBlockSink for Stitch {
        fn block(&mut self, start: u64, blocks: &[Vec<f32>]) -> bool {
            let frames = blocks.first().map(Vec::len).unwrap_or(0) / CHANNELS;
            self.blocks.push((start, frames));
            if self.streams.is_empty() {
                self.streams = vec![Vec::new(); blocks.len()];
            }
            for (acc, block) in self.streams.iter_mut().zip(blocks) {
                acc.extend_from_slice(block);
            }
            true
        }
    }

    let model = ensure_model(&HTDEMUCS, None, &no_progress()).expect("the model");
    let separator = CharonSeparator::from_options(
        model,
        SeparateOptions {
            provider: Provider::Cpu,
            ..Default::default()
        },
    );

    let frames = 25 * SAMPLE_RATE as u64;
    let mix = excerpt(frames);
    let started = std::time::Instant::now();
    let mut stitched = Stitch {
        blocks: Vec::new(),
        streams: Vec::new(),
    };
    hypermixx_stems::stream_uncached(
        &separator,
        mix.as_ref(),
        &mut stitched,
        &no_progress(),
        &Cancelled::new(),
    )
    .expect("streaming separation");
    eprintln!(
        "streamed: {frames} frames in {:.1}s in {} blocks",
        started.elapsed().as_secs_f64(),
        stitched.blocks.len()
    );

    // Blocks are in order and cover the track exactly once — no gap, no overlap, no reordering.
    let mut at = 0u64;
    for (start, block_frames) in &stitched.blocks {
        assert_eq!(*start, at, "a block must start where the last one ended");
        at += *block_frames as u64;
    }
    assert_eq!(at, frames, "the blocks must cover the track exactly once");
    // The cadence is the model's own: one stride of audio per block (5.85 s at overlap 0.25), so a
    // 25 s excerpt is a handful of them — and the last one is short.
    let stride = hypermixx_stems::stride_frames(HTDEMUCS.segment_frames, 0.25);
    assert!(
        stitched.blocks.len() >= 4,
        "one block per window stride ({stride} frames): {:?}",
        stitched.blocks
    );
    assert!(
        stitched.blocks[..stitched.blocks.len() - 1]
            .iter()
            .all(|(_, block_frames)| *block_frames as u64 == stride),
        "every full block is one stride: {:?}",
        stitched.blocks
    );

    // And they must be the audio a whole-track run produces, sample for sample.
    let whole = separator
        .separate(mix.as_ref(), &no_progress(), &Cancelled::new())
        .expect("whole-track separation");
    let mut worst = 0.0f32;
    let mut worst_at = 0usize;
    for stem in Stem::ALL {
        let want = read_all(whole.get(stem).as_ref());
        let got = &stitched.streams[stem.index()];
        assert_eq!(got.len(), want.len(), "{stem}: the same length");
        for (index, (a, b)) in got.iter().zip(&want).enumerate() {
            let diff = (a - b).abs();
            if diff > worst {
                worst = diff;
                worst_at = index;
            }
        }
    }
    eprintln!(
        "streamed vs whole-track: worst sample difference {worst:e} (at frame {})",
        worst_at / CHANNELS
    );
    assert_eq!(
        worst, 0.0,
        "streaming must reproduce whole-track separation exactly; worst sample differed by \
         {worst:e} at frame {}",
        worst_at / CHANNELS
    );

    // And the stitched streams still reconstruct the input.
    let mut sum = vec![0.0f32; frames as usize * CHANNELS];
    for stream in &stitched.streams {
        for (acc, sample) in sum.iter_mut().zip(stream) {
            *acc += sample;
        }
    }
    let original = read_all(mix.as_ref());
    let residual: Vec<f32> = sum.iter().zip(&original).map(|(a, b)| a - b).collect();
    let residual_db = 20.0 * (rms(&residual) / rms(&original).max(1e-12)).log10();
    eprintln!("streamed: Σstems − input = {residual_db:.1} dB");
    assert!(residual_db < -20.0, "streamed Σ residual {residual_db:.1} dB");
}

#[test]
#[ignore = "needs the 302 MB model *and* `--features cuda`; run with --ignored"]
#[cfg(feature = "cuda")]
fn real_model_separates_frame_exactly_on_cuda() {
    check(Provider::Cuda);
}
