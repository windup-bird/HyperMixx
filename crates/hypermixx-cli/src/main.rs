//! Command line front-end for the Hypermixx audio engine.
//!
//! Owns file IO and analysis: `load` decodes on a worker thread and hands the pipeline a ready
//! source; `analyse` runs the library analyser and publishes a compiled grid. The pipeline only ever
//! receives executable commands. Two front-ends share the parsing and rendering: the default line
//! REPL and the `--tui` terminal UI.

mod command;
mod notices;
mod repl;
mod response;
mod tui;

use crossbeam_channel::unbounded;
use hypermixx_audio::{reference_toml, simple_dj, AudioPipeline, MixerConfig, DECK_COUNT, SAMPLE_RATE};
use hypermixx_core::Backend;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args = CliArgs::parse();
    if args.print_config {
        print!("{}", reference_toml());
        return;
    }
    let backend = args.backend;
    // `--config` overrides the built-in reference topology; a bad file is a clean exit, not a
    // half-alive engine (and the mixer's own validation — unknown FX, empty channels — runs
    // inside `start`, on the thread that will own the result).
    let cfg = match &args.config {
        Some(path) => match read_config(path) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!("error: {path}: {err}");
                std::process::exit(1);
            }
        },
        None => simple_dj(),
    };
    // `start` builds the mixer on the producer thread (cpal streams are !Send) and reports a bad
    // config back synchronously, so a failure here is a clean exit, not a half-alive engine.
    let pipeline = match AudioPipeline::start(cfg) {
        Ok(pipeline) => pipeline,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    let command_tx = pipeline.command_tx();
    // A custom topology may name any number of channels, so the deck-id range comes from the
    // engine rather than the built-in constant.
    let decks = pipeline.channel_count().unwrap_or(DECK_COUNT).max(1);
    // Worker progress (decode, analysis) travels beside engine responses so neither front-end has
    // to print from a background thread.
    let (notice_tx, notice_rx) = unbounded();

    if args.tui {
        tui::run(&pipeline, command_tx, backend, decks, notice_tx, notice_rx);
        return;
    }

    let config_note = args
        .config
        .as_deref()
        .map(|path| format!(", config {path}"))
        .unwrap_or_default();
    println!(
        "hypermixx {VERSION} — {decks} decks, {SAMPLE_RATE}Hz stereo, backend {backend:?}{config_note}. \
         `help` for commands, `quit` to exit."
    );
    repl::run(&pipeline, command_tx, backend, decks, notice_tx, notice_rx);
}

/// Reads `--config <path>` (topology file), `--print-config` (emit the reference TOML and exit),
/// `--backend auto|stratum|timestretch` (default `auto`), and `--tui` (terminal UI). Unknown flags
/// are errors rather than silently ignored: a typo'd `--conifg` that launches the default topology
/// instead is the kind of surprise nobody wants mid-set.
#[derive(Debug, Default)]
struct CliArgs {
    config: Option<String>,
    print_config: bool,
    backend: Backend,
    tui: bool,
}

impl CliArgs {
    fn parse() -> Self {
        let mut args = Self::default();
        let mut raw = std::env::args().skip(1);
        while let Some(arg) = raw.next() {
            match arg.as_str() {
                "--print-config" => args.print_config = true,
                "--tui" => args.tui = true,
                "--backend" => {
                    args.backend = match raw.next().as_deref() {
                        Some("stratum") => Backend::Stratum,
                        Some("timestretch") => Backend::Timestretch,
                        Some("auto") | None => Backend::Auto,
                        Some(other) => {
                            eprintln!("error: unknown backend `{other}` (auto|stratum|timestretch)");
                            std::process::exit(1);
                        }
                    };
                }
                "--config" => {
                    args.config = raw.next().or_else(|| {
                        eprintln!("error: --config needs a path");
                        std::process::exit(1);
                    });
                }
                other => {
                    eprintln!(
                        "error: unknown argument `{other}` (--config, --print-config, --backend, --tui)"
                    );
                    std::process::exit(1);
                }
            }
        }
        args
    }
}

/// Reads a topology file. Parse errors carry the TOML positions, so a typo points at its line.
fn read_config(path: &str) -> Result<MixerConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    MixerConfig::from_toml_str(&text).map_err(|err| err.message())
}
