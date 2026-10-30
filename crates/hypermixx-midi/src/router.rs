//! The runtime layer: one event in, zero or more commands out, with per-unit mode state.
//!
//! The map compiler decides *what* each slot can mean. This module decides *which* mode is active,
//! routes a physical endpoint to its slot, captures the action a held pad started with, and re-arms
//! soft-takeover when a mode changes underneath a knob.

use std::collections::HashMap;

use hypermixx_core::{BrowserAction, Command, DeckId};

use crate::map::{
    BindingSpec, Endpoint, Encoding, EventKind, Map, SelectorBehavior, SlotRole, Unit,
};
use crate::msg::Event;
use crate::translate::{apply, relative_delta, BindState, REL_SCALE};

/// A snapshot of every unit's active mode, for the TUI and the `midi mode` command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouterState {
    pub units: Vec<UnitModeState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitModeState {
    pub id: String,
    pub deck: Option<DeckId>,
    pub current: String,
    pub modes: Vec<String>,
}

/// A command from the front-end into the running router.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouterCommand {
    SetMode { unit: String, mode: String },
    NextMode { unit: String },
}

/// What one event produced.
#[derive(Debug, Default)]
pub struct Feed {
    pub commands: Vec<Command>,
    pub browser: Vec<BrowserAction>,
    /// A selector (or a command) changed the active mode, so the caller should republish
    /// [`Router::snapshot`].
    pub state_changed: bool,
}

#[derive(Clone, Copy, Debug)]
enum Route {
    Control {
        unit: usize,
        control: usize,
        endpoint: usize,
    },
    Selector {
        unit: usize,
        selector: usize,
    },
}

struct ControlRuntime {
    /// The binding captured on the press/down edge. `Some(None)` means "pressed while the slot was
    /// inactive": the release must stay a no-op even if a later mode binds the slot. `None` means
    /// no capture (the release falls back to the current binding).
    held: Option<Option<BindingSpec>>,
}

struct UnitRuntime {
    current: usize,
    controls: Vec<ControlRuntime>,
    /// Parallel to `Unit::endpoints`: physical position, pickup mirror, edge flags.
    endpoints: Vec<BindState>,
    /// Parallel to `Unit::selectors`: its own edge state.
    selectors: Vec<BindState>,
}

/// The live mapping. Owns the compiled [`Map`] plus the mode and per-endpoint state.
pub struct Router {
    map: Map,
    units: Vec<UnitRuntime>,
    routes: HashMap<(EventKind, u8, u8), Route>,
}

impl Router {
    pub fn new(map: Map) -> Self {
        let mut units = Vec::with_capacity(map.units.len());
        let mut routes = HashMap::new();

        for (unit_index, unit) in map.units.iter().enumerate() {
            let mut endpoints = Vec::with_capacity(unit.endpoints.len());
            for (control_index, control) in unit.controls.iter().enumerate() {
                let mode_slot = control.role != SlotRole::Action;

                for &endpoint_index in &control.endpoints {
                    let spec = spec_in_mode(unit, unit.start, control_index, endpoint_index);
                    endpoints.push(BindState::seeded(spec.as_ref()));
                    if mode_slot {
                        continue;
                    }
                    for key in unit.endpoints[endpoint_index].keys() {
                        routes.insert(
                            key,
                            Route::Control {
                                unit: unit_index,
                                control: control_index,
                                endpoint: endpoint_index,
                            },
                        );
                    }
                }
            }
            let mut selectors = Vec::with_capacity(unit.selectors.len());
            for (selector_index, selector) in unit.selectors.iter().enumerate() {
                selectors.push(BindState::default());
                routes.insert(
                    selector.endpoint.key(),
                    Route::Selector {
                        unit: unit_index,
                        selector: selector_index,
                    },
                );
            }
            units.push(UnitRuntime {
                current: unit.start,
                controls: unit
                    .controls
                    .iter()
                    .map(|_| ControlRuntime { held: None })
                    .collect(),
                endpoints,
                selectors,
            });
        }

        Self {
            map,
            units,
            routes,
        }
    }

    pub fn map(&self) -> &Map {
        &self.map
    }

    pub fn snapshot(&self) -> RouterState {
        RouterState {
            units: self
                .map
                .units
                .iter()
                .zip(&self.units)
                .map(|(unit, runtime)| UnitModeState {
                    id: unit.id.clone(),
                    deck: unit.deck,
                    current: unit.modes[runtime.current].name.clone(),
                    modes: unit.modes.iter().map(|mode| mode.name.clone()).collect(),
                })
                .collect(),
        }
    }

