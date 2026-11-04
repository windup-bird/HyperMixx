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
mod usb;

use std::sync::Arc;

use crossbeam_channel::unbounded;
use hypermixx_audio::{reference_toml, simple_dj, AudioPipeline, MixerConfig, DECK_COUNT};
use hypermixx_core::{Backend, LiveConfig};
use hypermixx_library::LibraryDb;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args = CliArgs::parse();
    if args.print_config {
        print!("{}", reference_toml());
        return;
    }
    // The guide is a self-contained editor: no audio engine, no mixer. It runs before the topology
    // is read, so mapping a controller never depends on a working sound card.
    if let Some(path) = args.midi_guide.as_deref() {
        let decks = args.decks.unwrap_or(1);
        // The file is the source of truth for the guide: its `[meta].device` wins, and only an
        // explicit `--controller` overrides it. A profile device belongs to the engine session, not to a
        // map file the user picked here.
        let guide_port = if args.headless { None } else { args.midi.clone() };
        // The guide never starts an engine, so it cannot ask the mixer what the outputs are. Probing
        // cpal here is enough to show the devices and whether each takes the configured rate — the
        // question a controller mapping session actually has ("will my interface work?").
        let live_path = args
            .live
            .clone()
            .unwrap_or_else(|| "profile/live.toml".to_owned());
        let rate = command::read_live_config(&live_path)
            .map(|(live, _)| live.audio.sample_rate)
            .unwrap_or(hypermixx_core::SAMPLE_RATE);
        // `Output` in the guide writes the mixer file, so hand it the path the session would use.
        let mixer_path = args
            .config
            .clone()
            .unwrap_or_else(|| profile::DEFAULT_MIXER.to_owned());
        let outputs = tui::guide::Outputs {
            // The guide draws its modals through the same popup as the TUI, so it needs the same
            // colours: the profile's own `preference.toml`, not always the shipped path.
            preference: tui::preference_path(args.profile.as_deref()),
            mixer_path,
            devices: if args.headless {
                Vec::new()
            } else {
                hypermixx_audio::output_devices(Some(rate))
            },
            engine_rate: rate,
            // Filled from the map (or the port) once the guide has opened it.
            controller: guide_port.clone(),
        };
        if let Err(err) = tui::guide::run(path, guide_port.as_deref(), decks, outputs) {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
        return;
    }
    let backend = args.backend;
    // A profile names the topology and the live file. It is a pointer, not a third schema: the two
    // files keep their own validation, and an explicit `--config`/`--live` still wins over it.
    let (profile, profile_path) = read_profile_at(args.profile.as_deref());
    let config_path = args
        .config
        .clone()
        .or_else(|| profile.as_ref().and_then(|p| p.mixer.clone()));
    let live_path = args
        .live
        .clone()
        .or_else(|| profile.as_ref().and_then(|p| p.live.clone()));
    // MIDI comes from the profile too: naming a controller there is what makes `--profile dj` bring
    // up a whole rig rather than just a topology. Explicit flags still win.
    //
    // When the profile names neither a device nor a map, the folder default (`profile/controller.toml`)
    // applies — the same shape as `mixer.toml` / `live.toml`. A missing file means "no MIDI", which
    // is the shipped state: no controller is assumed.
    let midi_from_profile = match profile.as_ref().map(|profile| profile.midi.clone()) {
        Some(midi) if !midi.device.is_none() || midi.map.is_some() => midi,
        _ => match profile::load_midi(profile::default_midi_path()) {
            Ok((midi, notes)) => {
                for note in &notes {
                    eprintln!("warning: {}: {note}", profile::default_midi_path());
                }
                midi
            }
            Err(err) => {
                eprintln!("warning: {}: {err}", profile::default_midi_path());
                profile::MidiSettings::default()
            }
        },
    };
    let midi_map = args
        .midi_map
        .clone()
        .or_else(|| midi_from_profile.map.clone())
        .unwrap_or_else(|| profile::default_map_path().to_owned());
    // An explicit map is validated even when no port opens: otherwise a typo would be swallowed by
    // the "no device" warning and the user would wonder why the session ignored the file.
    if args.midi_map.is_some() {
        if let Err(err) = map_error(&midi_map) {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
    // A map file can name its own device. An explicit `--controller-map` is the source of truth for
    // that choice; without one the profile already carried the device out of the same map.
    let midi_port = args.midi.clone().or_else(|| {
        if args.midi_map.is_some() {
            map_device(&midi_map)
        } else {
            midi_from_profile
                .device
                .clone()
                .or_else(|| map_device(&midi_map))
        }
    });
    if args.midi_map.is_some() && midi_port.is_none() {
        eprintln!(
            "warning: `{midi_map}` has no `[meta].device`; no MIDI input will open \
             (use --controller, or pick one in the F9 settings screen)"
        );
    }

    // `--config` overrides the built-in reference topology; a bad file is a clean exit, not a
    // half-alive engine (and the mixer's own validation — unknown FX, empty channels — runs
    // inside `start`, on the thread that will own the result).
    let mut cfg = match &config_path {
        Some(path) => match read_config(path) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!("error: {path}: {err}");
                std::process::exit(1);
            }
        },
        None => default_config(),
    };
    if args.headless {
        cfg.headless();
        eprintln!("[headless] audio outputs are sinks; MIDI input is disabled");
    }
    // The live settings are this machine's, not the music's: the engine rate, the ring sizes and
    // the stem defaults. A missing file is not an error (the shipped defaults are the built-ins);
    // a broken one is, because silently running at another rate is the surprise this file exists
    // to prevent.
    let (live, live_source) = match live_path.as_deref() {
        Some(path) => match command::read_live_config(path) {
            Ok((live, notes)) => {
                for note in &notes {
                    eprintln!("warning: {note}");
                }
                (live, Some(path.to_owned()))
            }
            Err(err) => {
                eprintln!("error: {path}: {err}");
                std::process::exit(1);
            }
        },
        None => match command::read_live_config(command::DEFAULT_LIVE) {
            Ok((live, notes)) => {
                for note in &notes {
                    eprintln!("warning: {note}");
                }
                (live, Some(command::DEFAULT_LIVE.to_owned()))
            }
            Err(_) => (LiveConfig::default(), None),
        },
    };
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
                backend,
                decks,
                notice_tx,
                notice_rx,
                topology: config_path.clone(),
                profile: profile_path.clone(),
                live,
                live_source,
                midi_port: midi_port.clone(),
                midi_map,
                midi_control: None,
                midi_state: None,
                library: library.clone(),
                headless: args.headless,
            },
        );
        return;
    }

    // A port the *user* named on the command line must open, or the run is a mistake; a port that
    // came from the profile is "what this rig usually is", and a controller that is not plugged in
    // yet should not stop the session — say so and carry on (F9 can still pick one). A headless
    // session skips the backend entirely, so a machine with no MIDI sequencer starts quietly.
    let explicit_midi = args.midi.is_some();
    let midi_session = if args.headless {
        None
    } else {
        match midi_port.as_deref() {
            Some(port) => match midi::attach_file(
                port,
                &midi_map,
                decks,
                command_tx.clone(),
                pipeline.response_rx(),
                &notice_tx,
            ) {
                Ok(session) => Some(session),
                Err(err) if explicit_midi => {
                    eprintln!("error: controller: {err}");
                    std::process::exit(1);
                }
                Err(err) => {
                    eprintln!("warning: controller: {err}; continuing without it");
                    None
                }
            },
            None => None,
        }
    };

    // Name the files this run picked up, so "which profile am I on?" is answerable without asking.
    let midi_control = midi_session.as_ref().map(|session| session.control());
    let midi_state = midi_session.as_ref().map(|session| session.state());
    let config_note = match (&profile_path, &config_path) {
        (Some(profile), Some(config)) => format!(", profile {profile}, config {config}"),
        (Some(profile), None) => format!(", profile {profile}"),
        (None, Some(config)) => format!(", config {config}"),
        (None, None) => String::new(),
    };
    println!(
        "hypermixx {VERSION} — {decks} decks, {}Hz stereo, backend {backend:?}{config_note}. \
         `help` for commands, `quit` to exit.",
        live.audio.sample_rate
    );
    repl::run(
        &pipeline,
        session::Session {
            command_tx,
            backend,
            decks,
            notice_tx,
            notice_rx,
            topology: config_path.clone(),
            profile: profile_path.clone(),
            live,
            live_source,
            midi_port: midi_port.clone(),
            midi_map,
            midi_control,
            midi_state,
            library,
            headless: args.headless,
        },
    );
    drop(midi_session);
}

