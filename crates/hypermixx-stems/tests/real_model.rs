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

#[test]
#[ignore = "needs the 302 MB model *and* `--features cuda`; run with --ignored"]
#[cfg(feature = "cuda")]
fn real_model_separates_frame_exactly_on_cuda() {
    check(Provider::Cuda);
}