    /// Applies one event. Selector events only change mode; control events go through the active
    /// mode's binding (or are ignored when the slot is inactive in that mode).
    pub fn feed(&mut self, event: &Event) -> Feed {
        let mut out = Feed::default();
        let Some(key) = event_key(event) else {
            return out;
        };
        match self.routes.get(&key).copied() {
            Some(Route::Selector { unit, selector }) => {
                self.selector_event(unit, selector, event, &mut out);
            }
            Some(Route::Control {
                unit,
                control,
                endpoint,
            }) => {
                self.control_event(unit, control, endpoint, event, &mut out);
            }
            None => {}
        }
        out
    }

    /// Applies a front-end command. Returns `true` when the active mode changed.
    pub fn command(&mut self, command: RouterCommand) -> Result<bool, String> {
        match command {
            RouterCommand::SetMode { unit, mode } => {
                let unit_index = self
                    .map
                    .units
                    .iter()
                    .position(|candidate| candidate.id == unit)
                    .ok_or_else(|| format!("unknown unit `{unit}`"))?;
                let mode_index = self.map.units[unit_index]
                    .mode_index(&mode)
                    .ok_or_else(|| format!("unit `{unit}` has no mode `{mode}`"))?;
                Ok(self.select(unit_index, mode_index))
            }
            RouterCommand::NextMode { unit } => {
                let unit_index = self
                    .map
                    .units
                    .iter()
                    .position(|candidate| candidate.id == unit)
                    .ok_or_else(|| format!("unknown unit `{unit}`"))?;
                let modes = self.map.units[unit_index].modes.len();
                let next = (self.units[unit_index].current + 1) % modes;
                Ok(self.select(unit_index, next))
            }
        }
    }

    fn selector_event(&mut self, unit: usize, selector: usize, event: &Event, out: &mut Feed) {
        let behavior = self.map.units[unit].selectors[selector].behavior;
        let state = &mut self.units[unit].selectors[selector];
        let pressed = match *event {
            Event::NoteOn { velocity, .. } if velocity > 0 => {
                if state.held {
                    return;
                }
                state.held = true;
                true
            }
            Event::NoteOn { .. } | Event::NoteOff { .. } => {
                state.held = false;
                false
            }
            Event::ControlChange { value, .. } => {
                let down = value > 0;
                let pressed = down && !state.down;
                state.down = down;
                pressed
            }
            Event::PitchBend { .. } => false,
        };
        if !pressed {
            return;
        }
        let target = match behavior {
            SelectorBehavior::Trigger { mode } => mode,
            SelectorBehavior::Cycle => {
                let modes = self.map.units[unit].modes.len();
                (self.units[unit].current + 1) % modes
            }
        };
        if self.select(unit, target) {
            out.state_changed = true;
        }
    }

