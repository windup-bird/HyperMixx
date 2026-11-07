//! `beatprobe` — backend comparison on one file: runs both analysis backends end to end and
//! reports the fitted rigid grid each produces.
//!
//! ```text
//! cargo run --release -p hypermixx-library --example beatprobe -- <audio-file>
//! ```

use std::sync::Arc;
use std::time::Instant;

use hypermixx_library::analyser::{analyze, Backends};

const RATE: u32 = 44_100;

fn main() -> std::process::ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: beatprobe <audio-file>");
        return std::process::ExitCode::FAILURE;
    };
    let decoded = match hypermixx_media::decode_file_at(&path, RATE) {
        Ok(decoded) => decoded,
        Err(err) => {
            eprintln!("decode: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let source = Arc::new(hypermixx_media::PcmPool::from_decoded(decoded));

    for (label, backends) in [
        ("timestretch", Backends::default()),
        (
            "stratum",
            Backends {
                beats: hypermixx_library::analyser::Backend::Stratum,
                key: hypermixx_library::analyser::Backend::Stratum,
            },
        ),
    ] {
        let started = Instant::now();
        match analyze(source.clone(), RATE, backends) {
            Ok(analysis) => {
                let grid = &analysis.beatgrid;
                let first = grid.beat_frames.first().copied().unwrap_or(0);
                eprintln!(
                    "{label}: {:.2} BPM, {} beats, first beat at {:.3}s, key {} — in {:.1}s",
                    analysis.bpm(),
                    grid.beat_frames.len(),
                    first as f64 / f64::from(RATE),
                    analysis
                        .key
                        .map(|key| key.traditional())
                        .unwrap_or_else(|| "--".into()),
                    started.elapsed().as_secs_f32()
                );
            }
            Err(err) => eprintln!("{label}: {err} — in {:.1}s", started.elapsed().as_secs_f32()),
        }
    }
    std::process::ExitCode::SUCCESS
}
