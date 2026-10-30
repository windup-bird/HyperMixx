//! The mapping file: a TOML schema built around **units**, plus its validation.
//!
//! ```toml
//! [[unit]]
//! id = "deck1.mixer"
//! preset = "mixer"
//! deck = 1
//!
//! [[unit.control]]
//! slot = "filter"
//! type = "BipolKnob"
//! endpoint = { midi = "cc", channel = 0, id = 12 }
//!
//! [[unit.mode]]
//! name = "default"
//! [[unit.mode.bind]]
//! slot = "filter"
//! action = "fx param value"
//! fx = "filter"
//! ```
//!
//! Three layers, each with one job:
//!
//! * **`type`** is the slot's physical kind — `Button`, `Knob`, `BipolKnob`, `RelKnob`,
//!   `JogWheel`. It decides which values the slot can produce and which actions it can carry.
//! * **`endpoint`** is the hardware fact — a MIDI message, a channel, a controller number, and how
//!   its value is encoded.
//! * **`role`** says what the slot *does*: emit an action, or switch the unit's mode.
//!
//! Parsing is `deny_unknown_fields`, so a typo is a TOML error rather than a silently ignored
//! binding. Endpoints must be unique across the whole file: one physical control belongs to one
//! slot. FX are named, never indexed, because indices shift when an earlier slot is removed.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use hypermixx_core::{
    BeatLen, BrowserAction, DeckId, FaderTarget, FxChainId, HotCueOp, KeylockMode, PhaseMode, Stem,
};
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The MIDI input this map belongs to. The guide writes it; `--midi` still overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// One line of prose, shown by the guide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// User preset files, relative to the map file's folder. A user preset with the same name as a
    /// built-in replaces it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<String>,
}

/// Deck numbers are written 1-based in TOML and stored 0-based. Keeping the conversion at the file
/// boundary avoids an off-by-one in every audio/mixer index.
mod deck_number {
    use super::DeckId;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(deck: &Option<DeckId>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match deck {
            Some(id) => serializer.serialize_some(&(id + 1)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<DeckId>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Option::<u8>::deserialize(deserializer)? {
            Some(0) => Err(serde::de::Error::custom(
                "deck numbers start at 1 (deck1, deck2, ...)",
            )),
            Some(number) => Ok(Some(number - 1)),
            None => Ok(None),
        }
    }
}

/// Which MIDI message an endpoint listens for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Cc,
    Note,
    Bend,
}

/// How one endpoint's data bytes become movement.
///
/// Absolute controls carry a position; the rest carry a step and differ only in how a vendor packs
/// its sign. Naming them by *meaning* rather than by `rel1`/`rel2` keeps a map readable: what matters
/// is "this wheel reports two's complement", not which of three vendor conventions the number was.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    /// A position, `0..=127` (or `0..=16383` with `lsb`).
    #[default]
    Absolute,
    /// Offset binary around 64: `value - 64`, so 64 is "no movement", 65..127 = +, 1..63 = −.
    Offset64,
    /// Two's complement around 64: 1..63 = +, 65..127 = − (127 = −1).
    TwosComplement,
    /// Direction only: 1..63 = +, 65..127 = −. The magnitude is ignored.
    Direction,
    /// A two-point encoder: 1 = one step up, 127 = one step down, everything else is neutral.
    /// This is the `{0, 1, 127}` shape most small controllers send.
    TwoPoint,
}

impl Encoding {
    /// Whether this encoding carries a step rather than a position.
    pub fn is_relative(self) -> bool {
        !matches!(self, Encoding::Absolute)
    }
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

/// Which edge of a one-shot control fires the action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    #[default]
    Press,
    Release,
}

/// What kind of physical control a slot is. This is the reusable half of a slot: a preset names a
/// slot's kind, and a unit only supplies the endpoint and the deck.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum SlotKind {
    /// A note or absolute-CC button. One-shot actions fire on an edge.
    #[serde(rename = "Button", alias = "button", alias = "pad")]
    Button,
    /// An absolute control read as a position `0..=1`.
    #[serde(rename = "Knob", alias = "knob")]
    Knob,
    /// An absolute control whose centre is zero: `0..=127` becomes `-1..=1`.
    #[serde(rename = "BipolKnob", alias = "bipolknob")]
    BipolKnob,
    /// A relative encoder. It drives a continuous action, or one action per direction.
    #[serde(rename = "RelKnob", alias = "relknob")]
    RelKnob,
    /// A platter: a press/touch note plus a relative wheel.
    #[serde(rename = "JogWheel", alias = "jogwheel", alias = "jog")]
    JogWheel,
}

/// What a slot does besides carrying an action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotRole {
    /// The normal case: the slot's mode bindings fire commands.
    #[default]
    Action,
    /// Pressing selects the mode named by `target`.
    ModeTrigger,
    /// Pressing advances to the next mode in the unit's order.
    ModeCycle,
}

/// The part of a compound slot an endpoint belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointRole {
    /// The slot's only (or main) endpoint.
    #[default]
    Main,
    /// A jog's press/touch sensor.
    Press,
    /// A jog's wheel.
    Turn,
}

/// One physical endpoint, in the shape the file writes it.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawEndpoint {
    /// Only needed for a jog, whose two endpoints are told apart by role.
    #[serde(default, skip_serializing_if = "is_default_role")]
    pub role: EndpointRole,
    /// The MIDI message kind.
    #[serde(rename = "midi")]
    pub midi: EventKind,
    #[serde(default, skip_serializing_if = "is_absolute")]
    pub encoding: Encoding,
    /// The LSB controller of a 14-bit CC pair; `id` is then the MSB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsb: Option<u8>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reverse: bool,
    pub channel: u8,
    /// The CC controller or note key. Pitch bend has no number, so it omits this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u8>,
}

fn is_default_role(role: &EndpointRole) -> bool {
    *role == EndpointRole::Main
}

fn is_absolute(encoding: &Encoding) -> bool {
    *encoding == Encoding::Absolute
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One value or a list, so a jog can write two endpoints and everything else stays a table.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    pub fn as_slice(&self) -> &[T] {
        match self {
            OneOrMany::One(value) => std::slice::from_ref(value),
            OneOrMany::Many(values) => values,
        }
    }
}

impl<T: Serialize> Serialize for OneOrMany<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            OneOrMany::One(value) => value.serialize(serializer),
            OneOrMany::Many(values) => values.serialize(serializer),
        }
    }
}

/// One `[[unit.control]]`: a slot plus the hardware it listens to.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawControl {
    pub slot: String,
    /// The slot's kind. A preset normally supplies it; a unit may repeat it, and it must agree.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<SlotKind>,
    #[serde(default, skip_serializing_if = "is_action_role")]
    pub role: SlotRole,
    /// The mode a `mode_trigger` slot selects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<OneOrMany<RawEndpoint>>,
}

fn is_action_role(role: &SlotRole) -> bool {
    *role == SlotRole::Action
}

/// One slot → action row inside a mode.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawModeBind {
    pub slot: String,
    /// The action for the slot's main endpoint (see [`EndpointRole`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// A jog's press action; overrides `action` for the press endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub press: Option<String>,
    /// A jog's turn action; overrides `action` for the turn endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    /// The reverse direction of a `RelKnob`: one action per negative detent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub down: Option<String>,
    /// Written 1-based in TOML; stored 0-based internally.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "deck_number"
    )]
    pub deck: Option<DeckId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curve: Option<Curve>,
    /// The FX chain address, when the action's line does not name one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    /// The FX slot name an `fx …` action addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx: Option<String>,
    /// `press` (default) or `release` for one-shot actions. Two-edge actions reject it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<Edge>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawMode {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, rename = "bind")]
    pub binds: Vec<RawModeBind>,
}

/// One `[[unit]]` table.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawUnit {
    pub id: String,
    /// A built-in or user preset that supplies slot kinds and default modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// The deck this unit targets by default, written 1-based in TOML. Omit for a global unit.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "deck_number"
    )]
    pub deck: Option<DeckId>,
    /// The mode the unit starts in. Defaults to the preset's `start`, then the first mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// An explicit mode order: the modes named here come first, in this order, and any other mode
    /// follows in declaration order. Cycle selectors walk the resulting list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_order: Option<Vec<String>>,
    #[serde(default, rename = "control")]
    pub controls: Vec<RawControl>,
    #[serde(default, rename = "mode")]
    pub modes: Vec<RawMode>,
}