    fn control_event(
        &mut self,
        unit: usize,
        control: usize,
        endpoint: usize,
        event: &Event,
        out: &mut Feed,
    ) {
        let spec = self.active_spec(unit, control, endpoint);
        let was_held = self.units[unit].endpoints[endpoint].held;
        let was_down = self.units[unit].endpoints[endpoint].down;
        let edge = spec
            .as_ref()
            .is_none_or(|spec| !spec.action.is_continuous() && !spec.encoding.is_relative());

        match *event {
            Event::NoteOn { velocity, .. } if velocity > 0 => {
                if was_held {
                    return;
                }
                self.units[unit].controls[control].held = Some(spec.clone());
                if let Some(spec) = &spec {
                    let state = &mut self.units[unit].endpoints[endpoint];
                    apply(
                        &self.map,
                        spec,
                        state,
                        event,
                        &mut out.commands,
                        &mut out.browser,
                    );
                } else {
                    self.units[unit].endpoints[endpoint].held = true;
                }
            }
            Event::NoteOn { .. } | Event::NoteOff { .. } => {
                if !was_held {
                    return;
                }
                let captured = self.units[unit].controls[control].held.take();
                let chosen = match captured {
                    Some(captured) => captured,
                    None => spec,
                };
                if let Some(spec) = chosen {
                    let state = &mut self.units[unit].endpoints[endpoint];
                    apply(
                        &self.map,
                        &spec,
                        state,
                        event,
                        &mut out.commands,
                        &mut out.browser,
                    );
                } else {
                    self.units[unit].endpoints[endpoint].held = false;
                }
            }
            Event::ControlChange { value, .. } => {
                if edge {
                    let down = value > 0;
                    if down && !was_down {
                        self.units[unit].controls[control].held = Some(spec.clone());
                        if let Some(spec) = &spec {
                            let state = &mut self.units[unit].endpoints[endpoint];
                            apply(
                                &self.map,
                                spec,
                                state,
                                event,
                                &mut out.commands,
                                &mut out.browser,
                            );
                        } else {
                            let endpoint_spec = &self.map.units[unit].endpoints[endpoint];
                            let state = &mut self.units[unit].endpoints[endpoint];
                            state.down = true;
                            observe_unmapped(state, endpoint_spec, value);
                        }
                    } else if !down && was_down {
                        let captured = self.units[unit].controls[control].held.take();
                        let chosen = match captured {
                            Some(captured) => captured,
                            None => spec,
                        };
                        if let Some(spec) = chosen {
                            let state = &mut self.units[unit].endpoints[endpoint];
                            apply(
                                &self.map,
                                &spec,
                                state,
                                event,
                                &mut out.commands,
                                &mut out.browser,
                            );
                        } else {
                            let state = &mut self.units[unit].endpoints[endpoint];
                            state.down = false;
                            state.norm = 0.0;
                        }
                    } else if spec.is_none() {
                        let endpoint_spec = &self.map.units[unit].endpoints[endpoint];
                        let state = &mut self.units[unit].endpoints[endpoint];
                        observe_unmapped(state, endpoint_spec, value);
                    }
                } else if let Some(spec) = &spec {
                    let state = &mut self.units[unit].endpoints[endpoint];
                    apply(
                        &self.map,
                        spec,
                        state,
                        event,
                        &mut out.commands,
                        &mut out.browser,
                    );
                } else {
                    let endpoint_spec = &self.map.units[unit].endpoints[endpoint];
                    let state = &mut self.units[unit].endpoints[endpoint];
                    observe_unmapped(state, endpoint_spec, value);
                }
            }
            Event::PitchBend { value, .. } => {
                if let Some(spec) = &spec {
                    let state = &mut self.units[unit].endpoints[endpoint];
                    apply(
                        &self.map,
                        spec,
                        state,
                        event,
                        &mut out.commands,
                        &mut out.browser,
                    );
                } else {
                    let state = &mut self.units[unit].endpoints[endpoint];
                    state.norm = (f32::from(value) + 8192.0) / 16384.0;
                    state.mirror = state.norm;
                }
            }
        }
    }

    /// The binding a slot would use right now, if the current mode binds it.
    fn active_spec(&self, unit: usize, control: usize, endpoint: usize) -> Option<BindingSpec> {
        spec_in_mode(
            &self.map.units[unit],
            self.units[unit].current,
            control,
            endpoint,
        )
    }

    fn select(&mut self, unit: usize, target: usize) -> bool {
        if self.units[unit].current == target {
            return false;
        }
        self.units[unit].current = target;
        self.rearm(unit);
        true
    }

    /// After a mode change, every continuous control in the unit re-arms its pickup mirror. The
    /// physical position and the edge flags stay: a held pad still releases with the action it
    /// started on.
    fn rearm(&mut self, unit: usize) {
        let unit_spec = &self.map.units[unit];
        let current = self.units[unit].current;
        for (control_index, control) in unit_spec.controls.iter().enumerate() {
            for &endpoint_index in &control.endpoints {
                let spec = spec_in_mode(unit_spec, current, control_index, endpoint_index);
                self.units[unit].endpoints[endpoint_index].rearm(spec.as_ref());
            }
        }
    }
}

/// Keeps a control's physical position live even while the active mode leaves its slot unbound.
/// Without this, re-entering a mode with a relative encoder would resume from a stale position
/// (and an absolute knob would have no pickup reference).
fn observe_unmapped(state: &mut BindState, endpoint: &Endpoint, value: u8) {
    match endpoint.encoding {
        Encoding::Absolute => state.norm = f32::from(value) / 127.0,
        mode => {
            let delta = relative_delta(mode, value, endpoint.reverse);
            state.norm = (state.norm + f32::from(delta) * REL_SCALE).clamp(0.0, 1.0);
        }
    }
    state.mirror = state.norm;
}

fn spec_in_mode(
    unit: &Unit,
    mode: usize,
    control: usize,
    endpoint: usize,
) -> Option<BindingSpec> {
    unit.binding(mode, control)
        .and_then(|binding| binding.spec(&unit.endpoints[endpoint]))
}

