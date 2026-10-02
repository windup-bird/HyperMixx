//! Event → [`Command`] translation: a pure function plus the per-binding state it needs.
//!
//! Everything stateful lives in [`TranslateState`] (soft-takeover mirrors, relative-encoder
//! accumulators, held-note and toggle state), so the mapping is testable without hardware and the
//! caller owns the lifetime. [`translate`] returns commands in the order they should be sent;
//! [`MergeBuffer`] coalesces the floods a fast fader sweep produces.

use hypermixx_core::{
    Command, CueOp, DeckId, FaderTarget, FxSlotRef, LoopEditOp, LoopOp, NudgeOp, PhaseMode, StemOp,
    SyncOp, VinylOp,
};

use crate::map::{
    Action, BindingSpec, Curve, EventKind, FxAction, LoopAction, Map, RelMode, SyncAction,
    DEFAULT_NUDGE_SECONDS,
};
use crate::msg::Event;

/// How far one detent of a relative encoder moves the normalised position. 64 detents cover the
/// full travel — fast enough to feel like a fader, slow enough to place a value.
const REL_SCALE: f32 = 1.0 / 64.0;

/// A control within half a CC step of the mirror counts as *starting on* it. Without this a
/// physical fader at centre would send CC 64 (`64/127 = 0.5039`) and never match a mirror of
/// `0.5`, so unity-to-unity could never take over.
const PICKUP_DEADBAND: f32 = 0.5 / 127.0;

/// The state translation accumulates. One entry per binding, indexed in parallel with
/// [`Map::binds`].
#[derive(Clone, Debug)]
pub struct TranslateState {
    binds: Vec<BindState>,
}

#[derive(Clone, Debug)]
struct BindState {
    /// Normalised physical position, `0.0..=1.0`.
    norm: f32,
    /// Software mirror for soft-takeover, same normalised space.
    mirror: f32,
    /// Whether the physical control has picked up the software value.
    engaged: bool,
    /// Previous physical position, for crossing detection.
    prev: f32,
    /// A momentary control is down (note or CC).
    held: bool,
    /// Edge state for edge-triggered CC bindings.
    down: bool,
    /// Toggle state for `fx.toggle`.
    toggled: bool,
}

impl TranslateState {
    /// Builds state for `map`, seeding each soft-takeover mirror from the mixer's default position
    /// for the target (unity faders sit at the centre of travel, a cue send is full open, FX
    /// parameters start at zero). A front-end that knows better can overwrite with
    /// [`TranslateState::set_mirror`].
    pub fn new(map: &Map) -> Self {
        Self {
            binds: map.binds.iter().map(BindState::seeded).collect(),
        }
    }

    /// Overrides a binding's takeover mirror with an engine-known value (normalised). A no-op for
    /// an out-of-range index, so a caller iterating a shorter list cannot panic.
    pub fn set_mirror(&mut self, index: usize, norm: f32) {
        if let Some(state) = self.binds.get_mut(index) {
            state.mirror = norm.clamp(0.0, 1.0);
            state.prev = state.mirror;
        }
    }

    pub fn len(&self) -> usize {
        self.binds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.binds.is_empty()
    }
}

impl BindState {
    fn seeded(spec: &BindingSpec) -> Self {
        let mirror = default_norm(&spec.action);
        Self {
            norm: mirror,
            mirror,
            engaged: false,
            prev: mirror,
            held: false,
            down: false,
            toggled: false,
        }
    }
}

/// Where the engine leaves a control when the process starts. Used to seed soft-takeover.
fn default_norm(action: &Action) -> f32 {
    match action {
        // Bipolar faders and the tempo fader are unity/centred at 0.
        Action::Fader(target) if target.is_bipolar() => 0.5,
        Action::TempoFader => 0.5,
        // `simple_dj` opens the cue send fully.
        Action::Fader(FaderTarget::CueSend(_)) => 1.0,
        // Everything else (FX params) starts at the bottom of its domain.
        _ => 0.0,
    }
}

/// Translates one event into zero or more commands. Pure with respect to the map; only `state`
/// changes.
pub fn translate(map: &Map, event: &Event, state: &mut TranslateState) -> Vec<Command> {
    let mut out = Vec::new();
    match *event {
        Event::ControlChange {
            channel,
            controller,
            value,
        } => {
            for (index, spec) in map.binds.iter().enumerate() {
                if spec.kind != EventKind::Cc || !spec.matches(channel, controller) {
                    continue;
                }
                let state = &mut state.binds[index];
                let mut binding = BindingOut::new(spec, state, map, &mut out);
                binding.on_control(value);
            }
        }
        Event::NoteOn {
            channel,
            key,
            velocity,
        } if velocity > 0 => {
            for (index, spec) in map.binds.iter().enumerate() {
                if spec.kind != EventKind::Note || !spec.matches(channel, key) {
                    continue;
                }
                let state = &mut state.binds[index];
                if state.held {
                    continue;
                }
                state.held = true;
                let mut binding = BindingOut::new(spec, state, map, &mut out);
                binding.press();
            }
        }
        Event::NoteOn { channel, key, .. } | Event::NoteOff { channel, key, .. } => {
            for (index, spec) in map.binds.iter().enumerate() {
                if spec.kind != EventKind::Note || !spec.matches(channel, key) {
                    continue;
                }
                let state = &mut state.binds[index];
                if !state.held {
                    continue;
                }
                state.held = false;
                let mut binding = BindingOut::new(spec, state, map, &mut out);
                binding.release();
            }
        }
        Event::PitchBend { channel, value } => {
            let norm = (f32::from(value) + 8192.0) / 16384.0;
            for (index, spec) in map.binds.iter().enumerate() {
                if spec.kind != EventKind::Bend
                    || !spec.channel.is_none_or(|wanted| wanted == channel)
                {
                    continue;
                }
                let state = &mut state.binds[index];
                let mut binding = BindingOut::new(spec, state, map, &mut out);
                if binding.accepts(norm) {
                    binding.emit_continuous(norm);
                }
            }
        }
    }
    out
}