/// The whole file: an optional `[meta]` and a list of `[[unit]]`.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MapFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
    #[serde(default, rename = "unit")]
    pub units: Vec<RawUnit>,
}

impl MapFile {
    pub fn from_toml_str(text: &str) -> Result<Self, MapError> {
        toml::from_str(text).map_err(|err| MapError::new(err.to_string()))
    }

    pub fn to_toml_string(&self) -> Result<String, MapError> {
        toml::to_string(self).map_err(|err| MapError::new(err.to_string()))
    }
}

/// A compiled, physical endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub role: EndpointRole,
    pub kind: EventKind,
    pub encoding: Encoding,
    pub reverse: bool,
    pub channel: u8,
    pub id: u8,
    /// The LSB of a 14-bit CC pair; `id` is the MSB. `None` for every other endpoint.
    pub lsb: Option<u8>,
}

impl Endpoint {
    /// The primary lookup key. Pitch bend has no controller number, so all bend endpoints on a
    /// channel share id 0; validation rejects a second one.
    pub fn key(&self) -> (EventKind, u8, u8) {
        (
            self.kind,
            self.channel,
            if self.kind == EventKind::Bend { 0 } else { self.id },
        )
    }

    /// Every key that routes to this endpoint. A 14-bit CC claims both its MSB and LSB.
    pub fn keys(&self) -> Vec<(EventKind, u8, u8)> {
        let mut keys = vec![self.key()];
        if let Some(lsb) = self.lsb {
            keys.push((EventKind::Cc, self.channel, lsb));
        }
        keys
    }
}

/// One compiled slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub slot: String,
    pub kind: SlotKind,
    pub role: SlotRole,
    /// The mode name for a `ModeTrigger` slot; `None` for every other kind.
    pub target: Option<String>,
    /// Indices into [`Unit::endpoints`].
    pub endpoints: Vec<usize>,
}

impl Control {
    fn endpoint_index(&self, endpoints: &[Endpoint], role: EndpointRole) -> Option<usize> {
        self.endpoints
            .iter()
            .copied()
            .find(|&index| endpoints[index].role == role)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectorBehavior {
    Trigger { mode: usize },
    Cycle,
}

/// A slot that switches the unit's mode. Compiled from a control's `role`, never written directly.
#[derive(Clone, Debug, PartialEq)]
pub struct Selector {
    pub endpoint: Endpoint,
    pub behavior: SelectorBehavior,
    pub slot: String,
}

/// The actions one slot carries, one per endpoint role.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActionSet {
    pub main: Option<Action>,
    pub press: Option<Action>,
    pub turn: Option<Action>,
    /// The reverse direction of a `RelKnob`; `None` means the delta is continuous.
    pub down: Option<Action>,
}

impl ActionSet {
    /// The action that handles one endpoint. A jog's explicit `press`/`turn` rows win; otherwise the
    /// shorthand `action` covers both halves.
    pub fn for_role(&self, role: EndpointRole) -> Option<&Action> {
        match role {
            EndpointRole::Main => self.main.as_ref(),
            EndpointRole::Press => self.press.as_ref().or(self.main.as_ref()),
            EndpointRole::Turn => self.turn.as_ref().or(self.main.as_ref()),
        }
    }

    fn iter(&self) -> impl Iterator<Item = &Action> {
        [&self.main, &self.press, &self.turn, &self.down]
            .into_iter()
            .flatten()
    }

    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    fn any_continuous(&self) -> bool {
        self.iter().any(|action| action.is_continuous())
    }

