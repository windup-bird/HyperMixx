//! Command line front-end for the Hypermixx audio engine.
//!
//! Owns file IO and analysis: `load` decodes on a worker thread and hands the pipeline a ready
//! source; `analyse` runs the library analyser and publishes a compiled grid. The pipeline only ever
//! receives executable commands. Two front-ends share the parsing and rendering: the default line
//! REPL and the default TUI.

mod command;
mod midi;
mod notices;
mod preference;
mod procmon;
mod profile;
mod repl;
mod response;
mod session;
mod tui;

use std::sync::Arc;

use crossbeam_channel::unbounded;
use hypermixx_audio::{AudioPipeline, DECK_COUNT};
use hypermixx_library::LibraryDb;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args = CliArgs::parse();
    // One config for the whole run: `--config` names it, otherwise the platform default is used
    // and created from the shipped template on the first start.
    let mut config = match profile::load_or_create(args.config.as_deref()) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    for note in &config.notes {
        eprintln!("warning: {note}");
    }
    // The deck count is the front-end's (`--4-decks`), not the config's: a config only maps decks to
    // controller faces, so the same file runs a two- or a four-deck rig.
    let deck_count = if args.four_decks { 4 } else { DECK_COUNT };
    config.mixer.set_channel_count(deck_count);
    let config_path = config.path.to_string_lossy().into_owned();
    let map_path = config
        .controller
        .map
        .clone()
        .unwrap_or_else(|| profile::map_path_for(&config.path));
    let controllers = config.controllers.clone();
    let live = config.live;
    let cfg = config.mixer;
    let auto_analyse = config.analysis.auto_analyse;
    let backends = hypermixx_library::analyser::Backends {
        beats: config.analysis.beats,
        key: config.analysis.key,
    };

    // The guide is a self-contained editor: no audio engine, no mixer. It still reads the config,
    // so the rate it probes with and the file its `Output` pane edits are the ones a session uses.
    if let Some(path) = args.midi_guide.as_deref() {
        // The conventional map sits next to the config and does not exist on a fresh install.
        // Give the guide a real file to edit; an explicit `--guide <file>` that is missing gets the
        // same starting point rather than an empty editor.
        if !std::path::Path::new(path).is_file() {
            if let Some(parent) = std::path::Path::new(path)
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                if let Err(err) = std::fs::create_dir_all(parent) {
                    eprintln!("warning: {}: {err}", parent.display());
                }
            }
            if let Err(err) = std::fs::write(path, profile::MAP_TEMPLATE) {
                eprintln!("warning: {path}: {err}");
            }
        }
        let decks = if args.four_decks { 4 } else { 1 };
        let rate = live.audio.sample_rate;
        let outputs = tui::guide::Outputs {
            preference: config_path.clone(),
            mixer_path: config_path.clone(),
            devices: hypermixx_audio::output_devices(Some(rate)),
            engine_rate: rate,
            // The map names the controller; the guide marks its device in the list once it opens.
            controller: None,
        };
        if let Err(err) = tui::guide::run(path, decks, outputs) {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
        return;
    }
    // Each `[[controller]]` becomes its own session: own port, own unit map, and its own slice of
    // the face → deck table (so two devices cannot address the same deck by accident). The port is
    // the entry's `device`, or the one the map names; there is no command-line override.
    // The unfiltered table is the fallback for a front-end that has no explicit controller list.
    let deck_routes = profile::deck_routes(&cfg, None);
    let controller_sessions: Vec<profile::ControllerSession> = controllers
        .iter()
        .map(|entry| {
            let map = entry.map.clone().unwrap_or_else(|| map_path.clone());
            let port = entry
                .device
                .clone()
                .or_else(|| entry.map.as_ref().and_then(|_| map_device(&map)));
            let routes = profile::deck_routes(&cfg, Some(&entry.id));
            profile::ControllerSession {
                id: entry.id.clone(),
                port,
                map,
                routes,
            }
        })
        .collect();
    let midi_port = controller_sessions.first().and_then(|entry| entry.port.clone());
    // `start` builds the mixer on the producer thread (cpal streams are !Send) and reports a bad
    // config back synchronously, so a failure here is a clean exit, not a half-alive engine.
    let pipeline = match AudioPipeline::start_with(
        cfg,
        live.audio.sample_rate,
        hypermixx_audio::AudioTuning {
            ring_frames: live.audio.ring_frames,
            prefill_frames: live.audio.prefill_frames,
            device_buffer_frames: live.audio.device_buffer_frames,
            keylock: live.keylock,
        },
    ) {
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
    // The library is a cache, not a reason to refuse to start: an unwritable cache disables the
    // browser and leaves playback untouched.
    let library = match LibraryDb::open_default() {
        Ok(db) => Some(Arc::new(db)),
        Err(err) => {
            eprintln!("warning: library: {err}; browser disabled");
            None
        }
    };

    // The TUI attaches MIDI itself (and can pick a port/map at runtime), so its session lives in
    // the app rather than here. The REPL has no picker, so its session is attached up front — a bad
    // port or map still fails the launch cleanly.
    // The terminal UI is the default front-end; `--cli` asks for the line REPL instead. The old
    // `--tui` / `--midi` / `--midi-map` / `--midi-guide` spellings are refused, not aliased.
    if !args.cli {
        tui::run(
            &pipeline,
            session::Session {
                command_tx,
                backends,
                decks,
                notice_tx,
                notice_rx,
                topology: Some(config_path.clone()),
                live: live.clone(),
                live_source: Some(config_path.clone()),
                midi_port: midi_port.clone(),
                midi_map: map_path.clone(),
                midi_control: None,
                midi_state: None,
                deck_routes: deck_routes.clone(),
                controller_sessions: controller_sessions.clone(),
                auto_analyse,
                library: library.clone(),
            },
        );
        return;
    }

    // A controller the config names but that is not plugged in yet should not stop the session —
    // say so and carry on (F9 can still pick one).
    let mut midi_sessions: Vec<midi::MidiSession> = Vec::new();
    for entry in &controller_sessions {
        let Some(port) = entry.port.as_deref() else {
            continue;
        };
        match midi::attach_file(
            port,
            &entry.map,
            &entry.routes,
            decks,
            command_tx.clone(),
            pipeline.response_rx(),
            &notice_tx,
        ) {
            Ok(session) => midi_sessions.push(session),
            Err(err) => {
                eprintln!("warning: controller `{}`: {err}; continuing without it", entry.id);
            }
        }
    }

    let midi_control = midi_sessions.first().map(|session| session.control());
    let midi_state = midi_sessions.first().map(|session| session.state());
    let config_note = format!(", config {config_path}");
    println!(
        "hypermixx {VERSION} — {decks} decks, {}Hz stereo, analysis beats {}/key {}{config_note}. \
         `help` for commands, `quit` to exit.",
        live.audio.sample_rate,
        backends.beats.as_str(),
        backends.key.as_str()
    );
    repl::run(
        &pipeline,
        session::Session {
            command_tx,
            backends,
            decks,
            notice_tx,
            notice_rx,
            topology: Some(config_path.clone()),
            live,
            live_source: Some(config_path.clone()),
            midi_port: midi_port.clone(),
            midi_map: map_path,
            midi_control,
            midi_state,
            deck_routes,
            controller_sessions,
            auto_analyse,
            library,
        },
    );
    drop(midi_sessions);
}