fn event_key(event: &Event) -> Option<(EventKind, u8, u8)> {
    match *event {
        Event::ControlChange {
            channel,
            controller,
            ..
        } => Some((EventKind::Cc, channel, controller)),
        Event::NoteOn { channel, key, .. } | Event::NoteOff { channel, key, .. } => {
            Some((EventKind::Note, channel, key))
        }
        Event::PitchBend { channel, .. } => Some((EventKind::Bend, channel, 0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Map;
    use hypermixx_core::{BeatLen, FaderTarget, HotCueOp, LoopOp};

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

    fn cc(channel: u8, controller: u8, value: u8) -> Event {
        Event::ControlChange {
            channel,
            controller,
            value,
        }
    }

    fn router(text: &str) -> Router {
        let map = Map::from_toml_str(text).expect("test map");
        Router::new(map)
    }

    const TWO_MODES: &str = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "pad1"
type = "Button"
endpoint = { midi = "note", channel = 0, id = 16 }

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
[[unit.mode.bind]]
slot = "pad1"
action = "hotcue trigger 1"

[[unit.mode]]
name = "loop"
[[unit.mode.bind]]
slot = "pad1"
action = "loop 4"
"#;

    #[test]
    fn a_trigger_selector_changes_what_the_same_pad_does_and_emits_nothing() {
        let mut router = router(TWO_MODES);
        let feed = router.feed(&note(0, 16, true));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::HotCue {
                deck_id: 0,
                op: HotCueOp::Trigger(0)
            }]
        ));
        router.feed(&note(0, 16, false));

        let feed = router.feed(&note(0, 24, true));
        assert!(feed.commands.is_empty(), "a mode key is not an action");
        assert!(!feed.state_changed, "it is already in hotcue");
        router.feed(&note(0, 24, false));

        let feed = router.feed(&note(0, 25, true));
        assert!(feed.commands.is_empty());
        assert!(feed.state_changed);
        assert_eq!(router.snapshot().units[0].current, "loop");
        router.feed(&note(0, 25, false));

        let feed = router.feed(&note(0, 16, true));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::Loop { deck_id: 0, op: LoopOp::Beats(len) }] if *len == BeatLen::beats(4)
        ));
    }

    #[test]
    fn a_cycle_selector_walks_declaration_order_and_wraps() {
        let text = r#"
[[unit]]
id = "u"

[[unit.control]]
slot = "mode-next"
type = "Button"
role = "mode_cycle"
endpoint = { midi = "note", channel = 0, id = 1 }

[[unit.mode]]
name = "a"
[[unit.mode]]
name = "b"
[[unit.mode]]
name = "c"
"#;
        let mut router = router(text);
        assert_eq!(router.snapshot().units[0].current, "a");
        for expected in ["b", "c", "a"] {
            let feed = router.feed(&note(0, 1, true));
            assert!(feed.state_changed);
            assert_eq!(router.snapshot().units[0].current, expected);
            router.feed(&note(0, 1, false));
        }
    }

    #[test]
    fn a_held_pad_releases_with_the_action_it_started_with() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "pad"
type = "Button"
endpoint = { midi = "note", channel = 0, id = 1 }

[[unit.control]]
slot = "mode-next"
type = "Button"
role = "mode_cycle"
endpoint = { midi = "note", channel = 0, id = 2 }

[[unit.mode]]
name = "cue"
[[unit.mode.bind]]
slot = "pad"
action = "cue"

[[unit.mode]]
name = "play"
[[unit.mode.bind]]
slot = "pad"
action = "play"
"#;
        let mut router = router(text);
        router.feed(&note(0, 1, true));
        // Switch mode while the button is held: the release must still be the cue release.
        router.feed(&note(0, 2, true));
        router.feed(&note(0, 2, false));
        let feed = router.feed(&note(0, 1, false));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::Cue {
                deck_id: 0,
                op: hypermixx_core::CueOp::Back
            }]
        ));
    }

    #[test]
    fn a_one_shot_action_can_fire_on_the_release_edge() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "pad"
type = "Button"
endpoint = { midi = "note", channel = 0, id = 1 }

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "pad"
action = "play"
on = "release"
"#;
        let mut router = router(text);
        let feed = router.feed(&note(0, 1, true));
        assert!(feed.commands.is_empty());
        let feed = router.feed(&note(0, 1, false));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::TogglePlay { deck_id: 0 }]
        ));
    }

    #[test]
    fn a_cc_button_can_fire_on_the_release_edge() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "pad"
