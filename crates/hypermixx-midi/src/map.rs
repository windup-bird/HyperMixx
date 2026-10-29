//! The mapping file: a TOML schema built around **units**, plus its validation.
//!
//! A unit is one physical cluster (a pad grid, a channel strip, a browser row). It owns:
//!
//! * **controls** — hardware facts only: a slot name, an event kind, a channel and an id;
//! * **selectors** — pads/buttons that choose the active mode (trigger or cycle);
//! * **modes** — one slot → action table per mode.
//!
//! ```toml
//! [[unit]]
//! id = "deck0.pad"
//! preset = "touchpad"
//! deck = 1
//!
//! [[unit.control]]
//! slot = "pad1"
//! type = "note"
//! channel = 0
//! id = 16
//!
//! [[unit.selector]]
//! type = "note"
//! channel = 0
//! id = 24
//! behavior = "trigger"
//! target = "hotcue"
//!
//! [[unit.mode]]
//! name = "hotcue"
//! [[unit.mode.bind]]
//! slot = "pad1"
//! action = "hotcue trigger 1"
//! ```
//!
//! Parsing is `deny_unknown_fields`, so a typo is a TOML error rather than a silently ignored
//! binding. Endpoints must be unique across the whole file: one physical control belongs to one
//! slot. Units may mix pads, knobs and jogs; that is the "crossing" the design allows.
//!
//! The file names FX by **name** (`chain`/`fx`/`param`), never by slot index: indices shift when an
//! earlier slot is removed, and the file must survive a restart. The CLI resolves names to indices
//! via `ListFx` and installs them with [`Map::resolve_fx`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use hypermixx_core::{
    BrowserAction, DeckId, FaderTarget, FxChainId, HotCueOp, KeylockMode, PhaseMode, Stem,
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
    /// User preset files, relative to the map file's folder. A user preset with the same name as a
    /// built-in replaces it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<String>,
}

/// Which MIDI message kind an endpoint listens for.
/// Deck numbers are written 1-based in TOML and stored 0-based in the engine. Keeping the
/// conversion at the file boundary avoids an off-by-one in every audio/mixer index.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Cc,
    Note,
    Bend,
}

/// How a CC carries movement. Absolute knobs and faders are `Abs`; endless encoders differ by
/// vendor, so the exact wraparound rule is per endpoint.
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
    /// the same as a slow one.
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

/// Which edge of a one-shot control fires the action. Two-edge actions (`cue`, `vinyl`, held
/// `nudge`, `fx pad`) always use both edges and reject this field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    #[default]
    Press,
    Release,
}

/// What kind of physical control a slot is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum ControlShape {
    /// A note/CC button. One-shot actions fire on an edge; `cue`/`vinyl`/`fx pad` use both.
    #[serde(rename = "Button", alias = "button", alias = "pad")]
    Button,
    /// A continuous control: an absolute CC, a relative encoder, or pitch bend.
    #[serde(rename = "Knob", alias = "knob")]
    Knob,
    /// An absolute control whose centre is zero: `0..127` becomes `-1..1`.
    #[serde(rename = "BipolKnob", alias = "bipolknob")]
    BipolKnob,
    /// A relative encoder. Its delta can drive a continuous action, or an `action`/`down` pair.
    #[serde(rename = "RelKnob", alias = "relknob")]
    RelKnob,
    /// A platter: a touch/press note plus a relative wheel, both driving `vinyl`.
    #[serde(rename = "JogWheel", alias = "jogwheel", alias = "jog")]
    JogWheel,
    /// A mode selector slot: press selects `target`, no command is emitted.
    #[serde(rename = "ModeTrigger", alias = "modetrigger")]
    ModeTrigger,
    /// A mode selector slot: press advances to the next mode, no command is emitted.
    #[serde(rename = "ModeCycle", alias = "modecycle")]
    ModeCycle,
}

/// One endpoint used inside a jog's `touch`/`turn` tables.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawEndpoint {
    #[serde(rename = "type")]
    pub kind: EventKind,
    /// Optional LSB controller for a 14-bit CC pair. Only valid with `type = "cc"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsb: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RelMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse: Option<bool>,
    pub channel: u8,
    pub id: u8,
}

/// One `[[unit.control]]`: the hardware side of a slot.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawControl {
    pub slot: String,
    /// Optional override when the event kind alone is ambiguous (a CC button is `kind = "pad"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ControlShape>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub event_type: Option<EventKind>,
    /// Optional LSB controller for a 14-bit CC pair; `id` is then the MSB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsb: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RelMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u8>,
    /// A jog's press/touch sensor (usually a note). `press` is accepted as an alias.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "press")]
    pub touch: Option<RawEndpoint>,
    /// A jog's wheel (a relative CC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<RawEndpoint>,
    /// The mode selected by a `ModeTrigger` slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RawSelectorBehavior {
    /// Pressing selects the named mode and leaves it there.
    Trigger,
    /// Pressing advances to the next mode in declaration order, wrapping.
    Cycle,
}

/// One `[[unit.selector]]`: a mode button. It is not an action slot.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawSelector {
    #[serde(rename = "type")]
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RelMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse: Option<bool>,
    pub channel: u8,
    pub id: u8,
    pub behavior: RawSelectorBehavior,
    /// The mode a `trigger` selector selects. Required for `trigger`, refused for `cycle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// One slot → action row inside a mode.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawModeBind {
    pub slot: String,
    pub action: String,
    /// The reverse direction of a `RelKnob`: one edge action per negative detent.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<f32>,
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
    /// A built-in or user preset that supplies slot names and default modes.
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
    /// The exact mode order (and whitelist) for cycle selectors. Defaults to declaration order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_order: Option<Vec<String>>,
    #[serde(default, rename = "control")]
    pub controls: Vec<RawControl>,
    #[serde(default, rename = "selector")]
    pub selectors: Vec<RawSelector>,
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

/// The role an endpoint plays inside one control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointRole {
    Main,
    Touch,
    Turn,
}

/// A compiled, physical endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub role: EndpointRole,
    pub kind: EventKind,
    pub mode: RelMode,
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

