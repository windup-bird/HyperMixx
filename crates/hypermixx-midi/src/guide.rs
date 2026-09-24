//! The MIDI-learn state machine: pure logic, no I/O and no rendering.
//!
//! A guide session is an editable [`MapFile`] plus a cursor over the [`manifest`] of targets. The
//! renderer (in `hypermixx-cli`) only reads accessors and calls the small mutators here; the file
//! is written by the caller after [`Guide::to_toml`] has round-tripped through [`Map`] to prove the
//! result is loadable. That split is what lets the whole interaction be unit-tested without a
//! terminal or a controller.

use crate::map::{manifest, EventKind, Map, MapError, MapFile, RawBinding, RelMode, Target};
use crate::msg::Event;

/// What [`Guide::feed`] did with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuideStep {
    /// The guide was not armed; the event belongs to the monitor only.
    Ignored,
    /// The event became (or refined) the candidate control.
    Captured,
    /// Consecutive values wrapped, so the candidate's mode was promoted to [`RelMode::Rel1`].
    RelativeDetected,
}

/// A control detected while armed, before it is committed to a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub kind: EventKind,
    pub channel: u8,
    pub id: u8,
    pub mode: RelMode,
}

/// How a target's binding stands in this session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindStatus {
    /// No binding, and none this session.
    Unbound,
    /// The loaded file binds it.
    Bound,
    /// This session added, replaced or cleared it (unsaved).
    Edited,
}

/// The guide session.
pub struct Guide {
    map: MapFile,
    targets: Vec<Target>,
    /// Parallel to `targets`: changed since load, for the `(✎)` marker.
    touched: Vec<bool>,
    selected: usize,
    armed: bool,
    candidate: Option<Candidate>,
    /// Previous CC value while armed, for relative auto-detection.
    last_cc: Option<u8>,
    dirty: bool,
    message: Option<String>,
}

impl Guide {
    /// Starts a session from an optional file. The text is validated both structurally and
    /// semantically, so a bad file fails here rather than when the map is later handed to `--midi`.
    pub fn new(decks: usize, text: Option<&str>) -> Result<Self, MapError> {
        let map = match text {
            Some(text) => {
                Map::from_toml_str(text)?;
                MapFile::from_toml_str(text)?
            }
            None => MapFile {
                meta: None,
                binds: Vec::new(),
            },
        };
        let targets: Vec<Target> = manifest(decks)
            .into_iter()
            .filter(|target| !target.heading)
            .collect();
        let touched = vec![false; targets.len()];
        Ok(Self {
            map,
            targets,
            touched,
            selected: 0,
            armed: false,
            candidate: None,
            last_cc: None,
            dirty: false,
            message: None,
        })
    }