type = "Button"
endpoint = { midi = "cc", channel = 0, id = 1 }

[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "pad"
action = "play"
on = "release"
"#;
        let mut router = router(text);
        assert!(router.feed(&cc(0, 1, 127)).commands.is_empty());
        let feed = router.feed(&cc(0, 1, 0));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::TogglePlay { deck_id: 0 }]
        ));
    }

    #[test]
    fn a_slot_missing_from_the_current_mode_is_inert_and_comes_back() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "pad"
type = "Button"
endpoint = { midi = "note", channel = 0, id = 1 }

[[unit.control]]
slot = "mode-next"
type = "Button"
role = "mode_cycle"
endpoint = { midi = "note", channel = 0, id = 2 }

[[unit.mode]]
name = "a"
[[unit.mode.bind]]
slot = "pad"
action = "play"

[[unit.mode]]
name = "b"
"#;
        let mut router = router(text);
        router.feed(&note(0, 2, true));
        router.feed(&note(0, 2, false));
        let feed = router.feed(&note(0, 1, true));
        assert!(feed.commands.is_empty());
        router.feed(&note(0, 1, false));

        router.feed(&note(0, 2, true));
        router.feed(&note(0, 2, false));
        let feed = router.feed(&note(0, 1, true));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::TogglePlay { deck_id: 0 }]
        ));
    }

    #[test]
    fn a_press_on_an_inactive_slot_does_not_fire_when_a_later_mode_binds_it() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "pad"
type = "Button"
endpoint = { midi = "note", channel = 0, id = 1 }

[[unit.control]]
slot = "mode-next"
type = "Button"
role = "mode_cycle"
endpoint = { midi = "note", channel = 0, id = 2 }

[[unit.mode]]
name = "a"

[[unit.mode]]
name = "b"
[[unit.mode.bind]]
slot = "pad"
action = "play"
"#;
        let mut router = router(text);
        router.feed(&note(0, 1, true));
        router.feed(&note(0, 2, true));
        router.feed(&note(0, 2, false));
        let feed = router.feed(&note(0, 1, false));
        assert!(feed.commands.is_empty(), "the press was captured as inert");
    }

    #[test]
    fn an_absolute_knob_rearms_pickup_on_a_mode_change() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "knob"
type = "Knob"
endpoint = { midi = "cc", channel = 0, id = 7 }

[[unit.control]]
slot = "mode-next"
type = "Button"
role = "mode_cycle"
endpoint = { midi = "note", channel = 0, id = 2 }

[[unit.mode]]
name = "a"
[[unit.mode.bind]]
slot = "knob"
action = "fader flow"