/// Reads `--config <path>` (the one config file), `--4-decks` (a four-deck rig), `--cli` (line REPL
/// instead of the TUI) and `--guide [<file>]` (the unit map guide, no engine). Without `--config`
/// the platform default path is used and created on the first start. The terminal UI is the default
/// front-end; `--cli` asks for the line REPL. Controllers are only ever described by the config
/// (`[[controller]]`), never by a flag. Unknown flags are errors rather than silently ignored: a
/// typo'd `--conifg` that launches the default rig instead is the kind of surprise nobody wants
/// mid-set.
#[derive(Debug, Default)]
struct CliArgs {
    config: Option<String>,
    /// `--4-decks`: run four decks instead of the default two.
    four_decks: bool,
    /// `--cli`: the line REPL instead of the terminal UI.
    cli: bool,
    /// `--guide [<file>]`: run the unit map guide instead of the engine.
    midi_guide: Option<String>,
}

impl CliArgs {
    fn parse() -> Self {
        let mut args = Self::default();
        let mut raw = std::env::args().skip(1).peekable();
        while let Some(arg) = raw.next() {
            match arg.as_str() {
                "--4-decks" => args.four_decks = true,
                "--cli" => args.cli = true,
                "--config" => {
                    args.config = raw.next().or_else(|| {
                        eprintln!("error: --config needs a path");
                        std::process::exit(1);
                    });
                }
                "--guide" => {
                    // Path is optional: without one the map next to the config is edited, and the
                    // TUI's file picker can choose another.
                    args.midi_guide = Some(match raw.peek() {
                        Some(next) if !next.starts_with("--") => raw.next().unwrap(),
                        _ => profile::default_path()
                            .map(|path| profile::map_path_for(&path))
                            .unwrap_or_else(|| "mapping.toml".to_owned()),
                    });
                }
                other => {
                    eprintln!(
                        "error: unknown argument `{other}`\n  flags: --config, --4-decks, \
                         --cli, --guide"
                    );
                    std::process::exit(1);
                }
            }
        }
        args
    }
}

fn read_map(path: &str) -> Result<hypermixx_midi::Map, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
    let base = std::path::Path::new(path)
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    hypermixx_midi::Map::from_toml_str_at(&text, base).map_err(|err| format!("{path}: {err}"))
}

/// The device a map names in `[meta].device`, if the file parses. A map that does not parse is
/// left to `attach`, which reports the real error with the file name.
fn map_device(path: &str) -> Option<String> {
    read_map(path).ok()?.meta.and_then(|meta| meta.device)
}

