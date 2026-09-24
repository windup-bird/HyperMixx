//! Command line front-end for the Hypermixx audio engine.
//!
//! Owns file IO and analysis: `load` decodes on a worker thread and hands the pipeline a ready
//! source; `analyse` runs the library analyser and publishes a compiled grid. The pipeline only ever
//! receives executable commands. Two front-ends share the parsing and rendering: the default line
//! REPL and the `--tui` terminal UI.

mod command;
mod midi;
mod notices;
mod repl;
mod response;
mod tui;

use crossbeam_channel::unbounded;
use hypermixx_audio::{
    reference_toml, simple_dj, AudioPipeline, MixerConfig, DECK_COUNT, SAMPLE_RATE,
};
use hypermixx_core::Backend;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args = CliArgs::parse();
    if args.print_config {
        print!("{}", reference_toml());
        return;
    }
    // The guide is a self-contained editor: no audio engine, no mixer. It runs before the topology
    // is even read, so mapping a controller never depends on a working sound card.
    if let Some(path) = args.midi_guide.as_deref() {
        let decks = args.decks.unwrap_or(DECK_COUNT);
        if let Err(err) = tui::guide::run(path, args.midi.as_deref(), decks) {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
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

    // The TUI attaches MIDI itself (and can pick a port/map at runtime), so its session lives in
    // the app rather than here. The REPL has no picker, so its session is attached up front — a bad
    // port or map still fails the launch cleanly.
    if args.tui {
        tui::run(
            &pipeline,
            command_tx,
            backend,
            decks,
            notice_tx,
            notice_rx,
            args.midi.as_deref(),
            args.midi_map.as_deref().unwrap_or("midi-map.toml"),
        );
        return;
    }

    let midi_session = match args.midi.as_deref() {
        Some(port) => match midi::attach_file(
            port,
            args.midi_map.as_deref().unwrap_or("midi-map.toml"),
            decks,
            command_tx.clone(),
            pipeline.response_rx(),
            &notice_tx,
        ) {
            Ok(session) => Some(session),
            Err(err) => {
                eprintln!("error: midi: {err}");
                std::process::exit(1);
            }
        },
        None => None,
    };

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
    drop(midi_session);
}

/// Reads `--config <path>` (topology file), `--print-config` (emit the reference TOML and exit),
/// `--backend auto|stratum|timestretch` (default `auto`), `--tui` (terminal UI),
/// `--midi <port>` (open a MIDI input onto the engine) and `--midi-map <file>` (the mapping file,
/// default `midi-map.toml`), and `--midi-guide <file>` (the learn-mode editor, which starts no
/// engine; `--decks` sets its target list). Unknown flags are errors rather than silently ignored:
/// a typo'd `--conifg` that launches the default topology instead is the kind of surprise nobody
/// wants mid-set.
#[derive(Debug, Default)]
struct CliArgs {
    config: Option<String>,
    print_config: bool,
    backend: Backend,
    tui: bool,
    /// `--midi <port>`: a port name or index, opened and mapped onto the engine.
    midi: Option<String>,
    /// `--midi-map <file>`: only meaningful with `--midi`.
    midi_map: Option<String>,
    /// `--midi-guide <file>`: run the guide editor instead of the engine.
    midi_guide: Option<String>,
    /// `--decks <n>`: the guide's target list width (no engine to ask).
    decks: Option<usize>,
}

impl CliArgs {
    fn parse() -> Self {
        let mut args = Self::default();
        let mut raw = std::env::args().skip(1).peekable();
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
                "--midi" => {
                    args.midi = raw.next().or_else(|| {
                        eprintln!("error: --midi needs a port name or index (`midi ports` lists them)");
                        std::process::exit(1);
                    });
                }
                "--midi-map" => {
                    args.midi_map = raw.next().or_else(|| {
                        eprintln!("error: --midi-map needs a path");
                        std::process::exit(1);
                    });
                }
                "--midi-guide" => {
                    // Path is optional: without one the conventional map is edited, and the TUI's
                    // file picker can choose another.
                    args.midi_guide = Some(match raw.peek() {
                        Some(next) if !next.starts_with("--") => raw.next().unwrap(),
                        _ => "midi-map.toml".to_owned(),
                    });
                }
                "--decks" => {
                    args.decks = Some(
                        raw.next()
                            .and_then(|value| value.parse().ok())
                            .unwrap_or_else(|| {
                                eprintln!("error: --decks needs a number");
                                std::process::exit(1);
                            }),
                    );
                }
                other => {
                    eprintln!(
                        "error: unknown argument `{other}` (--config, --print-config, --backend, --tui, --midi, --midi-map, --midi-guide, --decks)"
                    );
                    std::process::exit(1);
                }
            }
        }
        // A map without a port would be silently ignored, which reads as "my bindings do nothing".
        // The guide takes its port from `--midi` optionally, so it is exempt.
        if args.midi.is_none() && args.midi_map.is_some() && args.midi_guide.is_none() {
            eprintln!("error: --midi-map requires --midi");
            std::process::exit(1);
        }
        args
    }
}

/// Reads a topology file. Parse errors carry the TOML positions, so a typo points at its line.
fn read_config(path: &str) -> Result<MixerConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    MixerConfig::from_toml_str(&text).map_err(|err| err.message())
}
