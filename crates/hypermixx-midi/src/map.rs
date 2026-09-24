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

use hypermixx_core::{DeckId, FaderTarget, FxChainId};
use serde::{Deserialize, Serialize};

/// A parse or validation failure. The message carries the TOML position when the error came from
/// the parser itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapError {
    message: String,
}

impl MapError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
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
    /// 1..63 = +, 65..127 = − (64 is "no movement").
    Rel1,
    /// 65..127 = +, 1..63 = −.
    Rel2,
    /// Two's-complement around 64.
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
    Pause,
    BeatJump { beats: i64 },
    Fader(FaderTarget),
    /// Tempo fader: `range` is the fraction of the rate at full deflection (0.08 = ±8%).
    Rate { range: f32 },
    /// Momentary bend, held while the control is down.
    Nudge { delta: f32 },
    Loop(LoopAction),
    Sync(SyncAction),
    Fx(FxAction),
}

impl Action {
    /// Continuous actions read a value from every event; the rest fire on an edge.
    pub fn is_continuous(&self) -> bool {
        matches!(
            self,
            Action::Fader(_) | Action::Rate { .. } | Action::Fx(FxAction::Param { .. })
        )
    }

    /// Momentary actions also act on the release edge.
    pub fn is_momentary(&self) -> bool {
        matches!(
            self,
            Action::Nudge { .. } | Action::Fx(FxAction::Pad { .. })
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncAction {
    Tempo,
    Phase,
    PhaseLock,
    TempoLock,
    Leader,
    Unlock,
}

/// FX actions address a chain and a slot by name. Slot indices are resolved by the CLI after it
/// has asked the engine for each chain, never written into the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FxAction {
    On { chain: FxChainId, fx: String },
    Off { chain: FxChainId, fx: String },
    Toggle { chain: FxChainId, fx: String },
    Pad { chain: FxChainId, fx: String },
    Trigger { chain: FxChainId, fx: String },
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

/// One entry in the action registry. The registry is the single source of truth: the TOML
/// validator, the guide's target list and the translator all read it, so they cannot drift.
pub struct ActionSpec {
    pub name: &'static str,
    /// Whether the action must name a `deck`.
    pub deck: bool,
    /// Whether a note binding is held (both edges) rather than edge-triggered.
    pub momentary: bool,
}

/// Every action the mapping layer knows, in the order a guide should list them.
pub const ACTIONS: &[ActionSpec] = &[
    ActionSpec { name: "play", deck: true, momentary: false },
    ActionSpec { name: "pause", deck: true, momentary: false },
    ActionSpec { name: "beatjump+", deck: true, momentary: false },
    ActionSpec { name: "beatjump-", deck: true, momentary: false },
    ActionSpec { name: "fader.flow", deck: true, momentary: false },
    ActionSpec { name: "fader.deck", deck: true, momentary: false },
    ActionSpec { name: "fader.cuesend", deck: true, momentary: false },
    ActionSpec { name: "fader.cross", deck: false, momentary: false },
    ActionSpec { name: "fader.master", deck: false, momentary: false },
    ActionSpec { name: "fader.cue", deck: false, momentary: false },
    ActionSpec { name: "rate", deck: true, momentary: false },
    ActionSpec { name: "nudge", deck: true, momentary: true },
    ActionSpec { name: "loop.in", deck: true, momentary: false },
    ActionSpec { name: "loop.out", deck: true, momentary: false },
    ActionSpec { name: "loop.exit", deck: true, momentary: false },
    ActionSpec { name: "loop.cancel", deck: true, momentary: false },
    ActionSpec { name: "loop.halve", deck: true, momentary: false },
    ActionSpec { name: "loop.double", deck: true, momentary: false },
    ActionSpec { name: "loop.beat", deck: true, momentary: false },
    ActionSpec { name: "sync.tempo", deck: true, momentary: false },
    ActionSpec { name: "sync.phase", deck: true, momentary: false },
    ActionSpec { name: "sync.phaselock", deck: true, momentary: false },
    ActionSpec { name: "sync.tempolock", deck: true, momentary: false },
    ActionSpec { name: "sync.leader", deck: true, momentary: false },
    ActionSpec { name: "sync.unlock", deck: true, momentary: false },
    ActionSpec { name: "fx.on", deck: false, momentary: false },
    ActionSpec { name: "fx.off", deck: false, momentary: false },
    ActionSpec { name: "fx.toggle", deck: false, momentary: false },
    ActionSpec { name: "fx.pad", deck: false, momentary: true },
    ActionSpec { name: "fx.trigger", deck: false, momentary: false },
    ActionSpec { name: "fx.param", deck: false, momentary: false },
];

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
        Self { meta: None, binds: Vec::new(), fx_slots: HashMap::new() }
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

/// Resolves an action name plus its optional parameters.
fn parse_action(raw: &RawBinding) -> Result<Action, String> {
    let name = raw.action.as_str();
    let deck = |spec: &str| -> Result<DeckId, String> {
        raw.deck
            .ok_or_else(|| format!("action `{spec}` needs a `deck`"))
    };
    match name {
        "play" => {
            deck("play")?;
            Ok(Action::Play)
        }
        "pause" => {
            deck("pause")?;
            Ok(Action::Pause)
        }
        "beatjump+" | "beatjump-" => {
            deck(name)?;
            let magnitude = raw.step.unwrap_or(1.0);
            if !magnitude.is_finite() || magnitude < 1.0 {
                return Err("`step` must be at least 1 beat".to_owned());
            }
            let beats = magnitude.round() as i64;
            Ok(Action::BeatJump {
                beats: if name.ends_with('-') { -beats } else { beats },
            })
        }
        "fader.flow" => Ok(Action::Fader(FaderTarget::Flow(deck(name)?))),
        "fader.deck" => Ok(Action::Fader(FaderTarget::Deck(deck(name)?))),
        "fader.cuesend" => Ok(Action::Fader(FaderTarget::CueSend(deck(name)?))),
        "fader.cross" => Ok(Action::Fader(FaderTarget::Crossfader)),
        "fader.master" => Ok(Action::Fader(FaderTarget::Master)),
        "fader.cue" => Ok(Action::Fader(FaderTarget::Cue)),
        "rate" => {
            deck(name)?;
            let range = raw.step.unwrap_or(0.08);
            if !range.is_finite() || range <= 0.0 {
                return Err("`step` (rate range) must be positive".to_owned());
            }
            Ok(Action::Rate { range })
        }
        "nudge" => {
            deck(name)?;
            let delta = raw.step.unwrap_or(0.04);
            if !delta.is_finite() || delta == 0.0 {
                return Err("`step` (nudge delta) must be non-zero".to_owned());
            }
            Ok(Action::Nudge { delta })
        }
        "loop.in" => {
            deck(name)?;
            Ok(Action::Loop(LoopAction::In))
        }
        "loop.out" => {
            deck(name)?;
            Ok(Action::Loop(LoopAction::Out))
        }
        "loop.exit" => {
            deck(name)?;
            Ok(Action::Loop(LoopAction::Exit))
        }
        "loop.cancel" => {
            deck(name)?;
            Ok(Action::Loop(LoopAction::Cancel))
        }
        "loop.halve" => {
            deck(name)?;
            Ok(Action::Loop(LoopAction::Halve))
        }
        "loop.double" => {
            deck(name)?;
            Ok(Action::Loop(LoopAction::Double))
        }
        "loop.beat" => {
            deck(name)?;
            let beats = raw
                .beats
                .filter(|beats| *beats > 0)
                .ok_or_else(|| "`loop.beat` needs a positive `beats`".to_owned())?;
            Ok(Action::Loop(LoopAction::Beats(beats)))
        }
        _ if name.starts_with("loop.beat") => {
            deck(name)?;
            let beats: u64 = name["loop.beat".len()..]
                .parse()
                .map_err(|_| format!("unknown action `{name}`"))?;
            if beats == 0 {
                return Err("beat-loop length must be positive".to_owned());
            }
            Ok(Action::Loop(LoopAction::Beats(beats)))
        }
        "sync.tempo" => {
            deck(name)?;
            Ok(Action::Sync(SyncAction::Tempo))
        }
        "sync.phase" => {
            deck(name)?;
            Ok(Action::Sync(SyncAction::Phase))
        }
        "sync.phaselock" => {
            deck(name)?;
            Ok(Action::Sync(SyncAction::PhaseLock))
        }
        "sync.tempolock" => {
            deck(name)?;
            Ok(Action::Sync(SyncAction::TempoLock))
        }
        "sync.leader" => {
            deck(name)?;
            Ok(Action::Sync(SyncAction::Leader))
        }
        "sync.unlock" => {
            deck(name)?;
            Ok(Action::Sync(SyncAction::Unlock))
        }
        "fx.on" => Ok(Action::Fx(fx_slot_action(raw, FxActionKind::On)?)),
        "fx.off" => Ok(Action::Fx(fx_slot_action(raw, FxActionKind::Off)?)),
        "fx.toggle" => Ok(Action::Fx(fx_slot_action(raw, FxActionKind::Toggle)?)),
        "fx.pad" => Ok(Action::Fx(fx_slot_action(raw, FxActionKind::Pad)?)),
        "fx.trigger" => Ok(Action::Fx(fx_slot_action(raw, FxActionKind::Trigger)?)),
        "fx.param" => {
            let (chain, fx) = fx_address(raw)?;
            let param = raw.param.clone().ok_or("`fx.param` needs a `param` name")?;
            Ok(Action::Fx(FxAction::Param { chain, fx, param }))
        }
        other => Err(format!("unknown action `{other}`")),
    }
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
fn fx_slot_action(raw: &RawBinding, kind: FxActionKind) -> Result<FxAction, String> {
    let (chain, fx) = fx_address(raw)?;
    Ok(match kind {
        FxActionKind::On => FxAction::On { chain, fx },
        FxActionKind::Off => FxAction::Off { chain, fx },
        FxActionKind::Toggle => FxAction::Toggle { chain, fx },
        FxActionKind::Pad => FxAction::Pad { chain, fx },
        FxActionKind::Trigger => FxAction::Trigger { chain, fx },
    })
}

/// The `chain` + `fx` address every FX action carries.
fn fx_address(raw: &RawBinding) -> Result<(FxChainId, String), String> {
    let chain = raw
        .chain
        .as_deref()
        .ok_or_else(|| "an FX action needs a `chain`".to_owned())
        .and_then(|token| parse_chain(token).ok_or_else(|| format!("unknown chain `{token}`")))?;
    let fx = raw.fx.clone().ok_or("an FX action needs an `fx` name")?;
    Ok((chain, fx))
}

/// Parses a chain address: `master`/`m`, `deck0`, `d0`, or a bare index.
pub fn parse_chain(token: &str) -> Option<FxChainId> {
    let lower = token.to_ascii_lowercase();
    if lower == "master" || lower == "m" {
        return Some(FxChainId::Master);
    }
    let digits = lower
        .strip_prefix("deck")
        .or_else(|| lower.strip_prefix('d'))
        .unwrap_or(&lower);
    digits.parse::<u8>().ok().map(FxChainId::Deck)
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
action = "loop.beat8"

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
        assert_eq!(
            map.binds[0].action,
            Action::Fader(FaderTarget::Flow(0))
        );
        assert_eq!(map.binds[1].action, Action::Nudge { delta: 0.05 });
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
        assert!(err.message().contains("unknown action"), "{}", err.message());
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
        assert!(err.message().contains("needs a `deck`"), "{}", err.message());
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

    #[test]
    fn the_manifest_and_registry_agree_on_every_action_name() {
        let decks = 2;
        let targets = manifest(decks);
        for spec in ACTIONS {
            let count = targets.iter().filter(|t| t.action == spec.name).count();
            let expected = if spec.deck { decks } else { 1 };
            assert_eq!(count, expected, "action `{}` listed {count} times", spec.name);
        }
        // Every registry entry is accepted by the parser (given the fields it needs) and every
        // listed name is either accepted or requires parameters.
        for spec in ACTIONS {
            let needs_beats = spec.name == "loop.beat";
            let needs_fx = matches!(spec.name, "fx.on" | "fx.off" | "fx.toggle" | "fx.pad" | "fx.trigger" | "fx.param");
            let needs_deck = spec.deck;
            let mut toml = String::from("[[bind]]\ntype = \"cc\"\nid = 1\n");
            if needs_deck {
                toml.push_str("deck = 0\n");
            }
            toml.push_str(&format!("action = \"{}\"\n", spec.name));
            if needs_beats {
                toml.push_str("beats = 4\n");
            }
            if needs_fx {
                toml.push_str("chain = \"master\"\n");
                toml.push_str("fx = \"gain\"\n");
                if spec.name == "fx.param" {
                    toml.push_str("param = \"value\"\n");
                }
            }
            let parsed = Map::from_toml_str(&toml);
            assert!(parsed.is_ok(), "registry action `{}` rejected: {:?}", spec.name, parsed.err());
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