    // ---- reading (for the renderer) ---------------------------------------

    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    pub fn candidate(&self) -> Option<&Candidate> {
        self.candidate.as_ref()
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The binding backing `target`, if the file (or this session) has one.
    pub fn binding_for(&self, target: &Target) -> Option<&RawBinding> {
        self.binding_index(target).map(|index| &self.map.binds[index])
    }

    /// `target`'s status in this session.
    pub fn status(&self, target: &Target) -> BindStatus {
        let index = self.target_index(target);
        let touched = index.is_some_and(|index| self.touched[index]);
        match (self.binding_index(target), touched) {
            (Some(_), true) => BindStatus::Edited,
            (Some(_), false) => BindStatus::Bound,
            (None, _) => BindStatus::Unbound,
        }
    }

    /// The currently selected target, if any.
    pub fn selected_target(&self) -> Option<&Target> {
        self.targets.get(self.selected)
    }

    // ---- editing ----------------------------------------------------------

    /// Moves the cursor by `delta`, wrapping at both ends.
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.targets.len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// Arms capture: the next inbound event becomes the candidate.
    pub fn arm(&mut self) {
        self.armed = true;
        self.candidate = None;
        self.last_cc = None;
        let label = self
            .selected_target()
            .map(|target| target.label.clone())
            .unwrap_or_default();
        self.message = Some(format!("等待控件 — 请拨动/按下 {label}"));
    }

    /// Drops capture and any candidate.
    pub fn disarm(&mut self) {
        self.armed = false;
        self.candidate = None;
        self.last_cc = None;
    }

    /// Consumes one inbound event. Only the press edge of a note counts, and a value that wraps
    /// between two consecutive CCs promotes the candidate to a relative encoder.
    pub fn feed(&mut self, event: &Event) -> GuideStep {
        if !self.armed {
            return GuideStep::Ignored;
        }
        match *event {
            Event::ControlChange {
                channel,
                controller,
                value,
            } => {
                let mut mode = self
                    .candidate
                    .as_ref()
                    .map_or(RelMode::Abs, |candidate| candidate.mode);
                let step = if mode == RelMode::Abs {
                    match self.last_cc {
                        // A near-full-scale jump between adjacent messages is a wrap, not a sweep.
                        Some(previous) if (i16::from(value) - i16::from(previous)).abs() >= 100 => {
                            mode = RelMode::Rel1;
                            GuideStep::RelativeDetected
                        }
                        _ => GuideStep::Captured,
                    }
                } else {
                    GuideStep::Captured
                };
                self.last_cc = Some(value);
                self.candidate = Some(Candidate {
                    kind: EventKind::Cc,
                    channel,
                    id: controller,
                    mode,
                });
                step
            }
            Event::NoteOn {
                channel,
                key,
                velocity,
            } if velocity > 0 => {
                self.candidate = Some(Candidate {
                    kind: EventKind::Note,
                    channel,
                    id: key,
                    mode: RelMode::Abs,
                });
                GuideStep::Captured
            }
            // A release carries no new control, and a bend has no id.
            Event::NoteOn { .. } | Event::NoteOff { .. } => GuideStep::Ignored,
            Event::PitchBend { channel, .. } => {
                self.candidate = Some(Candidate {
                    kind: EventKind::Bend,
                    channel,
                    id: 0,
                    mode: RelMode::Abs,
                });
                GuideStep::Captured
            }
        }
    }

    /// Cycles the candidate's relative mode: `abs → rel1 → rel2 → rel3 → abs`.
    pub fn cycle_mode(&mut self) {
        let Some(candidate) = self.candidate.as_mut() else {
            self.message = Some("先按 Enter 捕获一个控件,再切换模式".to_owned());
            return;
        };
        candidate.mode = match candidate.mode {
            RelMode::Abs => RelMode::Rel1,
            RelMode::Rel1 => RelMode::Rel2,
            RelMode::Rel2 => RelMode::Rel3,
            RelMode::Rel3 => RelMode::Abs,
        };
        self.message = Some(format!("模式:{}", mode_label(candidate.mode)));
    }

    /// Commits the candidate to the selected target, replacing any existing binding (and carrying
    /// over its FX parameters). Advances the cursor to the next target.
    pub fn confirm(&mut self) -> Result<(), String> {
        let Some(candidate) = self.candidate.clone() else {
            return Err("还没有捕获控件 — 先按 Enter 再拨动".to_owned());
        };
        let Some(target) = self.selected_target().cloned() else {
            return Err("没有选中目标".to_owned());
        };
        let mut bind = RawBinding {
            kind: candidate.kind,
            mode: if candidate.kind == EventKind::Cc {
                Some(candidate.mode)
            } else {
                None
            },
            channel: Some(candidate.channel),
            id: if candidate.kind == EventKind::Bend {
                None
            } else {
                Some(candidate.id)
            },
            deck: target.deck,
            action: target.action.to_owned(),
            min: None,
            max: None,
            curve: None,
            step: None,
            beats: None,
            chain: None,
            fx: None,
            param: None,
        };
        if target.action == "loop.beat" {
            bind.beats = Some(4);
        }
        match self.binding_index(&target) {
            Some(existing) => {
                // Keep the parameters a rebind cannot discover without an engine.
                let old = &self.map.binds[existing];
                bind.chain = old.chain.clone();
                bind.fx = old.fx.clone();
                bind.param = old.param.clone();
                self.map.binds[existing] = bind;
            }
            None if target.action.starts_with("fx.") => {
                return Err(format!(
                    "`{}` 需要 chain/fx/param,请在文件里补全后再重绑",
                    target.action
                ));
            }
            None => self.map.binds.push(bind),
        }
        if let Some(index) = self.target_index(&target) {
            self.touched[index] = true;
        }
        self.dirty = true;
        self.disarm();
        self.message = Some(format!("已绑定 {}", target.label));
        self.move_selection(1);
        Ok(())
    }

    /// Drops the candidate if there is one, otherwise removes the selected target's binding.
    pub fn clear(&mut self) {
        if self.candidate.is_some() {
            self.candidate = None;
            self.last_cc = None;
            self.message = Some("已丢弃候选".to_owned());
            return;
        }
        let Some(target) = self.selected_target().cloned() else {
            return;
        };
        match self.binding_index(&target) {
            Some(index) => {
                self.map.binds.remove(index);
                if let Some(target_index) = self.target_index(&target) {
                    self.touched[target_index] = true;
                }
                self.dirty = true;
                self.message = Some(format!("已清除 {}", target.label));
            }
            None => self.message = Some("该目标本无绑定".to_owned()),
        }
    }

    /// A transient status line (also used by the caller to report its own errors).
    pub fn note(&mut self, text: impl Into<String>) {
        self.message = Some(text.into());
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
        let label = self
            .map
            .meta
            .as_ref()
            .and_then(|meta| meta.name.clone())
            .unwrap_or_else(|| "map".to_owned());
        self.message = Some(format!("已保存 {label}"));
    }

    /// Serialises the map, validating the result with the same parser `--midi-map` uses so a saved
    /// file is guaranteed loadable. The caller still does the atomic write.
    pub fn to_toml(&self) -> Result<String, MapError> {
        let text = self.map.to_toml_string()?;
        Map::from_toml_str(&text)?;
        Ok(text)
    }

    // ---- internals --------------------------------------------------------

    fn binding_index(&self, target: &Target) -> Option<usize> {
        self.map.binds.iter().position(|bind| {
            // A global target (an FX action) ignores the binding's `deck`, which is meaningless
            // for a chain-addressed action and may be present from a hand-written file.
            bind.action == target.action
                && target.deck.is_none_or(|deck| bind.deck == Some(deck))
        })
    }

    fn target_index(&self, target: &Target) -> Option<usize> {
        self.targets
            .iter()
            .position(|candidate| candidate.action == target.action && candidate.deck == target.deck)
    }
}

fn mode_label(mode: RelMode) -> &'static str {
    match mode {
        RelMode::Abs => "abs",
        RelMode::Rel1 => "rel1",
        RelMode::Rel2 => "rel2",
        RelMode::Rel3 => "rel3",
    }
}