/// Applies one binding's action against one event, then writes into the output vector. Kept as a
/// short-lived borrow of the binding's state so [`translate`]'s loops stay flat.
struct BindingOut<'a> {
    spec: &'a BindingSpec,
    state: &'a mut BindState,
    map: &'a Map,
    out: &'a mut Vec<Command>,
}

impl<'a> BindingOut<'a> {
    fn new(
        spec: &'a BindingSpec,
        state: &'a mut BindState,
        map: &'a Map,
        out: &'a mut Vec<Command>,
    ) -> Self {
        Self {
            spec,
            state,
            map,
            out,
        }
    }

    /// Handles a CC value: continuous actions read it as a position (absolute or relative), button
    /// actions as an edge.
    fn on_control(&mut self, value: u8) {
        if self.spec.mode != RelMode::Abs {
            let delta = relative_delta(self.spec.mode, value);
            let norm = (self.state.norm + delta as f32 * REL_SCALE).clamp(0.0, 1.0);
            self.state.norm = norm;
            // A relative encoder is endless: there is nothing to pick up.
            self.state.engaged = true;
            self.state.mirror = norm;
            // A wheel is a *direction*, not a position: one tick forward bends one way, one tick back
            // the other way. It also has no release edge, so the bend has to end on its own — hence
            // the duration default (`seconds` in the map still wins). Turning faster sends more
            // ticks, and each one re-arms that timer, so the bend lasts as long as the wheel moves.
            // A `vinyl` wheel sends the same shape, but the *deck* decides whether a tick scrubs
            // (platter held) or bends (nobody on it) — the mapping cannot know, and must not guess.
            if let Action::Vinyl { frames_per_tick } = self.spec.action.clone() {
                if delta != 0 {
                    let Some(deck_id) = self.spec.deck else { return };
                    self.out.push(Command::Vinyl {
                        deck_id,
                        op: VinylOp::Turn {
                            ticks: i32::from(delta.signum()),
                            frames_per_tick,
                        },
                    });
                }
                return;
            }
            if let Action::Nudge { delta: step, seconds } = self.spec.action.clone() {
                if delta != 0 {
                    let Some(deck_id) = self.spec.deck else { return };
                    // `Bend`, not `Start`: each message *adds* to the bend (so turning faster bends
                    // harder, up to the engine's clamp) instead of re-stating it (which would only
                    // make it last longer). The magnitude is the tick count, so an encoder that
                    // reports ±3 per detent counts three times.
                    self.out.push(Command::Nudge {
                        deck_id,
                        op: NudgeOp::Bend {
                            delta: step * f32::from(delta),
                            seconds: seconds.unwrap_or(DEFAULT_NUDGE_SECONDS),
                        },
                    });
                }
                return;
            }
            self.emit_continuous(norm);
            return;
        }
        let norm = f32::from(value) / 127.0;
        self.state.norm = norm;
        if self.spec.action.is_continuous() {
            if self.accepts(norm) {
                self.emit_continuous(norm);
            }
        } else {
            let down = value > 0;
            if down && !self.state.down {
                self.state.down = true;
                self.press();
            } else if !down && self.state.down {
                self.state.down = false;
                self.release();
            }
        }
    }

    /// Soft-takeover: an absolute control only takes over once the physical position crosses the
    /// stored software value (or starts exactly on it). Until then its movement is discarded.
    fn accepts(&mut self, norm: f32) -> bool {
        if self.state.engaged {
            self.state.prev = norm;
            self.state.mirror = norm;
            return true;
        }
        let mirror = self.state.mirror;
        let crossed = (self.state.prev < mirror && norm >= mirror)
            || (self.state.prev > mirror && norm <= mirror);
        let starts_on = (norm - mirror).abs() <= PICKUP_DEADBAND;
        self.state.prev = norm;
        if crossed || starts_on {
            self.state.engaged = true;
            self.state.mirror = norm;
            true
        } else {
            false
        }
    }

