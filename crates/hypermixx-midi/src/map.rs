//! The mapping file: a TOML schema, its validation, and the action registry that both the file
//! checker and the guide's target list are generated from.
//!
//! ```toml
//! [meta]
//! name = "my-controller"
//!
//! [[bind]]
//! type = "cc"          # cc | note | bend
//! mode = "abs"         # abs | rel1 | rel2 | rel3 (cc only)
//! channel = 0          # 0-based; omit to match any channel
//! id = 7               # CC controller / note key
//! deck = 0             # omit for global targets
//! action = "fader.flow"
//! ```
//!
//! Parsing is `deny_unknown_fields`, so a typo'd key is an error with the TOML position rather
//! than a silently ignored binding. The file names FX by **name** (`chain`/`fx`/`param`), never by
//! slot index: indices shift when an earlier slot is removed, and the file must survive a restart.
//! The CLI resolves names to indices via `ListFx` and installs them with [`Map::resolve_fx`].

use std::collections::HashMap;

use hypermixx_core::{BrowserAction, DeckId, FaderTarget, FxChainId, HotCueOp, PhaseMode, Stem};
use serde::{Deserialize, Serialize};

/// A parse or validation failure. The message carries the TOML position when the error came from
/// the parser itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapError {
    message: String,
}

impl MapError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for MapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MapError {}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Which MIDI message kind a binding listens for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Cc,
    Note,
    Bend,
}

/// How a CC carries movement. Absolute knobs and faders are `Abs`; endless encoders differ by
/// vendor, so the exact wraparound rule is per binding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RelMode {
    #[default]
    Abs,
    /// Offset binary around 64: `value - 64`, so 64 is "no movement", 65..127 = +, 1..63 = −.
    Rel1,
    /// Two's complement around 64: 1..63 = +, 65..127 = − (127 = −1).
    Rel2,
    /// Direction only (1..63 = +, 65..127 = −); the magnitude is ignored, so a fast turn is worth
    /// the same as a slow one. For a device that reports only the sign of each step.
    Rel3,
}

/// A response shaping applied after the value is normalised to `0..=1`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Curve {
    #[default]
    Linear,
    /// Square law — finer control near the bottom, coarser near the top.
    Sharp,
}

/// One `[[bind]]` table, straight off the parser. The guide edits these and serialises them back,
/// so every optional field is skipped when absent.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawBinding {
    #[serde(rename = "type")]
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RelMode>,
    /// Flip the sign of a relative encoder's delta. Some controllers number their detents the other
    /// way round; the guide cannot tell, so it is a per-binding choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deck: Option<DeckId>,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curve: Option<Curve>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<f32>,
    /// Nudge duration in seconds; when present the bend releases itself and the note-up edge is
    /// ignored (otherwise the binding is momentary — held for as long as the button is down).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beats: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

/// The whole file: an optional `[meta]` and a list of `[[bind]]`.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MapFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
    #[serde(default, rename = "bind")]
    pub binds: Vec<RawBinding>,
}

impl MapFile {
    /// Parses a mapping file without validating the actions (see [`Map::from_toml_str`] for that).
    pub fn from_toml_str(text: &str) -> Result<Self, MapError> {
        toml::from_str(text).map_err(|err| MapError::new(err.to_string()))
    }

    /// Serialises back to TOML. `[meta]` is omitted when absent; each `[[bind]]` only writes the
    /// fields it actually uses.
    pub fn to_toml_string(&self) -> Result<String, MapError> {
        toml::to_string(self).map_err(|err| MapError::new(err.to_string()))
    }
}

/// A validated, executable binding.
#[derive(Clone, Debug, PartialEq)]
pub struct BindingSpec {
    pub kind: EventKind,
    pub mode: RelMode,
    /// Sign flip for relative encoders ([`RawBinding::reverse`]).
    pub reverse: bool,
    /// `None` matches any channel.
    pub channel: Option<u8>,
    /// CC controller / note key. Ignored for bend.
    pub id: u8,
    /// The deck the action addresses, when it needs one.
    pub deck: Option<DeckId>,
    pub action: Action,
    /// Value at normalised position 0.
    pub min: f32,
    /// Value at normalised position 1.
    pub max: f32,
    pub curve: Curve,
}

impl BindingSpec {
    /// Whether this binding listens to `(channel, id)`.
    pub fn matches(&self, channel: u8, id: u8) -> bool {
        self.channel.is_none_or(|wanted| wanted == channel) && self.id == id
    }
}

/// A binding's parsed, parameterised action.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Play,
    /// Momentary cue button. The press edge is [`CueOp::Hold`](hypermixx_core::CueOp::Hold): while
    /// playing it is a back cue, while paused it cues the playhead and plays it. The release edge is
    /// `Back`. This is the only cue row the guide lists.
    Cue,
    /// One-shot cue: the deck decides between "back to cue" and "set cue here", and never plays.
    ///
    /// Kept — and kept out of [`ACTIONS`], so the guide offers one cue row instead of two — for a
    /// button that should only *place* the cue point. `cue.smart` is the name a map uses for it.
    CueSmart,
    BeatJump {
        beats: i64,
    },
    Fader(FaderTarget),
    /// Tempo fader: a bipolar position in `-1..=1`. The deck maps it to a rate under its own
    /// `temporange`, so the range is a deck setting, not a per-binding constant.
    TempoFader,
    /// Rate bend. `seconds` set = timed (release edge ignored); `None` = momentary, held while
    /// the control is down.
    Nudge {
        delta: f32,
        seconds: Option<f64>,
    },
    /// A DJ wheel, which is two controls in one message stream and is therefore bound **twice** on a
    /// real controller:
    ///
    /// * a **note** (the platter sensor) → the touch and its release;
    /// * a **relative CC** (the wheel itself) → turns, `step` frames of audio per tick.
    ///
    /// What a turn *means* — move the playhead or bend the tempo — is the deck's decision, because
    /// only the deck knows whether the platter is held. That is why both bindings carry the same
    /// action name.
    Vinyl {
        /// Frames of audio per wheel tick (the `step` field), applied only while the platter is held.
        frames_per_tick: f32,
    },
    Loop(LoopAction),
    Sync(SyncAction),
    Fx(FxAction),
    /// Latch one stem's mute (a button, so it flips rather than holding a value).
    StemMute {
        stem: Stem,
    },
    /// Latch one stem's solo membership.
    StemSolo {
        stem: Stem,
    },
    /// Browser navigation/load. Consumed by the front-end, never sent to the engine.
    Browser(BrowserAction),
    /// One hot-cue slot operation.
    HotCue(HotCueOp),
}

