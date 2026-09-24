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
    let mut line = format!(
        "deck{}  {transport:<7} {} / {duration}  [{}/{}]  {tempo}  {key}",
        state.deck_id,
        time(state.current_frame),
        state.current_frame,
        state.total_frames,
    );
    // Loop furniture, when there is any: the engaged range, a pending manual in point, and the
    // slip clock whenever it has drifted away from what is being heard.
    if let Some((in_frame, out_frame)) = state.loop_range {
        line.push_str(&format!("  loop [{in_frame}-{out_frame}]"));
    }
    if let Some(armed) = state.loop_in_armed {
        line.push_str(&format!("  in armed@{armed}"));
    }
    if let Some(badge) = sync_badge(state) {
        line.push_str(&format!("  {badge}"));
    }
    if state.virtual_frame != state.current_frame {
        line.push_str(&format!("  slip {}", state.virtual_frame));
    }
    line
}

/// The sync/nudge tail of a deck line: its shared-tempo mode, the group BPM, who it tracks, the
/// running phase correction and any bend. `None` for a deck with nothing to report, so an idle
/// `state` line stays exactly as short as it always was (and `phase_probe.sh`'s parsing is unchanged).
pub fn sync_badge(state: &DeckState) -> Option<String> {
    if state.group_bpm <= 0.0 && state.nudgerate.abs() < 1e-4 && state.sync_leader.is_none() {
        return None;
    }
    let mut parts = vec![format!("sync {}", state.sync_mode)];
    if state.group_bpm > 0.0 {
        parts.push(format!("{:.1} BPM", state.group_bpm));
    }
    if let Some(leader) = state.sync_leader {
        parts.push(format!("← deck{leader}"));
    }
    if let Some(align) = &state.align {
        parts.push(format!("phase {align}"));
    }
    if state.nudge.abs() > 1e-4 {
        parts.push(format!("nudge {:+.3}", state.nudge));
    }
    if state.nudgerate.abs() > 1e-4 && state.nudge.abs() < 1e-4 {
        parts.push(format!("pll {:+.3}", state.nudgerate));
    }
    if state.lock {
        parts.push("locked".to_owned());
    }
    Some(parts.join("  "))
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
  [deck] rate <ratio>          set the tempo (1.0 = unity, 0.5 = half speed)
  [deck] sync tempo            match the other deck's BPM once
  [deck] sync phase <mode>     match tempo, then close the beat phase
                               mode: instant | linear [seconds] | pid
  [deck] sync tempolock        both decks share one tempo — either fader moves both
  [deck] sync phaselock <mode> follower tracks the leader exactly (its own fader is ignored)
  [deck] sync set-leader       name *this* deck as the one the others follow
  [deck] sync unlock           drop the lock and the correction, keeping the tempo
  [deck] nudge <delta> [sec]   temporary rate bend to hand-align phase (0.04 = 4% fast)
  [deck] nudge off             release the bend (it ramps back, no click)
  [deck] profile <name>        tape / keylock / wide (default: tape)
  [deck] loop in | out         manual loop: arm at the beat, engage at the quantized out
  [deck] loop <beats>          beat loop; while looping it re-times out to in+n (halve/double)
  [deck] loop exit | cancel    leave the loop (slip resumes) / drop an armed in
  [deck] loop edit <len|move|in|out> <beats>   retime a running loop in place (no gap)
  [deck] loop halve | double     ÷2 / ×2 the running loop's length (clamped 1/32 .. 64 beats)
  [deck] loop quantum <q>      out-point grid: beat | half | quarter | eighth
  [deck|master] fx ...         effects on a chain — `fx help` for the subcommands
  state                        show every deck
  midi ports                   list MIDI input ports (open one at launch with --midi)
  zoom in|out|fit              waveform zoom (UI only)
  quit                         exit

In the TUI: `load` with no path opens a file picker; F2 picks a MIDI port and F3 the map file.

A leading `deck0` / `0` selects a deck, `master` the summed output. Without one the command
follows the focused deck (TUI: `Tab` switches it).
startup flags: --config <file> (custom topology), --print-config (reference TOML),
               --backend auto|stratum|timestretch, --tui (terminal UI),
               --midi <port> [--midi-map <file>] (MIDI input; default map midi-map.toml),
               --midi-guide [<file>] [--midi <port>] [--decks <n>] (learn-mode map editor)",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully idle deck: nothing to report, so no badge.
    fn idle() -> DeckState {
        DeckState {
            deck_id: 1,
            current_frame: 0,
            playing: true,
            total_frames: 1000,
            bpm: 122.0,
            key: None,
            virtual_frame: 0,
            loop_range: None,
            loop_in_armed: None,
            tempo: 1.0,
            nudgerate: 0.0,
            playing_rate: 1.0,
            lock: false,
            align: None,
            nudge: 0.0,
            sync_leader: None,
            sync_mode: "free".to_owned(),
            group_bpm: 0.0,
        }
    }

    #[test]
    fn an_idle_deck_reports_nothing() {
        assert_eq!(sync_badge(&idle()), None);
        // …so an ordinary `state` line is exactly as short as it was before sync existed, which
        // is what keeps phase_probe.sh's `deck[01][^[]*\[[0-9]+` match working.
        assert!(deck_line(&idle()).ends_with("122.0 BPM  --"), "{}", deck_line(&idle()));
    }

    #[test]
    fn a_locked_pair_reports_its_group() {
        let mut state = idle();
        state.sync_mode = "tempolock".to_owned();
        state.group_bpm = 122.0;
        state.lock = true;
        let badge = sync_badge(&state).expect("a locked deck must show its group");
        assert!(badge.contains("sync tempolock"), "{badge}");
        assert!(badge.contains("122.0 BPM"), "{badge}");
        assert!(badge.contains("locked"), "{badge}");
    }

    #[test]
    fn a_follower_names_its_leader_and_the_leader_names_nobody() {
        let mut follower = idle();
        follower.sync_mode = "phaselock".to_owned();
        follower.group_bpm = 128.0;
        follower.lock = true;
        follower.align = Some("pid".to_owned());
        follower.sync_leader = Some(0);
        let badge = sync_badge(&follower).unwrap();
        assert!(badge.contains("← deck0"), "{badge}");
        assert!(badge.contains("phase pid"), "{badge}");

        // The leader tracks nobody: the engine filters `leader == deck_id`, and the badge must
        // not claim otherwise (a wrong arrow here reads as "deck0 follows itself").
        let mut leader = follower.clone();
        leader.sync_leader = None;
        let badge = sync_badge(&leader).unwrap();
        assert!(!badge.contains("← deck"), "{badge}");
    }

    #[test]
    fn a_bend_is_reported_apart_from_the_controller() {
        let mut bent = idle();
        bent.nudge = 0.04;
        bent.nudgerate = 0.04;
        let badge = sync_badge(&bent).unwrap();
        assert!(badge.contains("nudge +0.040"), "{badge}");
        assert!(!badge.contains("pll"), "a bend must not also read as a controller: {badge}");

        let mut corrected = idle();
        corrected.nudgerate = 0.02; // phase correction only, no nudge
        let badge = sync_badge(&corrected).unwrap();
        assert!(badge.contains("pll +0.020"), "{badge}");
        assert!(!badge.contains("nudge"), "{badge}");
    }

    #[test]
    fn the_badge_rides_after_the_position_so_parsers_still_see_it() {
        let mut state = idle();
        state.sync_mode = "phaselock".to_owned();
        state.group_bpm = 122.0;
        state.sync_leader = Some(0);
        let line = deck_line(&state);
        let position = line.find("[0/1000]").expect("position bracket must survive");
        let badge = line.find("sync phaselock").expect("badge must be rendered");
        assert!(badge > position, "the badge must come after `[current/total]`: {line}");
    }
}