    fn emit_continuous(&mut self, norm: f32) {
        let deck = self.spec.deck;
        match self.spec.action.clone() {
            Action::Fader(target) => self.out.push(Command::SetFader {
                target,
                value: command_value(self.spec, norm),
            }),
            Action::TempoFader => {
                let Some(deck_id) = deck else { return };
                // The deck owns the range; the mapping only ever sends the fader position.
                self.out.push(Command::SetTempoFader {
                    deck_id,
                    position: norm * 2.0 - 1.0,
                });
            }
            Action::Fx(FxAction::Param { chain, fx, param }) => {
                let Some(index) = self.map.fx_slot(chain, &fx) else {
                    return;
                };
                self.out.push(Command::SetFxParam {
                    slot: FxSlotRef { chain, index },
                    name: param,
                    value: command_value(self.spec, norm),
                });
            }
            _ => {}
        }
    }

    fn press(&mut self) {
        let Some(deck) = self.spec.deck else {
            // Non-deck actions below (FX) still need to run; only deck-scoped ones bail.
            self.press_deckless();
            return;
        };
        match self.spec.action.clone() {
            Action::Play => self.out.push(Command::TogglePlay { deck_id: deck }),
            Action::Cue => self.out.push(Command::Cue {
                deck_id: deck,
                op: CueOp::Hold,
            }),
            // The platter sensor is a note: pressing it is a hand on the record.
            Action::Vinyl { .. } => self.out.push(Command::Vinyl {
                deck_id: deck,
                op: VinylOp::Touch,
            }),
            Action::CueSmart => self.out.push(Command::Cue {
                deck_id: deck,
                op: CueOp::Smart,
            }),
            // Latching, so the engine flips its own state rather than the mapping holding one.
            Action::StemMute { stem } => self.out.push(Command::Stem {
                deck_id: deck,
                op: StemOp::ToggleMute { stem },
            }),
            Action::StemSolo { stem } => self.out.push(Command::Stem {
                deck_id: deck,
                op: StemOp::ToggleSolo { stem },
            }),
            Action::BeatJump { beats } => self.out.push(Command::BeatJump {
                deck_id: deck,
                beats,
            }),
            Action::Nudge { delta, seconds } => self.out.push(Command::Nudge {
                deck_id: deck,
                op: NudgeOp::Start { delta, seconds },
            }),
            Action::Loop(op) => self.out.push(Command::Loop {
                deck_id: deck,
                op: loop_op(op),
            }),
            Action::Sync(op) => self.out.push(Command::Sync {
                deck_id: deck,
                op: sync_op(op),
            }),
            _ => self.press_deckless(),
        }
    }

    fn press_deckless(&mut self) {
        if let Action::Fx(action) = self.spec.action.clone() {
            self.fx_press(action);
        }
    }

    fn fx_press(&mut self, action: FxAction) {
        match action {
            FxAction::On { chain, fx } => {
                if let Some(slot) = self.slot(chain, &fx) {
                    self.out.push(Command::SetFxEnabled {
                        slot,
                        enabled: true,
                    });
                }
            }
            FxAction::Off { chain, fx } => {
                if let Some(slot) = self.slot(chain, &fx) {
                    self.out.push(Command::SetFxEnabled {
                        slot,
                        enabled: false,
                    });
                }
            }
            FxAction::Toggle { chain, fx } => {
                if let Some(slot) = self.slot(chain, &fx) {
                    let enabled = !self.state.toggled;
                    self.state.toggled = enabled;
                    self.out.push(Command::SetFxEnabled { slot, enabled });
                }
            }
            FxAction::Pad { chain, fx } => {
                if let Some(slot) = self.slot(chain, &fx) {
                    self.out.push(Command::PadPress { slot });
                }
            }
            FxAction::Trigger { chain, fx } => {
                if let Some(slot) = self.slot(chain, &fx) {
                    self.out.push(Command::FxTrigger { slot });
                }
            }
            // Parameter changes are continuous, not edge-triggered.
            FxAction::Param { .. } => {}
        }
    }

    fn release(&mut self) {
        let Some(deck) = self.spec.deck else {
            self.release_deckless();
            return;
        };
        match self.spec.action.clone() {
            // A timed bend releases itself; the note-up edge must not cut it short.
            Action::Nudge { seconds: None, .. } => self.out.push(Command::Nudge {
                deck_id: deck,
                op: NudgeOp::Stop,
            }),
            Action::Cue => self.out.push(Command::Cue {
                deck_id: deck,
                op: CueOp::Back,
            }),
            Action::Vinyl { .. } => self.out.push(Command::Vinyl {
                deck_id: deck,
                op: VinylOp::Release,
            }),
            _ => self.release_deckless(),
        }
    }

    fn release_deckless(&mut self) {
        if let Action::Fx(FxAction::Pad { chain, fx }) = self.spec.action.clone() {
            if let Some(slot) = self.slot(chain, &fx) {
                self.out.push(Command::PadRelease { slot });
            }
        }
    }

    fn slot(&self, chain: hypermixx_core::FxChainId, fx: &str) -> Option<FxSlotRef> {
        self.map
            .fx_slot(chain, fx)
            .map(|index| FxSlotRef { chain, index })
    }
}

/// Applies the binding's curve and value range to a normalised position.
fn command_value(spec: &BindingSpec, norm: f32) -> f32 {
    let shaped = match spec.curve {
        Curve::Linear => norm,
        Curve::Sharp if spec.action.is_bipolar() => {
            // Shape the bipolar position so the centre stays put.
            let bipolar = norm * 2.0 - 1.0;
            ((bipolar.signum() * bipolar * bipolar) + 1.0) / 2.0
        }
        Curve::Sharp => norm * norm,
    };
    spec.min + shaped * (spec.max - spec.min)
}