impl Action {
    /// Continuous actions read a value from every event; the rest fire on an edge.
    pub fn is_continuous(&self) -> bool {
        matches!(
            self,
            Action::Fader(_) | Action::TempoFader | Action::Fx(FxAction::Param { .. })
        )
    }

    /// Momentary actions also act on the release edge.
    pub fn is_momentary(&self) -> bool {
        matches!(
            self,
            Action::Cue
                | Action::Vinyl { .. }
                | Action::Fx(FxAction::Pad { .. })
                | Action::Nudge { seconds: None, .. }
        )
    }

    /// Whether this action's default value domain is bipolar (`-1..=1`) rather than `0..=1`.
    pub fn is_bipolar(&self) -> bool {
        matches!(self, Action::Fader(target) if target.is_bipolar())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopAction {
    In,
    Out,
    Exit,
    Cancel,
    Halve,
    Double,
    Beats(u64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SyncAction {
    /// Match the other deck's BPM once.
    Tempo,
    /// Match the BPM and close the phase with `mode` over `t_seconds` (the engine's default when
    /// the line does not say).
    Phase {
        mode: PhaseMode,
        t_seconds: Option<f64>,
    },
    PhaseLock {
        mode: PhaseMode,
        t_seconds: Option<f64>,
    },
    TempoLock,
    Leader,
    Unlock,
}

/// FX actions address a chain and a slot by name. Slot indices are resolved by the CLI after it
/// has asked the engine for each chain, never written into the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FxAction {
    On {
        chain: FxChainId,
        fx: String,
    },
    Off {
        chain: FxChainId,
        fx: String,
    },
    Toggle {
        chain: FxChainId,
        fx: String,
    },
    Pad {
        chain: FxChainId,
        fx: String,
    },
    Trigger {
        chain: FxChainId,
        fx: String,
    },
    Param {
        chain: FxChainId,
        fx: String,
        param: String,
    },
}

impl FxAction {
    /// The chain this action addresses and the slot's name.
    pub fn address(&self) -> (FxChainId, &str) {
        match self {
            FxAction::On { chain, fx }
            | FxAction::Off { chain, fx }
            | FxAction::Toggle { chain, fx }
            | FxAction::Pad { chain, fx }
            | FxAction::Trigger { chain, fx }
            | FxAction::Param { chain, fx, .. } => (*chain, fx),
        }
    }
}

/// The action vocabulary, re-exported from `core`: there is exactly one table, and its names **are**
/// the session command lines a front-end takes (minus the target).
pub use hypermixx_core::action::{ActionSpec, ACTIONS};

/// The baseline nudge, as a rate: one wheel message bends by this much (`0.15` = 15 % fast), and a
/// binding that omits `step` gets it.
///
/// Deliberately large, because most controller encoders report ±1 per message however fast you turn:
/// the message *rate* is the only speed information there is, so one message has to be worth
/// something on its own. With `Nudge::bend` accumulating up to `NUDGE_LIMIT`, two messages reach a
/// wheel-sized bend.
pub const DEFAULT_NUDGE_STEP: f32 = 0.15;

/// How long a bend started by a **relative encoder tick** lasts when the binding omits `seconds`.
///
/// Short on purpose: each message bends, then gets out of the way, so turning feels like nudging the
/// platter rather than gradually changing the tempo. The release is re-armed by every message, so a
/// turn that keeps going keeps bending.
///
/// An encoder has no release edge — one tick is one message — so a bend with no end would never be
/// released. (A *note* has both edges, so `seconds` omitted there still means "hold while the button
/// is down", which is why this default is applied by the translator rather than at parse time.)
pub const DEFAULT_NUDGE_SECONDS: f64 = 0.06;

/// Frames of audio one wheel tick moves the playhead while the platter is held, when a `vinyl`
/// binding omits `step` (or an `action = "vinyl"` line omits its argument).
///
/// Tune it with the physics: a record at 33⅓ RPM carries **1.8 s per revolution**, so
///
/// ```text
/// frames_per_tick = 79380 / (ticks your wheel sends per revolution)
/// ```
///
/// A slow encoder that reports ±1 a few times a second needs a big number to feel like a platter at
/// all; 1323 is that formula for a 60-detent wheel (and 441 = ⅓ of it, for deliberately fine
/// placement). To measure yours: open the guide's raw monitor, turn the wheel exactly one revolution,
/// and count the messages.
pub const DEFAULT_VINYL_FRAMES_PER_TICK: f32 = 1323.0;

/// One selectable target in the guide's checklist: an action, scoped to a deck when it needs one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub deck: Option<DeckId>,
    pub action: &'static str,
    pub label: String,
    /// Hidden from the checklist cursor (group headers, if a renderer adds them).
    pub heading: bool,
}

/// Builds the standard checklist for `decks` decks from [`ACTIONS`].
///
/// The label is the command line, so what the guide shows is exactly what it writes into the file —
/// and exactly what a user could have typed in the session.
pub fn manifest(decks: usize) -> Vec<Target> {
    let mut out = Vec::new();
    for spec in ACTIONS {
        if spec.deck {
            for deck in 0..decks {
                out.push(Target {
                    deck: Some(deck as DeckId),
                    action: spec.name,
                    label: format!("deck{deck}  {}", spec.name),
                    heading: false,
                });
            }
        } else {
            out.push(Target {
                deck: None,
                action: spec.name,
                label: spec.name.to_owned(),
                heading: false,
            });
        }
    }
    out
}

/// A parsed mapping, ready to translate events.
#[derive(Clone, Debug)]
pub struct Map {
    pub meta: Option<Meta>,
    pub binds: Vec<BindingSpec>,
    /// Resolved `chain + fx name -> slot index`, installed by [`Map::resolve_fx`] once a front-end
    /// has asked the engine for its chains.
    fx_slots: HashMap<(FxChainId, String), usize>,
}

impl Map {
    /// Parses and validates a map file. Parser errors keep the TOML position; semantic errors name
    /// the offending bind.
    pub fn from_toml_str(text: &str) -> Result<Self, MapError> {
        let file = MapFile::from_toml_str(text)?;
        let mut binds = Vec::with_capacity(file.binds.len());
        for raw in &file.binds {
            let bind = compile(raw)
                .map_err(|msg| MapError::new(format!("bind `{}`: {msg}", raw.action)))?;
            binds.push(bind);
        }
        Ok(Self {
            meta: file.meta,
            binds,
            fx_slots: HashMap::new(),
        })
    }

    /// An empty map (no bindings), for a fresh guide session.
    pub fn empty() -> Self {
        Self {
            meta: None,
            binds: Vec::new(),
            fx_slots: HashMap::new(),
        }
    }

    /// Records that `name` is slot `index` in `chain`. Called by the CLI after `ListFx`.
    pub fn resolve_fx(&mut self, chain: FxChainId, name: impl Into<String>, index: usize) {
        self.fx_slots.insert((chain, name.into()), index);
    }

    /// The slot index for a named effect, if it has been resolved.
    pub fn fx_slot(&self, chain: FxChainId, name: &str) -> Option<usize> {
        self.fx_slots.get(&(chain, name.to_owned())).copied()
    }
}

/// Turns a raw binding into an executable one, or explains why it cannot.
fn compile(raw: &RawBinding) -> Result<BindingSpec, String> {
    let action = parse_action(raw)?;
    if raw.mode.is_some() && raw.kind != EventKind::Cc {
        return Err("`mode` is only meaningful for `type = \"cc\"`".to_owned());
    }
    let mode = match raw.kind {
        EventKind::Cc => raw.mode.unwrap_or_default(),
        // Bend is always absolute.
        EventKind::Note | EventKind::Bend => RelMode::Abs,
    };
    if raw.reverse == Some(true) && mode == RelMode::Abs {
        return Err(
            "`reverse` is only meaningful for a relative `mode` (rel1 | rel2 | rel3)".to_owned(),
        );
    }
    let bipolar = action.is_bipolar();
    // FX params default to the effect's own 0..1 domain; faders use their target's domain.
    let (def_min, def_max) = if bipolar { (-1.0, 1.0) } else { (0.0, 1.0) };
    if let (Some(min), Some(max)) = (raw.min, raw.max) {
        if !(min.is_finite() && max.is_finite()) || min >= max {
            return Err("`min` must be finite and less than `max`".to_owned());
        }
    }
    let spec = BindingSpec {
        kind: raw.kind,
        mode,
        reverse: raw.reverse.unwrap_or(false),
        channel: raw.channel,
        id: raw.id.unwrap_or(0),
        deck: raw.deck,
        action,
        min: raw.min.unwrap_or(def_min),
        max: raw.max.unwrap_or(def_max),
        curve: raw.curve.unwrap_or_default(),
    };
    if let Some(channel) = spec.channel {
        if channel > 15 {
            return Err(format!("channel {channel} is out of range (0..=15)"));
        }
    }
    Ok(spec)
}

/// Parses a stem out of an action name, naming the alternatives when it is wrong — the same
/// treatment `deck` gets.
fn stem_of(token: &str) -> Result<Stem, String> {
    Stem::parse(token).ok_or_else(|| {
        format!(
            "unknown stem `{token}` (expected {})",
            Stem::ALL
                .iter()
                .map(|stem| stem.name())
                .collect::<Vec<_>>()
                .join(" | ")
        )
    })
}

/// Resolves a binding's `action` plus its optional parameters.
///
/// The action is **a session command line without the target**: `"play"`, `"beatjump 4"`,
/// `"sync phase pid"`, `"vocals mute"`, `"fader flow"`. The target is the binding's `deck` field
/// (or, when the line names one itself, that word). Legacy dot-spelled names are still accepted —
/// see [`hypermixx_core::action::canonical`] — because the file is user-editable and a map that
/// silently stops working after an upgrade is worse than one that keeps an old spelling.
///
/// Binding-level parameters stay fields (`step`, `seconds`, `beats`, `chain`/`fx`/`param`) because
/// they describe the *control* rather than the command: a hardware encoder's detent size, a
/// controller's nudge duration, the slot an FX button addresses by name.
fn parse_action(raw: &RawBinding) -> Result<Action, String> {
    let Some(line) = hypermixx_core::action::canonical(&raw.action) else {
        return Err(format!(
            "unknown action `{}` (see `docs/midi-mapping.md` for the list)",
            raw.action
        ));
    };
    let words: Vec<&str> = line.split_whitespace().collect();
    let original = raw.action.as_str();
    let deck = |what: &str| -> Result<DeckId, String> {
        raw.deck
            .ok_or_else(|| format!("action `{what}` needs a `deck`"))
    };
    let number = |word: Option<&str>, what: &str| -> Result<Option<f64>, String> {
        match word {
            None => Ok(None),
            Some(word) => word
                .parse::<f64>()
                .map(Some)
                .map_err(|_| format!("`{what}` must be a number, got `{word}`")),
        }
    };

    match words.as_slice() {
        ["play"] => {
            deck("play")?;
            Ok(Action::Play)
        }
        ["hotcue", what, slot] => {
            deck("hotcue")?;
            let slot = slot
                .parse::<u8>()
                .ok()
                .filter(|slot| (1..=hypermixx_core::HOT_CUE_SLOTS as u8).contains(slot))
                .ok_or_else(|| "hotcue slot must be 1..=8".to_owned())?
                - 1;
            Ok(Action::HotCue(match *what {
                "set" => HotCueOp::Set(slot),
                "trigger" | "play" => HotCueOp::Trigger(slot),
                "clear" => HotCueOp::Clear(slot),
                other => return Err(format!("unknown hotcue action `{other}`")),
            }))
        }
        ["browser", what] => Ok(Action::Browser(match *what {
            "up" => BrowserAction::Up,
            "down" => BrowserAction::Down,
            "open" => BrowserAction::Open,
            "exit" => BrowserAction::Exit,
            "refresh" => BrowserAction::Refresh,
            "load" => BrowserAction::Load { deck: raw.deck },
            other => return Err(format!("unknown browser action `{other}`")),
        })),
        ["cue"] => {
            deck("cue")?;
            Ok(Action::Cue)
        }
        // Not in the registry (the guide offers one cue row), but a valid spelling: a button that
        // only *places* the cue point.
        ["cue", "smart"] => {
            deck("cue smart")?;
            Ok(Action::CueSmart)
        }
        ["vinyl"] | ["vinyl", _] => {
            deck("vinyl")?;
            let step = match number(words.get(1).copied(), "vinyl frames per tick")? {
                Some(frames) => frames as f32,
                None => raw.step.unwrap_or(DEFAULT_VINYL_FRAMES_PER_TICK),
            };
            if !step.is_finite() || step <= 0.0 {
                return Err("vinyl frames per tick must be a positive number".to_owned());
            }
            Ok(Action::Vinyl {
                frames_per_tick: step,
            })
        }
        ["beatjump"] | ["beatjump", _] => {
            deck("beatjump")?;
            // The sign can come from either the line (`beatjump -8`) or a legacy name
            // (`beatjump-`), and the magnitude from the line or the binding's `step`.
            let from_line = number(words.get(1).copied(), "beatjump beats")?;
            let magnitude = match from_line {
                Some(beats) => beats.abs(),
                None => f64::from(raw.step.unwrap_or(1.0)).abs(),
            };
            if !magnitude.is_finite() || magnitude < 1.0 {
                return Err("a beatjump needs at least one beat".to_owned());
            }
            let negative =
                from_line.map(|beats| beats < 0.0).unwrap_or(false) || original.ends_with('-');
            let beats = magnitude.round() as i64;
            Ok(Action::BeatJump {
                beats: if negative { -beats } else { beats },
            })
        }
        ["fader", "flow"] => Ok(Action::Fader(FaderTarget::Flow(deck("fader flow")?))),
        ["fader", "deck"] => Ok(Action::Fader(FaderTarget::Deck(deck("fader deck")?))),
        ["fader", "gain"] => Ok(Action::Fader(FaderTarget::Gain(deck("fader gain")?))),
        ["fader", "cuesend"] => Ok(Action::Fader(FaderTarget::CueSend(deck("fader cuesend")?))),
        ["fader", "cross"] => Ok(Action::Fader(FaderTarget::Crossfader)),
        ["fader", "master"] => Ok(Action::Fader(FaderTarget::Master)),
        ["fader", "cue"] => Ok(Action::Fader(FaderTarget::Cue)),
        // `<stem> fader | mute | solo` — the stem is the first word, exactly as a session line
        // writes it (`deck0 vocals mute`).
        [stem_name, what @ ("fader" | "mute" | "solo")] => {
            let stem = stem_of(stem_name)?;
            let deck_id = deck(original)?;
            match *what {
                "fader" => Ok(Action::Fader(FaderTarget::Stem {
                    deck: deck_id,
                    stem,
                })),
                "mute" => Ok(Action::StemMute { stem }),
                _ => Ok(Action::StemSolo { stem }),
            }
        }
        ["tempofader"] => {
            deck("tempofader")?;
            Ok(Action::TempoFader)
        }
        ["nudge"] | ["nudge", _, _] => {
            deck("nudge")?;
            let delta = match number(words.get(1).copied(), "nudge delta")? {
                Some(delta) => delta as f32,
                None => raw.step.unwrap_or(DEFAULT_NUDGE_STEP),
            };
            if !delta.is_finite() || delta == 0.0 {
                return Err("a nudge delta must be non-zero".to_owned());
            }
            let seconds = match number(words.get(2).copied(), "nudge seconds")? {
                Some(seconds) => Some(seconds),
                None => raw.seconds,
            };
            Ok(Action::Nudge { delta, seconds })
        }
        ["loop", what @ ("in" | "out" | "exit" | "cancel" | "halve" | "double")] => {
            deck("loop")?;
            Ok(Action::Loop(match *what {
                "in" => LoopAction::In,
                "out" => LoopAction::Out,
                "exit" => LoopAction::Exit,
                "cancel" => LoopAction::Cancel,
                "halve" => LoopAction::Halve,
                _ => LoopAction::Double,
            }))
        }
        // `loop 8`, or `loop` with the length in the binding's `beats` field.
        ["loop"] | ["loop", _] => {
            deck("loop")?;
            // A button with no length is the DJ default: four beats. (The CLI requires the number —
            // a *line* has room for it, a button does not.)
            let beats = match number(words.get(1).copied(), "loop beats")? {
                Some(beats) if beats >= 1.0 => beats.round() as u64,
                Some(_) => return Err("a beat loop needs at least one beat".to_owned()),
                None => raw.beats.filter(|beats| *beats > 0).unwrap_or(4),
            };
            Ok(Action::Loop(LoopAction::Beats(beats)))
        }
        ["sync", what] => {
            deck("sync")?;
            Ok(Action::Sync(match *what {
                "tempo" => SyncAction::Tempo,
                "tempolock" => SyncAction::TempoLock,
                "set-leader" | "leader" => SyncAction::Leader,
                "unlock" => SyncAction::Unlock,
                "phase" => SyncAction::Phase {
                    mode: PhaseMode::Pid,
                    t_seconds: None,
                },
                "phaselock" => SyncAction::PhaseLock {
                    mode: PhaseMode::Pid,
                    t_seconds: None,
                },
                other => return Err(format!("unknown sync action `{other}`")),
            }))
        }
        ["sync", phase @ ("phase" | "phaselock"), rest @ ..] => {
            deck("sync")?;
            let (mode, seconds) = phase_args(rest)?;
            Ok(Action::Sync(if *phase == "phase" {
                SyncAction::Phase {
                    mode,
                    t_seconds: seconds,
                }
            } else {
                SyncAction::PhaseLock {
                    mode,
                    t_seconds: seconds,
                }
            }))
        }
        // `<stem> fx …` / `master fx …` / `d1 fx …`: the action names its chain, the way a session
        // line's leading target word does (`deck0 vocals fx toggle filter`).
        [target, "fx", what @ ("on" | "off" | "toggle" | "pad" | "trigger")] => {
            let named = chain_of(raw, target)?;
            let kind = match *what {
                "on" => FxActionKind::On,
                "off" => FxActionKind::Off,
                "toggle" => FxActionKind::Toggle,
                "pad" => FxActionKind::Pad,
                _ => FxActionKind::Trigger,
            };
            Ok(Action::Fx(fx_slot_action(raw, Some(named), kind)?))
        }
        [target, "fx", "param"] => {
            let named = chain_of(raw, target)?;
            let (chain, fx) = fx_address(raw, Some(named))?;
            let param = raw.param.clone().ok_or("`fx param` needs a `param` name")?;
            Ok(Action::Fx(FxAction::Param { chain, fx, param }))
        }
        ["fx", what @ ("on" | "off" | "toggle" | "pad" | "trigger")] => {
            let kind = match *what {
                "on" => FxActionKind::On,
                "off" => FxActionKind::Off,
                "toggle" => FxActionKind::Toggle,
                "pad" => FxActionKind::Pad,
                _ => FxActionKind::Trigger,
            };
            Ok(Action::Fx(fx_slot_action(raw, None, kind)?))
        }
        ["fx", "param"] => {
            let (chain, fx) = fx_address(raw, None)?;
            let param = raw.param.clone().ok_or("`fx param` needs a `param` name")?;
            Ok(Action::Fx(FxAction::Param { chain, fx, param }))
        }
        // The *canonical* line is what is named, so a legacy spelling gets an error about the
        // spelling it resolves to (`vocals solo_only`) rather than about the string in the file.
        _ => Err(format!("unknown action `{line}`")),
    }
}

/// The `<mode> [seconds]` tail of a `sync phase` line. Defaults to the PI controller, which is what
/// a button wants: press it, and the decks converge.
fn phase_args(rest: &[&str]) -> Result<(PhaseMode, Option<f64>), String> {
    let mut mode = PhaseMode::Pid;
    let mut seconds = None;
    for word in rest {
        if let Some(parsed) = PhaseMode::parse(word) {
            mode = parsed;
            continue;
        }
        let value = word
            .parse::<f64>()
            .map_err(|_| format!("`{word}` is neither a phase mode nor a duration"))?;
        if value <= 0.0 {
            return Err("a linear phase sync needs a positive duration".to_owned());
        }
        seconds = Some(value);
    }
    if mode == PhaseMode::Linear && seconds.is_none() {
        return Err("`sync phase linear` needs a duration".to_owned());
    }
    Ok((mode, seconds))
}

/// The five slot-level FX actions, which all need `chain` + `fx` but no parameter.
enum FxActionKind {
    On,
    Off,
    Toggle,
    Pad,
    Trigger,
}

/// Builds a slot-level FX action from its `chain`/`fx` fields.
fn fx_slot_action(
    raw: &RawBinding,
    named: Option<FxChainId>,
    kind: FxActionKind,
) -> Result<FxAction, String> {
    let (chain, fx) = fx_address(raw, named)?;
    Ok(match kind {
        FxActionKind::On => FxAction::On { chain, fx },
        FxActionKind::Off => FxAction::Off { chain, fx },
        FxActionKind::Toggle => FxAction::Toggle { chain, fx },
        FxActionKind::Pad => FxAction::Pad { chain, fx },
        FxActionKind::Trigger => FxAction::Trigger { chain, fx },
    })
}

/// The `chain` + `fx` address every FX action carries: the chain from the action line if it named
/// one, else from the binding's `chain` field.
fn fx_address(raw: &RawBinding, named: Option<FxChainId>) -> Result<(FxChainId, String), String> {
    let chain = match named {
        Some(chain) => chain,
        None => {
            let token = raw
                .chain
                .as_deref()
                .ok_or_else(|| "an FX action needs a `chain`".to_owned())?;
            chain_of(raw, token)?
        }
    };
    let fx = raw.fx.clone().ok_or("an FX action needs an `fx` name")?;
    Ok((chain, fx))
}

/// Resolves an FX chain address the way the session grammar writes it:
///
/// * `master` / `m` — the summed mix;
/// * `deck0` / `d0` / `0` — that deck's channel chain;
/// * `<stem>` — that stem's own chain, with the deck from the binding's `deck` field
///   (`deck = 0` + `chain = "vocals"` is `deck0 vocals`);
/// * `deck0/<stem>` / `d1/<stem>` — the same, spelled outright.
///
/// The stem form is what makes "every stem its own FX" reachable from a controller: the CLI has had
/// `deck0 vocals fx …` all along, and this is the mapping side of it.
fn chain_of(raw: &RawBinding, token: &str) -> Result<FxChainId, String> {
    let token = token.trim();
    if let Some((head, stem_name)) = token.split_once('/') {
        let stem = stem_of(stem_name.trim())?;
        let deck = if head.trim().is_empty() {
            raw.deck
        } else {
            parse_deck_token(head.trim())
        };
        let deck = deck.ok_or_else(|| format!("chain `{token}` needs a deck (`deck0/{stem}`)"))?;
        return Ok(FxChainId::Stem { deck, stem });
    }
    if let Some(deck) = parse_deck_token(token) {
        return Ok(FxChainId::Deck(deck));
    }
    if token.eq_ignore_ascii_case("master") || token.eq_ignore_ascii_case("m") {
        return Ok(FxChainId::Master);
    }
    // A bare stem name: the deck comes from the binding, exactly like `fader flow` does.
    if let Some(stem) = Stem::parse(token) {
        let deck = raw
            .deck
            .ok_or_else(|| format!("chain `{token}` needs a `deck`"))?;
        return Ok(FxChainId::Stem { deck, stem });
    }
    Err(format!(
        "unknown chain `{token}` (master | deck0 | <stem> | deck0/<stem>)"
    ))
}

/// `deck0` / `d0` / `0` → that deck's id.
fn parse_deck_token(token: &str) -> Option<DeckId> {
    let lower = token.to_ascii_lowercase();
    let digits = lower
        .strip_prefix("deck")
        .or_else(|| lower.strip_prefix('d'))
        .unwrap_or(&lower);
    digits.parse::<u8>().ok()
}

/// Parses a chain address without a binding: `master`/`m` or a deck. Kept for callers that have no
/// binding to take a stem's deck from.
pub fn parse_chain(token: &str) -> Option<FxChainId> {
    if token.eq_ignore_ascii_case("master") || token.eq_ignore_ascii_case("m") {
        return Some(FxChainId::Master);
    }
    parse_deck_token(token).map(FxChainId::Deck)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[meta]
name = "unit"

[[bind]]
type = "cc"
mode = "abs"
channel = 0
id = 7
deck = 0
action = "fader.flow"

[[bind]]
type = "note"
channel = 1
id = 48
deck = 0
action = "nudge"
step = 0.05

[[bind]]
type = "cc"
id = 74
deck = 0
action = "loop 8"

[[bind]]
type = "cc"
id = 20
action = "fx.param"
chain = "deck1"
fx = "filter"
param = "value"
"#;

    #[test]
    fn a_valid_file_compiles_every_v1_shape() {
        let map = Map::from_toml_str(SAMPLE).unwrap();
        assert_eq!(map.meta.as_ref().unwrap().name.as_deref(), Some("unit"));
        assert_eq!(map.binds.len(), 4);
        assert_eq!(map.binds[0].action, Action::Fader(FaderTarget::Flow(0)));
        assert_eq!(
            map.binds[1].action,
            Action::Nudge {
                delta: 0.05,
                seconds: None
            }
        );
        assert_eq!(map.binds[1].kind, EventKind::Note);
        assert_eq!(map.binds[2].action, Action::Loop(LoopAction::Beats(8)));
        assert_eq!(
            map.binds[3].action,
            Action::Fx(FxAction::Param {
                chain: FxChainId::Deck(1),
                fx: "filter".to_owned(),
                param: "value".to_owned(),
            })
        );
        // Faders default to a bipolar domain; the FX param to the unit domain.
        assert_eq!((map.binds[0].min, map.binds[0].max), (-1.0, 1.0));
        assert_eq!((map.binds[3].min, map.binds[3].max), (0.0, 1.0));
    }

    /// Every stem has its own FX chain, and a binding can address it three ways: a bare stem in the
    /// `chain` field, a spelled-out `deckN/<stem>`, or a leading target word on the action line.
    #[test]
    fn fx_chains_can_address_one_stems_chain() {
        let bind = |action: &str, extra: &str| -> Action {
            let text = format!("[[bind]]\ntype = \"note\"\nid = 1\n{extra}action = \"{action}\"\n");
            Map::from_toml_str(&text).unwrap().binds[0].action.clone()
        };
        let bind_err = |action: &str, extra: &str| -> String {
            let text = format!("[[bind]]\ntype = \"note\"\nid = 1\n{extra}action = \"{action}\"\n");
            Map::from_toml_str(&text).unwrap_err().message().to_owned()
        };
        let on = |deck, stem| {
            Action::Fx(FxAction::Toggle {
                chain: FxChainId::Stem { deck, stem },
                fx: "filter".to_owned(),
            })
        };

        // A bare stem name in `chain`, with the deck from the binding.
        assert_eq!(
            bind(
                "fx toggle",
                "deck = 1\nchain = \"vocals\"\nfx = \"filter\"\n"
            ),
            on(1, Stem::Vocals)
        );
        // Spelled out, so a binding can address another deck's stem too.
        assert_eq!(
            bind("fx toggle", "chain = \"d0/bass\"\nfx = \"filter\"\n"),
            on(0, Stem::Bass)
        );
        assert_eq!(
            bind("fx toggle", "chain = \"deck1/other\"\nfx = \"filter\"\n"),
            on(1, Stem::Other)
        );
        // The action line names it — the session grammar's own form.
        assert_eq!(
            bind("vocals fx toggle", "deck = 0\nfx = \"filter\"\n"),
            on(0, Stem::Vocals)
        );
        // ...including the deck: `master` / `d1` as a leading word.
        assert_eq!(
            bind("master fx toggle", "fx = \"filter\"\n"),
            Action::Fx(FxAction::Toggle {
                chain: FxChainId::Master,
                fx: "filter".to_owned()
            })
        );
        assert_eq!(
            bind("d1 fx toggle", "fx = \"filter\"\n"),
            Action::Fx(FxAction::Toggle {
                chain: FxChainId::Deck(1),
                fx: "filter".to_owned()
            })
        );
        // A parameter is addressed the same way, and the parameter name still comes from the field.
        assert_eq!(
            bind(
                "vocals fx param",
                "deck = 0\nfx = \"filter\"\nparam = \"value\"\n"
            ),
            Action::Fx(FxAction::Param {
                chain: FxChainId::Stem {
                    deck: 0,
                    stem: Stem::Vocals
                },
                fx: "filter".to_owned(),
                param: "value".to_owned(),
            })
        );

        // A stem chain with no deck to hang it off is refused, and names what was wrong.
        let err = bind_err("fx toggle", "chain = \"vocals\"\nfx = \"filter\"\n");
        assert!(err.contains("deck"), "{err}");
        let err = bind_err("vocals fx toggle", "fx = \"filter\"\n");
        assert!(err.contains("deck"), "{err}");
        let err = bind_err("fx toggle", "chain = \"guitar\"\nfx = \"filter\"\n");
        assert!(
            err.contains("unknown chain") && err.contains("guitar"),
            "{err}"
        );
        let err = bind_err("fx toggle", "chain = \"vocals\"\nfx = \"filter\"\n");
        assert!(!err.is_empty());
    }

    /// The channel trim is reachable by name, like every other level.
    #[test]
    fn the_channel_trim_is_addressable_by_name() {
        let map = Map::from_toml_str(
            "[[bind]]\ntype = \"cc\"\nmode = \"abs\"\nid = 7\ndeck = 1\naction = \"fader.gain\"\n",
        )
        .unwrap();
        assert_eq!(map.binds[0].action, Action::Fader(FaderTarget::Gain(1)));
    }

    /// The action is a session command line, so everything the CLI can say about these verbs, a map
    /// can say too — including arguments.
    #[test]
    fn action_lines_take_the_arguments_a_session_line_takes() {
        let bind = |action: &str, extra: &str| -> Action {
            let text = format!(
                "[[bind]]\ntype = \"note\"\nid = 1\ndeck = 1\n{extra}action = \"{action}\"\n"
            );
            Map::from_toml_str(&text).unwrap().binds[0].action.clone()
        };
        assert_eq!(bind("beatjump -8", ""), Action::BeatJump { beats: -8 });
        assert_eq!(
            bind("beatjump", "step = 4\n"),
            Action::BeatJump { beats: 4 }
        );
        // The legacy minus name still means "backwards", with the magnitude from `step`.
        assert_eq!(
            bind("beatjump-", "step = 3\n"),
            Action::BeatJump { beats: -3 }
        );
        assert_eq!(bind("loop 8", ""), Action::Loop(LoopAction::Beats(8)));
        assert_eq!(
            bind("loop", ""),
            Action::Loop(LoopAction::Beats(4)),
            "the button default"
        );
        assert_eq!(
            bind("loop 8", "beats = 2\n"),
            Action::Loop(LoopAction::Beats(8)),
            "the line wins"
        );
        assert_eq!(
            bind("sync phase linear 0.5", ""),
            Action::Sync(SyncAction::Phase {
                mode: PhaseMode::Linear,
                t_seconds: Some(0.5)
            })
        );
        assert_eq!(
            bind("sync phase", ""),
            Action::Sync(SyncAction::Phase {
                mode: PhaseMode::Pid,
                t_seconds: None
            })
        );
        assert_eq!(
            bind("sync set-leader", ""),
            Action::Sync(SyncAction::Leader)
        );
        assert_eq!(
            bind("vinyl 441", ""),
            Action::Vinyl {
                frames_per_tick: 441.0
            }
        );
        assert_eq!(
            bind("nudge 0.05 0.5", ""),
            Action::Nudge {
                delta: 0.05,
                seconds: Some(0.5)
            }
        );
        assert_eq!(
            bind("nudge", "step = 0.2\n"),
            Action::Nudge {
                delta: 0.2,
                seconds: None
            }
        );
        assert_eq!(
            bind("vocals mute", ""),
            Action::StemMute { stem: Stem::Vocals }
        );
        assert_eq!(
            bind("drums solo", ""),
            Action::StemSolo { stem: Stem::Drums }
        );
        assert_eq!(
            bind("bass fader", ""),
            Action::Fader(FaderTarget::Stem {
                deck: 1,
                stem: Stem::Bass
            })
        );
        assert_eq!(bind("fader gain", ""), Action::Fader(FaderTarget::Gain(1)));

        // A malformed argument is refused, and the message says which word was wrong.
        let bad = "[[bind]]\ntype = \"note\"\nid = 1\ndeck = 0\naction = \"beatjump soon\"\n";
        let err = Map::from_toml_str(bad).unwrap_err();
        assert!(err.message().contains("soon"), "{}", err.message());
        // A linear phase sync without a duration cannot converge, so it is refused rather than
        // silently turning into a PI controller.
        let bad = "[[bind]]\ntype = \"note\"\nid = 1\ndeck = 0\naction = \"sync phase linear\"\n";
        assert!(Map::from_toml_str(bad)
            .unwrap_err()
            .message()
            .contains("duration"));
    }

    #[test]
    fn unknown_keys_and_actions_are_errors() {
        let bad_key = r#"
[[bind]]
type = "cc"
id = 7
action = "play"
deck = 0
dcek = 1
"#;
        assert!(Map::from_toml_str(bad_key).is_err());

        let bad_action = r#"
[[bind]]
type = "cc"
id = 7
action = "fader.volume"
"#;
        let err = Map::from_toml_str(bad_action).unwrap_err();
        assert!(
            err.message().contains("unknown action"),
            "{}",
            err.message()
        );
    }

    /// `reverse` flips a relative encoder and is refused where it would mean nothing.
    #[test]
    fn reverse_is_a_relative_only_switch() {
        let map = Map::from_toml_str(
            r#"
[[bind]]
type = "cc"
mode = "rel2"
reverse = true
id = 6
deck = 0
action = "vinyl"

[[bind]]
type = "cc"
mode = "rel1"
id = 7
deck = 0
action = "vinyl"
"#,
        )
        .unwrap();
        assert!(map.binds[0].reverse, "the flag reaches the binding");
        assert!(!map.binds[1].reverse, "and defaults to off");

        // On an absolute control it can only be a mistake, so it is an error rather than a
        // silently ignored key.
        let err = Map::from_toml_str(
            r#"
[[bind]]
type = "cc"
mode = "abs"
reverse = true
id = 6
deck = 0
action = "fader.flow"
"#,
        )
        .unwrap_err();
        assert!(err.message().contains("reverse"), "{}", err.message());
    }

    /// The map the CLI loads by default must parse.
    ///
    /// Nothing else in the test suite reads it, and it is the file a user's first session uses — so
    /// it is exactly the kind of thing that rots unnoticed.
    #[test]
    fn the_repositorys_default_map_parses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../midi-map.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            // A vendored build has no repository root to check; that is not a failure.
            return;
        };
        let map = Map::from_toml_str(&text).expect("the shipped default map must parse");
        assert!(!map.binds.is_empty());
        // It should exercise the newer families, so changing one of them is noticed here.
        assert!(map
            .binds
            .iter()
            .any(|bind| matches!(bind.action, Action::StemMute { .. })));
        assert!(map
            .binds
            .iter()
            .any(|bind| matches!(bind.action, Action::Fader(FaderTarget::Stem { .. }))));
    }

