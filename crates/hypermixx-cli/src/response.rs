//! Human-readable rendering of engine responses.
//!
//! Shared by the REPL and the TUI so the two front-ends can never disagree about what the engine
//! said. These functions return data rather than printing: the REPL writes it to stdout/stderr and
//! the TUI pushes it into its scrollback.

use hypermixx_audio::fx::fader_amp;
use hypermixx_core::{BeatGrid, CommandResponse, DeckState, OutputStatus, Stem, StemStatus};

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
pub fn format_response(response: &CommandResponse, sample_rate: u32) -> Vec<LogLine> {
    match response {
        CommandResponse::Loaded {
            deck_id,
            total_frames,
        } => vec![LogLine::info(format!(
            "deck{deck_id} loaded: {total_frames} frames ({})",
            time(*total_frames, sample_rate)
        ))],
        CommandResponse::State(state) => vec![LogLine::info(deck_line(state, sample_rate))],
        CommandResponse::States(states) => states
            .iter()
            .map(|state| deck_line(state, sample_rate))
            .map(LogLine::info)
            .collect(),
        CommandResponse::Stems(state) => {
            stem_report(state).into_iter().map(LogLine::info).collect()
        }
        CommandResponse::FxAdded { chain, index, kind } => vec![LogLine::info(format!(
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
        CommandResponse::Outputs(outputs) => {
            outputs.iter().map(output_line).map(LogLine::info).collect()
        }
        CommandResponse::Ok => Vec::new(),
        CommandResponse::Error(message) => vec![LogLine::error(message.clone())],
    }
}

/// One line per output destination: what it is, where it is bound, and whether it is audible.
///
/// The state word is the load-bearing part — `missing` says "plug it in and `output refresh`",
/// `disabled` says "this device will not take the engine's rate".
fn output_line(status: &OutputStatus) -> String {
    let device = status.device.as_deref().unwrap_or("-");
    let route = if status.channels == (0, 1) {
        String::new()
    } else {
        format!("  ch {}-{}", status.channels.0, status.channels.1)
    };
    format!(
        "output {:<12} {:<6} {:<8} {device}{route}",
        status.name,
        status.role.name(),
        status.state.label()
    )
}

/// The BPM the deck is *actually playing at*: the local grid BPM scaled by the stable tempo.
/// `None` without a grid. Deliberately excludes `playing_rate`'s nudge and phase correction —
/// those are transient, not the tempo.
pub fn current_bpm(state: &DeckState) -> Option<f32> {
    (state.bpm_at_frame > 0.0).then_some(state.bpm_at_frame * state.tempo)
}

/// The deck's BPM readout: the local grid BPM, with the tempo-scaled "now" BPM appended once it
/// actually differs. Shared so the REPL and TUI can never disagree.
pub fn bpm_label(state: &DeckState) -> String {
    let grid = if state.bpm_at_frame > 0.0 {
        state.bpm_at_frame
    } else {
        state.bpm
    };
    let mut label = if grid > 0.0 {
        format!("{grid:.1} BPM")
    } else {
        "no grid".into()
    };
    if let Some(current) = current_bpm(state).filter(|now| (now - grid).abs() >= 0.05) {
        label.push_str(&format!(" → {current:.1} BPM"));
    }
    label
}

/// One deck's transport line, the shared shape behind both front-ends' deck readouts.
pub fn deck_line(state: &DeckState, sample_rate: u32) -> String {
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
        time(state.total_frames, sample_rate)
    };
    let tempo = bpm_label(state);
    let key = state.key.as_deref().unwrap_or("--");
    let mut line = format!(
        "deck{}  {transport:<7} {} / {duration}  [{}/{}]  {tempo}  {key}  keylock {}",
        state.deck_id,
        time(state.current_frame, sample_rate),
        state.current_frame,
        state.total_frames,
        state.keylock.label(),
    );
    if state.key_shift != 0 {
        line.push_str(&format!("  key {:+}", state.key_shift));
    }
    // Loop furniture, when there is any: the engaged range, a pending manual in point, and the
    // slip clock whenever it has drifted away from what is being heard.
    if state.loop_range.is_some() {
        if let Some(length) = loop_length(state, None, sample_rate) {
            line.push_str(&format!("  loop {length}"));
        }
    }
    if let Some(armed) = state.loop_in_armed {
        line.push_str(&format!("  in armed@{armed}"));
    }
    line.push_str("  ");
    line.push_str(sync_label(state));
    if state.virtual_frame != state.current_frame {
        line.push_str(&format!("  slip {}", state.virtual_frame));
    }
    if let Some(stems) = stems_badge(&state.stems) {
        line.push_str(&format!("  {stems}"));
    }
    line
}

/// The loop's length: a bare beat count when a grid is available, otherwise a clock duration.
pub fn loop_length(
    state: &DeckState,
    grid: Option<&BeatGrid>,
    sample_rate: u32,
) -> Option<String> {
    let (in_frame, out_frame) = state.loop_range?;
    let frames = out_frame.checked_sub(in_frame)?;
    if frames == 0 {
        return None;
    }
    if let Some(grid) = grid {
        let bpm = grid.bpm_at_frame(in_frame);
        if bpm > 0.0 {
            let beats = frames as f64 * f64::from(bpm) / (f64::from(sample_rate) * 60.0);
            if (beats - beats.round()).abs() < 0.02 {
                return Some(format!("{}", beats.round() as u64));
            }
            return Some(format!("{beats:.2}"));
        }
    }
    Some(time(frames, sample_rate))
}

/// The per-stem summary that rides a deck line once stems are live: one letter per stem, its level,
/// and `S`/`M` when a solo or a mute is masking it. `None` for a track with no stems, so an ordinary
/// `state` line is exactly as short as it always was.
///
/// Shared with the TUI, so the REPL and the deck header cannot disagree about what a stem is doing.
pub fn stems_badge(status: &StemStatus) -> Option<String> {
    if !status.ready {
        return None;
    }
    Some(
        Stem::ALL
            .iter()
            .map(|stem| {
                let marker = if status.is_soloed(*stem) {
                    'S'
                } else if status.mute[stem.index()] {
                    'M'
                } else {
                    ' '
                };
                format!(
                    "{}{:+.1}{marker}",
                    stem.name().chars().next().unwrap().to_ascii_uppercase(),
                    status.level[stem.index()]
                )
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// The full per-stem report behind `stem status`: the badge, plus the *effective* position each
/// stem is actually being played at (what mute and solo did to the levels), plus where the audio is.
pub fn stem_report(state: &DeckState) -> Vec<String> {
    if !state.stems.ready {
        return vec![format!(
            "deck{}: no stems — `stem separate` splits the loaded track into four",
            state.deck_id
        )];
    }
    let mut out = vec![format!(
        "deck{}  stems  {}{}   (level 0.0 = unity, -1.0 = silence; S solo, M mute)",
        state.deck_id,
        stems_badge(&state.stems).unwrap_or_default(),
        // How much of the track is *separated*: a chunked job installs its set before the first
        // chunk lands, so "four streams exist" is not "there are stems yet".
        if state.stems.separated > 0.0 && state.stems.separated < 1.0 {
            format!("  {:.0}% separated", state.stems.separated * 100.0)
        } else {
            String::new()
        }
    )];
    for stem in Stem::ALL {
        let effective = state.stems.effective_position(stem);
        let flag = if state.stems.is_soloed(stem) {
            "solo"
        } else if state.stems.mute[stem.index()] {
            "muted"
        } else if state.stems.any_solo() {
            "soloed out"
        } else {
            "audible"
        };
        // `fader_amp` is the law the fader applies, so the dB figure is what is heard, not a
        // re-derivation: -1.0 is silence rather than -80 dB, and that is worth showing as such.
        let gain = fader_amp(effective);
        let gain_text = if gain <= 0.0 {
            "silent".to_owned()
        } else {
            format!("{:+.1} dB", 20.0 * gain.log10())
        };
        out.push(format!(
            "  {:<7} level {:+.2}  {gain_text:>9}  [{flag}]",
            stem.name(),
            state.stems.level[stem.index()]
        ));
    }
    out
}

/// The sync/nudge tail of a deck line: its shared-tempo mode, the group BPM, who it tracks, the
/// running phase correction and any bend. `None` for a deck with nothing to report, so an idle
/// `state` line stays exactly as short as it always was (and `phase_probe.sh`'s parsing is unchanged).
pub fn sync_label(state: &DeckState) -> &'static str {
    if state.sync_mode == "free" {
        "sync off"
    } else if state.sync_leader.is_some() {
        "sync follower"
    } else {
        "sync leader"
    }
}

/// The root command list.
pub fn help_text(decks: usize) -> String {
    format!(
        "commands ({decks} decks, ids 0..{}):
  [deck] load <path> [bpm]     decode a file (deck defaults to the focused one)
  [deck] analyse               run the analyser and publish its grid
  [deck] play                  toggle play/pause
  [deck] cue [play|back|set|hold]
                               play from / return to / set the cue point (bare `cue` = smart);
                               `hold` = the press edge of a held cue button: cue the playhead when
                               paused, then play from the cue point
  [deck] hotcue set|trigger|clear <1..8>
                               one hot-cue slot (A..H); trigger keeps the transport state
  [deck] jump <frame>          seek to a frame (1 second = the engine rate in frames)
  [deck] beatjump <beats>      seek by whole beats, keeping the phase
  [deck] tempo <ratio>         set the tempo (1.0 = unity, 0.5 = half speed)
  [deck] tempofader <-1..1>    pitch fader position (tempo = 1 + position × range)
  [deck] temporange <range>    fader range (default 0.1 = ±10%)
  [deck] sync tempo            match the other deck's BPM once
  [deck] sync phase <mode>     match tempo, then close the beat phase
                               mode: instant | linear [seconds] | pid
  [deck] sync tempolock        both decks share one tempo — either fader moves both
  [deck] sync phaselock <mode> follower tracks the leader exactly (its own fader is ignored)
  [deck] sync set-leader       name *this* deck as the one the others follow
  [deck] sync unlock           drop the lock and the correction, keeping the tempo
  [deck] fader <stage> <pos>   levels: flow|deck|gain|cuesend on a deck, cross|master|cue shared
  [deck] nudge <delta> <sec>   temporary rate bend to hand-align phase (0.04 = 4% fast)
  [deck] nudge off             release the bend (it ramps back, no click)
  [deck] keylock on|off        keylock profile (default: on; window in live.toml)
  [deck] key <semitones>       pitch shift (placeholder: stored, not yet audible)
  [deck] loop in | out         manual loop: arm at the beat, engage at the quantized out
  [deck] loop <beats>          beat loop; while looping it re-times out to in+n (halve/double)
  [deck] loop exit | cancel    leave the loop (slip resumes) / drop an armed in
  [deck] loop edit <len|move|in|out> <beats>   retime a running loop in place (no gap)
  [deck] loop halve | double     ÷2 / ×2 the running loop's length (clamped 1/32 .. 64 beats)
  [deck] loop quantum <q>      out-point grid: beat | half | quarter | eighth
  [deck|master] fx ...         effects on a chain — `fx help` for the subcommands
  [deck] stem separate [--model <name|path>]
                               separate the loaded track and install it block by block as it comes.
                               model/provider/overlap/shifts/threads/memory are `[stems]` keys in
                               live.toml (`live show`, `live reload`)
  [deck] stem model list       the models this build can fetch, and which are downloaded
  [deck] stem model pull <m>   fetch and verify one
  [deck] stem status           per-stem level / mute / solo, and how much is separated
  [deck] stem <preset>         full | acapella | instrumental | drums | bass
  [deck] stem clear            all four stems audible at unity
  [deck] stem cache            where separated stems are cached, and how much they take
  [deck] <stem> level <-1..1>  one stem's level (0 = unity, -1 = silence)
  [deck] <stem> mute [on|off]  mute one stem (solo still wins)
  [deck] <stem> solo [on|off]  add/remove one stem from the solo set
  [deck] <stem> fx ...         inserts on *that stem's* chain, e.g. `deck0 vocals fx add filter`
                               (<stem> = drums | bass | other | vocals)
  output list|refresh          output destinations and their bindings
  live show|reload [<file>]    this machine's settings (engine rate, ring, stem defaults)
  profile show|reload [<name>] which files this session runs with; reload switches them
  browser list                 configured sources and their ids
  browser refresh [<id>]       rescan one source, or every configured source
  browser tracks <list-id>     show one list's tracks
  browser show <location-id>   inspect one track
  state                        show every deck
  midi ports                   list MIDI input ports (open one at launch with --midi)
  zoom in|out|fit              waveform zoom (UI only)
  quit                         exit

In the TUI: `load` with no path opens a file picker; F2 picks a MIDI port and F3 the map file.
With the browser target only `browser ...` commands are accepted. Arrows navigate the Miller
columns; Enter on a list opens it, Enter on a track opens the deck chooser, Esc goes back.

A leading `deck0` / `0` selects a deck, `master` the summed output. Without one the command
follows the focused deck (TUI: `Tab` cycles the command target deck0 → deck1 → browser).
startup flags: --config <file> (custom topology), --profile <name|file> (which files to use),
               --print-config (reference TOML), --live <file> (engine rate, ring, stem defaults),
               --backend auto|stratum|timestretch, --tui (terminal UI),
               --midi <port> [--midi-map <file>] (MIDI input; default map midi-map.toml),
               --midi-guide [<file>] [--midi <port>] [--decks <n>] (learn-mode map editor)",
        decks - 1
    )
}

/// The `stem` family's own help, for the `stem` verb with no subcommand.
pub fn stem_help_text() -> String {
    "stem commands (the deck is the line's target, focused by default):
  stem separate [--model <name|path>]
                                 separate the loaded track and install it block by block as the
                                 model finishes each window. How it runs is `[stems]` in live.toml:
                                 model / provider / overlap (the block length) / shifts / threads /
                                 low_memory — `live show` lists them, `live reload` applies an edit
  stem model list                the models this build can fetch, and which are downloaded
  stem model pull <name>         fetch and verify one (or point `[stems] model` at a local .onnx)
  stem status                    show the per-stem level / mute / solo
  stem full | acapella | instrumental | drums | bass
                                 a named arrangement (mute flags; your levels survive)
  stem clear                     all four audible at unity, nothing soloed
  stem cancel                    stop a running separation (at its current model window)
  stem cache                     the cache directory and its size
  stem cache prune [--keep <n>]  drop all but the n most recent (default 4); `clear` = all

One stem at a time (target it directly):
  deck0 vocals level -0.5        that stem's level (0 = unity, -1 = exact silence)
  deck0 vocals mute [on|off]     mute it (a solo elsewhere still wins)
  deck0 vocals solo [on|off]     add/remove it from the solo set
  deck0 vocals unsolo            leave the solo set
  deck0 vocals fx add filter     inserts on *that stem's* chain

Separation is offline and asynchronous: the deck keeps playing the mix (stream 0) until the first
block lands, then turns into stems window by window. Re-running a separated track is served from the
cache."
        .to_owned()
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

/// Frames -> `m:ss.mmm` at the given engine rate.
pub fn time(frames: u64, sample_rate: u32) -> String {
    let millis = frames as f64 * 1000.0 / sample_rate.max(1) as f64;
    let minutes = (millis / 60_000.0) as u64;
    let seconds = (millis / 1000.0) as u64 % 60;
    let remainder = (millis % 1000.0) as u32;
    format!("{minutes}:{seconds:02}.{remainder:03}")
}

#[cfg(test)]
mod tests {
    use hypermixx_core::{OutputRole, OutputState};

    /// The output line carries the three things a user needs: what it is, where it is bound, and
    /// whether it is audible.
    #[test]
    fn an_output_line_names_state_role_and_device() {
        let status = |state, device: Option<&str>, channels| OutputStatus {
            name: "cans".into(),
            role: OutputRole::Cue,
            state,
            device: device.map(str::to_owned),
            channels,
            gain: 1.0,
            writes: 0,
            overruns: 0,
        };
        let live = output_line(&status(OutputState::Live, Some("USB Headset"), (0, 1)));
        assert!(live.contains("cans") && live.contains("cue") && live.contains("live"));
        assert!(live.contains("USB Headset"));
        // A missing device has nothing to name, and says so rather than leaving a gap that reads
        // like a bug.
        let missing = output_line(&status(OutputState::Missing, None, (0, 1)));
        assert!(missing.contains("missing") && missing.contains('-'));
        // A non-default channel pair is worth showing; (0,1) is not.
        assert!(output_line(&status(OutputState::Live, Some("x"), (2, 3))).contains("ch 2-3"));
        assert!(!live.contains("ch 0-1"));
    }

    use super::*;
    use hypermixx_core::{KeylockMode, OutputStatus, StemStatus, SAMPLE_RATE};

    /// A fully idle deck: nothing to report, so no badge.
    fn idle() -> DeckState {
        DeckState {
            vinyl: false,
            deck_id: 1,
            current_frame: 0,
            playing: true,
            total_frames: 1000,
            bpm: 122.0,
            bpm_at_frame: 122.0,
            key: None,
            virtual_frame: 0,
            loop_range: None,
            loop_in_armed: None,
            tempo: 1.0,
            tempo_fader: 0.0,
            tempo_range: 0.1,
            nudgerate: 0.0,
            playing_rate: 1.0,
            lock: false,
            align: None,
            nudge: 0.0,
            sync_leader: None,
            sync_mode: "free".to_owned(),
            group_bpm: 0.0,
            cue_frame: 0,
            hot_cues: [None; hypermixx_core::HOT_CUE_SLOTS],
            keylock: KeylockMode::On,
            key_shift: 0,
            stems: StemStatus::default(),
        }
    }

    #[test]
    fn current_bpm_scales_with_tempo_not_nudgerate() {
        let mut state = idle();
        state.tempo = 1.05;
        state.playing_rate = 1.11; // nudge present; must be ignored
        let now = current_bpm(&state).expect("a grid deck has a current BPM");
        assert!((now - 122.0 * 1.05).abs() < 1e-3, "got {now}");
        assert!(
            !deck_line(&state, SAMPLE_RATE).contains("1.11"),
            "playing_rate leaked into the line"
        );
        assert!(
            deck_line(&state, SAMPLE_RATE).contains("122.0 BPM → 128.1 BPM"),
            "{}",
            deck_line(&state, SAMPLE_RATE)
        );
        // No grid means no current BPM to show.
        state.bpm_at_frame = 0.0;
        assert_eq!(current_bpm(&state), None);
    }

    #[test]
    fn sync_is_only_leader_follower_or_off() {
        let idle = idle();
        assert_eq!(sync_label(&idle), "sync off");

        let mut leader = idle.clone();
        leader.sync_mode = "tempolock".to_owned();
        leader.group_bpm = 122.0;
        leader.lock = true;
        assert_eq!(sync_label(&leader), "sync leader");

        let mut follower = idle.clone();
        follower.sync_mode = "phaselock".to_owned();
        follower.sync_leader = Some(0);
        assert_eq!(sync_label(&follower), "sync follower");

        // Nudges and phase correction detail belong in the engine, not the sync label.
        let mut bent = idle.clone();
        bent.nudge = 0.04;
        bent.nudgerate = 0.04;
        assert_eq!(sync_label(&bent), "sync off");
    }

    #[test]
    fn loop_length_is_beats_with_a_grid_and_time_without_one() {
        let mut state = idle();
        state.loop_range = Some((11_025, 55_125));
        let grid = BeatGrid::from_constant_bpm(120.0, 0, 441_000, SAMPLE_RATE);
        assert_eq!(
            loop_length(&state, Some(&grid), SAMPLE_RATE).as_deref(),
            Some("2")
        );
        assert_eq!(
            loop_length(&state, None, SAMPLE_RATE).as_deref(),
            Some("0:01.000")
        );
    }

    #[test]
    fn time_uses_the_rate_the_frames_were_rendered_at() {
        assert_eq!(time(44_100, 44_100), "0:01.000");
        assert_eq!(time(48_000, 48_000), "0:01.000");
        assert_eq!(time(48_000, 44_100), "0:01.088");
    }

    #[test]
    fn the_badge_rides_after_the_position_so_parsers_still_see_it() {
        let mut state = idle();
        state.sync_mode = "phaselock".to_owned();
        state.group_bpm = 122.0;
        state.sync_leader = Some(0);
        let line = deck_line(&state, SAMPLE_RATE);
        let position = line
            .find("[0/1000]")
            .expect("position bracket must survive");
        let badge = line.find("sync follower").expect("badge must be rendered");
        assert!(
            badge > position,
            "the badge must come after `[current/total]`: {line}"
        );
    }
}