/// The signed movement a relative-encoder CC encodes.
///
/// Vendor conventions differ, which is why the mode is per binding:
/// - `Rel1`: offset binary around 64 (`value - 64`, so 64 is no movement);
/// - `Rel2`: two's-complement around 64 (1..63 up, 65..127 down);
/// - `Rel3`: direction only, magnitude ignored (1..63 up, 65..127 down).
fn relative_delta(mode: RelMode, value: u8) -> i16 {
    match mode {
        RelMode::Abs => 0,
        RelMode::Rel1 => i16::from(value) - 64,
        RelMode::Rel2 => {
            if value < 0x40 {
                i16::from(value)
            } else {
                i16::from(value) - 0x80
            }
        }
        RelMode::Rel3 => match value.cmp(&64) {
            std::cmp::Ordering::Less => 1,
            std::cmp::Ordering::Greater => -1,
            std::cmp::Ordering::Equal => 0,
        },
    }
}

fn loop_op(op: LoopAction) -> LoopOp {
    match op {
        LoopAction::In => LoopOp::In,
        LoopAction::Out => LoopOp::Out,
        LoopAction::Exit => LoopOp::Exit,
        LoopAction::Cancel => LoopOp::Cancel,
        LoopAction::Halve => LoopOp::Edit(LoopEditOp::Halve),
        LoopAction::Double => LoopOp::Edit(LoopEditOp::Double),
        LoopAction::Beats(beats) => LoopOp::Beats(beats),
    }
}

/// One-button sync defaults to the PI controller: `pid` converges and then holds with the
/// correction at zero, which is what a single mapped key should do. A front-end that wants a
/// different mode can extend the registry.
fn sync_op(op: SyncAction) -> SyncOp {
    match op {
        SyncAction::Tempo => SyncOp::Tempo,
        SyncAction::Phase => SyncOp::Phase {
            mode: PhaseMode::Pid,
            t_seconds: None,
        },
        SyncAction::PhaseLock => SyncOp::PhaseLock {
            mode: PhaseMode::Pid,
            t_seconds: None,
        },
        SyncAction::TempoLock => SyncOp::TempoLock,
        SyncAction::Leader => SyncOp::SetLeader,
        SyncAction::Unlock => SyncOp::Unlock,
    }
}

/// A key identifying a command whose latest value supersedes earlier ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeKey {
    Fader(FaderTarget),
    Tempo(DeckId),
    FxParam(FxSlotRef, String),
}

impl MergeKey {
    fn of(command: &Command) -> Option<Self> {
        match command {
            Command::SetFader { target, .. } => Some(MergeKey::Fader(*target)),
            Command::SetTempo { deck_id, .. } | Command::SetTempoFader { deck_id, .. } => {
                Some(MergeKey::Tempo(*deck_id))
            }
            Command::SetFxParam { slot, name, .. } => Some(MergeKey::FxParam(*slot, name.clone())),
            _ => None,
        }
    }
}

/// Latest-wins coalescing for continuous commands.
///
/// A fader sweep can emit thousands of CCs a second. Every intermediate value is discarded in
/// favour of the newest for the same target; the **final** value is always delivered. This is a
/// correctness property, not rate limiting: the producer drains at block boundaries and has no use
/// for a backlog of superseded positions.
#[derive(Default)]
pub struct MergeBuffer {
    entries: Vec<(Option<MergeKey>, Command)>,
}

