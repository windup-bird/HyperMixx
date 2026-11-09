//! Event → [`Command`] translation: one binding at a time, plus the state that binding needs.
//!
//! The router owns the mode state and picks the binding; [`apply`] applies it. Everything stateful
//! lives in [`BindState`] (soft-takeover mirrors, relative-encoder accumulators, held-note and
//! toggle state), so the mapping is testable without hardware. [`MergeBuffer`] coalesces the floods
//! a fast fader sweep produces.

use hypermixx_core::{
    BrowserAction, Command, CueOp, DeckId, FaderTarget, FxSlotRef, LoopEditOp, LoopOp, NudgeOp,
    StemOp, SyncOp, VinylOp,
};

use crate::map::{
    Action, BindingSpec, Curve, Edge, Encoding, EventKind, FxAction, FxParamRef, LoopAction, Map,
    SyncAction, DEFAULT_NUDGE_SECONDS,
};
use crate::msg::Event;

/// How far one detent of a relative encoder moves the normalised position. 64 detents cover the
/// full travel — fast enough to feel like a fader, slow enough to place a value.
pub(crate) const REL_SCALE: f32 = 1.0 / 64.0;

/// A control within half a CC step of the mirror counts as *starting on* it. Without this a
/// physical fader at centre would send CC 64 (`64/127 = 0.5039`) and never match a mirror of
/// `0.5`, so unity-to-unity could never take over.
const PICKUP_DEADBAND: f32 = 0.5 / 127.0;

/// The state one physical endpoint accumulates. The router owns one per endpoint, and it
/// deliberately survives mode changes: a knob keeps its physical position and a held pad keeps
/// its edges even when the action behind it is swapped.
#[derive(Clone, Debug)]
pub struct BindState {
    /// Normalised physical position, `0.0..=1.0`.
    pub norm: f32,
    /// Software mirror for soft-takeover, same normalised space.
    pub mirror: f32,
    /// Whether the physical control has picked up the software value.
    pub engaged: bool,
    /// Previous physical position, for crossing detection.
    pub prev: f32,
    /// A momentary control is down (note or CC).
    pub held: bool,
    /// Edge state for edge-triggered CC bindings.
    pub down: bool,
    /// Most recent MSB of a 14-bit CC pair, waiting for its LSB.
    pub cc14_msb: Option<u8>,
}

impl Default for BindState {
    fn default() -> Self {
        Self {
            norm: 0.0,
            mirror: 0.0,
            engaged: false,
            prev: 0.0,
            held: false,
            down: false,
            cc14_msb: None,
        }
    }
}

impl BindState {
    /// Seeds a control from the action it starts on: unity faders sit at the centre of travel, a
    /// cue send is full open, FX parameters start at the bottom of their domain.
    pub fn seeded(spec: Option<&BindingSpec>) -> Self {
        let mirror = spec.map_or(0.0, default_norm);
        Self {
            norm: mirror,
            mirror,
            engaged: false,
            prev: mirror,
            held: false,
            down: false,
            cc14_msb: None,
        }
    }

    /// Re-arms the pickup mirror after a mode change. The physical position (`norm`) and the edge
    /// flags stay; an absolute control must cross the new action's value before it takes over, a
    /// relative encoder keeps turning. No-op bindings simply forget their mirror.
    pub fn rearm(&mut self, spec: Option<&BindingSpec>) {
        match spec {
            Some(spec) if spec.encoding.is_relative() => {
                self.engaged = true;
                self.mirror = self.norm;
            }
            Some(spec) => {
                self.mirror = default_norm(spec);
                self.engaged = false;
            }
            None => {
                self.mirror = self.norm;
                self.engaged = false;
            }
        }
        self.prev = self.norm;
    }
}

/// Where the engine leaves a control when the process starts. Used to seed soft-takeover.
pub fn default_norm(spec: &BindingSpec) -> f32 {
    match spec.action {
        // `simple_dj` opens the cue send fully. It is a send, not a bipolar fader, even if a map
        // accidentally gives it a negative `min`.
        Action::Fader(FaderTarget::CueSend(_)) => 1.0,
        // The tempo fader is unity at the centre even though the action is not a `FaderTarget`.
        Action::TempoFader => 0.5,
        _ if spec.action.is_bipolar() || spec.min < 0.0 => 0.5,
        // Everything else (unipolar FX params) starts at the bottom of its domain.
        _ => 0.0,
    }
}