    fn any_bipolar(&self) -> bool {
        self.iter().any(|action| action.is_bipolar())
    }
}

/// A slot's actions in one mode, before the physical endpoint is attached.
#[derive(Clone, Debug, PartialEq)]
pub struct ModeBinding {
    pub deck: Option<DeckId>,
    pub actions: ActionSet,
    /// The slot kind decides the centre of an absolute `BipolKnob`.
    pub kind: SlotKind,
    pub min: f32,
    pub max: f32,
    pub curve: Curve,
    pub on: Edge,
}

impl ModeBinding {
    /// Combines the mode's action with one endpoint into an executable binding. `None` means this
    /// endpoint has no action in this mode, so the router treats it as unbound.
    pub fn spec(&self, endpoint: &Endpoint) -> Option<BindingSpec> {
        let action = self.actions.for_role(endpoint.role)?.clone();
        let down = match endpoint.role {
            EndpointRole::Main => self.actions.down.clone(),
            _ => None,
        };
        Some(BindingSpec {
            kind: endpoint.kind,
            encoding: endpoint.encoding,
            reverse: endpoint.reverse,
            channel: endpoint.channel,
            id: endpoint.id,
            lsb: endpoint.lsb,
            center: (self.kind == SlotKind::BipolKnob).then(|| match endpoint.kind {
                EventKind::Bend => 0.5,
                _ if endpoint.lsb.is_some() => 8192.0 / 16383.0,
                _ => 64.0 / 127.0,
            }),
            deck: self.deck,
            action,
            down,
            min: self.min,
            max: self.max,
            curve: self.curve,
            on: self.on,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    pub name: String,
    pub label: String,
    /// Parallel to [`Unit::controls`]: `None` means the slot is inactive in this mode.
    pub binds: Vec<Option<ModeBinding>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    pub id: String,
    pub deck: Option<DeckId>,
    pub start: usize,
    pub controls: Vec<Control>,
    pub endpoints: Vec<Endpoint>,
    pub selectors: Vec<Selector>,
    pub modes: Vec<Mode>,
}

impl Unit {
    pub fn mode(&self, index: usize) -> Option<&Mode> {
        self.modes.get(index)
    }

    pub fn mode_index(&self, name: &str) -> Option<usize> {
        self.modes.iter().position(|mode| mode.name == name)
    }

    /// The actions one slot carries in one mode.
    pub fn binding(&self, mode: usize, control: usize) -> Option<&ModeBinding> {
        self.modes.get(mode)?.binds.get(control)?.as_ref()
    }
}

/// A validated, executable binding: one physical endpoint plus one mode action.
#[derive(Clone, Debug, PartialEq)]
pub struct BindingSpec {
    pub kind: EventKind,
    pub encoding: Encoding,
    pub reverse: bool,
    pub channel: u8,
    pub id: u8,
    /// The LSB of a 14-bit CC pair; `id` is the MSB.
    pub lsb: Option<u8>,
    /// Normalised centre of a `BipolKnob`; the absolute value is remapped around it.
    pub center: Option<f32>,
    /// The deck the action addresses, when it needs one.
    pub deck: Option<DeckId>,
    pub action: Action,
    /// The reverse direction of a `RelKnob`; `None` means the delta is continuous.
    pub down: Option<Action>,
    /// Value at normalised position 0.
    pub min: f32,
    /// Value at normalised position 1.
    pub max: f32,
    pub curve: Curve,
    /// Which edge fires a one-shot action.
    pub on: Edge,
}

impl BindingSpec {
    /// Whether this binding listens to `(channel, id)`.
    pub fn matches(&self, channel: u8, id: u8) -> bool {
        self.channel == channel
            && (self.kind == EventKind::Bend || self.id == id || self.lsb == Some(id))
    }
}

/// A binding's parsed, parameterised action.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Play,
    /// Momentary cue button. The press edge is [`CueOp::Hold`](hypermixx_core::CueOp::Hold): while
    /// playing it is a back cue, while paused it cues the playhead and plays it. The release edge is
    /// `Back`.
    Cue,
    /// One-shot cue: the deck decides between "back to cue" and "set cue here", and never plays.
    /// Kept out of [`ACTIONS`], so the guide offers one cue row instead of two.
    CueSmart,
    BeatJump {
        beats: i64,
    },
    Fader(FaderTarget),
    /// Latch one channel's cue send between off and full.
    ToggleCueSend,
    /// Tempo fader: a bipolar position in `-1..=1`. The deck maps it to a rate under its own
    /// `temporange`, so the range is a deck setting, not a per-binding constant.
    TempoFader,
    /// Flip the deck's keylock profile.
    ToggleKeylock,
    /// Switch keylock to a named profile.
    Keylock(KeylockMode),
    /// Pitch shift in semitones (the engine currently remembers it without moving pitch).
    Key(i32),
    /// Rate bend. `seconds` set = timed (release edge ignored); `None` = momentary, held while
    /// the control is down.
    Nudge {
        delta: f32,
        seconds: Option<f64>,
    },
    /// A DJ wheel: the press edge is the platter touch and the turn edge is a scrub. What a turn
    /// *means* is the deck's decision, because only the deck knows whether the platter is held.
    Vinyl {
        /// Frames of audio per wheel tick, applied only while the platter is held.
        frames_per_tick: f32,
    },
    /// Arm the next platter touch as a slip scratch (press-only).
    Slip,
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

    /// The action a jog wheel may carry on one of its halves.
    fn is_jog_action(&self) -> bool {
        matches!(
            self,
            Action::Vinyl { .. } | Action::Slip | Action::Nudge { .. }
        )
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
    Beats(BeatLen),
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

/// The baseline nudge, as a rate: one wheel message bends by this much (`0.15` = 15 % fast).
pub const DEFAULT_NUDGE_STEP: f32 = 0.15;

/// How long a bend started by a relative-encoder tick lasts when the line omits a duration.
pub const DEFAULT_NUDGE_SECONDS: f64 = 0.06;

/// Frames of audio one wheel tick moves the playhead while the platter is held.
///
/// A record at 33⅓ RPM carries **1.8 s per revolution**, so `frames_per_tick = 79380 / (ticks your
/// wheel sends per revolution)`. 1323 is that formula for a 60-detent wheel.
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
pub fn manifest(decks: usize) -> Vec<Target> {
    let mut out = Vec::new();
    for spec in ACTIONS {
        if spec.deck {
            for deck in 0..decks {
                out.push(Target {
                    deck: Some(deck as DeckId),
                    action: spec.name,
                    label: format!("deck{}  {}", deck + 1, spec.name),
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
    pub units: Vec<Unit>,
    /// Non-fatal findings (an unresolved FX name is reported separately by the CLI).
    pub warnings: Vec<String>,
    /// Resolved `chain + fx name -> slot index`, installed by [`Map::resolve_fx`] once a front-end
    /// has asked the engine for its chains.
    fx_slots: HashMap<(FxChainId, String), usize>,
}

impl Map {
    /// Parses and validates a map file. Parser errors keep the TOML position; semantic errors name
    /// the offending unit/slot/mode.
    pub fn from_toml_str(text: &str) -> Result<Self, MapError> {
        Self::from_toml_str_at(text, Path::new("."))
    }

    /// Parses a file, resolving user presets relative to its own folder.
    pub fn from_toml_path(path: impl AsRef<Path>) -> Result<Self, MapError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|err| MapError::new(format!("{}: {err}", path.display())))?;
        let base = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        Self::from_toml_str_at(&text, base)
    }

    /// Parses text with a base folder for `[meta] presets`.
    pub fn from_toml_str_at(text: &str, base: &Path) -> Result<Self, MapError> {
        let file = MapFile::from_toml_str(text)?;
        let presets = crate::preset::PresetRegistry::load(file.meta.as_ref(), base)
            .map_err(MapError::new)?;
        let mut units = Vec::new();
        let mut ids = HashSet::new();
        for raw in &file.units {
            check_name("unit id", &raw.id).map_err(MapError::new)?;
            if !ids.insert(raw.id.clone()) {
                return Err(MapError::new(format!(
                    "unit `{}` is declared twice",
                    raw.id
                )));
            }
            let unit = compile_unit(raw, &presets)
                .map_err(|message| MapError::new(format!("unit `{}`: {message}", raw.id)))?;
            units.push(unit);
        }
        let map = Self {
            meta: file.meta,
            units,
            warnings: Vec::new(),
            fx_slots: HashMap::new(),
        };
        map.validate_endpoints().map_err(MapError::new)?;
        Ok(map)
    }

    /// An empty map (no units).
    pub fn empty() -> Self {
        Self {
            meta: None,
            units: Vec::new(),
            warnings: Vec::new(),
            fx_slots: HashMap::new(),
        }
    }

    pub fn unit(&self, id: &str) -> Option<&Unit> {
        self.units.iter().find(|unit| unit.id == id)
    }

    /// Records that `name` is slot `index` in `chain`. Called by the CLI after `ListFx`.
    pub fn resolve_fx(&mut self, chain: FxChainId, name: impl Into<String>, index: usize) {
        self.fx_slots.insert((chain, name.into()), index);
    }

    /// The slot index for a named effect, if it has been resolved.
    pub fn fx_slot(&self, chain: FxChainId, name: &str) -> Option<usize> {
        self.fx_slots.get(&(chain, name.to_owned())).copied()
    }

    /// Every endpoint in the file must be claimed once. A physical control cannot drive two slots.
    fn validate_endpoints(&self) -> Result<(), String> {
        let mut seen: HashMap<(EventKind, u8, u8), String> = HashMap::new();
        for unit in &self.units {
            for control in &unit.controls {
                let owner = format!("`{}/{}`", unit.id, control.slot);
                for &endpoint in &control.endpoints {
                    for key in unit.endpoints[endpoint].keys() {
                        seen.insert(key, owner.clone());
                    }
                }
            }
        }
        // A second pass so the error names both owners rather than only the survivor.
        let mut claimed: HashMap<(EventKind, u8, u8), String> = HashMap::new();
        for unit in &self.units {
            for control in &unit.controls {
                let owner = format!("`{}/{}`", unit.id, control.slot);
                for &endpoint in &control.endpoints {
                    for key in unit.endpoints[endpoint].keys() {
                        if let Some(previous) = claimed.insert(key, owner.clone()) {
                            return Err(format!(
                                "endpoint {:?} ch{} id{} is claimed by {previous} and {owner}",
                                key.0, key.1, key.2
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Names are spoken by `midi mode` and by selector targets; whitespace would make them
/// unaddressable, and an empty string is never useful.
fn check_name(what: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(format!("{what} `{value}` cannot contain whitespace"));
    }
    Ok(())
}

/// Merges a unit with its preset and compiles every layer.
fn compile_unit(raw: &RawUnit, presets: &crate::preset::PresetRegistry) -> Result<Unit, String> {
    let preset = match raw.preset.as_deref() {
        Some(name) => Some(
            presets
                .get(name)
                .ok_or_else(|| format!("unknown preset `{name}`"))?,
        ),
        None => None,
    };

    // Slot names: preset order first, then unit-only slots in declaration order.
    let mut slot_order: Vec<String> = Vec::new();
    let mut preset_slots: HashMap<String, &crate::preset::PresetSlot> = HashMap::new();
    if let Some(preset) = preset {
        for slot in &preset.slots {
            check_name("slot", &slot.slot)?;
            if preset_slots.insert(slot.slot.clone(), slot).is_some() {
                return Err(format!(
                    "preset `{}` declares slot `{}` twice",
                    preset.name, slot.slot
                ));
            }
            slot_order.push(slot.slot.clone());
        }
    }
    let mut unit_controls: HashMap<String, &RawControl> = HashMap::new();
    for control in &raw.controls {
        check_name("slot", &control.slot)?;
        if unit_controls.insert(control.slot.clone(), control).is_some() {
            return Err(format!("slot `{}` is declared twice", control.slot));
        }
        if !preset_slots.contains_key(&control.slot) {
            slot_order.push(control.slot.clone());
        }
    }

    // Modes: preset defaults, same-name unit modes replace, new names append.
    let mut modes: Vec<RawMode> = preset.map(|preset| preset.modes.clone()).unwrap_or_default();
    let mut seen_modes = HashSet::new();
    for mode in &modes {
        check_name("mode", &mode.name)?;
        if !seen_modes.insert(mode.name.clone()) {
            return Err(format!(
                "preset `{}` declares mode `{}` twice",
                raw.preset.as_deref().unwrap_or(""),
                mode.name
            ));
        }
    }
    for mode in &raw.modes {
        check_name("mode", &mode.name)?;
        if let Some(existing) = modes.iter_mut().find(|existing| existing.name == mode.name) {
            *existing = mode.clone();
        } else {
            seen_modes.insert(mode.name.clone());
            modes.push(mode.clone());
        }
    }
    if modes.is_empty() {
        return Err("has no modes".to_owned());
    }
    // An explicit unit order moves the named modes to the front; everything else keeps its place.
    if let Some(order) = raw.mode_order.clone() {
        let mut ordered = Vec::with_capacity(modes.len());
        for name in &order {
            let index = modes
                .iter()
                .position(|mode| &mode.name == name)
                .ok_or_else(|| format!("`mode_order` names unknown mode `{name}`"))?;
            ordered.push(modes.remove(index));
        }
        ordered.extend(modes);
        modes = ordered;
    }

    // Controls and endpoints.
    let mut controls = Vec::with_capacity(slot_order.len());
    let mut endpoints = Vec::new();
    for slot in &slot_order {
        let raw_control = unit_controls.get(slot).copied();
        let preset_slot = preset_slots.get(slot).copied();
        let (control, compiled) = compile_control(slot, raw_control, preset_slot)?;
        let base = endpoints.len();
        endpoints.extend(compiled);
        controls.push(Control {
            endpoints: (base..endpoints.len()).collect(),
            ..control
        });
    }

    // Mode bindings.
    let mut compiled_modes = Vec::with_capacity(modes.len());
    for raw_mode in &modes {
        let mut binds: Vec<Option<ModeBinding>> = vec![None; controls.len()];
        let mut seen_slots = HashSet::new();
        for raw_bind in &raw_mode.binds {
            let Some(control) = controls
                .iter()
                .position(|control| control.slot == raw_bind.slot)
            else {
                return Err(format!(
                    "mode `{}` binds unknown slot `{}`",
                    raw_mode.name, raw_bind.slot
                ));
            };
            if !seen_slots.insert(control) {
                return Err(format!(
                    "mode `{}` binds slot `{}` twice",
                    raw_mode.name, raw_bind.slot
                ));
            }
            let binding = compile_mode_binding(raw_bind, &controls[control], raw.deck)?;
            validate_binding(&controls[control], &binding, &endpoints)?;
            binds[control] = Some(binding);
        }
        compiled_modes.push(Mode {
            name: raw_mode.name.clone(),
            label: raw_mode
                .label
                .clone()
                .unwrap_or_else(|| raw_mode.name.clone()),
            binds,
        });
    }

    // Selectors come from mode slots, never from a separate table.
    let mut selectors = Vec::new();
    for control in &controls {
        let behavior = match control.role {
            SlotRole::Action => continue,
            SlotRole::ModeTrigger => {
                let target = control.target.as_deref().ok_or_else(|| {
                    format!("slot `{}`: a mode_trigger needs a `target`", control.slot)
                })?;
                let mode = compiled_modes
                    .iter()
                    .position(|mode| mode.name == target)
                    .ok_or_else(|| {
                        format!("slot `{}` targets unknown mode `{target}`", control.slot)
                    })?;
                SelectorBehavior::Trigger { mode }
            }
            SlotRole::ModeCycle => SelectorBehavior::Cycle,
        };
        // A mode slot with no endpoint yet is simply not wired: the guide learns it later.
        let Some(endpoint) = control.endpoint_index(&endpoints, EndpointRole::Main) else {
            continue;
        };
        selectors.push(Selector {
            endpoint: endpoints[endpoint].clone(),
            behavior,
            slot: control.slot.clone(),
        });
    }

    let start_name = raw
        .start
        .clone()
        .or_else(|| preset.and_then(|preset| preset.start.clone()));
    let start = match start_name {
        Some(name) => compiled_modes
            .iter()
            .position(|mode| mode.name == name)
            .ok_or_else(|| format!("start mode `{name}` is not in the mode order"))?,
        None => 0,
    };

    Ok(Unit {
        id: raw.id.clone(),
        deck: raw.deck,
        start,
        controls,
        endpoints,
        selectors,
        modes: compiled_modes,
    })
}

fn compile_control(
    slot: &str,
    raw: Option<&RawControl>,
    preset: Option<&crate::preset::PresetSlot>,
) -> Result<(Control, Vec<Endpoint>), String> {
    let kind = match (raw.and_then(|raw| raw.kind), preset.map(|slot| slot.kind)) {
        (Some(from_unit), Some(from_preset)) if from_unit != from_preset => {
            return Err(format!(
                "slot `{slot}`: `type = \"{from_unit:?}\"` disagrees with the preset's `{from_preset:?}`"
            ))
        }
        (Some(kind), _) | (None, Some(kind)) => kind,
        (None, None) => {
            return Err(format!("slot `{slot}`: add `type` (Button | Knob | BipolKnob | RelKnob | JogWheel)"))
        }
    };
    // A `[[unit.control]]` carries a role only when it overrides the preset's; otherwise the
    // preset's role (a mode key, usually) wins.
    let role = match raw {
        Some(raw) if raw.role != SlotRole::Action => raw.role,
        _ => preset.map(|slot| slot.role).unwrap_or_default(),
    };
    let preset_target = preset.and_then(|slot| slot.target.clone());
    let target = raw
        .and_then(|raw| raw.target.clone())
        .or(preset_target.clone());
    if role != SlotRole::ModeTrigger && target.is_some() {
        return Err(format!(
            "slot `{slot}`: `target` is only for `role = \"mode_trigger\"`"
        ));
    }
    if role == SlotRole::ModeTrigger && target.is_none() {
        return Err(format!(
            "slot `{slot}`: `role = \"mode_trigger\"` needs a `target` mode"
        ));
    }
    if role != SlotRole::Action && kind != SlotKind::Button {
        return Err(format!(
            "slot `{slot}`: a mode slot is a `Button`, not `{kind:?}`"
        ));
    }

    let raw_endpoints = raw
        .and_then(|raw| raw.endpoint.as_ref())
        .map(OneOrMany::as_slice)
        .unwrap_or_default();
    let mut endpoints = Vec::with_capacity(raw_endpoints.len());
    for raw_endpoint in raw_endpoints {
        endpoints.push(compile_endpoint(slot, raw_endpoint)?);
    }
    validate_endpoint_roles(slot, kind, &endpoints)?;

    Ok((
        Control {
            slot: slot.to_owned(),
            kind,
            role,
            target,
            endpoints: Vec::new(),
        },
        endpoints,
    ))
}

/// Which endpoint roles a slot kind needs.
fn validate_endpoint_roles(slot: &str, kind: SlotKind, endpoints: &[Endpoint]) -> Result<(), String> {
    if endpoints.is_empty() {
        return Ok(());
    }
    match kind {
        SlotKind::JogWheel => {
            if endpoints.len() != 2 {
                return Err(format!(
                    "slot `{slot}`: a JogWheel needs two endpoints (`press` and `turn`)"
                ));
            }
            let press = endpoints
                .iter()
                .find(|endpoint| endpoint.role == EndpointRole::Press)
                .ok_or_else(|| format!("slot `{slot}`: a JogWheel needs an endpoint with `role = \"press\"`"))?;
            let turn = endpoints
                .iter()
                .find(|endpoint| endpoint.role == EndpointRole::Turn)
                .ok_or_else(|| format!("slot `{slot}`: a JogWheel needs an endpoint with `role = \"turn\"`"))?;
            if press.kind == EventKind::Bend {
                return Err(format!("slot `{slot}`: a jog press is a note or a button CC"));
            }
            if turn.kind != EventKind::Cc || !turn.encoding.is_relative() {
                return Err(format!(
                    "slot `{slot}`: a jog turn is a relative CC (offset64 | twos_complement | direction | two_point)"
                ));
            }
        }
        _ => {
            if endpoints.len() != 1 {
                return Err(format!("slot `{slot}`: one endpoint, or none while unbound"));
            }
            if endpoints[0].role != EndpointRole::Main {
                return Err(format!(
                    "slot `{slot}`: `role` is only for a JogWheel's two halves"
                ));
            }
        }
    }
    Ok(())
}

fn compile_endpoint(slot: &str, raw: &RawEndpoint) -> Result<Endpoint, String> {
    if raw.channel > 15 {
        return Err(format!("slot `{slot}`: channel {} is out of range (0..=15)", raw.channel));
    }
    let mut encoding = raw.encoding;
    if raw.midi != EventKind::Cc {
        if encoding != Encoding::Absolute {
            return Err(format!(
                "slot `{slot}`: `encoding` is only for `midi = \"cc\"`"
            ));
        }
        if raw.lsb.is_some() {
            return Err(format!(
                "slot `{slot}`: `lsb` is only for `midi = \"cc\"`"
            ));
        }
    }
    let id = match (raw.midi, raw.id) {
        (EventKind::Bend, _) => 0,
        (_, Some(id)) => id,
        (_, None) => return Err(format!("slot `{slot}`: add `id` (a CC controller or note key)")),
    };
    if let Some(lsb) = raw.lsb {
        if lsb == id {
            return Err(format!("slot `{slot}`: `lsb` must differ from `id`"));
        }
        if lsb > 127 {
            return Err(format!("slot `{slot}`: `lsb` must be 0..=127"));
        }
        encoding = Encoding::Absolute;
    }
    if raw.reverse && !encoding.is_relative() {
        return Err(format!(
            "slot `{slot}`: `reverse` is only meaningful for a relative `encoding`"
        ));
    }
    Ok(Endpoint {
        role: raw.role,
        kind: raw.midi,
        encoding,
        reverse: raw.reverse,
        channel: raw.channel,
        id,
        lsb: raw.lsb,
    })
}

/// Everything an action line may need from the row that carries it.
struct ActionCtx<'a> {
    deck: Option<DeckId>,
    chain: Option<&'a str>,
    fx: Option<&'a str>,
}

fn compile_mode_binding(
    raw: &RawModeBind,
    control: &Control,
    unit_deck: Option<DeckId>,
) -> Result<ModeBinding, String> {
    if control.role != SlotRole::Action {
        return Err(format!(
            "slot `{}`: a mode slot does not emit commands; remove the `bind` row",
            control.slot
        ));
    }
    let ctx = ActionCtx {
        deck: raw.deck.or(unit_deck),
        chain: raw.chain.as_deref(),
        fx: raw.fx.as_deref(),
    };
    let parse = |line: Option<&str>| -> Result<Option<Action>, String> {
        line.map(|line| {
            parse_action(line, &ctx)
                .map_err(|message| format!("slot `{}` action `{line}`: {message}", raw.slot))
        })
        .transpose()
    };
    let actions = ActionSet {
        main: parse(raw.action.as_deref())?,
        press: parse(raw.press.as_deref())?,
        turn: parse(raw.turn.as_deref())?,
        down: parse(raw.down.as_deref())?,
    };
    if actions.is_empty() {
        return Err(format!(
            "slot `{}`: a bind row needs `action` (or `press`/`turn` for a JogWheel)",
            raw.slot
        ));
    }
    if actions.down.is_some() && actions.main.is_none() {
        return Err(format!(
            "slot `{}`: `down` needs an `action` to pair with",
            raw.slot
        ));
    }
    if (actions.press.is_some() || actions.turn.is_some()) && actions.main.is_some() {
        return Err(format!(
            "slot `{}`: use `action`, or `press`/`turn`, not both",
            raw.slot
        ));
    }

    let bipolar = control.kind == SlotKind::BipolKnob
        || actions.any_bipolar()
        || raw.min.is_some_and(|min| min < 0.0);
    let (default_min, default_max) = if bipolar { (-1.0, 1.0) } else { (0.0, 1.0) };
    let min = raw.min.unwrap_or(default_min);
    let max = raw.max.unwrap_or(default_max);
    if !(min.is_finite() && max.is_finite()) || min >= max {
        return Err(format!(
            "slot `{}`: `min` must be finite and less than `max`",
            raw.slot
        ));
    }
    let on = match raw.on {
        Some(_) if actions.iter().any(|action| action.is_momentary()) => {
            return Err(format!(
                "slot `{}`: this action uses both edges; `on` is only for one-shot actions",
                raw.slot
            ))
        }
        Some(_) if actions.down.is_some() => {
            return Err(format!(
                "slot `{}`: a two-action RelKnob has no `on`; each detent fires one action",
                raw.slot
            ))
        }
        Some(_) if actions.any_continuous() => {
            return Err(format!(
                "slot `{}`: a continuous action has no press/release edge; remove `on`",
                raw.slot
            ))
        }
        Some(edge) => edge,
        None => Edge::Press,
    };

    Ok(ModeBinding {
        deck: raw.deck.or(unit_deck),
        actions,
        kind: control.kind,
        min,
        max,
        curve: raw.curve.unwrap_or_default(),
        on,
    })
}

/// Which actions a slot kind can carry, per endpoint role.
fn validate_binding(
    control: &Control,
    binding: &ModeBinding,
    endpoints: &[Endpoint],
) -> Result<(), String> {
    let slot = &control.slot;
    let actions = &binding.actions;
    let relative = control
        .endpoint_index(endpoints, EndpointRole::Main)
        .is_some_and(|index| endpoints[index].encoding.is_relative());

    if control.role != SlotRole::Action {
        return Err(format!("slot `{slot}`: a mode slot does not emit commands"));
    }

    // A jog's two halves carry their own actions.
    if actions.press.is_some() || actions.turn.is_some() {
        if control.kind != SlotKind::JogWheel {
            return Err(format!(
                "slot `{slot}`: `press`/`turn` are for a JogWheel, not `{:?}`",
                control.kind
            ));
        }
        if actions.down.is_some() {
            return Err(format!("slot `{slot}`: a JogWheel has no `down` direction"));
        }
        if let Some(press) = &actions.press {
            if !press.is_jog_action() {
                return Err(format!(
                    "slot `{slot}`: a jog press carries `vinyl` or `vinyl slip`, not {press:?}"
                ));
            }
        }
        if let Some(turn) = &actions.turn {
            if !matches!(turn, Action::Vinyl { .. } | Action::Nudge { .. }) {
                return Err(format!(
                    "slot `{slot}`: a jog turn carries `vinyl` or `nudge`, not {turn:?}"
                ));
            }
        }
        return Ok(());
    }

    let Some(main) = &actions.main else {
        return Err(format!("slot `{slot}`: missing `action`"));
    };

    match control.kind {
        SlotKind::Button => {
            if main.is_continuous() {
                return Err(format!("slot `{slot}`: a Button cannot drive a continuous action"));
            }
            if actions.down.is_some() {
                return Err(format!("slot `{slot}`: `down` is for a RelKnob"));
            }
        }
        SlotKind::BipolKnob => {
            if relative {
                return Err(format!(
                    "slot `{slot}`: a BipolKnob needs an absolute endpoint, not a relative encoding"
                ));
            }
            if !main.is_continuous() {
                return Err(format!(
                    "slot `{slot}`: a BipolKnob drives a continuous action, not {main:?}"
                ));
            }
            if actions.down.is_some() {
                return Err(format!("slot `{slot}`: `down` is for a RelKnob"));
            }
        }
        SlotKind::Knob => {
            if main.is_continuous() {
                return Ok(());
            }
            if !relative {
                return Err(format!(
                    "slot `{slot}`: `{main:?}` needs a relative `encoding`"
                ));
            }
            if !matches!(
                main,
                Action::Nudge { .. } | Action::Vinyl { .. } | Action::Slip
            ) {
                return Err(format!(
                    "slot `{slot}`: a Knob drives a continuous action, `nudge`, or `vinyl`; not {main:?}"
                ));
            }
            if actions.down.is_some() {
                return Err(format!(
                    "slot `{slot}`: `down` is for a RelKnob; a Knob's detent is a delta"
                ));
            }
        }
        SlotKind::RelKnob => {
            if !relative {
                return Err(format!(
                    "slot `{slot}`: a RelKnob needs a relative `encoding`"
                ));
            }
            match &actions.down {
                Some(down) => {
                    if main.is_continuous() || down.is_continuous() {
                        return Err(format!(
                            "slot `{slot}`: the `action`/`down` pair must both be edge actions"
                        ));
                    }
                    if main.is_momentary() || down.is_momentary() {
                        return Err(format!(
                            "slot `{slot}`: a RelKnob pair cannot use a held action"
                        ));
                    }
                }
                None => {
                    if !main.is_continuous() {
                        return Err(format!(
                            "slot `{slot}`: a RelKnob without `down` needs a continuous action (or add `down`)"
                        ));
                    }
                }
            }
        }
        SlotKind::JogWheel => {
            if !main.is_jog_action() {
                return Err(format!(
                    "slot `{slot}`: a JogWheel carries `vinyl`, `vinyl slip`, or `nudge`, not {main:?}"
                ));
            }
        }
    }
    Ok(())
}

/// Resolves a stem out of an action name, naming the alternatives when it is wrong.
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

/// Resolves an action line into its parameterised [`Action`].
///
/// The line is **a session command line without the target**: `"play"`, `"beatjump 4"`,
/// `"sync phase pid"`, `"vocals mute"`, `"fader flow"`, `"loop 1/4"`. Every value the action needs
/// is in the line; the row only supplies the deck, and the FX chain address. Legacy dot-spelled
/// names are still accepted — see [`hypermixx_core::action::canonical`].
fn parse_action(line: &str, ctx: &ActionCtx<'_>) -> Result<Action, String> {
    let line = line.trim();
    if line.is_empty() {
        return Err("the action line is empty".to_owned());
    }
    let Some(line) = hypermixx_core::action::canonical(line) else {
        return Err(format!(
            "unknown action `{line}` (see `docs/MIDI.md` for the list)"
        ));
    };
    let words: Vec<&str> = line.split_whitespace().collect();
    let deck = |_what: &str| -> Result<DeckId, String> {
        ctx.deck
            .ok_or_else(|| "this action needs a `deck` (the unit's, or the row's)".to_owned())
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

    // A leading target word addresses a chain (`master fx toggle limiter`, `deck1/vocals fx …`).
    let mut named_chain = None;
    let words: &[&str] = match words.as_slice() {
        [target, rest @ ..] if rest.first() == Some(&"fx") => {
            named_chain = Some(chain_of(ctx.deck, target)?);
            rest
        }
        other => other,
    };

    match words {
        ["fader", "cuesend", "toggle"] => {
            deck("fader cuesend toggle")?;
            Ok(Action::ToggleCueSend)
        }
        ["vinyl", "slip"] => {
            deck("vinyl slip")?;
            Ok(Action::Slip)
        }
        ["keylock"] | ["keylock", "toggle"] => {
            deck("keylock")?;
            Ok(Action::ToggleKeylock)
        }
        ["keylock", what @ ("on" | "off")] => {
            deck("keylock")?;
            let mode = KeylockMode::parse(what)
                .ok_or_else(|| format!("unknown keylock mode `{what}`"))?;
            Ok(Action::Keylock(mode))
        }
        ["key", semitones] => {
            deck("key")?;
            let semitones = semitones
                .parse::<i32>()
                .map_err(|_| format!("`key` needs a signed whole number, got `{semitones}`"))?;
            Ok(Action::Key(semitones))
        }
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
            "load" => BrowserAction::Load { deck: ctx.deck },
            other => return Err(format!("unknown browser action `{other}`")),
        })),
        ["cue"] => {
            deck("cue")?;
            Ok(Action::Cue)
        }
        ["cue", "smart"] => {
            deck("cue smart")?;
            Ok(Action::CueSmart)
        }
        ["vinyl"] | ["vinyl", _] => {
            deck("vinyl")?;
            let step = match number(words.get(1).copied(), "vinyl frames per tick")? {
                Some(frames) => frames as f32,
                None => DEFAULT_VINYL_FRAMES_PER_TICK,
            };
            if !step.is_finite() || step <= 0.0 {
                return Err("vinyl frames per tick must be a positive number".to_owned());
            }
            Ok(Action::Vinyl {
                frames_per_tick: step,
            })
        }
        ["beatjump", beats] => {
            deck("beatjump")?;
            let beats = beats
                .parse::<i64>()
                .ok()
                .filter(|beats| *beats != 0)
                .ok_or_else(|| {
                    format!("`beatjump` needs a non-zero whole number of beats, got `{beats}`")
                })?;
            Ok(Action::BeatJump { beats })
        }
        ["beatjump"] => Err("`beatjump` needs a whole number of beats (`beatjump -4`)".to_owned()),
        ["fader", "flow"] => Ok(Action::Fader(FaderTarget::Flow(deck("fader flow")?))),
        ["fader", "deck"] => Ok(Action::Fader(FaderTarget::Deck(deck("fader deck")?))),
        ["fader", "gain"] => Ok(Action::Fader(FaderTarget::Gain(deck("fader gain")?))),
        ["fader", "cuesend"] => Ok(Action::Fader(FaderTarget::CueSend(deck("fader cuesend")?))),
        ["fader", "cross"] => Ok(Action::Fader(FaderTarget::Crossfader)),
        ["fader", "master"] => Ok(Action::Fader(FaderTarget::Master)),
        ["fader", "cue"] => Ok(Action::Fader(FaderTarget::Cue)),
        [stem_name, what @ ("fader" | "mute" | "solo")] => {
            let stem = stem_of(stem_name)?;
            let deck_id = deck(line.as_str())?;
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
        ["nudge"] | ["nudge", _] | ["nudge", _, _] => {
            deck("nudge")?;
            let delta = match number(words.get(1).copied(), "nudge delta")? {
                Some(delta) => delta as f32,
                None => DEFAULT_NUDGE_STEP,
            };
            if !delta.is_finite() || delta == 0.0 {
                return Err("a nudge delta must be non-zero".to_owned());
            }
            let seconds = number(words.get(2).copied(), "nudge seconds")?;
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
        ["loop"] => {
            deck("loop")?;
            Ok(Action::Loop(LoopAction::Beats(BeatLen::beats(4))))
        }
        ["loop", len] => {
            deck("loop")?;
            let len = BeatLen::parse(len).ok_or_else(|| {
                format!("`loop` takes a beat count or a fraction (`4`, `1/2`), got `{len}`")
            })?;
            Ok(Action::Loop(LoopAction::Beats(len)))
        }
        ["sync", what @ ("tempo" | "tempolock" | "set-leader" | "leader" | "unlock")] => {
            deck("sync")?;
            Ok(Action::Sync(match *what {
                "tempo" => SyncAction::Tempo,
                "tempolock" => SyncAction::TempoLock,
                "set-leader" | "leader" => SyncAction::Leader,
                _ => SyncAction::Unlock,
            }))
        }
        ["sync", phase @ ("phase" | "phaselock")] => {
            deck("sync")?;
            let (mode, seconds) = phase_args(&[])?;
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
        ["fx", what @ ("on" | "off" | "toggle" | "pad" | "trigger")] => {
            let slot = fx_action(ctx, named_chain, *what)?;
            Ok(Action::Fx(slot))
        }
        ["fx", "param", param] => {
            let (chain, fx) = fx_address(ctx, named_chain)?;
            Ok(Action::Fx(FxAction::Param {
                chain,
                fx,
                param: (*param).to_owned(),
            }))
        }
        ["fx", "param"] => Err("`fx param` needs the parameter name (`fx param value`)".to_owned()),
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

/// Builds one of the five slot-level FX actions, which all need `chain` + `fx` but no parameter.
fn fx_action(
    ctx: &ActionCtx<'_>,
    named: Option<FxChainId>,
    what: &str,
) -> Result<FxAction, String> {
    let (chain, fx) = fx_address(ctx, named)?;
    Ok(match what {
        "on" => FxAction::On { chain, fx },
        "off" => FxAction::Off { chain, fx },
        "toggle" => FxAction::Toggle { chain, fx },
        "pad" => FxAction::Pad { chain, fx },
        _ => FxAction::Trigger { chain, fx },
    })
}

/// The `chain` + `fx` address every FX action carries: the chain from the action line if it named
/// one, else from the row's `chain` field, else the slot's deck.
fn fx_address(ctx: &ActionCtx<'_>, named: Option<FxChainId>) -> Result<(FxChainId, String), String> {
    let chain = match named {
        Some(chain) => chain,
        None => match ctx.chain {
            Some(token) => chain_of(ctx.deck, token)?,
            // A per-deck preset (`fx = "eq"`) becomes deck1 on one unit and deck2 on another.
            None => ctx
                .deck
                .map(FxChainId::Deck)
                .ok_or_else(|| "an FX action needs a `chain` or a `deck`".to_owned())?,
        },
    };
    let fx = ctx
        .fx
        .ok_or("an FX action needs an `fx` name (the slot it addresses)")?;
    Ok((chain, fx.to_owned()))
}

/// Resolves an FX chain address the way the session grammar writes it:
///
/// * `master` / `m` — the summed mix;
/// * `deck1` / `d1` / `1` — that deck's channel chain;
/// * `<stem>` — that stem's own chain, with the deck from the row
///   (`deck = 1` + `chain = "vocals"` is deck1's vocal chain);
/// * `deck1/<stem>` — the same, spelled outright.
fn chain_of(deck: Option<DeckId>, token: &str) -> Result<FxChainId, String> {
    let token = token.trim();
    if let Some((head, stem_name)) = token.split_once('/') {
        let stem = stem_of(stem_name.trim())?;
        let deck = if head.trim().is_empty() {
            deck
        } else {
            parse_deck_token(head.trim())
        };
        let deck = deck.ok_or_else(|| format!("chain `{token}` needs a deck (`deck1/{stem}`)"))?;
        return Ok(FxChainId::Stem { deck, stem });
    }
    if let Some(deck) = parse_deck_token(token) {
        return Ok(FxChainId::Deck(deck));
    }
    if token.eq_ignore_ascii_case("master") || token.eq_ignore_ascii_case("m") {
        return Ok(FxChainId::Master);
    }
    if let Some(stem) = Stem::parse(token) {
        let deck = deck.ok_or_else(|| format!("chain `{token}` needs a `deck`"))?;
        return Ok(FxChainId::Stem { deck, stem });
    }
    Err(format!(
        "unknown chain `{token}` (master | deck1 | <stem> | deck1/<stem>)"
    ))
}

/// `deck1` / `d1` / `1` → that deck's id. Deck numbers are 1-based, as everywhere in the file.
fn parse_deck_token(token: &str) -> Option<DeckId> {
    let lower = token.to_ascii_lowercase();
    let digits = lower
        .strip_prefix("deck")
        .or_else(|| lower.strip_prefix('d'))
        .unwrap_or(&lower);
    let number: u8 = digits.parse().ok()?;
    (number > 0).then_some(number - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Map, MapError> {
        Map::from_toml_str(text)
    }

    fn deck_unit(body: &str) -> String {
        format!(
            r#"
[[unit]]
id = "u"
deck = 1
{body}
"#
        )
    }

    #[test]
    fn a_button_slot_reads_a_note_and_fires_one_action() {
        let text = deck_unit(
            r#"
[[unit.control]]
slot = "play"
type = "Button"
endpoint = { midi = "note", channel = 0, id = 16 }

[[unit.mode]]
name = "default"
[[unit.mode.bind]]
slot = "play"
action = "play"
"#,
        );
        let map = parse(&text).expect("button slot");
        let unit = &map.units[0];
        assert_eq!(unit.controls[0].kind, SlotKind::Button);
        assert_eq!(unit.endpoints[0].role, EndpointRole::Main);
        let binding = unit.binding(0, 0).expect("binding");
        assert!(matches!(binding.actions.main, Some(Action::Play)));
    }

    #[test]
    fn a_bipol_knob_defaults_to_the_bipolar_range() {
        let text = deck_unit(
            r#"
[[unit.control]]
slot = "filter"
type = "BipolKnob"
endpoint = { midi = "cc", channel = 0, id = 12 }

[[unit.mode]]
name = "default"
[[unit.mode.bind]]
slot = "filter"
action = "fx param value"
fx = "filter"
"#,
        );
        let map = parse(&text).expect("bipolar knob");
        let binding = map.units[0].binding(0, 0).expect("binding");
        assert_eq!((binding.min, binding.max), (-1.0, 1.0));
        let spec = binding.spec(&map.units[0].endpoints[0]).expect("spec");
        assert!(spec.center.is_some());
    }

    #[test]
    fn a_rel_knob_carries_one_action_per_direction() {
        let text = deck_unit(
            r#"
[[unit.control]]
slot = "browse"
type = "RelKnob"
endpoint = { midi = "cc", encoding = "two_point", reverse = true, channel = 0, id = 1 }

[[unit.mode]]
name = "default"
[[unit.mode.bind]]
slot = "browse"
action = "browser up"
down = "browser down"
"#,
        );
        let map = parse(&text).expect("relknob pair");
        let unit = &map.units[0];
        assert_eq!(unit.endpoints[0].encoding, Encoding::TwoPoint);
        assert!(unit.endpoints[0].reverse);
        let binding = unit.binding(0, 0).expect("binding");
        assert!(matches!(
            binding.actions.main,
            Some(Action::Browser(BrowserAction::Up))
        ));
        assert!(matches!(
            binding.actions.down,
            Some(Action::Browser(BrowserAction::Down))
        ));
    }

    #[test]
    fn a_rel_knob_pair_rejects_continuous_actions() {
        let text = deck_unit(
            r#"
[[unit.control]]
slot = "fader"
type = "RelKnob"
endpoint = { midi = "cc", encoding = "twos_complement", channel = 0, id = 1 }

[[unit.mode]]
name = "default"
[[unit.mode.bind]]
slot = "fader"
action = "fader flow"
down = "play"
"#,
        );
        let error = parse(&text).expect_err("continuous pair");
        assert!(error.message().contains("edge actions"), "{error}");
    }

    #[test]
    fn a_jog_wheel_has_a_press_and_a_turn() {
        let text = deck_unit(
            r#"
[[unit.control]]
slot = "jog"
type = "JogWheel"
endpoint = [
  { role = "press", midi = "note", channel = 0, id = 80 },
  { role = "turn", midi = "cc", encoding = "two_point", channel = 0, id = 6 },
]

[[unit.mode]]
name = "vinyl"
[[unit.mode.bind]]
slot = "jog"
action = "vinyl"

[[unit.mode]]
name = "slip"
[[unit.mode.bind]]
slot = "jog"
press = "vinyl slip"
turn = "vinyl"

[[unit.mode]]
name = "nudge"
[[unit.mode.bind]]
slot = "jog"
turn = "nudge 0.15"
"#,
        );
        let map = parse(&text).expect("jog");
        let unit = &map.units[0];
        assert_eq!(unit.controls[0].kind, SlotKind::JogWheel);
        assert_eq!(unit.endpoints.len(), 2);

        let vinyl = unit.binding(0, 0).expect("vinyl");
        let press = vinyl.spec(&unit.endpoints[0]).expect("press spec");
        let turn = vinyl.spec(&unit.endpoints[1]).expect("turn spec");
        assert!(matches!(press.action, Action::Vinyl { .. }));
        assert!(matches!(turn.action, Action::Vinyl { .. }));

        let slip = unit.binding(1, 0).expect("slip");
        assert!(matches!(
            slip.spec(&unit.endpoints[0]).map(|spec| spec.action),
            Some(Action::Slip)
        ));

        let nudge = unit.binding(2, 0).expect("nudge");
        assert!(nudge.spec(&unit.endpoints[0]).is_none(), "press is inert");
        assert!(matches!(
            nudge.spec(&unit.endpoints[1]).map(|spec| spec.action),
            Some(Action::Nudge { .. })
        ));
    }

    #[test]
    fn a_jog_wheel_rejects_two_endpoints_without_roles() {
        let text = deck_unit(
            r#"
[[unit.control]]
slot = "jog"
type = "JogWheel"
endpoint = [
  { midi = "note", channel = 0, id = 80 },
  { midi = "cc", encoding = "two_point", channel = 0, id = 6 },
]

[[unit.mode]]
name = "m"
"#,
        );
        let error = parse(&text).expect_err("roles are required");
        assert!(error.message().contains("role"), "{error}");
    }

    #[test]
    fn a_cc14_pair_claims_both_controller_numbers() {
        let text = deck_unit(
            r#"
[[unit.control]]
slot = "tempo"
type = "Knob"
endpoint = { midi = "cc", lsb = 41, channel = 0, id = 9 }

[[unit.mode]]
name = "default"
[[unit.mode.bind]]
slot = "tempo"
action = "tempofader"
"#,
        );
        let map = parse(&text).expect("cc14");
        let endpoint = &map.units[0].endpoints[0];
        assert_eq!(endpoint.lsb, Some(41));
        assert_eq!(endpoint.keys().len(), 2);
    }

    #[test]
    fn a_cc14_pair_cannot_overlap_another_endpoint() {
        let text = deck_unit(
            r#"
[[unit.control]]
slot = "tempo"
type = "Knob"
endpoint = { midi = "cc", lsb = 41, channel = 0, id = 9 }

[[unit.control]]
slot = "other"
type = "Knob"
endpoint = { midi = "cc", channel = 0, id = 41 }

[[unit.mode]]
name = "default"
[[unit.mode.bind]]
slot = "tempo"
action = "tempofader"
[[unit.mode.bind]]
slot = "other"
action = "fader flow"
"#,
        );
        let error = parse(&text).expect_err("lsb must be unique");
        assert!(error.message().contains("claimed by"), "{error}");
    }

    #[test]
    fn mode_slots_are_controls_that_emit_no_commands() {
        let text = deck_unit(
            r#"
[[unit.control]]
slot = "mode-hotcue"
type = "Button"
role = "mode_trigger"
target = "hotcue"
endpoint = { midi = "note", channel = 0, id = 24 }

[[unit.control]]
slot = "mode-next"
type = "Button"
role = "mode_cycle"
endpoint = { midi = "note", channel = 0, id = 25 }

[[unit.mode]]
name = "hotcue"
[[unit.mode]]
name = "beatloop"
"#,
        );
        let map = parse(&text).expect("mode slots");
        let unit = &map.units[0];
        assert_eq!(unit.controls[0].role, SlotRole::ModeTrigger);
        assert_eq!(unit.selectors.len(), 2);
        assert!(matches!(
            unit.selectors[0].behavior,
            SelectorBehavior::Trigger { mode } if unit.modes[mode].name == "hotcue"
        ));
        assert_eq!(unit.selectors[0].slot, "mode-hotcue");
        assert!(matches!(unit.selectors[1].behavior, SelectorBehavior::Cycle));

        let bad = text.replace(
            "[[unit.mode]]\nname = \"hotcue\"",
            "[[unit.mode]]\nname = \"hotcue\"\n[[unit.mode.bind]]\nslot = \"mode-hotcue\"\naction = \"play\"",
        );
        let error = parse(&bad).expect_err("mode slots cannot bind actions");
        assert!(error.message().contains("does not emit"), "{error}");
    }

    #[test]
    fn deck_numbers_are_written_one_based() {
        let map = parse(
            r#"
[[unit]]
id = "u"
deck = 1
[[unit.mode]]
name = "m"
"#,
        )
        .expect("deck1");
        assert_eq!(map.units[0].deck, Some(0));

        let error = parse(
            r#"
[[unit]]
id = "u"
deck = 0
[[unit.mode]]
name = "m"
"#,
        )
        .expect_err("deck0 is not a valid TOML deck");
        assert!(error.message().contains("start at 1"), "{error}");
    }

    #[test]
    fn a_button_cannot_drive_a_continuous_action_and_a_knob_cannot_drive_an_edge() {
        let pad = deck_unit(
            r#"
[[unit.control]]
slot = "pad"
type = "Button"
endpoint = { midi = "note", channel = 0, id = 1 }

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "pad"
action = "fader flow"
"#,
        );
        let error = parse(&pad).expect_err("a button cannot be continuous");
        assert!(error.message().contains("continuous"), "{error}");

        let knob = deck_unit(
            r#"
[[unit.control]]
slot = "knob"
type = "Knob"
endpoint = { midi = "cc", channel = 0, id = 1 }

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "knob"
action = "play"
"#,
        );
        let error = parse(&knob).expect_err("a knob cannot be an edge");
        assert!(error.message().contains("relative"), "{error}");
    }

    #[test]
    fn on_is_only_for_one_shot_actions() {
        let text = deck_unit(
            r#"
[[unit.control]]
slot = "pad"
type = "Button"
endpoint = { midi = "note", channel = 0, id = 1 }

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "pad"
action = "cue"
on = "release"
"#,
        );
        let error = parse(&text).expect_err("cue uses both edges");
        assert!(error.message().contains("both edges"), "{error}");
    }

    #[test]
    fn action_lines_carry_their_own_arguments() {
        let text = deck_unit(
            r#"
[[unit.control]]
slot = "pad"
type = "Button"
endpoint = { midi = "note", channel = 0, id = 1 }

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "pad"
action = "loop 1/4"
"#,
        );
        let map = parse(&text).expect("fractional loop");
        assert!(matches!(
            map.units[0].binding(0, 0).map(|bind| &bind.actions.main),
            Some(Some(Action::Loop(LoopAction::Beats(len))))
                if *len == BeatLen::fraction(1, 4).unwrap()
        ));
    }

    #[test]
    fn a_preset_slot_supplies_the_kind_and_the_mode_keys() {
        let text = r#"
[[unit]]
id = "deck1.pad"
preset = "pad"
deck = 1
"#;
        let map = parse(text).expect("pad preset");
        let unit = &map.units[0];
        assert_eq!(unit.controls.len(), 12);
        assert_eq!(unit.modes.len(), 3);
        // Unbound mode keys produce no selector until the guide learns their endpoint.
        assert_eq!(unit.selectors.len(), 0);
        assert_eq!(unit.controls[8].role, SlotRole::ModeTrigger);
        assert_eq!(unit.controls[11].role, SlotRole::ModeCycle);
    }

    #[test]
    fn a_unit_may_override_a_preset_slot_kind_only_to_agree() {
        let text = r#"
[[unit]]
id = "deck1.mixer"
preset = "mixer"
deck = 1

[[unit.control]]
slot = "filter"
type = "Knob"
endpoint = { midi = "cc", channel = 0, id = 12 }
"#;
        let error = parse(text).expect_err("the preset says BipolKnob");
        assert!(error.message().contains("disagrees"), "{error}");
    }

    #[test]
    fn a_unit_can_add_a_slot_that_the_preset_does_not_have() {
        let text = r#"
[[unit]]
id = "deck1.mixer"
preset = "mixer"
deck = 1

[[unit.control]]
slot = "extra"
type = "Button"
endpoint = { midi = "note", channel = 0, id = 40 }

[[unit.mode]]
name = "mine"
[[unit.mode.bind]]
slot = "extra"
action = "play"
"#;
        let map = parse(text).expect("extra slot");
        let unit = &map.units[0];
        assert_eq!(unit.controls.len(), 9);
        assert_eq!(unit.controls.last().unwrap().slot, "extra");
        let mine = unit
            .modes
            .iter()
            .find(|mode| mode.name == "mine")
            .expect("mine mode");
        assert!(mine
            .binds
            .last()
            .and_then(|bind| bind.as_ref())
            .is_some_and(|bind| matches!(bind.actions.main, Some(Action::Play))));
    }

    #[test]
    fn an_explicit_mode_order_moves_the_named_modes_first() {
        let text = r#"
[[unit]]
id = "deck1.pad"
preset = "pad"
deck = 1
mode_order = ["beatjump", "hotcue"]
start = "beatjump"
"#;
        let map = parse(text).expect("ordered modes");
        let unit = &map.units[0];
        assert_eq!(unit.modes[0].name, "beatjump");
        assert_eq!(unit.modes[1].name, "hotcue");
        assert_eq!(unit.modes[2].name, "beatloop");
        assert_eq!(unit.start, 0);
    }

    #[test]
    fn the_repositorys_default_maps_parse() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for name in ["profile/midi.toml", "profile/midi-map.toml"] {
            let path = root.join(name);
            let text = std::fs::read_to_string(&path).expect("default map");
            Map::from_toml_str_at(&text, path.parent().unwrap())
                .unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }

    #[test]
    fn the_old_schema_is_refused() {
        let error = parse(
            r#"
[[bind]]
type = "note"
channel = 0
id = 1
action = "play"
"#,
        )
        .expect_err("the flat bind table is gone");
        assert!(error.message().contains("bind"), "{error}");
    }
}
