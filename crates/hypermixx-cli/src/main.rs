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
mod profile;
mod response;
mod session;
mod tui;

use crossbeam_channel::unbounded;
use hypermixx_audio::{reference_toml, simple_dj, AudioPipeline, MixerConfig, DECK_COUNT};
use hypermixx_core::{Backend, LiveConfig};

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
        // The guide can be told its port by the profile too, so mapping a controller does not need
        // the port repeated on the command line.
        let guide_port = args.midi.clone().or_else(|| {
            read_profile(args.profile.as_deref())
                .and_then(|profile| profile.midi.device)
        });
        if let Err(err) = tui::guide::run(path, guide_port.as_deref(), decks) {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
        return;
    }
    let backend = args.backend;
    // A profile names the topology and the live file. It is a pointer, not a third schema: the two
    // files keep their own validation, and an explicit `--config`/`--live` still wins over it.
    let (profile, profile_path) = read_profile_at(args.profile.as_deref());
    let config_path = args.config.clone().or_else(|| profile.as_ref().and_then(|p| p.mixer.clone()));
    let live_path = args.live.clone().or_else(|| profile.as_ref().and_then(|p| p.live.clone()));
    // MIDI comes from the profile too: naming a controller there is what makes `--profile dj` bring
    // up a whole rig rather than just a topology. Explicit flags still win.
    //
    // When the profile names neither a device nor a map, the folder default (`profile/midi.toml`)
    // applies — the same shape as `mixer.toml` / `live.toml`. A missing file means "no MIDI", which
    // is the shipped state: no controller is assumed.
    let midi_from_profile = match profile.as_ref().map(|profile| profile.midi.clone()) {
        Some(midi) if !midi.device.is_none() || midi.map.is_some() => midi,
        _ => match profile::load_midi(profile::DEFAULT_MIDI) {
            Ok((midi, notes)) => {
                for note in &notes {
                    eprintln!("warning: {}: {note}", profile::DEFAULT_MIDI);
                }
                midi
            }
            Err(_) => profile::MidiSettings::default(),
        },
    };
    let midi_port = args.midi.clone().or_else(|| midi_from_profile.device.clone());
    let midi_map = args
        .midi_map
        .clone()
        .or_else(|| midi_from_profile.map.clone())
        .unwrap_or_else(|| profile::DEFAULT_MAP.to_owned());

    // `--config` overrides the built-in reference topology; a bad file is a clean exit, not a
    // half-alive engine (and the mixer's own validation — unknown FX, empty channels — runs
    // inside `start`, on the thread that will own the result).
    let cfg = match &config_path {
        Some(path) => match read_config(path) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!("error: {path}: {err}");
                std::process::exit(1);
            }
        },
        None => default_config(),
    };
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

    // The TUI attaches MIDI itself (and can pick a port/map at runtime), so its session lives in
    // the app rather than here. The REPL has no picker, so its session is attached up front — a bad
    // port or map still fails the launch cleanly.
    if args.tui {
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
            },
        );
        return;
    }

    // A port the *user* named on the command line must open, or the run is a mistake; a port that
    // came from the profile is "what this rig usually is", and a controller that is not plugged in
    // yet should not stop the session — say so and carry on (the TUI's `F2` can still pick one).
    let explicit_midi = args.midi.is_some();
    let midi_session = match midi_port.as_deref() {
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
                eprintln!("error: midi: {err}");
                std::process::exit(1);
            }
            Err(err) => {
                eprintln!("warning: midi: {err}; continuing without it");
                None
            }
        },
        None => None,
    };

    // Name the files this run picked up, so "which profile am I on?" is answerable without asking.
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
        },
    );
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
                "--profile" => {
                    args.profile = raw.next().or_else(|| {
                        eprintln!(
                            "error: --profile needs a name or a path (profile/<name>.toml)"
                        );
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
                        "error: unknown argument `{other}` (--config, --print-config, --profile, \
                         --live, --backend, --tui, --midi, --midi-map, --midi-guide, --decks)"
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

/// Reads the profile `--profile` names, or the ambient one, and reports whatever it said that is
/// worth saying (a reserved `[preference]` key, for instance).
///
/// A *missing* ambient profile is not an error — "no profile" is the documented way to run the
/// built-in defaults. A missing **named** one is, because the user asked for it by name.
fn read_profile(arg: Option<&str>) -> Option<profile::Profile> {
    let (profile, _) = read_profile_at(arg);
    profile
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
                eprintln!("warning: {PATH}: {}; using the built-in topology", err.message());
                simple_dj()
            }
        },
        Err(_) => simple_dj(),
    }
}