    /// The stem actions are spelled `<family>.<stem>.<verb>`, and a wrong stem names the valid ones
    /// rather than failing as "unknown action".
    #[test]
    fn stem_actions_parse_and_name_their_stem() {
        let map = |action: &str, deck: &str| -> Result<Action, String> {
            let text = format!("[[bind]]\ntype = \"note\"\nid = 1\n{deck}action = \"{action}\"\n");
            Map::from_toml_str(&text)
                .map(|parsed| parsed.binds[0].action.clone())
                // Keep the parser's `bind \`…\`: ` prefix: assertions use `contains`.
                .map_err(|err| err.message().to_owned())
        };
        assert_eq!(
            map("fader.stem.bass", "deck = 1\n").unwrap(),
            Action::Fader(FaderTarget::Stem {
                deck: 1,
                stem: Stem::Bass
            })
        );
        assert_eq!(
            map("stem.vocals.mute", "deck = 0\n").unwrap(),
            Action::StemMute { stem: Stem::Vocals }
        );
        assert_eq!(
            map("stem.drums.solo", "deck = 0\n").unwrap(),
            Action::StemSolo { stem: Stem::Drums }
        );
        // A stem action is deck-scoped like any other.
        let err = map("stem.vocals.mute", "").unwrap_err();
        assert!(err.contains("needs a `deck`"), "{err}");
        // A bad stem lists what it could have been.
        let err = map("stem.guitar.mute", "deck = 0\n").unwrap_err();
        assert!(
            err.contains("vocals") && err.contains("unknown stem"),
            "{err}"
        );
        // A bad verb is not silently accepted, and the message points at the list.
        let err = map("stem.vocals.solo_only", "deck = 0\n").unwrap_err();
        assert!(
            err.contains("unknown action") && err.contains("vocals solo_only"),
            "{err}"
        );
        // Every stem name the engine has is addressable, on both families.
        for stem in Stem::ALL {
            assert!(
                map(&format!("fader.stem.{stem}"), "deck = 0\n").is_ok(),
                "{stem}"
            );
            assert!(
                map(&format!("stem.{stem}.mute"), "deck = 0\n").is_ok(),
                "{stem}"
            );
        }
    }

