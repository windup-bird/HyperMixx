//! Human-readable rendering of engine responses.
//!
//! Shared by the REPL and the TUI so the two front-ends can never disagree about what the engine
//! said. These functions return data rather than printing: the REPL writes it to stdout/stderr and
//! the TUI pushes it into its scrollback.

use hypermixx_core::{CommandResponse, DeckState, SAMPLE_RATE};

/// How a line should be presented. `Error` lines already carry the `error: ` prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Error,
}

/// One rendered line, ready to print or push into a log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub level: Level,
    pub text: String,
}

impl LogLine {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            level: Level::Info,
            text: text.into(),
        }
    }

    /// Adds the `error: ` prefix the REPL has always used, so both front-ends match.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            text: format!("error: {}", message.into()),
        }
    }
}

/// Renders a [`CommandResponse`]. `Ok` is deliberately silent — it is the ack for a command whose
/// own effect is visible elsewhere, so echoing it would only add noise.
pub fn format_response(response: &CommandResponse) -> Vec<LogLine> {
    match response {
        CommandResponse::Loaded {
            deck_id,
            total_frames,
        } => vec![LogLine::info(format!(
            "deck{deck_id} loaded: {total_frames} frames ({})",
            time(*total_frames)
        ))],
        CommandResponse::State(state) => vec![LogLine::info(deck_line(state))],
        CommandResponse::States(states) => states.iter().map(deck_line).map(LogLine::info).collect(),
        CommandResponse::FxAdded {
            chain,
            index,
            kind,
        } => vec![LogLine::info(format!(
            "{} fx[{index}] {kind} added",
            chain.label()
        ))],
        CommandResponse::FxListed { chain, slots } => {
            let mut out = Vec::new();
            if slots.is_empty() {
                out.push(LogLine::info(format!("{}: no effects", chain.label())));
            }
            for slot in slots {
                let state = if slot.enabled { "on" } else { "off" };
                out.push(LogLine::info(format!(
                    "{} fx[{}] {} [{state}]",
                    chain.label(),
                    slot.index,
                    slot.kind
                )));
                for (name, value) in &slot.params {
                    out.push(LogLine::info(format!("    {name} = {value:.4}")));
                }
            }
            out
        }
        CommandResponse::Ok => Vec::new(),
        CommandResponse::Error(message) => vec![LogLine::error(message.clone())],
    }
}

/// One deck's transport line, the shared shape behind both front-ends' deck readouts.
pub fn deck_line(state: &DeckState) -> String {
    let transport = if state.total_frames == 0 {
        "empty"
    } else if state.playing {
        "playing"
    } else {
        "paused"
    };
    let duration = if state.total_frames == 0 {
        "-".to_owned()
    } else {
        time(state.total_frames)
    };
    let tempo = if state.bpm > 0.0 {
        format!("{:.1} BPM", state.bpm)
    } else {
        "no grid".into()
    };
    let key = state.key.as_deref().unwrap_or("--");
    format!(
        "deck{}  {transport:<7} {} / {duration}  [{}/{}]  {tempo}  {key}",
        state.deck_id,
        time(state.current_frame),
        state.current_frame,
        state.total_frames,
    )
}

/// The root command list.
pub fn help_text(decks: usize) -> String {
    format!(
        "commands ({decks} decks, ids 0..{}):
  [deck] load <path> [bpm]     decode a file (deck defaults to the focused one)
  [deck] analyse               run the analyser and publish its grid
  [deck] play | pause          transport
  [deck] jump <frame>          seek to a frame (1 second = {SAMPLE_RATE} frames)
  [deck] beatjump <beats>      seek by whole beats, keeping the phase
  [deck] rate <ratio>          set tempo rate (1.0 = unity, 0.5 = half speed)
  [deck] profile <name>        tape / keylock / wide (default: tape)
  [deck|master] fx ...         effects on a chain — `fx help` for the subcommands
  state                        show every deck
  zoom in|out|fit              waveform zoom (UI only)
  quit                         exit

A leading `deck0` / `0` selects a deck, `master` the summed output. Without one the command
follows the focused deck (TUI: `Tab` switches it).
startup flags: --config <file> (custom topology), --print-config (reference TOML),
               --backend auto|stratum|timestretch, --tui (terminal UI)",
        decks - 1
    )
}

/// The `fx` subcommands, plus the effect kinds and their parameters (straight from the registry,
/// so this text can never disagree with what the engine actually builds).
pub fn fx_help_text() -> String {
    let mut text = String::from(
        "fx commands (the chain is the line's target: the focused deck by default,\n`master` for the summed output, e.g. `master fx list`):
  fx add <kind>                    append an effect to the end of the chain
  fx remove <slot>                 drop a slot (later slots shift down)
  fx list                          show every slot with its index, kind and parameters
  fx set <slot> <param> <value>    set one parameter (value is smoothed, no clicks)
  fx on|off <slot>                 engage / bypass a slot
  fx trigger <slot>                fire the effect's one-shot hook
  fx pad <slot> press|release      hold a slot engaged while the pad is down

`slot` is an index or an effect name (chains are preloaded at startup, so no `fx list` is needed).
effect kinds and their parameters:",
    );
    for kind in hypermixx_audio::fx::FxKind::ALL {
        text.push_str(&format!(
            "\n  {:<8} {}",
            kind.name(),
            kind.param_names().join(", ")
        ));
    }
    text
}

/// Frames -> `m:ss.mmm` at the engine rate.
pub fn time(frames: u64) -> String {
    let millis = frames as f64 * 1000.0 / SAMPLE_RATE as f64;
    let minutes = (millis / 60_000.0) as u64;
    let seconds = (millis / 1000.0) as u64 % 60;
    let remainder = (millis % 1000.0) as u32;
    format!("{minutes}:{seconds:02}.{remainder:03}")
}