[[unit.mode]]
name = "b"
[[unit.mode.bind]]
slot = "knob"
action = "fader gain"
"#;
        let mut router = router(text);
        // Take over at a known position, then change mode: the new action must wait for a pickup.
        router.feed(&cc(0, 7, 64));
        router.feed(&note(0, 2, true));
        router.feed(&note(0, 2, false));
        let feed = router.feed(&cc(0, 7, 70));
        assert!(feed.commands.is_empty(), "no pickup yet");
        let feed = router.feed(&cc(0, 7, 64));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::SetFader { target: FaderTarget::Gain(0), .. }]
        ));
    }

    #[test]
    fn a_relative_encoder_keeps_turning_across_a_mode_change() {
        let text = r#"
[[unit]]
id = "u"
deck = 1

[[unit.control]]
slot = "knob"
type = "RelKnob"
endpoint = { midi = "cc", encoding = "twos_complement", channel = 0, id = 7 }

[[unit.control]]
slot = "mode-next"
type = "Button"
role = "mode_cycle"
endpoint = { midi = "note", channel = 0, id = 2 }

[[unit.mode]]
name = "a"
[[unit.mode.bind]]
slot = "knob"
action = "fader flow"

[[unit.mode]]
name = "b"
[[unit.mode.bind]]
slot = "knob"
action = "fader flow"
"#;
        let mut router = router(text);
        router.feed(&cc(0, 7, 64));
        let feed = router.feed(&cc(0, 7, 1));
        assert_eq!(feed.commands.len(), 1);
        router.feed(&note(0, 2, true));
        router.feed(&note(0, 2, false));
        let feed = router.feed(&cc(0, 7, 1));
        assert_eq!(feed.commands.len(), 1, "a relative encoder has no pickup");
    }

    #[test]
    fn a_jog_drives_one_vinyl_action_from_its_press_and_wheel() {
        let text = r#"
[[unit]]
id = "deck1.jog"
preset = "jog"
deck = 1

[[unit.control]]
slot = "jog"
endpoint = [
  { role = "press", midi = "note", channel = 0, id = 80 },
  { role = "turn", midi = "cc", encoding = "twos_complement", channel = 0, id = 6 },
]
"#;
        let mut router = router(text);
        let feed = router.feed(&note(0, 80, true));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::Vinyl {
                deck_id: 0,
                op: hypermixx_core::VinylOp::Touch
            }]
        ));
        let feed = router.feed(&cc(0, 6, 1));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::Vinyl {
                deck_id: 0,
                op: hypermixx_core::VinylOp::Turn { ticks: 1, .. }
            }]
        ));
        let feed = router.feed(&note(0, 80, false));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::Vinyl {
                deck_id: 0,
                op: hypermixx_core::VinylOp::Release
            }]
        ));
    }

    #[test]
    fn a_jog_nudge_mode_leaves_the_press_inert() {
        let text = r#"
[[unit]]
id = "deck1.jog"
preset = "jog"
deck = 1
start = "nudge"

[[unit.control]]
slot = "jog"
endpoint = [
  { role = "press", midi = "note", channel = 0, id = 80 },
  { role = "turn", midi = "cc", encoding = "twos_complement", channel = 0, id = 6 },
]

[[unit.mode]]
name = "nudge"
[[unit.mode.bind]]
slot = "jog"
turn = "nudge 0.15"
"#;
        let mut router = router(text);
        let feed = router.feed(&note(0, 80, true));
        assert!(feed.commands.is_empty(), "no press action in nudge mode");
        let feed = router.feed(&cc(0, 6, 1));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::Nudge {
                deck_id: 0,
                op: hypermixx_core::NudgeOp::Bend { .. }
            }]
        ));
    }

    /// The shipped example map, driven end to end: a note, a CC, an FX knob and a mode key.
    #[test]
    fn the_shipped_map_drives_the_engine() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = root.join("profile/midi-map.toml");
        let text = std::fs::read_to_string(&path).expect("shipped map");
        let mut map = Map::from_toml_str_at(&text, path.parent().unwrap()).expect("parses");
        // FX are addressed by name; the engine's own slot order is what the CLI installs.
        for deck in 0..2u8 {
            map.resolve_fx(hypermixx_core::FxChainId::Deck(deck), "eq", 0);
            map.resolve_fx(hypermixx_core::FxChainId::Deck(deck), "filter", 1);
        }
        let mut router = Router::new(map);

        // Transport: deck1 play is note 0 on channel 0.
        let feed = router.feed(&note(0, 0, true));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::TogglePlay { deck_id: 0 }]
        ));
        router.feed(&note(0, 0, false));

        // The channel fader is an absolute CC: soft takeover means it must cross the mirror first.
        router.feed(&cc(0, 7, 0));
        let feed = router.feed(&cc(0, 7, 100));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::SetFader {
                target: FaderTarget::Flow(0),
                ..
            }]
        ));

        // An EQ knob reaches the named FX slot.
        let feed = router.feed(&cc(0, 9, 64));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::SetFxParam { .. }]
        ));

        // A pad fires hot cue 1, then the beat-loop mode key changes what the same pad does.
        let feed = router.feed(&note(0, 16, true));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::HotCue {
                deck_id: 0,
                op: HotCueOp::Trigger(0)
            }]
        ));
        router.feed(&note(0, 16, false));

        let feed = router.feed(&note(0, 25, true));
        assert!(feed.commands.is_empty(), "a mode key emits nothing");
        assert!(feed.state_changed);
        router.feed(&note(0, 25, false));
        assert_eq!(router.snapshot().units[0].current, "loop");

        let feed = router.feed(&note(0, 16, true));
        assert!(matches!(
            feed.commands.as_slice(),
            [Command::Loop { deck_id: 0, op: LoopOp::Beats(len) }] if *len == BeatLen::fraction(1, 4).unwrap()
        ));
    }

    #[test]
    fn the_front_end_can_select_a_mode_by_name_or_advance_it() {
        let mut router = router(TWO_MODES);
        assert!(router
            .command(RouterCommand::SetMode {
                unit: "u".into(),
                mode: "loop".into(),
            })
            .unwrap());
        assert_eq!(router.snapshot().units[0].current, "loop");
        assert!(router
            .command(RouterCommand::NextMode { unit: "u".into() })
            .unwrap());
        assert_eq!(router.snapshot().units[0].current, "hotcue");
    }
}