/// Reads `--config <path>` (topology file), `--print-config` (emit the reference TOML and exit),
/// `--backend auto|stratum|timestretch` (default `auto`), `--cli` (line REPL instead of the TUI),
/// `--controller <port>` (open a controller input onto the engine), `--controller-map <file>` (the
/// mapping file, default `midi-map.toml`; the map's `[meta].device` can supply the port) and
/// `--guide <file>` (the unit map guide, no engine). The terminal UI is the default front-end;
/// `--cli` asks for the line REPL.
/// Unknown flags are errors rather than silently ignored:
/// a typo'd `--conifg` that launches the default topology instead is the kind of surprise nobody
/// wants mid-set.
#[derive(Debug, Default)]
struct CliArgs {
    config: Option<String>,
    print_config: bool,
    backend: Backend,
    /// `--cli`: the line REPL instead of the terminal UI.
    cli: bool,
    /// `--headless`: run without opening any audio device or MIDI input.
    headless: bool,
    /// `--controller <port>` (alias `--midi`): a port name or index, opened and mapped onto the
    /// engine.
    midi: Option<String>,
    /// `--controller-map <file>` (alias `--midi-map`): only meaningful with a port.
    midi_map: Option<String>,
    /// `--guide [<file>]`: run the unit map guide instead of the engine.
    midi_guide: Option<String>,
    /// `--decks <n>`: how many deck groups the guide shows; `new` allocates beyond them.
    decks: Option<usize>,
    /// `--live <file>`: this machine's settings (engine rate, ring sizes, stem defaults).
    live: Option<String>,
    /// `--profile <name|file>`: a file naming the topology and the live settings.
    profile: Option<String>,
}