    #[test]
    fn deck_scoped_actions_require_a_deck() {
        let no_deck = r#"
[[bind]]
type = "cc"
id = 7
action = "play"
"#;
        let err = Map::from_toml_str(no_deck).unwrap_err();
        assert!(
            err.message().contains("needs a `deck`"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn fx_param_requires_chain_fx_and_param() {
        let missing = r#"
[[bind]]
type = "cc"
id = 20
action = "fx.param"
chain = "master"
"#;
        let err = Map::from_toml_str(missing).unwrap_err();
        assert!(err.message().contains("`fx` name"), "{}", err.message());
    }

    #[test]
    fn slot_level_fx_actions_need_a_slot_name() {
        let missing = r#"
[[bind]]
type = "note"
id = 20
action = "fx.toggle"
chain = "master"
"#;
        let err = Map::from_toml_str(missing).unwrap_err();
        assert!(err.message().contains("`fx` name"), "{}", err.message());
    }

    #[test]
    fn channel_out_of_range_is_rejected() {
        let bad = r#"
[[bind]]
type = "cc"
channel = 20
id = 7
deck = 0
action = "play"
"#;
        assert!(Map::from_toml_str(bad).is_err());
    }

    #[test]
    fn resolved_fx_names_map_to_indices() {
        let mut map = Map::from_toml_str(SAMPLE).unwrap();
        assert_eq!(map.fx_slot(FxChainId::Deck(1), "filter"), None);
        map.resolve_fx(FxChainId::Deck(1), "filter", 3);
        assert_eq!(map.fx_slot(FxChainId::Deck(1), "filter"), Some(3));
    }

    /// The guide offers one cue row; `cue.smart` stays a name a map can use for the one-shot
    /// behaviour (placing the cue point without ever playing it).
    #[test]
    fn the_guide_lists_one_cue_row_and_the_one_shot_stays_reachable_by_name() {
        let names: Vec<&str> = ACTIONS.iter().map(|spec| spec.name).collect();
        assert!(names.contains(&"cue"));
        assert!(!names.contains(&"cue.smart"), "one cue row, not two");
        let map = Map::from_toml_str(
            "[[bind]]\ntype = \"note\"\nid = 1\ndeck = 0\naction = \"cue.smart\"\n",
        )
        .expect("an existing map still loads");
        assert_eq!(map.binds[0].action, Action::CueSmart);
        // And the momentary one is the pair of edges.
        assert!(Action::Cue.is_momentary());
        assert!(!Action::CueSmart.is_momentary());
    }

    #[test]
    fn the_manifest_and_registry_agree_on_every_action_name() {
        let decks = 2;
        let targets = manifest(decks);
        for spec in ACTIONS {
            let count = targets.iter().filter(|t| t.action == spec.name).count();
            let expected = if spec.deck { decks } else { 1 };
            assert_eq!(
                count, expected,
                "action `{}` listed {count} times",
                spec.name
            );
        }
        // Every registry entry is accepted by the parser (given the fields it needs) and every
        // listed name is either accepted or requires parameters.
        for spec in ACTIONS {
            let needs_beats = spec.name == "loop";
            let needs_fx = spec.name.starts_with("fx ");
            let needs_deck = spec.deck;
            let mut toml = String::from("[[bind]]\ntype = \"cc\"\nid = 1\n");
            if needs_deck {
                toml.push_str("deck = 0\n");
            }
            let action = if spec.name == "hotcue" {
                "hotcue trigger 1"
            } else {
                spec.name
            };
            toml.push_str(&format!("action = \"{action}\"\n"));
            if needs_beats {
                toml.push_str("beats = 4\n");
            }
            if needs_fx {
                toml.push_str("chain = \"master\"\n");
                toml.push_str("fx = \"gain\"\n");
                if spec.name == "fx param" {
                    toml.push_str("param = \"value\"\n");
                }
            }
            let parsed = Map::from_toml_str(&toml);
            assert!(
                parsed.is_ok(),
                "registry action `{}` rejected: {:?}",
                spec.name,
                parsed.err()
            );
        }
    }

    #[test]
    fn chain_addresses_accept_the_documented_spellings() {
        assert_eq!(parse_chain("master"), Some(FxChainId::Master));
        assert_eq!(parse_chain("m"), Some(FxChainId::Master));
        assert_eq!(parse_chain("deck1"), Some(FxChainId::Deck(1)));
        assert_eq!(parse_chain("d1"), Some(FxChainId::Deck(1)));
        assert_eq!(parse_chain("1"), Some(FxChainId::Deck(1)));
        assert_eq!(parse_chain("nope"), None);
    }
}