/// One compiled slot: its physical endpoints (one for pad/knob, two for a jog).
#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub slot: String,
    pub shape: ControlShape,
    /// Indices into [`Unit::endpoints`].
    pub endpoints: Vec<usize>,
    /// The mode name for a `ModeTrigger` slot; `None` for every other kind.
    pub target: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectorBehavior {
    Trigger { mode: usize },
    Cycle,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Selector {
    pub endpoint: Endpoint,
    pub behavior: SelectorBehavior,
    /// Set when this selector came from a `ModeTrigger`/`ModeCycle` slot.
    pub slot: Option<String>,
}

/// A slot's action in one mode, before the physical endpoint is attached.
#[derive(Clone, Debug, PartialEq)]
pub struct ModeBinding {
    pub deck: Option<DeckId>,
    pub action: Action,
    /// The reverse direction of a `RelKnob`; `None` means the delta is continuous.
    pub down: Option<Action>,
    /// The slot type is needed to pick the centre of an absolute `BipolKnob`.
    pub shape: ControlShape,
    pub min: f32,
    pub max: f32,
    pub curve: Curve,
    pub on: Edge,
}

impl ModeBinding {
    /// Combines the mode's action with one endpoint into an executable binding.
    pub fn spec(&self, endpoint: &Endpoint) -> BindingSpec {
        BindingSpec {
            kind: endpoint.kind,
            mode: endpoint.mode,
            reverse: endpoint.reverse,
            channel: endpoint.channel,
            id: endpoint.id,
            lsb: endpoint.lsb,
            deck: self.deck,
            center: (self.shape == ControlShape::BipolKnob).then(|| match endpoint.kind {
                EventKind::Bend => 0.5,
                _ if endpoint.lsb.is_some() => 8192.0 / 16383.0,
                _ => 64.0 / 127.0,
            }),
            action: self.action.clone(),
            down: self.down.clone(),
            min: self.min,
            max: self.max,
            curve: self.curve,
            on: self.on,
        }
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

    /// The action bound to one control in one mode.
    pub fn binding(&self, mode: usize, control: usize) -> Option<&ModeBinding> {
        self.modes
            .get(mode)?
            .binds
            .get(control)?
            .as_ref()
    }
}

/// A validated, executable binding: one physical endpoint plus one mode action.
#[derive(Clone, Debug, PartialEq)]
pub struct BindingSpec {
    pub kind: EventKind,
    pub mode: RelMode,
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

/// The action-side fields of one mode binding, in the shape [`parse_action`] already understands.
/// The physical fields are filled from the slot's endpoint; the rest come from the mode row.
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

    /// Every endpoint in the file must be claimed once. A physical control cannot drive two slots
    /// or two mode selectors at the same time.
    fn validate_endpoints(&self) -> Result<(), String> {
        let mut seen: HashMap<(EventKind, u8, u8), String> = HashMap::new();
        for unit in &self.units {
            for control in &unit.controls {
                for &endpoint in &control.endpoints {
                    let endpoint = &unit.endpoints[endpoint];
                    let owner = format!("`{}/{}`", unit.id, control.slot);
                    for key in endpoint.keys() {
                        if let Some(previous) = seen.insert(key, owner.clone()) {
                            if previous != owner {
                                return Err(format!(
                                    "endpoint {:?} ch{} id{} is claimed by {previous} and {owner}",
                                    key.0, key.1, key.2
                                ));
                            }
                        }
                    }
                }
            }
            for selector in &unit.selectors {
                let endpoint = &selector.endpoint;
                let owner = match &selector.slot {
                    Some(slot) => format!("`{}/{}`", unit.id, slot),
                    None => format!("`{}/selector`", unit.id),
                };
                for key in endpoint.keys() {
                    if let Some(previous) = seen.insert(key, owner.clone()) {
                        if previous != owner {
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
        return Err(format!("{what} `{value}` must not contain whitespace"));
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
    let mut preset_shapes: HashMap<String, Option<ControlShape>> = HashMap::new();
    if let Some(preset) = preset {
        for slot in &preset.slots {
            check_name("slot", &slot.slot)?;
            if preset_shapes
                .insert(slot.slot.clone(), slot.shape)
                .is_some()
            {
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
        if unit_controls
            .insert(control.slot.clone(), control)
            .is_some()
        {
            return Err(format!("slot `{}` is declared twice", control.slot));
        }
        if !preset_shapes.contains_key(&control.slot) {
            slot_order.push(control.slot.clone());
        }
    }

    // Modes: preset defaults, same-name unit modes replace, new names append.
    let mut modes: Vec<RawMode> = preset.map(|preset| preset.modes.clone()).unwrap_or_default();
    let mut mode_names = HashSet::new();
    for mode in &modes {
        check_name("mode", &mode.name)?;
        if !mode_names.insert(mode.name.clone()) {
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
            modes.push(mode.clone());
        }
    }
    // A unit's own `mode_order` is an exact whitelist. A preset's order is a default: it orders
    // the preset modes, but a unit-added mode is appended rather than silently dropped, so user
    // extensions do not disappear behind a built-in list.
    let unit_modes: HashSet<String> = raw.modes.iter().map(|mode| mode.name.clone()).collect();
    if let Some(order) = raw.mode_order.clone() {
        let mut ordered = Vec::with_capacity(order.len());
        for name in order {
            let Some(index) = modes.iter().position(|mode| mode.name == name) else {
                return Err(format!("`mode_order` names unknown mode `{name}`"));
            };
            ordered.push(modes.remove(index));
        }
        modes = ordered;
    } else if let Some(order) = preset.and_then(|preset| preset.mode_order.clone()) {
        let mut ordered = Vec::with_capacity(modes.len());
        for name in order {
            let Some(index) = modes.iter().position(|mode| mode.name == name) else {
                return Err(format!(
                    "preset `{}` names unknown mode `{name}` in `mode_order`",
                    raw.preset.as_deref().unwrap_or("")
                ));
            };
            ordered.push(modes.remove(index));
        }
        // Keep modes the unit declared, even if the preset's default order does not name them.
        ordered.extend(
            modes
                .into_iter()
                .filter(|mode| unit_modes.contains(&mode.name)),
        );
        modes = ordered;
    }
    if modes.is_empty() {
        return Err("has no modes".to_owned());
    }
    let mut seen_modes = HashSet::new();
    for mode in &modes {
        if !seen_modes.insert(mode.name.clone()) {
            return Err(format!("mode `{}` is declared twice", mode.name));
        }
    }

    // Controls and endpoints.
    let mut controls = Vec::with_capacity(slot_order.len());
    let mut endpoints = Vec::new();
    for slot in &slot_order {
        let raw_control = unit_controls.get(slot).copied();
        let preset_shape = preset_shapes.get(slot).copied().flatten();
        let (shape, compiled, target) = compile_control(slot, raw_control, preset_shape)?;
        let base = endpoints.len();
        endpoints.extend(compiled);
        controls.push(Control {
            slot: slot.clone(),
            shape,
            endpoints: (base..endpoints.len()).collect(),
            target,
        });
    }

    // Mode bindings.
    let mut compiled_modes = Vec::with_capacity(modes.len());
    for raw_mode in &modes {
        let mut binds = vec![None; controls.len()];
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
            if matches!(
                controls[control].shape,
                ControlShape::ModeTrigger | ControlShape::ModeCycle
            ) {
                return Err(format!(
                    "mode `{}` binds mode slot `{}`; mode slots do not emit commands",
                    raw_mode.name, raw_bind.slot
                ));
            }
            let binding =
                compile_mode_binding(raw_bind, &controls[control], &endpoints, raw.deck)?;
            validate_shape(&controls[control], &binding, &endpoints)?;
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

    // Selectors: mode slots first, then the legacy `[[unit.selector]]` tables.
    let mut selectors = Vec::with_capacity(raw.selectors.len() + controls.len());
    for control in &controls {
        let behavior = match control.shape {
            ControlShape::ModeTrigger => {
                let target = control
                    .target
                    .as_deref()
                    .ok_or_else(|| format!("slot `{}`: ModeTrigger has no target", control.slot))?;
                let mode = compiled_modes
                    .iter()
                    .position(|mode| mode.name == target)
                    .ok_or_else(|| {
                        format!("slot `{}` targets unknown mode `{target}`", control.slot)
                    })?;
                SelectorBehavior::Trigger { mode }
            }
            ControlShape::ModeCycle => SelectorBehavior::Cycle,
            _ => continue,
        };
        if let Some(&endpoint_index) = control.endpoints.first() {
            selectors.push(Selector {
                endpoint: endpoints[endpoint_index].clone(),
                behavior,
                slot: Some(control.slot.clone()),
            });
        }
    }
    for raw_selector in &raw.selectors {
        let endpoint = compile_selector_endpoint(raw_selector)?;
        let behavior = match raw_selector.behavior {
            RawSelectorBehavior::Trigger => {
                let target = raw_selector.target.as_deref().ok_or_else(|| {
                    "a `trigger` selector needs a `target` mode".to_owned()
                })?;
                let mode = compiled_modes
                    .iter()
                    .position(|mode| mode.name == target)
                    .ok_or_else(|| format!("selector targets unknown mode `{target}`"))?;
                SelectorBehavior::Trigger { mode }
            }
            RawSelectorBehavior::Cycle => {
                if raw_selector.target.is_some() {
                    return Err("`target` is only meaningful for a `trigger` selector".to_owned());
                }
                SelectorBehavior::Cycle
            }
        };
        selectors.push(Selector { endpoint, behavior, slot: None });
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
    preset_shape: Option<ControlShape>,
) -> Result<(ControlShape, Vec<Endpoint>, Option<String>), String> {
    let Some(raw) = raw else {
        return match preset_shape {
            Some(ControlShape::ModeTrigger) => Err(format!(
                "slot `{slot}`: a ModeTrigger needs a `target` mode"
            )),
            Some(shape) => Ok((shape, Vec::new(), None)),
            None => Err(format!(
                "slot `{slot}` has no endpoint and the preset gives no `shape`"
            )),
        };
    };

    // A jog shares one action between its press/touch note and its wheel.
    if raw.touch.is_some() || raw.turn.is_some() {
        if raw.event_type.is_some()
            || raw.id.is_some()
            || raw.lsb.is_some()
            || raw.channel.is_some()
            || raw.mode.is_some()
            || raw.reverse.is_some()
        {
            return Err(format!(
                "slot `{slot}`: a jog declares `press`/`turn`; put `type`/`channel`/`id`/`mode` inside those tables"
            ));
        }
        if raw.target.is_some() {
            return Err(format!(
                "slot `{slot}`: `target` is only for ModeTrigger slots"
            ));
        }
        let touch = raw
            .touch
            .as_ref()
            .ok_or_else(|| format!("slot `{slot}`: a jog needs `press` (or `touch`)"))?;
        let turn = raw
            .turn
            .as_ref()
            .ok_or_else(|| format!("slot `{slot}`: a jog needs `turn`"))?;
        let shape = raw.kind.unwrap_or(ControlShape::JogWheel);
        if shape != ControlShape::JogWheel {
            return Err(format!(
                "slot `{slot}`: `press`/`turn` mean a jog, not {shape:?}"
            ));
        }
        if preset_shape.is_some_and(|preset| preset != ControlShape::JogWheel) {
            return Err(format!(
                "slot `{slot}`: the preset says {preset_shape:?}, the endpoints say jog"
            ));
        }
        let touch = compile_raw_endpoint(touch, EndpointRole::Touch)?;
        let turn = compile_raw_endpoint(turn, EndpointRole::Turn)?;
        if turn.kind != EventKind::Cc || turn.mode == RelMode::Abs {
            return Err(format!(
                "slot `{slot}`: the jog `turn` must be a relative CC (rel1 | rel2 | rel3)"
            ));
        }
        return Ok((ControlShape::JogWheel, vec![touch, turn], None));
    }

    let kind = raw
        .event_type
        .ok_or_else(|| format!("slot `{slot}`: add `type` (cc | note | bend)"))?;
    let channel = raw
        .channel
        .ok_or_else(|| format!("slot `{slot}`: add `channel`"))?;
    let id = raw
        .id
        .ok_or_else(|| format!("slot `{slot}`: add `id`"))?;
    let lsb = raw.lsb;
    if let Some(lsb) = lsb {
        if kind != EventKind::Cc {
            return Err(format!(
                "slot `{slot}`: `lsb` is only meaningful for `type = \"cc\"`"
            ));
        }
        if lsb == id {
            return Err(format!("slot `{slot}`: `lsb` must differ from `id`"));
        }
        if lsb > 127 || id > 127 {
            return Err(format!("slot `{slot}`: controller numbers must be 0..=127"));
        }
    }
    let mode = match kind {
        EventKind::Cc => raw.mode.unwrap_or(RelMode::Abs),
        EventKind::Note | EventKind::Bend => {
            if raw.mode.is_some() {
                return Err(format!(
                    "slot `{slot}`: `mode` is only meaningful for `type = \"cc\"`"
                ));
            }
            RelMode::Abs
        }
    };
    if lsb.is_some() && mode != RelMode::Abs {
        return Err(format!(
            "slot `{slot}`: a 14-bit CC pair is absolute; remove `mode`"
        ));
    }
    let reverse = raw.reverse.unwrap_or(false);
    if reverse && mode == RelMode::Abs {
        return Err(format!(
            "slot `{slot}`: `reverse` is only meaningful for a relative `mode`"
        ));
    }
    let inferred = match kind {
        EventKind::Note => ControlShape::Button,
        EventKind::Bend => ControlShape::Knob,
        EventKind::Cc => ControlShape::Knob,
    };
    let shape = raw.kind.unwrap_or(inferred);
    if lsb.is_some() && !matches!(shape, ControlShape::Knob | ControlShape::BipolKnob) {
        return Err(format!(
            "slot `{slot}`: a 14-bit CC pair is a Knob or BipolKnob"
        ));
    }
    if shape == ControlShape::JogWheel {
        return Err(format!(
            "slot `{slot}`: a jog needs `press` and `turn`, not `type`/`id`"
        ));
    }
    if let Some(preset) = preset_shape {
        if preset != shape {
            return Err(format!(
                "slot `{slot}`: endpoint shape {shape:?} does not match the preset's {preset:?}"
            ));
        }
    }

    if matches!(shape, ControlShape::ModeTrigger | ControlShape::ModeCycle) {
        if kind == EventKind::Bend {
            return Err(format!(
                "slot `{slot}`: a mode slot is a note or an absolute CC"
            ));
        }
        if mode != RelMode::Abs || reverse {
            return Err(format!(
                "slot `{slot}`: a mode slot is absolute; remove `mode`/`reverse`"
            ));
        }
        if lsb.is_some() {
            return Err(format!("slot `{slot}`: a mode slot does not take `lsb`"));
        }
        match shape {
            ControlShape::ModeTrigger => {
                let target = raw
                    .target
                    .clone()
                    .ok_or_else(|| format!("slot `{slot}`: a ModeTrigger needs a `target`"))?;
                check_name("mode", &target)?;
                let endpoint =
                    compile_endpoint(kind, mode, false, channel, id, None, EndpointRole::Main)?;
                return Ok((shape, vec![endpoint], Some(target)));
            }
            ControlShape::ModeCycle => {
                if raw.target.is_some() {
                    return Err(format!(
                        "slot `{slot}`: `target` is only for ModeTrigger slots"
                    ));
                }
                let endpoint =
                    compile_endpoint(kind, mode, false, channel, id, None, EndpointRole::Main)?;
                return Ok((shape, vec![endpoint], None));
            }
            _ => unreachable!(),
        }
    }
    if raw.target.is_some() {
        return Err(format!(
            "slot `{slot}`: `target` is only for ModeTrigger slots"
        ));
    }

    match (kind, shape) {
        (EventKind::Note, ControlShape::Button)
        | (EventKind::Bend, ControlShape::Knob)
        | (EventKind::Bend, ControlShape::BipolKnob)
        | (EventKind::Cc, ControlShape::Knob)
        | (EventKind::Cc, ControlShape::BipolKnob)
        | (EventKind::Cc, ControlShape::RelKnob)
        | (EventKind::Cc, ControlShape::Button) => {}
        (EventKind::Note, _) => {
            return Err(format!(
                "slot `{slot}`: a note is a Button; a continuous control must be a CC or bend"
            ))
        }
        (EventKind::Bend, _) => {
            return Err(format!("slot `{slot}`: pitch bend is a Knob or BipolKnob"))
        }
        (EventKind::Cc, ControlShape::JogWheel) => unreachable!(),
        (EventKind::Cc, ControlShape::ModeTrigger | ControlShape::ModeCycle) => unreachable!(),
    }
    if shape == ControlShape::BipolKnob && mode != RelMode::Abs {
        return Err(format!(
            "slot `{slot}`: a BipolKnob needs an absolute CC or bend"
        ));
    }
    if kind == EventKind::Cc && mode != RelMode::Abs && !matches!(shape, ControlShape::Knob | ControlShape::RelKnob) {
        return Err(format!(
            "slot `{slot}`: a relative CC is a Knob or RelKnob, not a Button"
        ));
    }
    if shape == ControlShape::RelKnob && mode == RelMode::Abs {
        return Err(format!(
            "slot `{slot}`: a RelKnob needs a relative `mode` (rel1 | rel2 | rel3)"
        ));
    }
    let endpoint = compile_endpoint(kind, mode, reverse, channel, id, lsb, EndpointRole::Main)?;
    Ok((shape, vec![endpoint], None))
}

fn compile_raw_endpoint(raw: &RawEndpoint, role: EndpointRole) -> Result<Endpoint, String> {
    let mode = match raw.kind {
        EventKind::Cc => raw.mode.unwrap_or(RelMode::Abs),
        EventKind::Note | EventKind::Bend => {
            if raw.mode.is_some() {
                return Err("`mode` is only meaningful for `type = \"cc\"`".to_owned());
            }
            RelMode::Abs
        }
    };
    let reverse = raw.reverse.unwrap_or(false);
    if reverse && mode == RelMode::Abs {
        return Err("`reverse` is only meaningful for a relative `mode`".to_owned());
    }
    if raw.lsb.is_some() && (raw.kind != EventKind::Cc || mode != RelMode::Abs) {
        return Err("`lsb` is only meaningful for an absolute `type = \"cc\"`".to_owned());
    }
    let id = if raw.kind == EventKind::Bend { 0 } else { raw.id };
    compile_endpoint(raw.kind, mode, reverse, raw.channel, id, raw.lsb, role)
}

fn compile_selector_endpoint(raw: &RawSelector) -> Result<Endpoint, String> {
    match raw.kind {
        EventKind::Bend => Err("a selector must be a note or an absolute CC".to_owned()),
        EventKind::Cc => {
            let mode = raw.mode.unwrap_or(RelMode::Abs);
            if mode != RelMode::Abs {
                return Err("a selector must be a note or an absolute CC".to_owned());
            }
            if raw.reverse == Some(true) {
                return Err("a selector does not take `reverse`".to_owned());
            }
            compile_endpoint(raw.kind, mode, false, raw.channel, raw.id, None, EndpointRole::Main)
        }
        EventKind::Note => {
            if raw.mode.is_some() {
                return Err("`mode` is only meaningful for `type = \"cc\"`".to_owned());
            }
            if raw.reverse == Some(true) {
                return Err("a selector does not take `reverse`".to_owned());
            }
            compile_endpoint(raw.kind, RelMode::Abs, false, raw.channel, raw.id, None, EndpointRole::Main)
        }
    }
}

fn compile_endpoint(
    kind: EventKind,
    mode: RelMode,
    reverse: bool,
    channel: u8,
    id: u8,
    lsb: Option<u8>,
    role: EndpointRole,
) -> Result<Endpoint, String> {
    if channel > 15 {
        return Err(format!("channel {channel} is out of range (0..=15)"));
    }
    Ok(Endpoint {
        role,
        kind,
        mode,
        reverse,
        channel,
        id,
        lsb,
    })
}

fn compile_mode_binding(
    raw: &RawModeBind,
    control: &Control,
    endpoints: &[Endpoint],
    unit_deck: Option<DeckId>,
) -> Result<ModeBinding, String> {
    let primary = control.endpoints.first().map(|&index| &endpoints[index]);
    let deck = raw.deck.or(unit_deck);
    let make_raw = |line: &str| RawBinding {
        kind: primary.map_or(EventKind::Cc, |endpoint| endpoint.kind),
        mode: Some(primary.map_or(RelMode::Abs, |endpoint| endpoint.mode)),
        reverse: Some(primary.is_some_and(|endpoint| endpoint.reverse)),
        channel: Some(primary.map_or(0, |endpoint| endpoint.channel)),
        id: Some(primary.map_or(0, |endpoint| endpoint.id)),
        deck,
        action: line.to_owned(),
        min: raw.min,
        max: raw.max,
        curve: raw.curve,
        step: raw.step,
        seconds: raw.seconds,
        beats: raw.beats,
        chain: raw.chain.clone(),
        fx: raw.fx.clone(),
        param: raw.param.clone(),
    };
    let action = parse_action(&make_raw(&raw.action))
        .map_err(|message| format!("slot `{}` action `{}`: {message}", raw.slot, raw.action))?;
    let down = raw
        .down
        .as_deref()
        .map(|line| {
            parse_action(&make_raw(line)).map_err(|message| {
                format!("slot `{}` down action `{line}`: {message}", raw.slot)
            })
        })
        .transpose()?;

    if let Some(down_action) = &down {
        if control.shape != ControlShape::RelKnob {
            return Err(format!(
                "slot `{}`: only a RelKnob takes a `down` action",
                raw.slot
            ));
        }
        if primary.is_some_and(|endpoint| endpoint.mode == RelMode::Abs) {
            return Err(format!(
                "slot `{}`: a RelKnob needs a relative `mode` (rel1 | rel2 | rel3)",
                raw.slot
            ));
        }
        if action.is_continuous() || down_action.is_continuous() {
            return Err(format!(
                "slot `{}`: the `action`/`down` pair must both be edge actions",
                raw.slot
            ));
        }
        if action.is_momentary() || down_action.is_momentary() {
            return Err(format!(
                "slot `{}`: a RelKnob pair cannot use a held/momentary action",
                raw.slot
            ));
        }
        if raw.on.is_some() || raw.min.is_some() || raw.max.is_some() || raw.curve.is_some() {
            return Err(format!(
                "slot `{}`: a RelKnob pair has no `on`/`min`/`max`/`curve`; each detent fires one action",
                raw.slot
            ));
        }
    }

    let bipolar = control.shape == ControlShape::BipolKnob
        || action.is_bipolar()
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
    let on = if down.is_some() {
        Edge::Press
    } else {
        match raw.on {
            Some(_) if action.is_momentary() => {
                return Err(format!(
                    "slot `{}`: this action uses both edges; `on` is only for one-shot actions",
                    raw.slot
                ))
            }
            Some(_) if action.is_continuous() => {
                return Err(format!(
                    "slot `{}`: a continuous action has no press/release edge; remove `on`",
                    raw.slot
                ))
            }
            Some(edge) => edge,
            None => Edge::Press,
        }
    };
    Ok(ModeBinding {
        deck,
        action,
        down,
        shape: control.shape,
        min,
        max,
        curve: raw.curve.unwrap_or_default(),
        on,
    })
}

/// The physical shape has to be able to carry every action the slot is bound to.
fn validate_shape(
    control: &Control,
    binding: &ModeBinding,
    endpoints: &[Endpoint],
) -> Result<(), String> {
    if control.endpoints.is_empty() {
        return Ok(());
    }
    let endpoint = &endpoints[control.endpoints[0]];
    let relative = endpoint.mode != RelMode::Abs;
    match control.shape {
        ControlShape::Button => {
            if binding.action.is_continuous() {
                return Err(format!(
                    "slot `{}`: a Button cannot drive a continuous action",
                    control.slot
                ));
            }
        }
        ControlShape::Knob => {
            match &binding.action {
                Action::Fader(_)
                | Action::TempoFader
                | Action::Fx(FxAction::Param { .. }) => {}
                Action::Nudge { .. } | Action::Vinyl { .. } if relative => {}
                Action::Nudge { .. } | Action::Vinyl { .. } => {
                    return Err(format!(
                        "slot `{}`: this action needs a relative encoder",
                        control.slot
                    ))
                }
                _ => {
                    return Err(format!(
                        "slot `{}`: a Knob can only drive a continuous action (or nudge/vinyl on a relative encoder)",
                        control.slot
                    ))
                }
            }
        }
        ControlShape::BipolKnob => {
            if relative {
                return Err(format!(
                    "slot `{}`: a BipolKnob needs an absolute CC or bend",
                    control.slot
                ));
            }
            if !binding.action.is_continuous() {
                return Err(format!(
                    "slot `{}`: a BipolKnob can only drive a continuous action",
                    control.slot
                ));
            }
        }
        ControlShape::RelKnob => {
            if !relative {
                return Err(format!(
                    "slot `{}`: a RelKnob needs a relative `mode`",
                    control.slot
                ));
            }
            if binding.down.is_none() {
                match &binding.action {
                    Action::Fader(_)
                    | Action::TempoFader
                    | Action::Fx(FxAction::Param { .. }) => {}
                    Action::Nudge { .. } | Action::Vinyl { .. } => {}
                    _ => {
                        return Err(format!(
                            "slot `{}`: a RelKnob needs a continuous action or an `action`/`down` pair",
                            control.slot
                        ))
                    }
                }
            }
        }
        ControlShape::JogWheel => {
            if !matches!(binding.action, Action::Vinyl { .. }) {
                return Err(format!(
                    "slot `{}`: a JogWheel can only drive `vinyl`",
                    control.slot
                ));
            }
        }
        ControlShape::ModeTrigger | ControlShape::ModeCycle => {
            return Err(format!(
                "slot `{}`: a mode slot does not emit commands",
                control.slot
            ));
        }
    }
    Ok(())
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
        // Edge-only additions: the sustained `fader cuesend` and `vinyl` forms stay below.
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
            let semitones = semitones.parse::<i32>().map_err(|_| {
                format!("`key` needs a signed whole number, got `{semitones}`")
            })?;
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
        None => match raw.chain.as_deref() {
            Some(token) => chain_of(raw, token)?,
            // A unit's mode can omit `chain`: the slot's deck is the chain. This is what lets one
            // per-deck preset (`fx = "eq"`) become `deck0` on one instance and `deck1` on another.
            None => raw
                .deck
                .map(FxChainId::Deck)
                .ok_or_else(|| "an FX action needs a `chain` or a `deck`".to_owned())?,
        },
    };
    let fx = raw.fx.clone().ok_or("an FX action needs an `fx` name")?;
    Ok((chain, fx))
}

/// Resolves an FX chain address the way the session grammar writes it:
///
/// * `master` / `m` — the summed mix;
/// * `deck0` / `d0` / `0` — that deck's channel chain;
/// * `<stem>` — that stem's own chain, with the deck from the binding's `deck` field
///   (`deck = 1` + `chain = "vocals"` is `deck0 vocals`);
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

    fn touchpad() -> &'static str {
        r#"
[[unit]]
id = "deck0.pad"
preset = "touchpad"
deck = 1

[[unit.control]]
slot = "pad1"
type = "note"
channel = 0
id = 16
[[unit.control]]
slot = "pad2"
type = "note"
channel = 0
id = 17
[[unit.control]]
slot = "pad3"
type = "note"
channel = 0
id = 18
[[unit.control]]
slot = "pad4"
type = "note"
channel = 0
id = 19
[[unit.control]]
slot = "pad5"
type = "note"
channel = 0
id = 20
[[unit.control]]
slot = "pad6"
type = "note"
channel = 0
id = 21
[[unit.control]]
slot = "pad7"
type = "note"
channel = 0
id = 22
[[unit.control]]
slot = "pad8"
type = "note"
channel = 0
id = 23

[[unit.selector]]
type = "note"
channel = 0
id = 24
behavior = "trigger"
target = "hotcue"

[[unit.selector]]
type = "note"
channel = 0
id = 25
behavior = "cycle"
"#
    }

    fn mixer(deck: DeckId, channel: u8) -> String {
        format!(
            r#"
[[unit]]
id = "deck{deck}.mixer"
preset = "mixer"
deck = {number}

[[unit.control]]
slot = "fader"
type = "cc"
channel = {channel}
id = 7
[[unit.control]]
slot = "gain"
type = "cc"
channel = {channel}
id = 8
[[unit.control]]
slot = "low"
type = "cc"
channel = {channel}
id = 9
[[unit.control]]
slot = "mid"
type = "cc"
channel = {channel}
id = 10
[[unit.control]]
slot = "high"
type = "cc"
channel = {channel}
id = 11
[[unit.control]]
slot = "filter"
kind = "BipolKnob"
type = "cc"
channel = {channel}
id = 12
[[unit.control]]
slot = "cue"
type = "note"
channel = {channel}
id = 13
[[unit.control]]
slot = "load"
type = "note"
channel = {channel}
id = 14
"#,
            number = deck + 1,
        )
    }

    #[test]
    fn a_touchpad_preset_compiles_its_slots_modes_and_selectors() {
        let map = Map::from_toml_str(touchpad()).expect("touchpad map");
        let unit = &map.units[0];
        assert_eq!(unit.controls.len(), 8);
        assert_eq!(unit.modes.len(), 4);
        assert_eq!(unit.modes[unit.start].name, "hotcue");
        assert_eq!(unit.selectors.len(), 2);
        assert!(matches!(
            unit.selectors[0].behavior,
            SelectorBehavior::Trigger { mode } if unit.modes[mode].name == "hotcue"
        ));
        assert!(matches!(unit.selectors[1].behavior, SelectorBehavior::Cycle));
        assert!(matches!(
            unit.modes[0].binds[0].as_ref().map(|bind| &bind.action),
            Some(Action::HotCue(HotCueOp::Trigger(0)))
        ));
    }

    #[test]
    fn a_mixer_preset_resolves_fx_chains_from_the_unit_deck() {
        let map = Map::from_toml_str(&mixer(1, 1)).expect("mixer map");
        let unit = &map.units[0];
        let low = unit.modes[0].binds[2].as_ref().expect("low is bound");
        assert!(matches!(
            &low.action,
            Action::Fx(FxAction::Param {
                chain: FxChainId::Deck(1),
                fx,
                param
            }) if fx == "eq" && param == "low"
        ));
        assert_eq!(low.min, -1.0);
        assert_eq!(low.max, 1.0);
        let cue = unit.modes[0].binds[6].as_ref().expect("cue is bound");
        assert!(matches!(cue.action, Action::ToggleCueSend));
        assert_eq!(cue.deck, Some(1));
        let load = unit.modes[0].binds[7].as_ref().expect("load is bound");
        assert!(matches!(
            load.action,
            Action::Browser(BrowserAction::Load { deck: Some(1) })
        ));
    }

    #[test]
    fn action_lines_take_the_arguments_a_session_line_takes() {
        for (line, expected) in [
            ("keylock on", "keylock on"),
            ("keylock toggle", "keylock toggle"),
            ("key 2", "key 2"),
            ("vinyl slip", "vinyl slip"),
            ("fader cuesend toggle", "fader cuesend toggle"),
        ] {
            let raw = RawBinding {
                kind: EventKind::Note,
                mode: None,
                reverse: None,
                channel: Some(0),
                id: Some(1),
                deck: Some(0),
                action: line.to_owned(),
                min: None,
                max: None,
                curve: None,
                step: None,
                seconds: None,
                beats: None,
                chain: None,
                fx: None,
                param: None,
            };
            assert!(parse_action(&raw).is_ok(), "{expected} should parse");
        }
    }

    #[test]
    fn an_endpoint_cannot_be_claimed_twice() {
        let text = touchpad().replace(
            "[[unit.selector]]\ntype = \"note\"\nchannel = 0\nid = 24",
            "[[unit.selector]]\ntype = \"note\"\nchannel = 0\nid = 16",
        );
        let error = Map::from_toml_str(&text).expect_err("duplicate endpoint");
        assert!(error.message().contains("claimed by"), "{error}");
    }

    #[test]
    fn a_pad_cannot_drive_a_continuous_action_and_a_knob_cannot_drive_an_edge() {
        let pad = r#"
[[unit]]
id = "u"
deck = 1
[[unit.control]]
slot = "pad"
type = "note"
channel = 0
id = 1
[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "pad"
action = "fader flow"
"#;
        let error = Map::from_toml_str(pad).expect_err("pad cannot be continuous");
        assert!(error.message().contains("continuous"), "{error}");

        let knob = r#"
[[unit]]
id = "u"
deck = 1
[[unit.control]]
slot = "knob"
type = "cc"
channel = 0
id = 1
[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "knob"
action = "play"
"#;
        let error = Map::from_toml_str(knob).expect_err("knob cannot be an edge");
        assert!(error.message().contains("continuous action"), "{error}");
    }

    #[test]
    fn deck_numbers_are_written_one_based() {
        let map = Map::from_toml_str(
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

        let error = Map::from_toml_str(
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
    fn mode_slots_are_controls_that_emit_no_commands() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "mode1"
kind = "ModeTrigger"
type = "note"
channel = 0
id = 24
target = "hotcue"

[[unit.control]]
slot = "mode2"
kind = "ModeCycle"
type = "note"
channel = 0
id = 25

[[unit.mode]]
name = "hotcue"
[[unit.mode]]
name = "loop"
"#;
        let map = Map::from_toml_str(text).expect("mode slots");
        let unit = &map.units[0];
        assert_eq!(unit.controls[0].shape, ControlShape::ModeTrigger);
        assert_eq!(unit.selectors.len(), 2);
        assert!(matches!(
            unit.selectors[0].behavior,
            SelectorBehavior::Trigger { mode } if unit.modes[mode].name == "hotcue"
        ));
        assert_eq!(unit.selectors[0].slot.as_deref(), Some("mode1"));
        assert!(matches!(unit.selectors[1].behavior, SelectorBehavior::Cycle));

        let bad = text.replace(
            "[[unit.mode]]
name = \"hotcue\"",
            "[[unit.mode]]
name = \"hotcue\"\n[[unit.mode.bind]]\nslot = \"mode1\"\naction = \"play\"",
        );
        let error = Map::from_toml_str(&bad).expect_err("mode slot cannot bind an action");
        assert!(error.message().contains("do not emit commands"), "{error}");
    }

    #[test]
    fn a_one_cc_jog_accepts_press_and_a_relative_turn() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "jog"
kind = "JogWheel"
press = { type = "note", channel = 0, id = 80 }
turn = { type = "cc", mode = "rel2", reverse = true, channel = 0, id = 6 }

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "jog"
action = "vinyl"
"#;
        let map = Map::from_toml_str(text).expect("one-CC jog");
        let control = &map.units[0].controls[0];
        assert_eq!(control.shape, ControlShape::JogWheel);
        assert_eq!(control.endpoints.len(), 2);
        let turn = &map.units[0].endpoints[control.endpoints[1]];
        assert_eq!(turn.mode, RelMode::Rel2);
        assert!(turn.reverse);
    }

    #[test]
    fn a_cc14_pair_claims_both_controller_numbers() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "tempo"
type = "cc"
lsb = 41
channel = 0
id = 9

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "tempo"
action = "tempofader"
"#;
        let map = Map::from_toml_str(text).expect("cc14");
        let endpoint = &map.units[0].endpoints[0];
        assert_eq!(endpoint.lsb, Some(41));
        assert_eq!(endpoint.keys().len(), 2);
    }

    #[test]
    fn a_cc14_pair_cannot_overlap_another_endpoint() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "tempo"
type = "cc"
lsb = 41
channel = 0
id = 9

[[unit.control]]
slot = "other"
type = "cc"
channel = 0
id = 41

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "tempo"
action = "tempofader"
[[unit.mode.bind]]
slot = "other"
action = "fader flow"
"#;
        let error = Map::from_toml_str(text).expect_err("lsb must be unique");
        assert!(error.message().contains("claimed by"), "{error}");
    }

    #[test]
    fn a_rel_knob_carries_both_directions_in_the_mode() {
        let text = r#"
[[unit]]
id = "u"

[[unit.control]]
slot = "browse"
type = "cc"
kind = "RelKnob"
mode = "rel2"
channel = 0
id = 1

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "browse"
action = "browser up"
down = "browser down"
"#;
        let map = Map::from_toml_str(text).expect("relknob pair");
        let unit = &map.units[0];
        assert_eq!(unit.controls[0].shape, ControlShape::RelKnob);
        let binding = unit.binding(0, 0).expect("binding");
        assert!(matches!(binding.action, Action::Browser(BrowserAction::Up)));
        assert!(matches!(
            binding.down,
            Some(Action::Browser(BrowserAction::Down))
        ));
    }

    #[test]
    fn a_rel_knob_pair_rejects_continuous_actions() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "fader"
type = "cc"
kind = "RelKnob"
mode = "rel2"
channel = 0
id = 1

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "fader"
action = "fader flow"
down = "play"
"#;
        let error = Map::from_toml_str(text).expect_err("continuous pair");
        assert!(error.message().contains("edge actions"), "{error}");
    }

    #[test]
    fn a_bipol_knob_defaults_to_the_bipolar_range() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "filter"
type = "cc"
kind = "BipolKnob"
channel = 0
id = 1

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "filter"
action = "fx param"
fx = "filter"
param = "value"
"#;
        let map = Map::from_toml_str(text).expect("bipol knob");
        let binding = map.units[0].binding(0, 0).expect("binding");
        assert_eq!((binding.min, binding.max), (-1.0, 1.0));
    }

    #[test]
    fn a_bipol_knob_cannot_take_a_relative_encoder() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "filter"
type = "cc"
kind = "BipolKnob"
mode = "rel2"
channel = 0
id = 1

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "filter"
action = "fx param"
fx = "filter"
param = "value"
"#;
        let error = Map::from_toml_str(text).expect_err("bipol knob must be absolute");
        assert!(error.message().contains("absolute"), "{error}");
    }

    #[test]
    fn on_is_only_for_one_shot_actions() {
        let text = r#"
[[unit]]
id = "u"
deck = 1
[[unit.control]]
slot = "pad"
type = "note"
channel = 0
id = 1
[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "pad"
action = "cue"
on = "release"
"#;
        let error = Map::from_toml_str(text).expect_err("cue uses both edges");
        assert!(error.message().contains("both edges"), "{error}");
    }

    #[test]
    fn selectors_must_name_a_real_mode_and_trigger_needs_a_target() {
        let text = touchpad().replace("target = \"hotcue\"", "target = \"missing\"");
        let error = Map::from_toml_str(&text).expect_err("unknown selector target");
        assert!(error.message().contains("unknown mode"), "{error}");
    }

    #[test]
    fn mode_order_selects_and_orders_the_visible_modes() {
        let text = touchpad().replace(
            "preset = \"touchpad\"",
            "preset = \"touchpad\"\nmode_order = [\"loop\", \"hotcue\"]\nstart = \"loop\"",
        );
        let map = Map::from_toml_str(&text).expect("ordered modes");
        let unit = &map.units[0];
        assert_eq!(unit.modes.len(), 2);
        assert_eq!(unit.modes[0].name, "loop");
        assert_eq!(unit.modes[1].name, "hotcue");
        assert_eq!(unit.start, 0);
    }

    #[test]
    fn a_unit_can_add_a_slot_that_the_preset_does_not_have() {
        let text = touchpad().replace(
            "[[unit.selector]]\ntype = \"note\"\nchannel = 0\nid = 24",
            "[[unit.control]]\nslot = \"extra\"\ntype = \"note\"\nchannel = 0\nid = 40\n\n[[unit.mode]]\nname = \"mine\"\n[[unit.mode.bind]]\nslot = \"extra\"\naction = \"play\"\n\n[[unit.selector]]\ntype = \"note\"\nchannel = 0\nid = 24",
        );
        let map = Map::from_toml_str(&text).expect("extra slot");
        let unit = &map.units[0];
        assert_eq!(unit.controls.len(), 9);
        assert_eq!(unit.controls.last().unwrap().slot, "extra");
        let mine = unit
            .modes
            .iter()
            .find(|mode| mode.name == "mine")
            .expect("mine mode");
        assert!(matches!(
            mine.binds.last().and_then(|bind| bind.as_ref()).map(|bind| &bind.action),
            Some(Action::Play)
        ));
    }

    #[test]
    fn a_unit_mode_not_in_the_preset_order_is_appended() {
        let text = touchpad().replace(
            "[[unit.selector]]\ntype = \"note\"\nchannel = 0\nid = 24",
            "[[unit.mode]]\nname = \"mine\"\n\n[[unit.selector]]\ntype = \"note\"\nchannel = 0\nid = 24",
        );
        let map = Map::from_toml_str(&text).expect("unit mode extension");
        let unit = &map.units[0];
        assert_eq!(unit.modes.len(), 5);
        assert_eq!(unit.modes.last().unwrap().name, "mine");
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
    fn the_old_flat_bind_table_is_gone() {
        let error = Map::from_toml_str(
            "[[bind]]\ntype = \"note\"\nid = 1\naction = \"play\"\n",
        )
        .expect_err("flat binds are no longer a schema");
        assert!(
            error.message().contains("unknown field") || error.message().contains("bind"),
            "{error}"
        );
    }
}