/// A one-line description of a detected control, for the status bar.
pub fn describe(candidate: &Candidate) -> String {
    let channel = candidate.channel + 1;
    match candidate.kind {
        EventKind::Cc => format!(
            "CC ch{channel} #{} {}",
            candidate.id,
            mode_label(candidate.mode)
        ),
        EventKind::Note => format!("NOTE ch{channel} #{}", candidate.id),
        EventKind::Bend => format!("BEND ch{channel}"),
    }
}

/// A short label for an event, for the raw monitor.
pub fn event_label(event: &Event) -> String {
    let channel = event.channel() + 1;
    match *event {
        Event::ControlChange {
            controller, value, ..
        } => format!("CC   ch{channel} #{controller:<3} = {value}"),
        Event::NoteOn { key, velocity, .. } => {
            format!("NOTE ch{channel} #{key:<3} on {velocity}")
        }
        Event::NoteOff { key, .. } => format!("NOTE ch{channel} #{key:<3} off"),
        Event::PitchBend { value, .. } => format!("BEND ch{channel} {value:+}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Action;

    fn cc(channel: u8, controller: u8, value: u8) -> Event {
        Event::ControlChange {
            channel,
            controller,
            value,
        }
    }

    fn note(channel: u8, key: u8) -> Event {
        Event::NoteOn {
            channel,
            key,
            velocity: 100,
        }
    }

    #[test]
    fn arming_then_feeding_a_cc_captures_a_candidate() {
        let mut guide = Guide::new(2, None).unwrap();
        assert_eq!(guide.feed(&cc(0, 7, 64)), GuideStep::Ignored, "idle");
        guide.arm();
        assert!(guide.is_armed());
        assert_eq!(guide.feed(&cc(3, 7, 64)), GuideStep::Captured);
        let candidate = guide.candidate().unwrap();
        assert_eq!(
            candidate,
            &Candidate {
                kind: EventKind::Cc,
                channel: 3,
                id: 7,
                mode: RelMode::Abs,
            }
        );
    }

    #[test]
    fn consecutive_wrapped_values_promote_to_a_relative_encoder() {
        let mut guide = Guide::new(2, None).unwrap();
        guide.arm();
        assert_eq!(guide.feed(&cc(0, 10, 0)), GuideStep::Captured);
        // 0 -> 127 is a full-scale wrap between adjacent messages: rel1.
        assert_eq!(guide.feed(&cc(0, 10, 127)), GuideStep::RelativeDetected);
        assert_eq!(guide.candidate().unwrap().mode, RelMode::Rel1);
    }

    #[test]
    fn a_note_press_captures_and_a_release_does_not() {
        let mut guide = Guide::new(2, None).unwrap();
        guide.arm();
        assert_eq!(guide.feed(&note(1, 48)), GuideStep::Captured);
        assert_eq!(guide.feed(&Event::NoteOff {
            channel: 1,
            key: 48,
            velocity: 0
        }), GuideStep::Ignored);
    }

    #[test]
    fn confirm_writes_a_binding_that_reloads_clean() {
        let mut guide = Guide::new(2, None).unwrap();
        // Select `deck0 play` and bind note 48.
        guide.move_selection(0);
        let target = guide.targets()[0].clone();
        assert_eq!((target.action, target.deck), ("play", Some(0)));
        guide.arm();
        guide.feed(&note(0, 48));
        guide.confirm().unwrap();
        assert_eq!(guide.status(&target), BindStatus::Edited);
        assert!(guide.is_dirty());

        let toml = guide.to_toml().unwrap();
        let reloaded = Guide::new(2, Some(&toml)).unwrap();
        assert_eq!(reloaded.status(&target), BindStatus::Bound);
        assert_eq!(
            reloaded.binding_for(&target).unwrap().id,
            Some(48)
        );
        // `to_toml` validates against `Map`, so the compiled form must also accept it.
        assert!(Map::from_toml_str(&toml).is_ok());
    }

    #[test]
    fn confirm_replaces_an_existing_binding_and_keeps_fx_parameters() {
        let source = r#"
[[bind]]
type = "note"
id = 10
deck = 0
action = "fx.toggle"
chain = "deck0"
fx = "filter"
"#;
        let mut guide = Guide::new(2, Some(source)).unwrap();
        // Find the fx.toggle target and rebind it to note 20.
        let index = guide
            .targets()
            .iter()
            .position(|target| target.action == "fx.toggle")
            .unwrap();
        guide.move_selection(index as isize - guide.selected() as isize);
        guide.arm();
        guide.feed(&note(0, 20));
        guide.confirm().unwrap();

        let toml = guide.to_toml().unwrap();
        assert!(toml.contains("id = 20"), "{toml}");
        assert!(toml.contains("filter"), "fx params carried over:\n{toml}");
        assert_eq!(toml.matches("fx.toggle").count(), 1, "replaced, not appended");
    }

    #[test]
    fn a_fresh_fx_target_asks_for_parameters() {
        let mut guide = Guide::new(2, None).unwrap();
        let index = guide
            .targets()
            .iter()
            .position(|target| target.action == "fx.toggle")
            .unwrap();
        guide.move_selection(index as isize);
        guide.arm();
        guide.feed(&note(0, 20));
        let err = guide.confirm().unwrap_err();
        assert!(err.contains("chain/fx/param"), "{err}");
    }

    #[test]
    fn cycle_mode_walks_the_four_modes_and_back() {
        let mut guide = Guide::new(2, None).unwrap();
        guide.arm();
        guide.feed(&cc(0, 7, 64));
        for expected in [RelMode::Rel1, RelMode::Rel2, RelMode::Rel3, RelMode::Abs] {
            guide.cycle_mode();
            assert_eq!(guide.candidate().unwrap().mode, expected);
        }
    }

    #[test]
    fn clear_removes_the_selected_bindings() {
        let mut guide = Guide::new(2, None).unwrap();
        let target = guide.selected_target().unwrap().clone();
        guide.arm();
        guide.feed(&note(0, 48));
        guide.confirm().unwrap();
        assert_eq!(guide.status(&target), BindStatus::Edited);

        // Move back to the same target (confirm advanced the cursor).
        let index = guide
            .targets()
            .iter()
            .position(|t| t.action == target.action && t.deck == target.deck)
            .unwrap();
        guide.move_selection(index as isize - guide.selected() as isize);
        guide.clear();
        assert_eq!(guide.status(&target), BindStatus::Unbound);
        assert!(guide.to_toml().unwrap().is_empty() || !guide.to_toml().unwrap().contains("play"));
    }

    #[test]
    fn a_bad_loaded_file_fails_at_construction() {
        let bad = "[[bind]]\ntype = \"cc\"\nid = 1\naction = \"nope\"\n";
        assert!(Guide::new(2, Some(bad)).is_err());
    }

    #[test]
    fn the_manifest_is_shared_with_the_action_registry() {
        let guide = Guide::new(3, None).unwrap();
        // Every deck-scoped action appears once per deck; global actions once.
        for spec in crate::map::ACTIONS {
            let count = guide
                .targets()
                .iter()
                .filter(|target| target.action == spec.name)
                .count();
            assert_eq!(count, if spec.deck { 3 } else { 1 }, "action {}", spec.name);
        }
        // And the action is one the registry can actually compile.
        let _ = Action::Play;
    }
}