/// Applies one binding to one event. The router resolves the endpoint and the active mode, then
/// calls this with the binding that should handle the event. `false` means the event did not match
/// the binding's kind/channel/id.
pub fn apply(
    map: &Map,
    spec: &BindingSpec,
    state: &mut BindState,
    event: &Event,
    out: &mut Vec<Command>,
    browser: &mut Vec<BrowserAction>,
) -> bool {
    match *event {
        Event::ControlChange {
            channel,
            controller,
            value,
        } => {
            if spec.kind != EventKind::Cc || !spec.matches(channel, controller) {
                return false;
            }
            let mut binding = BindingOut::new(spec, state, map, out, browser);
            if let Some(lsb) = spec.lsb {
                binding.on_cc14(controller, lsb, value);
            } else {
                binding.on_control(value);
            }
            true
        }
        Event::NoteOn {
            channel,
            key,
            velocity,
        } if velocity > 0 => {
            if spec.kind != EventKind::Note || !spec.matches(channel, key) {
                return false;
            }
            if state.held {
                return false;
            }
            state.held = true;
            let mut binding = BindingOut::new(spec, state, map, out, browser);
            binding.press();
            true
        }
        Event::NoteOn { channel, key, .. } | Event::NoteOff { channel, key, .. } => {
            if spec.kind != EventKind::Note || !spec.matches(channel, key) {
                return false;
            }
            if !state.held {
                return false;
            }
            state.held = false;
            let mut binding = BindingOut::new(spec, state, map, out, browser);
            binding.release();
            true
        }
        Event::PitchBend { channel, value } => {
            if spec.kind != EventKind::Bend || spec.channel != channel {
                return false;
            }
            let norm = (f32::from(value) + 8192.0) / 16384.0;
            let mut binding = BindingOut::new(spec, state, map, out, browser);
            if binding.accepts(norm) {
                binding.emit_continuous(norm);
            }
            true
        }
    }
}

/// Applies one binding's action against one event, then writes into the output vector. Kept as a
/// short-lived borrow so the router can hand it one endpoint at a time.
struct BindingOut<'a> {
    spec: &'a BindingSpec,
    state: &'a mut BindState,
    map: &'a Map,
    out: &'a mut Vec<Command>,
    browser: &'a mut Vec<BrowserAction>,
}

impl<'a> BindingOut<'a> {
    fn new(
        spec: &'a BindingSpec,
        state: &'a mut BindState,
        map: &'a Map,
        out: &'a mut Vec<Command>,
        browser: &'a mut Vec<BrowserAction>,
    ) -> Self {
        Self {
            spec,
            state,
            map,
            out,
            browser,
        }
    }

    /// Maps a normalised absolute position through the binding's centre. A `BipolKnob` centre
    /// becomes `0.5`, so the action's `min..max` range produces exact zero at the detent.
    fn centered(&self, norm: f32) -> f32 {
        let Some(center) = self.spec.center else {
            return norm;
        };
        if center <= 0.0 || center >= 1.0 {
            return norm;
        }
        let bipolar = if norm >= center {
            (norm - center) / (1.0 - center)
        } else {
            (norm - center) / center
        };
        (bipolar + 1.0) / 2.0
    }

    /// Handles one half of a 14-bit CC pair. The MSB is remembered; the LSB completes the value and
    /// emits exactly once, so a fast sweep does not send two commands per step.
    fn on_cc14(&mut self, controller: u8, lsb: u8, value: u8) {
        if controller == self.spec.id {
            self.state.cc14_msb = Some(value);
            return;
        }
        if controller != lsb {
            return;
        }
        let Some(msb) = self.state.cc14_msb else {
            // LSB before MSB: wait for the next complete pair.
            return;
        };
        let raw = (u16::from(msb) << 7) | u16::from(value);
        let norm = self.centered(f32::from(raw) / 16383.0);
        self.state.norm = norm;
        if self.accepts(norm) {
            self.emit_continuous(norm);
        }
    }