impl CliArgs {
    fn parse() -> Self {
        let mut args = Self::default();
        let mut raw = std::env::args().skip(1).peekable();
        while let Some(arg) = raw.next() {
            match arg.as_str() {
                "--print-config" => args.print_config = true,
                "--cli" => args.cli = true,
                "--headless" => args.headless = true,
                "--backend" => {
                    args.backend = match raw.next().as_deref() {
                        Some("stratum") => Backend::Stratum,
                        Some("timestretch") => Backend::Timestretch,
                        Some("auto") | None => Backend::Auto,
                        Some(other) => {
                            eprintln!(
                                "error: unknown backend `{other}` (auto|stratum|timestretch)"
                            );
                            std::process::exit(1);
                        }
                    };
                }
                "--profile" => {
                    args.profile = raw.next().or_else(|| {
                        eprintln!("error: --profile needs a name or a path (profile/<name>.toml)");
                        std::process::exit(1);
                    });
                }
                "--live" => {
                    args.live = raw.next().or_else(|| {
                        eprintln!("error: --live needs a path");
                        std::process::exit(1);
                    });
                }
                "--config" => {
                    args.config = raw.next().or_else(|| {
                        eprintln!("error: --config needs a path");
                        std::process::exit(1);
                    });
                }
                // `--controller*` is the current spelling; `--midi*` stays as an alias so existing
                // scripts and profiles keep working. The engine still speaks MIDI — only the
                // user-facing name of the thing that sends it changed.
                "--controller" => {
                    args.midi = raw.next().or_else(|| {
                        eprintln!(
                            "error: --controller needs a port name or index \
                             (`controller ports` lists them)"
                        );
                        std::process::exit(1);
                    });
                }
                "--controller-map" => {
                    args.midi_map = raw.next().or_else(|| {
                        eprintln!("error: --controller-map needs a path");
                        std::process::exit(1);
                    });
                }
                "--guide" => {
                    // Path is optional: without one the conventional map is edited, and the TUI's
                    // file picker can choose another.
                    args.midi_guide = Some(match raw.peek() {
                        Some(next) if !next.starts_with("--") => raw.next().unwrap(),
                        _ => profile::default_map_path().to_owned(),
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
                        "error: unknown argument `{other}`\n  flags: --config, --print-config, \
                         --profile, --live, --backend, --cli, --headless,\n         --guide, --controller, \
                         --controller-map, --decks"
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

fn map_error(path: &str) -> Result<(), String> {
    read_map(path).map(|_| ())
}

/// The device a map names in `[meta].device`, if the file parses. A map that does not parse is
/// left to `attach`, which reports the real error with the file name.
fn map_device(path: &str) -> Option<String> {
    read_map(path).ok()?.meta.and_then(|meta| meta.device)
}

/// The profile and the path it came from: the path is what `profile show`/`profile reload` speak
/// about, so the two travel together from the start.
fn read_profile_at(arg: Option<&str>) -> (Option<profile::Profile>, Option<String>) {
    let (path, required) = match arg {
        Some(name) => (profile::resolve(name), true),
        None => (profile::AMBIENT.to_owned(), false),
    };
    match profile::load(&path) {
        Ok((profile, notes)) => {
            for note in &notes {
                eprintln!("warning: {path}: {note}");
            }
            (Some(profile), Some(path))
        }
        Err(err) => {
            if required {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
            // The ambient profile is optional by design: no profile means the folder defaults.
            (None, None)
        }
    }
}

/// Reads a topology file. Parse errors carry the TOML positions, so a typo points at its line.
fn read_config(path: &str) -> Result<MixerConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    MixerConfig::from_toml_str(&text).map_err(|err| err.message())
}

/// The topology to run with when neither `--config` nor a profile names one: the shipped file under
/// `profile/`, or the built-in reference. The shipped file parses to exactly [`simple_dj`], so the
/// two paths describe one mixer.
fn default_config() -> MixerConfig {
    const PATH: &str = profile::DEFAULT_MIXER;
    match std::fs::read_to_string(PATH) {
        Ok(text) => match MixerConfig::from_toml_str(&text) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!(
                    "warning: {PATH}: {}; using the built-in topology",
                    err.message()
                );
                simple_dj()
            }
        },
        Err(_) => simple_dj(),
    }
}