impl MergeBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Adds a command. A mergeable command replaces the pending one with the same key, keeping its
    /// position in the order.
    pub fn push(&mut self, command: Command) {
        match MergeKey::of(&command) {
            Some(key) => {
                if let Some(entry) = self
                    .entries
                    .iter_mut()
                    .find(|(existing, _)| existing.as_ref() == Some(&key))
                {
                    entry.1 = command;
                } else {
                    self.entries.push((Some(key), command));
                }
            }
            None => self.entries.push((None, command)),
        }
    }

    /// Removes and returns every pending command, in order.
    pub fn flush(&mut self) -> Vec<Command> {
        self.entries.drain(..).map(|(_, command)| command).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{DEFAULT_NUDGE_STEP, DEFAULT_VINYL_FRAMES_PER_TICK};
    use crate::map::Map;
    use hypermixx_core::Stem;

    fn map(toml: &str) -> Map {
        Map::from_toml_str(toml).expect("test map should parse")
    }

    fn cc(channel: u8, controller: u8, value: u8) -> Event {
        Event::ControlChange {
            channel,
            controller,
            value,
        }
    }

    fn note(channel: u8, key: u8, on: bool) -> Event {
        if on {
            Event::NoteOn {
                channel,
                key,
                velocity: 100,
            }
        } else {
            Event::NoteOff {
                channel,
                key,
                velocity: 0,
            }
        }
    }

    fn fader(command: &Command) -> Option<(FaderTarget, f32)> {
        match command {
            Command::SetFader { target, value } => Some((*target, *value)),
            _ => None,
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn a_note_button_fires_on_press_only() {
        let map = map(r#"
[[bind]]
type = "note"
channel = 0
id = 48
deck = 1
action = "play"
"#);
        let mut state = TranslateState::new(&map);
        let commands = translate(&map, &note(0, 48, true), &mut state);
        assert!(matches!(
            commands.as_slice(),
            [Command::TogglePlay { deck_id: 1 }]
        ));
        // The release must not re-fire.
        assert!(translate(&map, &note(0, 48, false), &mut state).is_empty());
        // A different key or channel does nothing.
        assert!(translate(&map, &note(0, 49, true), &mut state).is_empty());
        assert!(translate(&map, &note(1, 48, true), &mut state).is_empty());
    }

    /// A stem button latches: the engine flips its own state, so the mapping never has to know it.
    #[test]
    fn a_stem_button_emits_a_toggle_and_nothing_on_release() {
        let map = map(r#"
[[bind]]
type = "note"
id = 20
deck = 0
action = "stem.vocals.mute"

[[bind]]
type = "note"
id = 21
deck = 1
action = "stem.bass.solo"
"#);
        let mut state = TranslateState::new(&map);
        let commands = translate(&map, &note(0, 20, true), &mut state);
        assert!(matches!(
            commands.as_slice(),
            [Command::Stem {
                deck_id: 0,
                op: StemOp::ToggleMute { stem: Stem::Vocals }
            }]
        ));
        // Latching: the release edge must not fire a second toggle.
        assert!(translate(&map, &note(0, 20, false), &mut state).is_empty());
        // A second press is another toggle, not a state the mapping tracks.
        let again = translate(&map, &note(0, 20, true), &mut state);
        assert_eq!(again.len(), 1);
        // The binding's deck goes with the action.
        let commands = translate(&map, &note(0, 21, true), &mut state);
        assert!(matches!(
            commands.as_slice(),
            [Command::Stem {
                deck_id: 1,
                op: StemOp::ToggleSolo { stem: Stem::Bass }
            }]
        ));
    }

    /// A per-stem fader is an ordinary fader: same soft takeover, different target.
    #[test]
    fn a_stem_fader_takes_the_soft_takeover_path_to_the_stem_target() {
        let map = map(r#"
[[bind]]
type = "cc"
mode = "abs"
id = 30
deck = 1
action = "fader.stem.other"
"#);
        let mut state = TranslateState::new(&map);
        // A first value only registers a position; the takeover needs a crossing, so approach the
        // mirror from above and then cross it (the same two-message shape as `fader.flow`).
        assert!(translate(&map, &cc(0, 30, 127), &mut state).is_empty());
        let commands = translate(&map, &cc(0, 30, 0), &mut state);
        let (target, value) = fader(&commands[0]).expect("SetFader");
        assert_eq!(
            target,
            FaderTarget::Stem {
                deck: 1,
                stem: Stem::Other
            }
        );
        assert!(close(value, -1.0), "value {value}");
    }

    #[test]
    fn soft_takeover_waits_for_the_control_to_cross_the_mirror() {
        let map = map(r#"
[[bind]]
type = "cc"
mode = "abs"
id = 7
deck = 0
action = "fader.flow"
"#);
        let mut state = TranslateState::new(&map);
        // Mirror starts at unity (norm 0.5). A value away from the mirror that has not crossed it
        // is discarded.
        assert!(translate(&map, &cc(0, 7, 100), &mut state).is_empty());
        // Moving below the mirror crosses it and takes over: the received value is emitted.
        let commands = translate(&map, &cc(0, 7, 0), &mut state);
        let (target, value) = fader(&commands[0]).expect("SetFader");
        assert_eq!(target, FaderTarget::Flow(0));
        assert!(close(value, -1.0), "value {value}");
        // Once engaged, every value is delivered.
        let commands = translate(&map, &cc(0, 7, 127), &mut state);
        let (_, value) = fader(&commands[0]).unwrap();
        assert!(close(value, 1.0), "value {value}");
    }

    #[test]
    fn a_centred_fader_takes_over_a_unity_mirror() {
        let map = map(r#"
[[bind]]
type = "cc"
mode = "abs"
id = 7
deck = 0
action = "fader.flow"
"#);
        let mut state = TranslateState::new(&map);
        // The engine fader is at unity (norm 0.5); the physical fader is at centre, which is CC 64
        // (norm 0.5039). The half-step deadband must let it take over rather than require the user
        // to nudge past a mirror they are already sitting on.
        let commands = translate(&map, &cc(0, 7, 64), &mut state);
        let (_, value) = fader(&commands[0]).expect("a centred fader should engage unity");
        // CC 64 quantises to norm 0.5039, so the value is a hair above unity — within a CC step.
        assert!(value.abs() < 0.01, "value {value}");
    }

    #[test]
    fn starting_exactly_on_the_mirror_engages_immediately() {
        let map = map(r#"
[[bind]]
type = "cc"
mode = "abs"
id = 7
deck = 0
action = "fader.flow"
"#);
        let mut state = TranslateState::new(&map);
        // norm 0.5 == mirror: the control starts where the software is, so it owns it at once.
        let commands = translate(&map, &cc(0, 7, 64), &mut state);
        // 64/127 ~= 0.5039, so this is not exactly on the mirror; use a mirror-seeded control.
        let _ = commands;
        state.set_mirror(0, 64.0 / 127.0);
        let commands = translate(&map, &cc(0, 7, 64), &mut state);
        assert_eq!(commands.len(), 1, "starting on the mirror should engage");
    }

    #[test]
    fn relative_encoders_accumulate_without_takeover() {
        let map = map(r#"
[[bind]]
type = "cc"
mode = "rel2"
id = 10
deck = 0
action = "tempofader"
"#);
        let mut state = TranslateState::new(&map);
        // One detent up moves the normalised position by 1/64; the fader is bipolar, so a detent
        // is +2/64 of the travel.
        let commands = translate(&map, &cc(0, 10, 1), &mut state);
        match &commands[0] {
            Command::SetTempoFader { deck_id: 0, position } => {
                assert!(close(*position, 2.0 / 64.0), "position {position}");
            }
            other => panic!("expected SetTempoFader, got {other:?}"),
        }
        // One detent down brings it back to centre.
        let commands = translate(&map, &cc(0, 10, 127), &mut state);
        match &commands[0] {
            Command::SetTempoFader { position, .. } => assert!(close(*position, 0.0), "position {position}"),
            other => panic!("expected SetTempoFader, got {other:?}"),
        }
    }

    #[test]
    fn relative_modes_decode_the_documented_wraparounds() {
        assert_eq!(relative_delta(RelMode::Rel1, 64), 0);
        assert_eq!(relative_delta(RelMode::Rel1, 65), 1);
        assert_eq!(relative_delta(RelMode::Rel1, 63), -1);
        assert_eq!(relative_delta(RelMode::Rel2, 1), 1);
        assert_eq!(relative_delta(RelMode::Rel2, 127), -1);
        assert_eq!(relative_delta(RelMode::Rel3, 1), 1);
        assert_eq!(relative_delta(RelMode::Rel3, 127), -1);
        assert_eq!(relative_delta(RelMode::Rel3, 64), 0);
    }

    /// A wheel is a *direction*, not a position: one tick clockwise bends forward, one tick the
    /// other way bends back. It also has no release edge, so each tick starts a bend that ends on its
    /// own — the `seconds` default is what makes that possible.
    #[test]
    fn a_relative_wheel_nudges_both_ways_and_releases_itself() {
        let map = map(r#"
[[bind]]
type = "cc"
mode = "rel1"
id = 20
deck = 0
action = "nudge"

[[bind]]
type = "cc"
mode = "rel1"
id = 21
deck = 0
action = "nudge"
step = 0.10
seconds = 0.5

[[bind]]
type = "cc"
id = 22
deck = 0
action = "nudge"
step = 0.05
"#);
        let mut state = TranslateState::new(&map);
        // Rel1 is centred on 64, so 65 and 63 are one tick each way. The defaults are the speed and
        // the self-release.
        assert!(matches!(
            translate(&map, &cc(0, 20, 65), &mut state).as_slice(),
            [Command::Nudge { op: NudgeOp::Bend { delta, seconds }, .. }]
                if close(*delta, DEFAULT_NUDGE_STEP)
                    && close(*seconds as f32, DEFAULT_NUDGE_SECONDS as f32)
        ));
        assert!(matches!(
            translate(&map, &cc(0, 20, 63), &mut state).as_slice(),
            [Command::Nudge { op: NudgeOp::Bend { delta, .. }, .. }]
                if close(*delta, -DEFAULT_NUDGE_STEP)
        ));
        // A message that reports several ticks counts several times: 67 is three ticks forward.
        assert!(matches!(
            translate(&map, &cc(0, 20, 67), &mut state).as_slice(),
            [Command::Nudge { op: NudgeOp::Bend { delta, seconds }, .. }]
                if close(*delta, 3.0 * DEFAULT_NUDGE_STEP)
                    && close(*seconds as f32, DEFAULT_NUDGE_SECONDS as f32)
        ));
        // Sitting on the centre value is not a tick at all.
        assert!(translate(&map, &cc(0, 20, 64), &mut state).is_empty());
        // A binding that names its own speed and time keeps both.
        assert!(matches!(
            translate(&map, &cc(0, 21, 65), &mut state).as_slice(),
            [Command::Nudge { op: NudgeOp::Bend { delta, seconds }, .. }]
                if close(*delta, 0.10) && close(*seconds as f32, 0.5)
        ));
        // An *absolute* control has no direction and does have edges, so it stays a held bend.
        assert!(matches!(
            translate(&map, &cc(0, 22, 64), &mut state).as_slice(),
            [Command::Nudge { op: NudgeOp::Start { delta, seconds: None }, .. }]
                if close(*delta, 0.05)
        ));
        assert!(matches!(
            translate(&map, &cc(0, 22, 0), &mut state).as_slice(),
            [Command::Nudge { op: NudgeOp::Stop, .. }]
        ));
    }

    /// A vinyl wheel is bound twice on a real controller: a note for the platter sensor and a
    /// relative CC for the wheel. Both carry the same action, and the *deck* decides what a turn
    /// means.
    #[test]
    fn a_vinyl_wheel_sends_platter_edges_and_signed_turns() {
        let map = map(r#"
[[bind]]
type = "note"
id = 80
deck = 0
action = "vinyl"

[[bind]]
type = "cc"
mode = "rel1"
id = 6
deck = 0
action = "vinyl"

[[bind]]
type = "cc"
mode = "rel1"
id = 7
deck = 0
action = "vinyl"
step = 1323
"#);
        let mut state = TranslateState::new(&map);
        // The platter sensor: press = a hand on the record, release = letting go.
        assert!(matches!(
            translate(&map, &note(0, 80, true), &mut state).as_slice(),
            [Command::Vinyl { op: VinylOp::Touch, .. }]
        ));
        assert!(matches!(
            translate(&map, &note(0, 80, false), &mut state).as_slice(),
            [Command::Vinyl { op: VinylOp::Release, .. }]
        ));
        // The wheel: one tick each way, in ticks (the deck converts, because only the deck knows
        // whether the platter is held).
        assert!(matches!(
            translate(&map, &cc(0, 6, 65), &mut state).as_slice(),
            [Command::Vinyl { op: VinylOp::Turn { ticks, frames_per_tick }, .. }]
                if *ticks == 1 && close(*frames_per_tick, DEFAULT_VINYL_FRAMES_PER_TICK)
        ));
        assert!(matches!(
            translate(&map, &cc(0, 6, 63), &mut state).as_slice(),
            [Command::Vinyl { op: VinylOp::Turn { ticks, .. }, .. }] if *ticks == -1
        ));
        // A wheel with more detents per revolution says so with `step`.
        assert!(matches!(
            translate(&map, &cc(0, 7, 65), &mut state).as_slice(),
            [Command::Vinyl { op: VinylOp::Turn { frames_per_tick, .. }, .. }]
                if close(*frames_per_tick, 1323.0)
        ));
        // The centre value is not a turn, and a note release on a *note* binding is not a turn
        // either: what a binding sends follows the event kind it was declared for.
        assert!(translate(&map, &cc(0, 6, 64), &mut state).is_empty());
    }

    /// Every button edge in one place: a momentary hold pairs press/release, a smart cue fires
    /// once, and a timed nudge ignores its release.
    #[test]
    fn button_edges_pair_on_press_and_release() {
        let map = map(r#"
[[bind]]
type = "note"
id = 60
deck = 0
action = "nudge"
step = 0.05

[[bind]]
type = "note"
id = 61
deck = 0
action = "nudge"
step = 0.05
seconds = 0.5

[[bind]]
type = "note"
id = 62
deck = 0
action = "cue"

[[bind]]
type = "note"
id = 63
deck = 0
action = "cue.smart"
"#);
        let mut state = TranslateState::new(&map);
        let edge = |state: &mut TranslateState, key, down| translate(&map, &note(0, key, down), state);

        // A plain nudge is momentary: press bends, release lets go.
        assert!(matches!(
            edge(&mut state, 60, true).as_slice(),
            [Command::Nudge { op: NudgeOp::Start { delta, seconds: None }, .. }] if close(*delta, 0.05)
        ));
        assert!(matches!(
            edge(&mut state, 60, false).as_slice(),
            [Command::Nudge { op: NudgeOp::Stop, .. }]
        ));
        // A timed nudge releases itself; the note-up edge must be silent.
        assert!(matches!(
            edge(&mut state, 61, true).as_slice(),
            [Command::Nudge { op: NudgeOp::Start { seconds: Some(s), .. }, .. }] if close(*s as f32, 0.5)
        ));
        assert!(edge(&mut state, 61, false).is_empty());
        // The cue button is a held pair: down cues the playhead (or restarts from the cue point)
        // and plays; up returns to the cue point and pauses.
        assert!(matches!(
            edge(&mut state, 62, true).as_slice(),
            [Command::Cue { op: CueOp::Hold, .. }]
        ));
        assert!(matches!(
            edge(&mut state, 62, false).as_slice(),
            [Command::Cue { op: CueOp::Back, .. }]
        ));
        // A smart cue fires once; its release is silent.
        assert!(matches!(
            edge(&mut state, 63, true).as_slice(),
            [Command::Cue { op: CueOp::Smart, .. }]
        ));
        assert!(edge(&mut state, 63, false).is_empty());
    }

    #[test]
    fn loop_and_sync_buttons_map_to_their_command_families() {
        let map = map(r#"
[[bind]]
type = "note"
id = 1
deck = 0
action = "loop.beat8"

[[bind]]
type = "note"
id = 2
deck = 0
action = "loop.halve"

[[bind]]
type = "note"
id = 3
deck = 0
action = "sync.tempo"
"#);
        let mut state = TranslateState::new(&map);
        assert!(matches!(
            translate(&map, &note(0, 1, true), &mut state).as_slice(),
            [Command::Loop {
                op: LoopOp::Beats(8),
                ..
            }]
        ));
        assert!(matches!(
            translate(&map, &note(0, 2, true), &mut state).as_slice(),
            [Command::Loop {
                op: LoopOp::Edit(LoopEditOp::Halve),
                ..
            }]
        ));
        assert!(matches!(
            translate(&map, &note(0, 3, true), &mut state).as_slice(),
            [Command::Sync {
                op: SyncOp::Tempo,
                ..
            }]
        ));
    }

    #[test]
    fn fx_actions_need_a_resolved_slot_to_emit() {
        let map = map(r#"
[[bind]]
type = "note"
id = 20
action = "fx.toggle"
chain = "deck0"
fx = "filter"
"#);
        let mut map = map;
        let mut state = TranslateState::new(&map);
        // Unresolved: the binding is silently skipped rather than sending a bogus index.
        assert!(translate(&map, &note(0, 20, true), &mut state).is_empty());
        translate(&map, &note(0, 20, false), &mut state);

        map.resolve_fx(hypermixx_core::FxChainId::Deck(0), "filter", 2);
        let commands = translate(&map, &note(0, 20, true), &mut state);
        assert!(matches!(
            commands.as_slice(),
            [Command::SetFxEnabled {
                slot: FxSlotRef {
                    chain: hypermixx_core::FxChainId::Deck(0),
                    index: 2,
                },
                enabled: true,
            }]
        ));
        // Second press toggles it back off.
        translate(&map, &note(0, 20, false), &mut state);
        let commands = translate(&map, &note(0, 20, true), &mut state);
        assert!(matches!(
            commands.as_slice(),
            [Command::SetFxEnabled { enabled: false, .. }]
        ));
    }

    #[test]
    fn an_fx_param_sweep_maps_the_unit_domain() {
        let map = map(r#"
[[bind]]
type = "cc"
mode = "abs"
id = 74
action = "fx.param"
chain = "master"
fx = "filter"
param = "value"
"#);
        let mut map = map;
        map.resolve_fx(hypermixx_core::FxChainId::Master, "filter", 0);
        let mut state = TranslateState::new(&map);
        // FX params start at 0; the control picks up when it reports the bottom of its travel.
        let commands = translate(&map, &cc(0, 74, 0), &mut state);
        match &commands[0] {
            Command::SetFxParam { value, .. } => assert!(close(*value, 0.0), "value {value}"),
            other => panic!("expected SetFxParam, got {other:?}"),
        }
        // Then it tracks the value into the unit domain.
        let commands = translate(&map, &cc(0, 74, 64), &mut state);
        match &commands[0] {
            Command::SetFxParam { name, value, .. } => {
                assert_eq!(name, "value");
                assert!(close(*value, 64.0 / 127.0), "value {value}");
            }
            other => panic!("expected SetFxParam, got {other:?}"),
        }
    }

    #[test]
    fn pitch_bend_is_a_continuous_control() {
        let map = map(r#"
[[bind]]
type = "bend"
id = 0
deck = 0
action = "fader.flow"
"#);
        let mut state = TranslateState::new(&map);
        // Bend centre is unity: it starts on the mirror and takes over.
        assert_eq!(
            translate(
                &map,
                &Event::PitchBend {
                    channel: 0,
                    value: 0,
                },
                &mut state,
            )
            .len(),
            1,
            "centre should engage the mirror"
        );
        // Moving to the bottom then closes the fader.
        let commands = translate(
            &map,
            &Event::PitchBend {
                channel: 0,
                value: -8192,
            },
            &mut state,
        );
        let (_, value) = fader(&commands[0]).unwrap();
        assert!(close(value, -1.0), "value {value}");
    }

    #[test]
    fn channel_filters_and_wildcards_both_work() {
        let map = map(r#"
[[bind]]
type = "note"
id = 5
deck = 0
action = "play"
"#);
        let mut state = TranslateState::new(&map);
        assert_eq!(translate(&map, &note(9, 5, true), &mut state).len(), 1);
        assert_eq!(translate(&map, &note(9, 6, true), &mut state).len(), 0);
    }

    #[test]
    fn merge_buffer_keeps_only_the_latest_value_per_target() {
        let mut buffer = MergeBuffer::new();
        for value in [0.1, 0.2, 0.3, 0.9] {
            buffer.push(Command::SetFader {
                target: FaderTarget::Flow(0),
                value,
            });
        }
        buffer.push(Command::SetFader {
            target: FaderTarget::Flow(1),
            value: 0.5,
        });
        buffer.push(Command::Play { deck_id: 0 });
        let flushed = buffer.flush();
        assert_eq!(flushed.len(), 3, "two faders plus one plain command");
        // Order is preserved and the fader carries its final value.
        let (_, value) = fader(&flushed[0]).unwrap();
        assert!(close(value, 0.9), "value {value}");
        assert!(matches!(flushed[1], Command::SetFader { target: FaderTarget::Flow(1), .. }));
        assert!(matches!(flushed[2], Command::Play { deck_id: 0 }));
        assert!(buffer.is_empty());
    }

    #[test]
    fn default_mirrors_match_the_mixer_defaults() {
        let map = map(r#"
[[bind]]
type = "cc"
id = 1
deck = 0
action = "fader.flow"

[[bind]]
type = "cc"
id = 2
deck = 0
action = "fader.cuesend"

[[bind]]
type = "cc"
id = 3
action = "fader.master"
"#);
        let state = TranslateState::new(&map);
        assert!(close(state.binds[0].mirror, 0.5), "flow is unity");
        assert!(close(state.binds[1].mirror, 1.0), "cue send opens fully");
        assert!(close(state.binds[2].mirror, 0.5), "master is unity");
    }
}