    /// Handles a CC value: continuous actions read it as a position (absolute or relative), button
    /// actions as an edge.
    fn on_control(&mut self, value: u8) {
        if self.spec.encoding.is_relative() {
            let delta = relative_delta(self.spec.encoding, value, self.spec.reverse);
            let norm = (self.state.norm + delta as f32 * REL_SCALE).clamp(0.0, 1.0);
            self.state.norm = norm;
            // A relative encoder is endless: there is nothing to pick up.
            self.state.engaged = true;
            self.state.mirror = norm;
            // A two-action `RelKnob` fires one edge action per detent. The sign picks the action;
            // the slot compiler has already rejected continuous/momentary actions for this shape.
            if let Some(down) = self.spec.down.clone() {
                if delta != 0 {
                    let action = if delta > 0 {
                        self.spec.action.clone()
                    } else {
                        down
                    };
                    self.fire_action(action);
                }
                return;
            }
            // A wheel is a *direction*, not a position: one tick forward bends one way, one tick back
            // the other way. It also has no release edge, so the bend has to end on its own — hence
            // the duration default (`seconds` in the map still wins). Turning faster sends more
            // ticks, and each one re-arms that timer, so the bend lasts as long as the wheel moves.
            // A `vinyl` wheel sends the same shape, but the *deck* decides whether a tick scrubs
            // (platter held) or bends (nobody on it) — the mapping cannot know, and must not guess.
            if let Action::Vinyl { frames_per_tick } = self.spec.action.clone() {
                if delta != 0 {
                    let Some(deck_id) = self.spec.deck else {
                        return;
                    };
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
            if let Action::Nudge {
                delta: step,
                seconds,
            } = self.spec.action.clone()
            {
                if delta != 0 {
                    let Some(deck_id) = self.spec.deck else {
                        return;
                    };
                    // `Bend`: each message re-states the offset and restarts the release, so a
                    // continuous turn stays bent instead of stacking into a runaway rate. The
                    // magnitude is the tick count, so an encoder that reports ±3 per detent still
                    // bends three times as far in one message.
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
        let norm = self.centered(f32::from(value) / 127.0);
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
                let value = command_value(self.spec, norm);
                let slot = FxSlotRef { chain, index };
                match param {
                    FxParamRef::Name(name) => {
                        self.out.push(Command::SetFxParam { slot, name, value })
                    }
                    FxParamRef::Index(index) => {
                        self.out
                            .push(Command::SetFxParamIndex { slot, index, value })
                    }
                }
            }
            _ => {}
        }
    }

    fn press(&mut self) {
        // A one-shot action can be assigned to the release edge; a two-edge action always fires
        // here. `Action::is_momentary` is the same predicate the file compiler uses to reject `on`.
        if !self.spec.action.is_momentary() && self.spec.on != Edge::Press {
            return;
        }
        self.fire();
    }

    /// Emits the action itself, without the edge policy. `press` uses it directly; `release` uses
    /// it for a one-shot action assigned to the release edge.
    fn fire(&mut self) {
        self.fire_action(self.spec.action.clone());
    }

    /// Emits one action with the binding's deck. `fire` passes the mode's action; a `RelKnob` pair
    /// passes whichever direction the detent selected.
    fn fire_action(&mut self, action: Action) {
        let Some(deck) = self.spec.deck else {
            // Non-deck actions below (FX) still need to run; only deck-scoped ones bail.
            self.press_deckless(action);
            return;
        };
        match action {
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
            Action::HotCue(op) => self.out.push(Command::HotCue { deck_id: deck, op }),
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
            Action::BeatJumpStep { forward } => self.out.push(Command::BeatJumpStep {
                deck_id: deck,
                direction: if forward { 1 } else { -1 },
            }),
            Action::ScaleJumpBeats { halve } => self.out.push(Command::ScaleJumpBeats {
                deck_id: deck,
                halve,
            }),
            Action::ScaleLoopBeats { halve } => self.out.push(Command::ScaleLoopBeats {
                deck_id: deck,
                halve,
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
            Action::Slip => self.out.push(Command::Vinyl {
                deck_id: deck,
                op: VinylOp::Slip,
            }),
            Action::ToggleCueSend => self.out.push(Command::ToggleCueSend { deck_id: deck }),
            Action::ToggleKeylock => self.out.push(Command::ToggleKeylock { deck_id: deck }),
            Action::Keylock(mode) => self.out.push(Command::SetKeylock {
                deck_id: deck,
                mode,
            }),
            Action::Key(semitones) => self.out.push(Command::SetKey {
                deck_id: deck,
                semitones,
            }),
            _ => self.press_deckless(action),
        }
    }

    fn press_deckless(&mut self, action: Action) {
        match action {
            Action::Fx(action) => self.fx_press(action),
            Action::Browser(action) => self.browser.push(action),
            _ => {}
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
                    // The engine owns the state; a front-end mirror would go wrong the first time
                    // anything else changed the slot.
                    self.out.push(Command::ToggleFx { slot });
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
        if self.spec.action.is_momentary() {
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
        } else if self.spec.on == Edge::Release {
            // A one-shot action on the release edge is the same emission as a press, just later.
            self.fire();
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
        Curve::Sharp if spec.action.is_bipolar() || spec.min < 0.0 => {
            // Shape the bipolar position so the centre stays put. A bipolar FX parameter (an EQ
            // band, `min = -1`) is shaped like a bipolar fader even though the action is `fx param`.
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
///
/// `reverse` flips the sign: a controller that numbers its detents the other way round is one
/// boolean away from working, and the guide cannot tell which way a wheel turns.
pub(crate) fn relative_delta(encoding: Encoding, value: u8, reverse: bool) -> i16 {
    let delta = match encoding {
        Encoding::Absolute => 0,
        Encoding::Offset64 => i16::from(value) - 64,
        Encoding::TwosComplement => {
            if value < 0x40 {
                i16::from(value)
            } else {
                i16::from(value) - 0x80
            }
        }
        Encoding::Direction => match value.cmp(&64) {
            std::cmp::Ordering::Less => 1,
            std::cmp::Ordering::Greater => -1,
            std::cmp::Ordering::Equal => 0,
        },
        // A two-point encoder reports one step, at one of two extreme values. Anything in between
        // is noise from a different device sharing the controller number, so it is not movement.
        Encoding::TwoPoint => match value {
            1 => 1,
            127 => -1,
            _ => 0,
        },
    };
    if reverse {
        -delta
    } else {
        delta
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
        LoopAction::Toggle => LoopOp::Toggle,
    }
}

/// One-button sync defaults to the PI controller: `pid` converges and then holds with the
/// correction at zero, which is what a single mapped key should do. A front-end that wants a
/// different mode can extend the registry.
fn sync_op(op: SyncAction) -> SyncOp {
    match op {
        SyncAction::Tempo => SyncOp::Tempo,
        SyncAction::Phase { mode, t_seconds } => SyncOp::Phase { mode, t_seconds },
        SyncAction::PhaseLock { mode, t_seconds } => SyncOp::PhaseLock { mode, t_seconds },
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
    use crate::map::{Curve, Edge, Map};
    use hypermixx_core::FxChainId;

    fn spec(action: Action, encoding: Encoding) -> BindingSpec {
        BindingSpec {
            kind: EventKind::Cc,
            encoding,
            reverse: false,
            channel: 0,
            id: 7,
            lsb: None,
            center: None,
            deck: Some(0),
            action,
            down: None,
            min: -1.0,
            max: 1.0,
            curve: Curve::Linear,
            on: Edge::Press,
        }
    }

    fn cc(value: u8) -> Event {
        Event::ControlChange {
            channel: 0,
            controller: 7,
            value,
        }
    }

    fn note(on: bool) -> Event {
        if on {
            Event::NoteOn {
                channel: 0,
                key: 7,
                velocity: 100,
            }
        } else {
            Event::NoteOff {
                channel: 0,
                key: 7,
                velocity: 0,
            }
        }
    }

    fn apply_one(spec: &BindingSpec, state: &mut BindState, event: &Event) -> Vec<Command> {
        let map = Map::empty();
        let mut commands = Vec::new();
        let mut browser = Vec::new();
        assert!(apply(&map, spec, state, event, &mut commands, &mut browser));
        commands
    }

    #[test]
    fn relative_modes_decode_the_documented_wraparounds() {
        assert_eq!(relative_delta(Encoding::Offset64, 64, false), 0);
        assert_eq!(relative_delta(Encoding::Offset64, 65, false), 1);
        assert_eq!(relative_delta(Encoding::Offset64, 1, false), -63);
        assert_eq!(relative_delta(Encoding::Offset64, 65, true), -1);
        assert_eq!(relative_delta(Encoding::TwosComplement, 1, false), 1);
        assert_eq!(relative_delta(Encoding::TwosComplement, 127, false), -1);
        assert_eq!(relative_delta(Encoding::TwosComplement, 65, false), -63);
        assert_eq!(relative_delta(Encoding::Direction, 1, false), 1);
        assert_eq!(relative_delta(Encoding::Direction, 127, false), -1);
        assert_eq!(relative_delta(Encoding::Direction, 64, false), 0);
    }

    #[test]
    fn a_bipol_knob_centre_maps_to_zero() {
        let mut binding = spec(Action::Fader(FaderTarget::Flow(0)), Encoding::Absolute);
        binding.center = Some(64.0 / 127.0);
        let mut state = BindState::seeded(Some(&binding));
        let commands = apply_one(&binding, &mut state, &cc(64));
        assert!(matches!(
            commands.as_slice(),
            [Command::SetFader { value, .. }] if value.abs() < 1e-6
        ));
        let top = apply_one(&binding, &mut state, &cc(127));
        assert!(matches!(
            top.as_slice(),
            [Command::SetFader { value, .. }] if (*value - 1.0).abs() < 1e-6
        ));
    }

    #[test]
    fn a_rel_knob_reverse_swaps_the_pair_directions() {
        let mut binding = spec(Action::Browser(BrowserAction::Up), Encoding::TwosComplement);
        binding.down = Some(Action::Browser(BrowserAction::Down));
        binding.reverse = true;
        let map = Map::empty();
        let mut state = BindState::default();
        let mut commands = Vec::new();
        let mut browser = Vec::new();

        assert!(apply(
            &map,
            &binding,
            &mut state,
            &cc(1),
            &mut commands,
            &mut browser
        ));
        assert!(matches!(browser.as_slice(), [BrowserAction::Down]));

        browser.clear();
        assert!(apply(
            &map,
            &binding,
            &mut state,
            &cc(127),
            &mut commands,
            &mut browser
        ));
        assert!(matches!(browser.as_slice(), [BrowserAction::Up]));
    }

    #[test]
    fn a_cc14_pair_emits_once_when_the_lsb_arrives() {
        let mut binding = spec(Action::Fader(FaderTarget::Flow(0)), Encoding::Absolute);
        binding.id = 9;
        binding.lsb = Some(41);
        let map = Map::empty();
        let mut state = BindState::seeded(Some(&binding));
        let mut commands = Vec::new();
        let mut browser = Vec::new();
        let msb = Event::ControlChange {
            channel: 0,
            controller: 9,
            value: 64,
        };
        let lsb = Event::ControlChange {
            channel: 0,
            controller: 41,
            value: 0,
        };

        assert!(apply(
            &map,
            &binding,
            &mut state,
            &msb,
            &mut commands,
            &mut browser
        ));
        assert!(commands.is_empty(), "MSB alone must not emit");

        assert!(apply(
            &map,
            &binding,
            &mut state,
            &lsb,
            &mut commands,
            &mut browser
        ));
        assert_eq!(commands.len(), 1, "LSB completes the pair");

        let next = Event::ControlChange {
            channel: 0,
            controller: 41,
            value: 127,
        };
        assert!(apply(
            &map,
            &binding,
            &mut state,
            &next,
            &mut commands,
            &mut browser
        ));
        assert_eq!(commands.len(), 2);
    }

    #[test]
    fn a_rel_knob_pair_fires_the_direction_action() {
        let mut binding = spec(Action::Browser(BrowserAction::Up), Encoding::TwosComplement);
        binding.down = Some(Action::Browser(BrowserAction::Down));
        let map = Map::empty();
        let mut state = BindState::default();
        let mut commands = Vec::new();
        let mut browser = Vec::new();

        assert!(apply(
            &map,
            &binding,
            &mut state,
            &cc(1),
            &mut commands,
            &mut browser
        ));
        assert!(matches!(browser.as_slice(), [BrowserAction::Up]));

        browser.clear();
        assert!(apply(
            &map,
            &binding,
            &mut state,
            &cc(127),
            &mut commands,
            &mut browser
        ));
        assert!(matches!(browser.as_slice(), [BrowserAction::Down]));
        assert!(commands.is_empty());
    }

    #[test]
    fn the_bipolar_curve_keeps_the_centre_and_the_sharp_curve_fines_the_bottom() {
        let mut linear = spec(Action::Fader(FaderTarget::Flow(0)), Encoding::Absolute);
        assert!(close(command_value(&linear, 0.5), 0.0));
        linear.curve = Curve::Sharp;
        assert!(close(command_value(&linear, 0.5), 0.0));
        assert!(close(command_value(&linear, 0.75), 0.25));

        let mut send = spec(Action::Fader(FaderTarget::CueSend(0)), Encoding::Absolute);
        send.min = 0.0;
        send.max = 1.0;
        send.curve = Curve::Sharp;
        assert!(close(command_value(&send, 0.5), 0.25));
    }

    #[test]
    fn a_note_button_fires_on_press_only_unless_the_binding_asks_for_the_release() {
        let mut play = spec(Action::Play, Encoding::Absolute);
        play.kind = EventKind::Note;
        let mut state = BindState::seeded(Some(&play));
        assert!(matches!(
            apply_one(&play, &mut state, &note(true)).as_slice(),
            [Command::TogglePlay { deck_id: 0 }]
        ));
        assert!(apply_one(&play, &mut state, &note(false)).is_empty());

        let mut release = play.clone();
        release.on = Edge::Release;
        let mut state = BindState::seeded(Some(&release));
        assert!(apply_one(&release, &mut state, &note(true)).is_empty());
        assert!(matches!(
            apply_one(&release, &mut state, &note(false)).as_slice(),
            [Command::TogglePlay { deck_id: 0 }]
        ));
    }

    #[test]
    fn a_two_edge_action_keeps_both_edges() {
        let mut cue = spec(Action::Cue, Encoding::Absolute);
        cue.kind = EventKind::Note;
        let mut state = BindState::seeded(Some(&cue));
        assert!(matches!(
            apply_one(&cue, &mut state, &note(true)).as_slice(),
            [Command::Cue {
                deck_id: 0,
                op: CueOp::Hold
            }]
        ));
        assert!(matches!(
            apply_one(&cue, &mut state, &note(false)).as_slice(),
            [Command::Cue {
                deck_id: 0,
                op: CueOp::Back
            }]
        ));
    }

    #[test]
    fn soft_takeover_waits_for_the_control_to_cross_the_mirror() {
        let fader = spec(Action::Fader(FaderTarget::Flow(0)), Encoding::Absolute);
        let mut state = BindState::seeded(Some(&fader));
        // Unity mirror at 0.5: a low value is ignored until it reaches the mirror.
        assert!(apply_one(&fader, &mut state, &cc(20)).is_empty());
        assert!(apply_one(&fader, &mut state, &cc(40)).is_empty());
        let commands = apply_one(&fader, &mut state, &cc(64));
        assert_eq!(commands.len(), 1);
        // Once engaged, movement is live.
        assert_eq!(apply_one(&fader, &mut state, &cc(80)).len(), 1);
    }

    #[test]
    fn a_relative_encoder_accumulates_without_takeover() {
        let tempo = spec(Action::TempoFader, Encoding::Offset64);
        let mut state = BindState::seeded(Some(&tempo));
        let commands = apply_one(&tempo, &mut state, &cc(65));
        assert!(matches!(
            commands.as_slice(),
            [Command::SetTempoFader { deck_id: 0, position }] if *position > 0.0
        ));
        let commands = apply_one(&tempo, &mut state, &cc(63));
        assert!(matches!(
            commands.as_slice(),
            [Command::SetTempoFader { deck_id: 0, position }] if *position < 0.02
        ));
    }

    #[test]
    fn default_mirrors_match_the_mixer_defaults_and_a_bipolar_fx_param() {
        let flow = spec(Action::Fader(FaderTarget::Flow(0)), Encoding::Absolute);
        assert!(close(default_norm(&flow), 0.5));
        let send = spec(Action::Fader(FaderTarget::CueSend(0)), Encoding::Absolute);
        assert!(close(default_norm(&send), 1.0));
        let tempo = spec(Action::TempoFader, Encoding::Absolute);
        assert!(close(default_norm(&tempo), 0.5));
        let mut eq = spec(
            Action::Fx(FxAction::Param {
                chain: FxChainId::Deck(0),
                fx: "eq".into(),
                param: FxParamRef::Name("low".into()),
            }),
            Encoding::Absolute,
        );
        eq.min = -1.0;
        assert!(close(default_norm(&eq), 0.5));
        eq.min = 0.0;
        assert!(close(default_norm(&eq), 0.0));
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
        assert_eq!(flushed.len(), 3);
        assert!(matches!(
            flushed[0],
            Command::SetFader {
                target: FaderTarget::Flow(0),
                value
            } if close(value, 0.9)
        ));
        assert!(matches!(
            flushed[1],
            Command::SetFader {
                target: FaderTarget::Flow(1),
                ..
            }
        ));
        assert!(matches!(flushed[2], Command::Play { deck_id: 0 }));
        assert!(buffer.is_empty());
    }

    fn close(left: f32, right: f32) -> bool {
        (left - right).abs() < 1e-4
    }
}
