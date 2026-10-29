//! The unit-aware mapping guide: pure state, no terminal and no engine.
//!
//! The guide edits the same raw [`MapFile`] the runtime parses. It knows about presets, units,
//! controls, modes and selectors; the renderer only draws [`Guide::view`] and calls the mutators.
//! That split is what lets the whole interaction be tested without hardware.

use std::io;
use std::path::{Path, PathBuf};

use hypermixx_core::{DeckId, ACTIONS};

use crate::map::{
    ControlShape, Edge, EventKind, Map, MapFile, Meta, RawControl, RawEndpoint, RawMode,
    RawModeBind, RawSelector, RawSelectorBehavior, RawUnit, RelMode,
};
use crate::msg::Event;
use crate::preset::{Preset, PresetRegistry};

/// The conventional map edited when `--midi-guide` is given no path.
pub const DEFAULT_MAP: &str = "midi-map.toml";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Setup,
    Units,
    Controls,
    Modes,
    Selectors,
    Actions,
    Save,
}

impl Stage {
    pub const ALL: [Stage; 7] = [
        Stage::Setup,
        Stage::Units,
        Stage::Controls,
        Stage::Modes,
        Stage::Selectors,
        Stage::Actions,
        Stage::Save,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Stage::Setup => "file & device",
            Stage::Units => "units",
            Stage::Controls => "controls",
            Stage::Modes => "modes",
            Stage::Selectors => "selectors",
            Stage::Actions => "actions",
            Stage::Save => "save",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|stage| *stage == self).unwrap_or(0)
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Prev,
    Next,
    File,
    Device,
    New,
    Clone,
    Delete,
    Learn,
    Confirm,
    Cancel,
    Mode,
    Up,
    Down,
    Start,
    Edit,
    On,
    Save,
    Trigger,
    Cycle,
}

impl Button {
    pub fn label(self) -> &'static str {
        match self {
            Button::Prev => "◀ Prev",
            Button::Next => "Next ▶",
            Button::File => "File",
            Button::Device => "Device",
            Button::New => "New",
            Button::Clone => "Clone",
            Button::Delete => "Delete",
            Button::Learn => "Learn",
            Button::Confirm => "Confirm",
            Button::Cancel => "Cancel",
            Button::Mode => "Mode",
            Button::Up => "Up",
            Button::Down => "Down",
            Button::Start => "Set start",
            Button::Edit => "Edit",
            Button::On => "On",
            Button::Save => "Save",
            Button::Trigger => "Trigger",
            Button::Cycle => "Cycle",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Setup,
    Unit,
    Control,
    Mode,
    Selector,
    Bind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    /// The index inside the row's own collection.
    pub index: usize,
    pub label: String,
    pub hint: String,
}

/// A detected control, before it is committed to a slot. The renderer shows its label while the
/// user decides whether to confirm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub kind: EventKind,
    pub channel: u8,
    pub id: u8,
    pub mode: RelMode,
}

impl Candidate {
    pub fn label(&self) -> String {
        let kind = match self.kind {
            EventKind::Cc => "cc",
            EventKind::Note => "note",
            EventKind::Bend => "bend",
        };
        match self.mode {
            RelMode::Abs => format!("{kind} ch{} #{}", self.channel, self.id),
            RelMode::Rel1 => format!("{kind} rel1 ch{} #{}", self.channel, self.id),
            RelMode::Rel2 => format!("{kind} rel2 ch{} #{}", self.channel, self.id),
            RelMode::Rel3 => format!("{kind} rel3 ch{} #{}", self.channel, self.id),
        }
    }
}

/// What `Guide::feed` did with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuideStep {
    Ignored,
    Captured,
    RelativeDetected,
}

/// A read-only snapshot for the renderer.
#[derive(Clone, Debug)]
pub struct GuideView {
    pub stage: Stage,
    pub path: String,
    pub device: Option<String>,
    pub dirty: bool,
    pub message: Option<String>,
    pub error: Option<String>,
    pub rows: Vec<Row>,
    pub selected: usize,
    /// Lines for the right-hand detail card.
    pub details: Vec<(String, String)>,
    /// Endpoints to highlight in the raw monitor.
    pub endpoints: Vec<(EventKind, u8, u8)>,
    pub buttons: Vec<Button>,
    pub capturing: bool,
    pub candidate: Option<Candidate>,
}

/// One row of the simplified editor tree. The left pane is the only editor; there are no wizard
/// stages.
/// One half of a compound control (currently a jog wheel). The underlying control stays a single
/// slot so press and turn can share `vinyl` and the touch state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlPart {
    Press,
    Turn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeNodeKind {
    Deck {
        deck: Option<DeckId>,
    },
    AddUnit {
        deck: Option<DeckId>,
    },
    Unit {
        unit: usize,
    },
    Control {
        unit: usize,
        control: usize,
    },
    ControlPart {
        unit: usize,
        control: usize,
        part: ControlPart,
    },
    Mode {
        unit: usize,
        mode: usize,
    },
    Action {
        unit: usize,
        mode: usize,
        control: usize,
    },
    Selector {
        unit: usize,
        selector: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeNode {
    pub kind: TreeNodeKind,
    pub depth: usize,
    pub label: String,
    pub hint: String,
}

/// The context-sensitive buttons in the bottom bar. Connect/Open/Save/Exit are always appended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeButton {
    Learn,
    NewUnit,
    Clone,
    NewMode,
    NewSelector,
    Delete,
    Up,
    Down,
    SetStart,
    Edit,
    On,
    Confirm,
    Cancel,
    Mode,
    Connect,
    Open,
    Save,
    Exit,
}

impl TreeButton {
    pub fn label(self) -> &'static str {
        match self {
            TreeButton::Learn => "Learn",
            TreeButton::NewUnit => "New unit",
            TreeButton::Clone => "Clone",
            TreeButton::NewMode => "New mode",
            TreeButton::NewSelector => "New sel",
            TreeButton::Delete => "Del",
            TreeButton::Up => "Up",
            TreeButton::Down => "Down",
            TreeButton::SetStart => "Start",
            TreeButton::Edit => "Edit",
            TreeButton::On => "On",
            TreeButton::Confirm => "Confirm",
            TreeButton::Cancel => "Cancel",
            TreeButton::Mode => "Mode",
            TreeButton::Connect => "Connect",
            TreeButton::Open => "Open",
            TreeButton::Save => "Save",
            TreeButton::Exit => "Exit",
        }
    }

    /// One short line under the button row, so the focused action explains itself.
    pub fn hint(self) -> &'static str {
        match self {
            TreeButton::Learn => "learn the selected control",
            TreeButton::NewUnit => "add a unit from a preset",
            TreeButton::Clone => "copy this unit to another deck",
            TreeButton::NewMode => "add a mode to this unit",
            TreeButton::NewSelector => "add a mode selector",
            TreeButton::Delete => "delete the selected item",
            TreeButton::Up => "move the mode up",
            TreeButton::Down => "move the mode down",
            TreeButton::SetStart => "mark this mode as the start mode",
            TreeButton::Edit => "edit the selected action or selector",
            TreeButton::On => "toggle press / release edge",
            TreeButton::Confirm => "confirm the learned control",
            TreeButton::Cancel => "cancel learning",
            TreeButton::Mode => "cycle abs / rel1 / rel2 / rel3",
            TreeButton::Connect => "choose the MIDI input",
            TreeButton::Open => "open a map file",
            TreeButton::Save => "save the map",
            TreeButton::Exit => "quit the guide",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TreeView {
    pub path: Option<String>,
    pub device: Option<String>,
    pub dirty: bool,
    pub message: Option<String>,
    pub error: Option<String>,
    pub nodes: Vec<TreeNode>,
    pub selected: usize,
    pub details: Vec<(String, String)>,
    pub endpoints: Vec<(EventKind, u8, u8)>,
    pub buttons: Vec<TreeButton>,
    pub capturing: bool,
    pub candidate: Option<Candidate>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JogPart {
    Touch,
    Turn,
}

#[derive(Clone, Debug)]
struct SelectorCapture {
    behavior: RawSelectorBehavior,
    target: Option<String>,
}

#[derive(Clone, Debug)]
struct Capture {
    unit: usize,
    slot: String,
    shape: ControlShape,
    part: JogPart,
    touch: Option<Candidate>,
    selector: Option<SelectorCapture>,
    candidate: Option<Candidate>,
    last_cc: Option<u8>,
}

/// The guide session.
#[derive(Debug)]
pub struct Guide {
    path: Option<String>,
    base: PathBuf,
    map: MapFile,
    presets: PresetRegistry,
    compiled: Option<Map>,
    error: Option<String>,
    stage: Stage,
    decks: usize,
    selected_setup: usize,
    selected_tree: usize,
    selected_unit: usize,
    selected_control: usize,
    selected_mode: usize,
    selected_selector: usize,
    capture: Option<Capture>,
    dirty: bool,
    message: Option<String>,
}

impl Guide {
    /// Opens `path`, or starts an empty map when the file does not exist yet.
    pub fn new(path: Option<&str>, decks: usize) -> Result<Self, String> {
        let path = path.unwrap_or(DEFAULT_MAP).to_owned();
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_toml_str(&text, &path, decks),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let base = parent_dir(&path);
                let map = MapFile {
                    meta: Some(Meta::default()),
                    units: Vec::new(),
                };
                let presets = PresetRegistry::load(map.meta.as_ref(), &base)?;
                let mut guide = Self {
                    path: Some(path),
                    base,
                    map,
                    presets,
                    compiled: None,
                    error: None,
                    stage: Stage::Setup,
                    decks: decks.max(1),
                    selected_setup: 0,
                    selected_tree: 0,
                    selected_unit: 0,
                    selected_control: 0,
                    selected_mode: 0,
                    selected_selector: 0,
                    capture: None,
                    dirty: false,
                    message: None,
                };
                guide.recompile();
                Ok(guide)
            }
            Err(err) => Err(format!("{path}: {err}")),
        }
    }

    /// A fresh, empty guide with no file and no device. This is what the TUI starts with; the
    /// user opens a file and connects a port explicitly.
    pub fn empty(decks: usize) -> Result<Self, String> {
        let base = PathBuf::from(".");
        let map = MapFile {
            meta: None,
            units: Vec::new(),
        };
        let presets = PresetRegistry::load(None, &base)?;
        let mut guide = Self {
            path: None,
            base,
            map,
            presets,
            compiled: None,
            error: None,
            stage: Stage::Setup,
            decks: decks.max(1),
            selected_setup: 0,
            selected_tree: 0,
            selected_unit: 0,
            selected_control: 0,
            selected_mode: 0,
            selected_selector: 0,
            capture: None,
            dirty: false,
            message: None,
        };
        guide.recompile();
        Ok(guide)
    }

    /// Replaces this guide with the map at `path`. The previous guide survives if the file does not
    /// parse. No device is opened here; `Connect` does that.
    pub fn open(&mut self, path: &str) -> Result<(), String> {
        let text = std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
        let mut next = Self::from_toml_str(&text, path, self.decks)?;
        next.message = Some(format!("opened {path}"));
        *self = next;
        Ok(())
    }

    /// Saves under a new path. The base folder changes too, so `[meta] presets` is re-resolved; a
    /// failure rolls the path/base back rather than leaving a half-switched guide.
    pub fn save_as(&mut self, path: &str) -> Result<(), String> {
        let path = path.trim();
        if path.is_empty() {
            return Err("a save path must not be empty".to_owned());
        }
        let old_path = self.path.clone();
        let old_base = self.base.clone();
        let old_presets = self.presets.clone();
        self.path = Some(path.to_owned());
        self.base = parent_dir(path);
        match PresetRegistry::load(self.map.meta.as_ref(), &self.base) {
            Ok(presets) => self.presets = presets,
            Err(err) => {
                self.path = old_path;
                self.base = old_base;
                self.presets = old_presets;
                self.recompile();
                return Err(err);
            }
        }
        self.recompile();
        if let Some(error) = self.error.clone() {
            self.path = old_path;
            self.base = old_base;
            self.presets = old_presets;
            self.recompile();
            return Err(error);
        }
        if let Err(err) = self.save() {
            self.path = old_path;
            self.base = old_base;
            self.presets = old_presets;
            self.recompile();
            return Err(err);
        }
        Ok(())
    }

    /// Opens map text with a path used for preset resolution and saving. Used by tests and by the
    /// file picker.
    pub fn from_toml_str(text: &str, path: &str, decks: usize) -> Result<Self, String> {
        let value: toml::Value =
            toml::from_str(text).map_err(|err| format!("{path}: {err}"))?;
        let table = value
            .as_table()
            .ok_or_else(|| format!("{path}: a midi guide file must be a map table"))?;
        if table.contains_key("device") || table.contains_key("map") {
            return Err(format!(
                "{path}: the old two-part midi file is no longer supported; keep one map file and \
                 put the port in `[meta].device`"
            ));
        }
        let map = MapFile::from_toml_str(text).map_err(|err| format!("{path}: {err}"))?;
        let base = parent_dir(path);
        let presets = PresetRegistry::load(map.meta.as_ref(), &base)?;
        let mut guide = Self {
            path: Some(path.to_owned()),
            base,
            map,
            presets,
            compiled: None,
            error: None,
            stage: Stage::Setup,
            decks: decks.max(1),
            selected_setup: 0,
            selected_tree: 0,
            selected_unit: 0,
            selected_control: 0,
            selected_mode: 0,
            selected_selector: 0,
            capture: None,
            dirty: false,
            message: None,
        };
        guide.recompile();
        if let Some(error) = guide.error.clone() {
            return Err(format!("{path}: {error}"));
        }
        Ok(guide)
    }

    // ---- reading ----------------------------------------------------------

    pub fn stage(&self) -> Stage {
        self.stage
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub fn device(&self) -> Option<String> {
        self.map
            .meta
            .as_ref()
            .and_then(|meta| meta.device.clone())
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Sets the status line. Mutators return errors; the renderer writes them here.
    pub fn note(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }

    /// The selected action's `on` edge, if it has one. Used by the `On` button to toggle.
    pub fn current_on(&self) -> Option<Edge> {
        let mode = self.selected_mode_name()?;
        let slot = self.selected_slot()?;
        self.on_for_unit(self.selected_unit, &mode, &slot)
    }

    pub fn decks(&self) -> usize {
        self.decks
    }

    pub fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }

    pub fn candidate(&self) -> Option<&Candidate> {
        self.capture
            .as_ref()
            .and_then(|capture| capture.candidate.as_ref())
    }

    pub fn preset_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.presets.names().map(str::to_owned).collect();
        names.sort();
        names
    }

    pub fn preset_is_global(&self, name: &str) -> bool {
        self.presets.is_global(name)
    }

    pub fn selected_unit_deck(&self) -> Option<DeckId> {
        self.unit().and_then(|unit| unit.deck)
    }

    pub fn selected_mode_name(&self) -> Option<String> {
        self.compiled_unit()?
            .modes
            .get(self.selected_mode)
            .map(|mode| mode.name.clone())
    }

    pub fn selected_slot(&self) -> Option<String> {
        self.compiled_unit()?
            .controls
            .get(self.selected_control)
            .map(|control| control.slot.clone())
    }

    pub fn action_text(&self) -> Option<String> {
        let mode = self.selected_mode_name()?;
        let slot = self.selected_slot()?;
        self.action_for(&mode, &slot)
    }

    /// A read-only snapshot for the renderer.
    pub fn view(&self) -> GuideView {
        let rows = self.rows();
        let selected = self.selected_row().min(rows.len().saturating_sub(1));
        GuideView {
            stage: self.stage,
            path: self.path.clone().unwrap_or_default(),
            device: self.device(),
            dirty: self.dirty,
            message: self.message.clone(),
            error: self.error.clone(),
            rows,
            selected,
            details: self.details(),
            endpoints: self.selected_endpoints(),
            buttons: self.buttons(),
            capturing: self.capture.is_some(),
            candidate: self.candidate().cloned(),
        }
    }

    // ---- navigation -------------------------------------------------------

    pub fn set_stage(&mut self, stage: Stage) {
        if self.stage != stage {
            self.cancel_learn();
            self.stage = stage;
            self.message = None;
            self.clamp_selection();
        }
    }

    pub fn next_stage(&mut self) {
        self.set_stage(self.stage.next());
    }

    pub fn prev_stage(&mut self) {
        self.set_stage(self.stage.prev());
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.rows().len();
        if len == 0 {
            return;
        }
        let current = self.selected_row() as isize;
        self.select_row((current + delta).rem_euclid(len as isize) as usize);
    }

    pub fn select_row(&mut self, index: usize) {
        let len = self.rows().len();
        if len == 0 {
            return;
        }
        let index = index.min(len - 1);
        match self.stage {
            Stage::Setup => self.selected_setup = index,
            Stage::Units => {
                if self.selected_unit != index {
                    self.selected_unit = index;
                    self.selected_control = 0;
                    self.selected_mode = 0;
                    self.selected_selector = 0;
                }
            }
            Stage::Controls | Stage::Actions => self.selected_control = index,
            Stage::Modes => self.selected_mode = index,
            Stage::Selectors => self.selected_selector = index,
            Stage::Save => {}
        }
    }

    fn selected_row(&self) -> usize {
        match self.stage {
            Stage::Setup => self.selected_setup,
            Stage::Units => self.selected_unit,
            Stage::Controls | Stage::Actions => self.selected_control,
            Stage::Modes => self.selected_mode,
            Stage::Selectors => self.selected_selector,
            Stage::Save => 0,
        }
    }

    fn clamp_selection(&mut self) {
        let controls = self.compiled_unit().map_or(0, |unit| unit.controls.len());
        let modes = self.compiled_unit().map_or(0, |unit| unit.modes.len());
        let selectors = self.compiled_unit().map_or(0, |unit| unit.selectors.len());
        self.selected_unit = self.selected_unit.min(self.map.units.len().saturating_sub(1));
        self.selected_control = self.selected_control.min(controls.saturating_sub(1));
        self.selected_mode = self.selected_mode.min(modes.saturating_sub(1));
        self.selected_selector = self.selected_selector.min(selectors.saturating_sub(1));
        self.selected_setup = self.selected_setup.min(1);
    }

    // ---- setup ------------------------------------------------------------

    pub fn set_device(&mut self, device: Option<String>) {
        let device = device.filter(|name| !name.trim().is_empty());
        if self.device() == device {
            return;
        }
        let meta = self.map.meta.get_or_insert_with(Meta::default);
        meta.device = device;
        self.dirty = true;
        self.message = Some("device saved into [meta].device".to_owned());
    }

    // ---- units ------------------------------------------------------------

    pub fn new_unit(
        &mut self,
        preset: &str,
        deck: DeckId,
        id: Option<String>,
    ) -> Result<(), String> {
        let deck = if self.preset_is_global(preset) {
            None
        } else {
            Some(deck)
        };
        self.new_unit_on(preset, deck, id)
    }

    /// Create a unit without a deck. Global presets use this; a deck preset can also be
    /// instantiated globally, but the guide normally only offers the matching kind.
    pub fn new_global_unit(&mut self, preset: &str, id: Option<String>) -> Result<(), String> {
        self.new_unit_on(preset, None, id)
    }

    fn new_unit_on(
        &mut self,
        preset: &str,
        deck: Option<DeckId>,
        id: Option<String>,
    ) -> Result<(), String> {
        if self.presets.get(preset).is_none() {
            return Err(format!("unknown preset `{preset}`"));
        }
        let id = id.filter(|id| !id.trim().is_empty()).unwrap_or_else(|| {
            match deck {
                Some(deck) => format!("deck{}.{preset}", deck + 1),
                None => format!("global.{preset}"),
            }
        });
        validate_name("unit id", &id)?;
        let id = self.unique_unit_id(id);
        let unit = RawUnit {
            id,
            preset: Some(preset.to_owned()),
            deck,
            start: None,
            mode_order: None,
            controls: Vec::new(),
            selectors: Vec::new(),
            modes: Vec::new(),
        };
        self.with_map(|map| {
            map.units.push(unit);
            Ok(())
        })?;
        self.selected_unit = self.map.units.len() - 1;
        self.selected_control = 0;
        self.selected_mode = 0;
        self.selected_selector = 0;
        self.stage = Stage::Controls;
        self.message = Some("unit created; bind its controls".to_owned());
        Ok(())
    }

    /// Deep-copies the selected deck unit to `target_deck`, shifting every endpoint channel by
    /// `target_deck - old_deck` and rewriting deck/chain references.
    pub fn clone_unit(&mut self, target_deck: DeckId) -> Result<(), String> {
        let source = self
            .unit()
            .cloned()
            .ok_or_else(|| "no unit selected".to_owned())?;
        let old_deck = source
            .deck
            .ok_or_else(|| "a global unit cannot be cloned to a deck".to_owned())?;
        if old_deck == target_deck {
            return Err(format!("`{}` is already on deck {target_deck}", source.id));
        }
        let delta = i16::from(target_deck) - i16::from(old_deck);
        let id = clone_id(&source.id, old_deck, target_deck);
        if self.map.units.iter().any(|unit| unit.id == id) {
            return Err(format!(
                "unit `{id}` already exists on deck {target_deck}; delete it first or pick another deck"
            ));
        }
        let cloned = clone_unit_raw(&source, target_deck, delta, id)?;
        self.with_map(|map| {
            map.units.push(cloned);
            Ok(())
        })?;
        self.selected_unit = self.map.units.len() - 1;
        self.selected_control = 0;
        self.selected_mode = 0;
        self.selected_selector = 0;
        self.message = Some(format!(
            "cloned to deck {target_deck}; endpoint channels {}",
            if delta >= 0 {
                format!("+{delta}")
            } else {
                delta.to_string()
            }
        ));
        Ok(())
    }

    pub fn remove_selected(&mut self) -> Result<(), String> {
        match self.stage {
            Stage::Units => {
                if self.map.units.is_empty() {
                    return Ok(());
                }
                let index = self.selected_unit;
                self.with_map(|map| {
                    map.units.remove(index);
                    Ok(())
                })?;
                self.clamp_selection();
                self.message = Some("unit removed".to_owned());
            }
            Stage::Controls => {
                let Some(unit) = self.unit().cloned() else {
                    return Ok(());
                };
                let Some(slot) = self.selected_slot() else {
                    return Ok(());
                };
                let unit_index = self.selected_unit;
                let preset_slot = self
                    .presets
                    .get(unit.preset.as_deref().unwrap_or_default())
                    .is_some_and(|preset| preset.slots.iter().any(|s| s.slot == slot));
                self.with_map(|map| {
                    let unit = &mut map.units[unit_index];
                    unit.controls.retain(|control| control.slot != slot);
                    if !preset_slot {
                        // A unit-added slot has no other definition: drop its binds too, or the
                        // mode would reference a slot that no longer exists.
                        for mode in &mut unit.modes {
                            mode.binds.retain(|bind| bind.slot != slot);
                        }
                    }
                    Ok(())
                })?;
                self.message = Some(format!("unbound `{slot}`"));
            }
            Stage::Modes => self.remove_selected_mode()?,
            Stage::Selectors => {
                let index = self.selected_selector;
                let unit_index = self.selected_unit;
                self.with_map(|map| {
                    if index < map.units[unit_index].selectors.len() {
                        map.units[unit_index].selectors.remove(index);
                    }
                    Ok(())
                })?;
                self.clamp_selection();
                self.message = Some("selector removed".to_owned());
            }
            Stage::Actions => self.unbind_action()?,
            Stage::Setup | Stage::Save => {}
        }
        Ok(())
    }

    // ---- controls / learn -------------------------------------------------

    pub fn begin_learn(&mut self) -> Result<(), String> {
        if self.stage != Stage::Controls {
            return Err("switch to the controls stage first".to_owned());
        }
        let Some(unit) = self.compiled_unit().cloned() else {
            return Err(self
                .error
                .clone()
                .unwrap_or_else(|| "the map is not valid".to_owned()));
        };
        let Some(control) = unit.controls.get(self.selected_control) else {
            return Err("no control selected".to_owned());
        };
        self.capture = Some(Capture {
            unit: self.selected_unit,
            slot: control.slot.clone(),
            shape: control.shape,
            part: JogPart::Touch,
            touch: None,
            selector: None,
            candidate: None,
            last_cc: None,
        });
        self.message = Some(match control.shape {
            ControlShape::JogWheel => "press/touch the platter sensor first".to_owned(),
            _ => format!("press or turn `{}` now", control.slot),
        });
        Ok(())
    }

    pub fn begin_selector(
        &mut self,
        behavior: RawSelectorBehavior,
        target: Option<String>,
    ) -> Result<(), String> {
        if self.stage != Stage::Selectors {
            return Err("switch to the selectors stage first".to_owned());
        }
        if matches!(behavior, RawSelectorBehavior::Trigger) {
            let target = target
                .clone()
                .ok_or_else(|| "a trigger selector needs a target mode".to_owned())?;
            if !self.mode_names().contains(&target) {
                return Err(format!("unknown mode `{target}`"));
            }
        }
        self.capture = Some(Capture {
            unit: self.selected_unit,
            slot: String::new(),
            shape: ControlShape::Button,
            part: JogPart::Touch,
            touch: None,
            selector: Some(SelectorCapture { behavior, target }),
            candidate: None,
            last_cc: None,
        });
        self.message = Some("press the selector pad now".to_owned());
        Ok(())
    }

    pub fn cancel_learn(&mut self) {
        if self.capture.take().is_some() {
            self.message = Some("learn cancelled".to_owned());
        }
    }

    /// Cycles the candidate's relative mode: `abs → rel1 → rel2 → rel3 → abs`.
    pub fn cycle_candidate_mode(&mut self) {
        let Some(capture) = self.capture.as_mut() else {
            self.message = Some("nothing captured yet".to_owned());
            return;
        };
        let Some(candidate) = capture.candidate.as_mut() else {
            self.message = Some("nothing captured yet".to_owned());
            return;
        };
        candidate.mode = match candidate.mode {
            RelMode::Abs => RelMode::Rel1,
            RelMode::Rel1 => RelMode::Rel2,
            RelMode::Rel2 => RelMode::Rel3,
            RelMode::Rel3 => RelMode::Abs,
        };
    }

    /// Consumes one inbound event while learning. A note press or a bend is captured immediately;
    /// CCs keep their value history so a wraparound can promote them to a relative encoder.
    pub fn feed(&mut self, event: &Event) -> GuideStep {
        let Some(capture) = self.capture.as_mut() else {
            return GuideStep::Ignored;
        };
        match *event {
            Event::ControlChange {
                channel,
                controller,
                value,
            } => {
                let mut mode = capture
                    .candidate
                    .as_ref()
                    .map_or(RelMode::Abs, |candidate| candidate.mode);
                let mut step = GuideStep::Captured;
                // Only a knob or a jog wheel can be an encoder. A pad's 127→0 release is not a
                // wraparound, and a fast fader sweep (20→127) is not one either: require the two
                // values to sit at opposite extremes.
                let wants_relative = capture.selector.is_none()
                    && (matches!(
                        capture.shape,
                        ControlShape::Knob | ControlShape::RelKnob
                    ) || (capture.shape == ControlShape::JogWheel
                        && capture.part == JogPart::Turn));
                if wants_relative && mode == RelMode::Abs {
                    if let Some(previous) = capture.last_cc {
                        let wrap = (previous <= 3 && value >= 124)
                            || (previous >= 124 && value <= 3);
                        if wrap {
                            // 1/127 two-point encoders use two's-complement, not offset binary.
                            mode = RelMode::Rel2;
                            step = GuideStep::RelativeDetected;
                        }
                    }
                }
                capture.last_cc = Some(value);
                capture.candidate = Some(Candidate {
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
                capture.last_cc = None;
                capture.candidate = Some(Candidate {
                    kind: EventKind::Note,
                    channel,
                    id: key,
                    mode: RelMode::Abs,
                });
                GuideStep::Captured
            }
            Event::NoteOn { .. } | Event::NoteOff { .. } => GuideStep::Ignored,
            Event::PitchBend { channel, .. } => {
                capture.last_cc = None;
                capture.candidate = Some(Candidate {
                    kind: EventKind::Bend,
                    channel,
                    id: 0,
                    mode: RelMode::Abs,
                });
                GuideStep::Captured
            }
        }
    }

    pub fn confirm_learn(&mut self) -> Result<(), String> {
        let Some(capture) = self.capture.take() else {
            return Err("nothing is being learned".to_owned());
        };
        let candidate = capture
            .candidate
            .clone()
            .ok_or_else(|| "press or turn a control first".to_owned())?;
        // A jog is two endpoints: remember the press, then wait for the wheel.
        if capture.selector.is_none()
            && capture.shape == ControlShape::JogWheel
            && capture.part == JogPart::Touch
        {
            if candidate.kind == EventKind::Cc && candidate.mode != RelMode::Abs {
                self.capture = Some(capture);
                return Err("that looks like the wheel; press the platter first".to_owned());
            }
            let mut next = capture;
            next.touch = Some(candidate);
            next.part = JogPart::Turn;
            next.candidate = None;
            next.last_cc = None;
            self.capture = Some(next);
            self.message = Some("now turn the wheel one detent (relative CC)".to_owned());
            return Ok(());
        }

        match self.commit_capture(&capture, &candidate) {
            Ok(()) => {
                self.message = Some(format!("bound {}", candidate.label()));
                Ok(())
            }
            Err(err) => {
                self.capture = Some(capture);
                Err(err)
            }
        }
    }

    fn commit_capture(&mut self, capture: &Capture, candidate: &Candidate) -> Result<(), String> {
        if let Some(selector) = &capture.selector {
            if candidate.kind == EventKind::Cc && candidate.mode != RelMode::Abs {
                return Err("a selector must be a note or an absolute CC".to_owned());
            }
            let raw = RawSelector {
                kind: candidate.kind,
                mode: if candidate.kind == EventKind::Cc {
                    Some(RelMode::Abs)
                } else {
                    None
                },
                reverse: None,
                channel: candidate.channel,
                id: candidate.id,
                behavior: selector.behavior,
                target: selector.target.clone(),
            };
            self.with_map(|map| {
                map.units[capture.unit].selectors.push(raw);
                Ok(())
            })?;
            self.stage = Stage::Selectors;
            self.selected_selector = self
                .compiled_unit()
                .map_or(0, |unit| unit.selectors.len().saturating_sub(1));
            return Ok(());
        }

        let touch = if capture.shape == ControlShape::JogWheel {
            let touch = capture
                .touch
                .clone()
                .ok_or_else(|| "the jog press note was not captured".to_owned())?;
            if candidate.kind == EventKind::Note && candidate.mode == RelMode::Abs {
                return Err("that is the press again; turn the wheel".to_owned());
            }
            if candidate.kind != EventKind::Cc || candidate.mode == RelMode::Abs {
                return Err("the jog turn must be a relative CC (turn it once more)".to_owned());
            }
            Some(touch)
        } else {
            None
        };


        let shape = control_shape_for(capture.shape, candidate);
        let raw = raw_control(&capture.slot, shape, candidate, touch.as_ref())?;
        self.with_map(|map| {
            let unit = &mut map.units[capture.unit];
            if let Some(existing) = unit.controls.iter_mut().find(|c| c.slot == capture.slot) {
                *existing = raw;
            } else {
                unit.controls.push(raw);
            }
            Ok(())
        })?;
        self.stage = Stage::Controls;
        Ok(())
    }

    // ---- modes ------------------------------------------------------------

    pub fn mode_names(&self) -> Vec<String> {
        self.compiled_unit()
            .map(|unit| unit.modes.iter().map(|mode| mode.name.clone()).collect())
            .unwrap_or_else(|| {
                let mut names: Vec<String> = self
                    .preset()
                    .map(|preset| preset.modes.iter().map(|mode| mode.name.clone()).collect())
                    .unwrap_or_default();
                if let Some(unit) = self.unit() {
                    for mode in &unit.modes {
                        if !names.contains(&mode.name) {
                            names.push(mode.name.clone());
                        }
                    }
                }
                names
            })
    }

    pub fn add_mode(&mut self, name: String) -> Result<(), String> {
        let name = name.trim().to_owned();
        validate_name("mode", &name)?;
        if self.mode_names().contains(&name) {
            return Err(format!("mode `{name}` already exists"));
        }
        let unit_index = self.selected_unit;
        self.with_map(|map| {
            let unit = &mut map.units[unit_index];
            unit.modes.push(RawMode {
                name: name.clone(),
                label: None,
                binds: Vec::new(),
            });
            if let Some(order) = &mut unit.mode_order {
                order.push(name.clone());
            }
            Ok(())
        })?;
        self.selected_mode = self.mode_names().len().saturating_sub(1);
        self.message = Some(format!("mode `{name}` added"));
        Ok(())
    }

    fn remove_selected_mode(&mut self) -> Result<(), String> {
        let Some(name) = self.selected_mode_name() else {
            return Ok(());
        };
        if let Some(selector) = self.compiled_unit().and_then(|unit| {
            unit.selectors.iter().find_map(|selector| match selector.behavior {
                crate::map::SelectorBehavior::Trigger { mode }
                    if unit.modes[mode].name == name =>
                {
                    Some(())
                }
                _ => None,
            })
        }) {
            let _ = selector;
            return Err(format!(
                "a trigger selector still targets `{name}`; delete/retarget it first"
            ));
        }
        let preset_has = self.preset_has_mode(&name);
        let had_order = self.unit().is_some_and(|unit| unit.mode_order.is_some());
        let order = self.mode_names();
        let next_start = order.iter().find(|candidate| **candidate != name).cloned();
        let unit_index = self.selected_unit;
        self.with_map(|map| {
            let unit = &mut map.units[unit_index];
            unit.modes.retain(|mode| mode.name != name);
            if preset_has || had_order {
                // Removing a preset-inherited mode needs an explicit whitelist; removing a mode
                // from a unit that already has an explicit order must rewrite that order too.
                let remaining: Vec<String> =
                    order.iter().filter(|candidate| **candidate != name).cloned().collect();
                if remaining.is_empty() {
                    return Err("a unit needs at least one mode".to_owned());
                }
                unit.mode_order = Some(remaining);
            }
            if unit.start.as_deref() == Some(name.as_str()) {
                unit.start = next_start.clone();
            }
            Ok(())
        })?;
        self.clamp_selection();
        self.message = Some(format!("mode `{name}` removed"));
        Ok(())
    }

    pub fn move_mode(&mut self, delta: isize) -> Result<(), String> {
        let mut order = self.mode_names();
        if order.len() < 2 {
            return Ok(());
        }
        let index = self
            .selected_mode
            .min(order.len() - 1);
        let target = (index as isize + delta).rem_euclid(order.len() as isize) as usize;
        order.swap(index, target);
        let unit_index = self.selected_unit;
        self.with_map(|map| {
            map.units[unit_index].mode_order = Some(order);
            Ok(())
        })?;
        self.selected_mode = target;
        Ok(())
    }

    pub fn set_start_mode(&mut self) -> Result<(), String> {
        let Some(name) = self.selected_mode_name() else {
            return Ok(());
        };
        let unit_index = self.selected_unit;
        self.with_map(|map| {
            map.units[unit_index].start = Some(name.clone());
            Ok(())
        })?;
        self.message = Some(format!("start mode is `{name}`"));
        Ok(())
    }

    // ---- actions ----------------------------------------------------------

    pub fn set_action(&mut self, text: String) -> Result<(), String> {
        let Some(mode) = self.selected_mode_name() else {
            return Err("select a mode first".to_owned());
        };
        let Some(slot) = self.selected_slot() else {
            return Err("select a slot first".to_owned());
        };
        let text = text.trim().to_owned();
        let preset_mode = self.preset_mode(&mode).cloned();
        let unit_index = self.selected_unit;
        self.with_map(|map| {
            let unit = &mut map.units[unit_index];
            ensure_unit_mode(unit, &mode, preset_mode.as_ref());
            let raw_mode = unit
                .modes
                .iter_mut()
                .find(|candidate| candidate.name == mode)
                .expect("mode was just created");
            if text.is_empty() {
                raw_mode.binds.retain(|bind| bind.slot != slot);
            } else if let Some(bind) = raw_mode.binds.iter_mut().find(|bind| bind.slot == slot) {
                bind.action = text.clone();
            } else {
                let mut bind = empty_bind(&slot);
                bind.action = text.clone();
                raw_mode.binds.push(bind);
            }
            Ok(())
        })?;
        self.message = Some(if text.is_empty() {
            format!("unbound `{slot}`")
        } else {
            format!("`{slot}` -> {text}")
        });
        Ok(())
    }

    pub fn set_on(&mut self, on: Option<Edge>) -> Result<(), String> {
        let Some(mode) = self.selected_mode_name() else {
            return Ok(());
        };
        let Some(slot) = self.selected_slot() else {
            return Ok(());
        };
        let preset_mode = self.preset_mode(&mode).cloned();
        let unit_index = self.selected_unit;
        self.with_map(|map| {
            let unit = &mut map.units[unit_index];
            ensure_unit_mode(unit, &mode, preset_mode.as_ref());
            let raw_mode = unit
                .modes
                .iter_mut()
                .find(|candidate| candidate.name == mode)
                .expect("mode was just created");
            let bind = raw_mode
                .binds
                .iter_mut()
                .find(|bind| bind.slot == slot)
                .ok_or_else(|| format!("`{slot}` has no action yet"))?;
            bind.on = on;
            Ok(())
        })?;
        Ok(())
    }

    fn unbind_action(&mut self) -> Result<(), String> {
        let Some(mode) = self.selected_mode_name() else {
            return Ok(());
        };
        let Some(slot) = self.selected_slot() else {
            return Ok(());
        };
        let preset_mode = self.preset_mode(&mode).cloned();
        let unit_index = self.selected_unit;
        self.with_map(|map| {
            let unit = &mut map.units[unit_index];
            ensure_unit_mode(unit, &mode, preset_mode.as_ref());
            let raw_mode = unit
                .modes
                .iter_mut()
                .find(|candidate| candidate.name == mode)
                .expect("mode was just created");
            raw_mode.binds.retain(|bind| bind.slot != slot);
            Ok(())
        })?;
        self.message = Some(format!("unbound `{slot}` in `{mode}`"));
        Ok(())
    }

    // ---- persistence ------------------------------------------------------

    pub fn save(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Err("no file yet; use Save to choose a path".to_owned());
        };
        self.recompile();
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let text = toml::to_string_pretty(&self.map).map_err(|err| err.to_string())?;
        let temp = format!("{path}.tmp");
        std::fs::write(&temp, text).map_err(|err| format!("{temp}: {err}"))?;
        std::fs::rename(&temp, &path).map_err(|err| format!("{path}: {err}"))?;
        self.dirty = false;
        self.message = Some(format!("saved {path}"));
        Ok(())
    }

    pub fn to_toml_string(&self) -> Result<String, String> {
        toml::to_string_pretty(&self.map).map_err(|err| err.to_string())
    }

    // ---- simplified tree editor ------------------------------------------

    /// The complete flat tree shown in the left pane.
    pub fn tree(&self) -> Vec<TreeNode> {
        let mut nodes = Vec::new();
        // Show every configured deck plus any deck a map already uses, even if `--decks` is
        // smaller. Otherwise a unit on deck 3 could be invisible and impossible to delete.
        let mut decks: Vec<DeckId> = (0..self.decks as u8).collect();
        for unit in &self.map.units {
            if let Some(deck) = unit.deck {
                if !decks.contains(&deck) {
                    decks.push(deck);
                }
            }
        }
        decks.sort_unstable();
        let groups: Vec<Option<DeckId>> = decks.into_iter().map(Some).chain(std::iter::once(None)).collect();
        for deck in groups {
            let units: Vec<usize> = self
                .map
                .units
                .iter()
                .enumerate()
                .filter(|(_, unit)| unit.deck == deck)
                .map(|(index, _)| index)
                .collect();
            nodes.push(TreeNode {
                kind: TreeNodeKind::Deck { deck },
                depth: 0,
                label: deck.map_or_else(|| "global".to_owned(), |deck| format!("deck{}", deck + 1)),
                hint: format!(
                    "{} unit{}",
                    units.len(),
                    if units.len() == 1 { "" } else { "s" }
                ),
            });
            for unit_index in units {
                let raw = &self.map.units[unit_index];
                nodes.push(TreeNode {
                    kind: TreeNodeKind::Unit { unit: unit_index },
                    depth: 1,
                    label: raw.id.clone(),
                    hint: raw
                        .preset
                        .clone()
                        .map_or_else(|| "no preset".to_owned(), |preset| format!("preset {preset}")),
                });
                let Some(unit) = self
                    .compiled
                    .as_ref()
                    .and_then(|map| map.units.get(unit_index))
                else {
                    continue;
                };
                for (control_index, control) in unit.controls.iter().enumerate() {
                    let endpoints = endpoint_labels(unit, control);
                    let hint = if endpoints.is_empty() {
                        format!("{:?} · unbound", control.shape)
                    } else {
                        format!("{:?} · {}", control.shape, endpoints.join(" + "))
                    };
                    nodes.push(TreeNode {
                        kind: TreeNodeKind::Control {
                            unit: unit_index,
                            control: control_index,
                        },
                        depth: 2,
                        label: control.slot.clone(),
                        hint,
                    });
                    if control.shape == ControlShape::JogWheel {
                        for (part, role, label) in [
                            (ControlPart::Press, crate::map::EndpointRole::Touch, "press"),
                            (ControlPart::Turn, crate::map::EndpointRole::Turn, "turn"),
                        ] {
                            let endpoint = control
                                .endpoints
                                .iter()
                                .map(|&index| &unit.endpoints[index])
                                .find(|endpoint| endpoint.role == role);
                            nodes.push(TreeNode {
                                kind: TreeNodeKind::ControlPart {
                                    unit: unit_index,
                                    control: control_index,
                                    part,
                                },
                                depth: 3,
                                label: label.to_owned(),
                                hint: endpoint.map_or_else(
                                    || "unbound".to_owned(),
                                    endpoint_label,
                                ),
                            });
                        }
                    }
                }
                for (mode_index, mode) in unit.modes.iter().enumerate() {
                    let binds = mode.binds.iter().flatten().count();
                    let mut hint = String::new();
                    if mode_index == unit.start {
                        hint.push_str("start · ");
                    }
                    hint.push_str(&format!(
                        "{} bind{} · {}",
                        binds,
                        if binds == 1 { "" } else { "s" },
                        self.mode_source_unit(unit_index, &mode.name)
                    ));
                    nodes.push(TreeNode {
                        kind: TreeNodeKind::Mode {
                            unit: unit_index,
                            mode: mode_index,
                        },
                        depth: 2,
                        label: mode.name.clone(),
                        hint,
                    });
                    for (control_index, control) in unit.controls.iter().enumerate() {
                        let action = self
                            .action_for_unit(unit_index, &mode.name, &control.slot)
                            .unwrap_or_else(|| "—".to_owned());
                        let on = self.on_for_unit(unit_index, &mode.name, &control.slot);
                        nodes.push(TreeNode {
                            kind: TreeNodeKind::Action {
                                unit: unit_index,
                                mode: mode_index,
                                control: control_index,
                            },
                            depth: 3,
                            label: format!("{} → {action}", control.slot),
                            hint: on.map_or_else(String::new, |edge| {
                                format!("on {}", format!("{edge:?}").to_lowercase())
                            }),
                        });
                    }
                }
                for (selector_index, selector) in unit.selectors.iter().enumerate() {
                    let hint = match selector.behavior {
                        crate::map::SelectorBehavior::Trigger { mode } => {
                            format!("trigger → {}", unit.modes[mode].name)
                        }
                        crate::map::SelectorBehavior::Cycle => "cycle".to_owned(),
                    };
                    nodes.push(TreeNode {
                        kind: TreeNodeKind::Selector {
                            unit: unit_index,
                            selector: selector_index,
                        },
                        depth: 2,
                        label: format!("sel {}", endpoint_label(&selector.endpoint)),
                        hint,
                    });
                }
            }
            nodes.push(TreeNode {
                kind: TreeNodeKind::AddUnit { deck },
                depth: 1,
                label: "+ add unit from preset".to_owned(),
                hint: String::new(),
            });
        }
        nodes
    }

    pub fn tree_view(&self) -> TreeView {
        let nodes = self.tree();
        let selected = self.selected_tree.min(nodes.len().saturating_sub(1));
        TreeView {
            path: self.path.clone(),
            device: self.device(),
            dirty: self.dirty,
            message: self.message.clone(),
            error: self.error.clone(),
            nodes,
            selected,
            details: self.tree_details(),
            endpoints: self.tree_endpoints(),
            buttons: self.tree_buttons(),
            capturing: self.capture.is_some(),
            candidate: self.candidate().cloned(),
        }
    }

    pub fn select_tree_row(&mut self, index: usize) {
        let nodes = self.tree();
        if nodes.is_empty() {
            self.selected_tree = 0;
            return;
        }
        let index = index.min(nodes.len() - 1);
        self.selected_tree = index;
        match nodes[index].kind {
            TreeNodeKind::Deck { .. } | TreeNodeKind::AddUnit { .. } => {
                self.stage = Stage::Units;
                self.selected_control = 0;
                self.selected_mode = 0;
                self.selected_selector = 0;
            }
            TreeNodeKind::Unit { unit } => {
                self.stage = Stage::Units;
                self.selected_unit = unit;
                self.selected_control = 0;
                self.selected_mode = 0;
                self.selected_selector = 0;
            }
            TreeNodeKind::Control { unit, control }
            | TreeNodeKind::ControlPart { unit, control, .. } => {
                self.stage = Stage::Controls;
                self.selected_unit = unit;
                self.selected_control = control;
            }
            TreeNodeKind::Mode { unit, mode } => {
                self.stage = Stage::Modes;
                self.selected_unit = unit;
                self.selected_mode = mode;
            }
            TreeNodeKind::Action {
                unit,
                mode,
                control,
            } => {
                self.stage = Stage::Actions;
                self.selected_unit = unit;
                self.selected_mode = mode;
                self.selected_control = control;
            }
            TreeNodeKind::Selector { unit, selector } => {
                self.stage = Stage::Selectors;
                self.selected_unit = unit;
                self.selected_selector = selector;
            }
        }
    }

    pub fn move_tree_selection(&mut self, delta: isize) {
        let len = self.tree().len();
        if len == 0 {
            return;
        }
        let index = (self.selected_tree as isize + delta).rem_euclid(len as isize) as usize;
        self.select_tree_row(index);
    }

    pub fn selected_tree_kind(&self) -> Option<TreeNodeKind> {
        let nodes = self.tree();
        nodes
            .get(self.selected_tree.min(nodes.len().saturating_sub(1)))
            .map(|node| node.kind)
    }

    fn select_tree_kind(&mut self, kind: TreeNodeKind) {
        if let Some(index) = self.tree().iter().position(|node| node.kind == kind) {
            self.select_tree_row(index);
        } else {
            let len = self.tree().len();
            self.selected_tree = self.selected_tree.min(len.saturating_sub(1));
            self.select_tree_row(self.selected_tree);
        }
    }

    fn tree_details(&self) -> Vec<(String, String)> {
        let Some(kind) = self.selected_tree_kind() else {
            return Vec::new();
        };
        match kind {
            TreeNodeKind::Deck { deck } => {
                let units = self
                    .map
                    .units
                    .iter()
                    .filter(|unit| unit.deck == deck)
                    .count();
                vec![
                    (
                        "group".to_owned(),
                        deck.map_or_else(|| "global".to_owned(), |deck| format!("deck{}", deck + 1)),
                    ),
                    ("units".to_owned(), units.to_string()),
                ]
            }
            TreeNodeKind::AddUnit { deck } => vec![
                (
                    "deck".to_owned(),
                    deck.map_or_else(|| "global".to_owned(), |deck| format!("deck{}", deck + 1)),
                ),
                ("action".to_owned(), "New unit (choose a preset)".to_owned()),
            ],
            TreeNodeKind::Unit { unit } => {
                let raw = &self.map.units[unit];
                let compiled = self.compiled.as_ref().and_then(|map| map.units.get(unit));
                vec![
                    ("unit".to_owned(), raw.id.clone()),
                    (
                        "deck".to_owned(),
                        raw.deck
                            .map_or_else(|| "global".to_owned(), |deck| format!("deck{}", deck + 1)),
                    ),
                    (
                        "preset".to_owned(),
                        raw.preset.clone().unwrap_or_else(|| "—".to_owned()),
                    ),
                    (
                        "modes".to_owned(),
                        compiled
                            .map(|unit| {
                                unit.modes
                                    .iter()
                                    .map(|mode| mode.name.clone())
                                    .collect::<Vec<_>>()
                                    .join(" | ")
                            })
                            .unwrap_or_default(),
                    ),
                    (
                        "selectors".to_owned(),
                        compiled.map_or(0, |unit| unit.selectors.len()).to_string(),
                    ),
                ]
            }
            TreeNodeKind::Control { unit, control } => {
                let Some(compiled) = self.compiled.as_ref().and_then(|map| map.units.get(unit))
                else {
                    return Vec::new();
                };
                let Some(control_spec) = compiled.controls.get(control) else {
                    return Vec::new();
                };
                let endpoints = endpoint_labels(compiled, control_spec);
                let mut details = vec![
                    ("slot".to_owned(), control_spec.slot.clone()),
                    ("shape".to_owned(), format!("{:?}", control_spec.shape)),
                    (
                        "endpoints".to_owned(),
                        if endpoints.is_empty() {
                            "unbound".to_owned()
                        } else {
                            endpoints.join(" + ")
                        },
                    ),
                ];
                for mode in &compiled.modes {
                    details.push((
                        mode.name.clone(),
                        self.action_for_unit(unit, &mode.name, &control_spec.slot)
                            .unwrap_or_else(|| "—".to_owned()),
                    ));
                }
                details
            }
            TreeNodeKind::ControlPart {
                unit,
                control,
                part,
            } => {
                let Some(compiled) = self.compiled.as_ref().and_then(|map| map.units.get(unit))
                else {
                    return Vec::new();
                };
                let Some(control_spec) = compiled.controls.get(control) else {
                    return Vec::new();
                };
                let role = match part {
                    ControlPart::Press => crate::map::EndpointRole::Touch,
                    ControlPart::Turn => crate::map::EndpointRole::Turn,
                };
                let endpoint = control_spec
                    .endpoints
                    .iter()
                    .map(|&index| &compiled.endpoints[index])
                    .find(|endpoint| endpoint.role == role);
                vec![
                    ("slot".to_owned(), control_spec.slot.clone()),
                    (
                        "part".to_owned(),
                        match part {
                            ControlPart::Press => "press".to_owned(),
                            ControlPart::Turn => "turn".to_owned(),
                        },
                    ),
                    (
                        "endpoint".to_owned(),
                        endpoint.map_or_else(|| "unbound".to_owned(), endpoint_label),
                    ),
                ]
            }
            TreeNodeKind::Mode { unit, mode } => {
                let Some(compiled) = self.compiled.as_ref().and_then(|map| map.units.get(unit))
                else {
                    return Vec::new();
                };
                let Some(mode_spec) = compiled.modes.get(mode) else {
                    return Vec::new();
                };
                let binds = mode_spec.binds.iter().flatten().count();
                vec![
                    ("mode".to_owned(), mode_spec.name.clone()),
                    ("source".to_owned(), self.mode_source_unit(unit, &mode_spec.name)),
                    (
                        "start".to_owned(),
                        if mode == compiled.start {
                            "yes".to_owned()
                        } else {
                            "no".to_owned()
                        },
                    ),
                    ("binds".to_owned(), binds.to_string()),
                ]
            }
            TreeNodeKind::Action {
                unit,
                mode,
                control,
            } => {
                let Some(compiled) = self.compiled.as_ref().and_then(|map| map.units.get(unit))
                else {
                    return Vec::new();
                };
                let (Some(mode_spec), Some(control_spec)) =
                    (compiled.modes.get(mode), compiled.controls.get(control))
                else {
                    return Vec::new();
                };
                let mut details = vec![
                    ("mode".to_owned(), mode_spec.name.clone()),
                    ("slot".to_owned(), control_spec.slot.clone()),
                    (
                        "action".to_owned(),
                        self.action_for_unit(unit, &mode_spec.name, &control_spec.slot)
                            .unwrap_or_else(|| "—".to_owned()),
                    ),
                ];
                if let Some(edge) = self.on_for_unit(unit, &mode_spec.name, &control_spec.slot) {
                    details.push(("on".to_owned(), format!("{edge:?}").to_lowercase()));
                }
                let endpoints = endpoint_labels(compiled, control_spec);
                details.push((
                    "endpoint".to_owned(),
                    if endpoints.is_empty() {
                        "unbound".to_owned()
                    } else {
                        endpoints.join(" + ")
                    },
                ));
                details
            }
            TreeNodeKind::Selector { unit, selector } => {
                let Some(compiled) = self.compiled.as_ref().and_then(|map| map.units.get(unit))
                else {
                    return Vec::new();
                };
                let Some(selector_spec) = compiled.selectors.get(selector) else {
                    return Vec::new();
                };
                vec![
                    ("endpoint".to_owned(), endpoint_label(&selector_spec.endpoint)),
                    (
                        "behavior".to_owned(),
                        match selector_spec.behavior {
                            crate::map::SelectorBehavior::Trigger { mode } => {
                                format!("trigger → {}", compiled.modes[mode].name)
                            }
                            crate::map::SelectorBehavior::Cycle => "cycle".to_owned(),
                        },
                    ),
                ]
            }
        }
    }

    fn tree_endpoints(&self) -> Vec<(EventKind, u8, u8)> {
        let Some(kind) = self.selected_tree_kind() else {
            return Vec::new();
        };
        let Some(compiled) = self.compiled.as_ref() else {
            return Vec::new();
        };
        match kind {
            TreeNodeKind::Control { unit, control } | TreeNodeKind::Action { unit, control, .. } => {
                let Some(unit) = compiled.units.get(unit) else {
                    return Vec::new();
                };
                let Some(control) = unit.controls.get(control) else {
                    return Vec::new();
                };
                control
                    .endpoints
                    .iter()
                    .map(|&index| unit.endpoints[index].key())
                    .collect()
            }
            TreeNodeKind::ControlPart {
                unit,
                control,
                part,
            } => {
                let Some(unit) = compiled.units.get(unit) else {
                    return Vec::new();
                };
                let Some(control) = unit.controls.get(control) else {
                    return Vec::new();
                };
                let role = match part {
                    ControlPart::Press => crate::map::EndpointRole::Touch,
                    ControlPart::Turn => crate::map::EndpointRole::Turn,
                };
                control
                    .endpoints
                    .iter()
                    .map(|&index| &unit.endpoints[index])
                    .filter(|endpoint| endpoint.role == role)
                    .map(|endpoint| endpoint.key())
                    .collect()
            }
            TreeNodeKind::Selector { unit, selector } => compiled
                .units
                .get(unit)
                .and_then(|unit| unit.selectors.get(selector))
                .map(|selector| vec![selector.endpoint.key()])
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn tree_buttons(&self) -> Vec<TreeButton> {
        if self.capture.is_some() {
            return vec![
                TreeButton::Confirm,
                TreeButton::Cancel,
                TreeButton::Mode,
                TreeButton::Connect,
                TreeButton::Open,
                TreeButton::Save,
                TreeButton::Exit,
            ];
        }
        let Some(kind) = self.selected_tree_kind() else {
            return vec![TreeButton::Connect, TreeButton::Open, TreeButton::Save, TreeButton::Exit];
        };
        let mut buttons = match kind {
            TreeNodeKind::Deck { .. } | TreeNodeKind::AddUnit { .. } => {
                vec![TreeButton::NewUnit]
            }
            TreeNodeKind::Unit { unit } => {
                let mut buttons = vec![TreeButton::NewUnit];
                if self.map.units.get(unit).is_some_and(|unit| unit.deck.is_some()) {
                    buttons.push(TreeButton::Clone);
                }
                buttons.extend([
                    TreeButton::NewMode,
                    TreeButton::NewSelector,
                    TreeButton::Delete,
                ]);
                buttons
            }
            TreeNodeKind::Control { .. } => vec![TreeButton::Learn, TreeButton::Delete],
            TreeNodeKind::ControlPart { .. } => vec![TreeButton::Learn],
            TreeNodeKind::Mode { .. } => vec![
                TreeButton::NewMode,
                TreeButton::Delete,
                TreeButton::Up,
                TreeButton::Down,
                TreeButton::SetStart,
            ],
            TreeNodeKind::Action { .. } => {
                vec![TreeButton::Edit, TreeButton::On, TreeButton::Delete]
            }
            TreeNodeKind::Selector { .. } => vec![
                TreeButton::Edit,
                TreeButton::NewSelector,
                TreeButton::Delete,
            ],
        };
        buttons.extend([
            TreeButton::Connect,
            TreeButton::Open,
            TreeButton::Save,
            TreeButton::Exit,
        ]);
        buttons
    }

    pub fn tree_selected_unit_deck(&self) -> Option<DeckId> {
        match self.selected_tree_kind()? {
            TreeNodeKind::Deck { deck } | TreeNodeKind::AddUnit { deck } => deck,
            TreeNodeKind::Unit { unit }
            | TreeNodeKind::Control { unit, .. }
            | TreeNodeKind::ControlPart { unit, .. }
            | TreeNodeKind::Mode { unit, .. }
            | TreeNodeKind::Action { unit, .. }
            | TreeNodeKind::Selector { unit, .. } => self.map.units.get(unit)?.deck,
        }
    }

    pub fn tree_new_unit(
        &mut self,
        preset: &str,
        deck: DeckId,
        id: Option<String>,
    ) -> Result<(), String> {
        self.new_unit(preset, deck, id)?;
        let unit = self.map.units.len() - 1;
        self.select_tree_kind(TreeNodeKind::Unit { unit });
        Ok(())
    }

    pub fn tree_new_global_unit(
        &mut self,
        preset: &str,
        id: Option<String>,
    ) -> Result<(), String> {
        self.new_global_unit(preset, id)?;
        let unit = self.map.units.len() - 1;
        self.select_tree_kind(TreeNodeKind::Unit { unit });
        Ok(())
    }

    pub fn tree_clone(&mut self, target_deck: DeckId) -> Result<(), String> {
        let TreeNodeKind::Unit { unit } = self.selected_tree_kind().ok_or_else(|| {
            "select a unit to clone".to_owned()
        })? else {
            return Err("select a unit to clone".to_owned());
        };
        self.selected_unit = unit;
        self.clone_unit(target_deck)?;
        let new = self.map.units.len() - 1;
        self.select_tree_kind(TreeNodeKind::Unit { unit: new });
        Ok(())
    }

    pub fn tree_begin_learn(&mut self) -> Result<(), String> {
        let TreeNodeKind::Control { unit, control } = self.selected_tree_kind().ok_or_else(|| {
            "select a control slot to learn".to_owned()
        })? else {
            return Err("select a control slot to learn".to_owned());
        };
        self.selected_unit = unit;
        self.selected_control = control;
        self.stage = Stage::Controls;
        self.begin_learn()
    }

    pub fn tree_confirm_learn(&mut self) -> Result<(), String> {
        let before = self.capture.clone();
        self.confirm_learn()?;
        if self.capture.is_some() {
            return Ok(());
        }
        let Some(capture) = before else {
            return Ok(());
        };
        if let Some(selector) = capture.selector {
            let unit = capture.unit;
            let index = self
                .compiled
                .as_ref()
                .and_then(|map| map.units.get(unit))
                .map_or(0, |unit| unit.selectors.len().saturating_sub(1));
            let _ = selector;
            self.select_tree_kind(TreeNodeKind::Selector {
                unit,
                selector: index,
            });
        } else if let Some(compiled) = self.compiled.as_ref().and_then(|map| map.units.get(capture.unit)) {
            if let Some(control) = compiled.controls.iter().position(|control| control.slot == capture.slot) {
                self.select_tree_kind(TreeNodeKind::Control {
                    unit: capture.unit,
                    control,
                });
            }
        }
        Ok(())
    }

    pub fn tree_begin_selector(
        &mut self,
        behavior: RawSelectorBehavior,
        target: Option<String>,
    ) -> Result<(), String> {
        let unit = match self.selected_tree_kind() {
            Some(TreeNodeKind::Unit { unit })
            | Some(TreeNodeKind::Selector { unit, .. }) => unit,
            _ => return Err("select a unit or selector first".to_owned()),
        };
        self.selected_unit = unit;
        self.stage = Stage::Selectors;
        self.begin_selector(behavior, target)
    }

    pub fn tree_set_selector_behavior(
        &mut self,
        behavior: RawSelectorBehavior,
        target: Option<String>,
    ) -> Result<(), String> {
        let TreeNodeKind::Selector { unit, selector } = self.selected_tree_kind().ok_or_else(|| {
            "select a selector first".to_owned()
        })? else {
            return Err("select a selector first".to_owned());
        };
        if matches!(behavior, RawSelectorBehavior::Trigger) {
            let target = target.as_deref().ok_or_else(|| {
                "a trigger selector needs a target mode".to_owned()
            })?;
            if !self
                .compiled
                .as_ref()
                .and_then(|map| map.units.get(unit))
                .is_some_and(|unit| unit.modes.iter().any(|mode| mode.name == target))
            {
                return Err(format!("unknown mode `{target}`"));
            }
        }
        self.with_map(|map| {
            let raw = map
                .units
                .get_mut(unit)
                .and_then(|unit| unit.selectors.get_mut(selector))
                .ok_or_else(|| "selector no longer exists".to_owned())?;
            raw.behavior = behavior;
            raw.target = target;
            Ok(())
        })?;
        self.select_tree_kind(TreeNodeKind::Selector { unit, selector });
        Ok(())
    }

    pub fn tree_add_mode(&mut self, name: String) -> Result<(), String> {
        let unit = match self.selected_tree_kind() {
            Some(TreeNodeKind::Unit { unit })
            | Some(TreeNodeKind::Mode { unit, .. })
            | Some(TreeNodeKind::Action { unit, .. }) => unit,
            _ => return Err("select a unit or mode first".to_owned()),
        };
        self.selected_unit = unit;
        self.stage = Stage::Modes;
        self.add_mode(name.clone())?;
        let mode = self
            .compiled
            .as_ref()
            .and_then(|map| map.units.get(unit))
            .and_then(|unit| unit.modes.iter().position(|mode| mode.name == name))
            .unwrap_or(0);
        self.select_tree_kind(TreeNodeKind::Mode { unit, mode });
        Ok(())
    }

    pub fn tree_delete(&mut self) -> Result<(), String> {
        let Some(kind) = self.selected_tree_kind() else {
            return Ok(());
        };
        match kind {
            TreeNodeKind::Deck { .. }
            | TreeNodeKind::AddUnit { .. }
            | TreeNodeKind::ControlPart { .. } => return Ok(()),
            TreeNodeKind::Unit { unit } => {
                let deck = self.map.units.get(unit).and_then(|unit| unit.deck);
                self.selected_unit = unit;
                self.stage = Stage::Units;
                self.remove_selected()?;
                self.select_tree_kind(TreeNodeKind::AddUnit { deck });
            }
            TreeNodeKind::Control { unit, control } => {
                self.selected_unit = unit;
                self.selected_control = control;
                self.stage = Stage::Controls;
                self.remove_selected()?;
                self.select_tree_kind(TreeNodeKind::Unit { unit });
            }
            TreeNodeKind::Mode { unit, mode } => {
                self.selected_unit = unit;
                self.selected_mode = mode;
                self.stage = Stage::Modes;
                self.remove_selected()?;
                self.select_tree_kind(TreeNodeKind::Unit { unit });
            }
            TreeNodeKind::Action {
                unit,
                mode,
                control,
            } => {
                self.selected_unit = unit;
                self.selected_mode = mode;
                self.selected_control = control;
                self.stage = Stage::Actions;
                self.remove_selected()?;
                self.select_tree_kind(TreeNodeKind::Action {
                    unit,
                    mode,
                    control,
                });
            }
            TreeNodeKind::Selector { unit, selector } => {
                self.selected_unit = unit;
                self.selected_selector = selector;
                self.stage = Stage::Selectors;
                self.remove_selected()?;
                self.select_tree_kind(TreeNodeKind::Unit { unit });
            }
        }
        Ok(())
    }

    pub fn tree_move_mode(&mut self, delta: isize) -> Result<(), String> {
        let TreeNodeKind::Mode { unit, mode } = self.selected_tree_kind().ok_or_else(|| {
            "select a mode first".to_owned()
        })? else {
            return Err("select a mode first".to_owned());
        };
        let name = self
            .compiled
            .as_ref()
            .and_then(|map| map.units.get(unit))
            .and_then(|unit| unit.modes.get(mode))
            .map(|mode| mode.name.clone())
            .ok_or_else(|| "mode no longer exists".to_owned())?;
        self.selected_unit = unit;
        self.selected_mode = mode;
        self.stage = Stage::Modes;
        self.move_mode(delta)?;
        let new_index = self
            .compiled
            .as_ref()
            .and_then(|map| map.units.get(unit))
            .and_then(|unit| unit.modes.iter().position(|candidate| candidate.name == name))
            .unwrap_or(0);
        self.select_tree_kind(TreeNodeKind::Mode {
            unit,
            mode: new_index,
        });
        Ok(())
    }

    pub fn tree_set_start(&mut self) -> Result<(), String> {
        let TreeNodeKind::Mode { unit, mode } = self.selected_tree_kind().ok_or_else(|| {
            "select a mode first".to_owned()
        })? else {
            return Err("select a mode first".to_owned());
        };
        self.selected_unit = unit;
        self.selected_mode = mode;
        self.stage = Stage::Modes;
        self.set_start_mode()?;
        self.select_tree_kind(TreeNodeKind::Mode { unit, mode });
        Ok(())
    }

    pub fn tree_action_text(&self) -> Option<String> {
        let TreeNodeKind::Action { unit, mode, control } = self.selected_tree_kind()? else {
            return None;
        };
        let compiled = self.compiled.as_ref()?.units.get(unit)?;
        let mode = compiled.modes.get(mode)?;
        let control = compiled.controls.get(control)?;
        self.action_for_unit(unit, &mode.name, &control.slot)
    }

    pub fn tree_on(&self) -> Option<Edge> {
        let TreeNodeKind::Action { unit, mode, control } = self.selected_tree_kind()? else {
            return None;
        };
        let compiled = self.compiled.as_ref()?.units.get(unit)?;
        let mode = compiled.modes.get(mode)?;
        let control = compiled.controls.get(control)?;
        self.on_for_unit(unit, &mode.name, &control.slot)
    }

    pub fn tree_set_action(&mut self, text: String) -> Result<(), String> {
        let TreeNodeKind::Action { unit, mode, control } = self.selected_tree_kind().ok_or_else(|| {
            "select an action row first".to_owned()
        })? else {
            return Err("select an action row first".to_owned());
        };
        self.selected_unit = unit;
        self.selected_mode = mode;
        self.selected_control = control;
        self.stage = Stage::Actions;
        self.set_action(text)?;
        self.select_tree_kind(TreeNodeKind::Action {
            unit,
            mode,
            control,
        });
        Ok(())
    }

    pub fn tree_set_on(&mut self, on: Option<Edge>) -> Result<(), String> {
        let TreeNodeKind::Action { unit, mode, control } = self.selected_tree_kind().ok_or_else(|| {
            "select an action row first".to_owned()
        })? else {
            return Err("select an action row first".to_owned());
        };
        self.selected_unit = unit;
        self.selected_mode = mode;
        self.selected_control = control;
        self.stage = Stage::Actions;
        self.set_on(on)?;
        self.select_tree_kind(TreeNodeKind::Action {
            unit,
            mode,
            control,
        });
        Ok(())
    }

    // ---- rows / details ---------------------------------------------------

    fn rows(&self) -> Vec<Row> {
        match self.stage {
            Stage::Setup => vec![
                Row {
                    kind: RowKind::Setup,
                    index: 0,
                    label: "file".to_owned(),
                    hint: self.path.clone().unwrap_or_else(|| "<none>".to_owned()),
                },
                Row {
                    kind: RowKind::Setup,
                    index: 1,
                    label: "device".to_owned(),
                    hint: self.device().unwrap_or_else(|| "<none>".to_owned()),
                },
            ],
            Stage::Units => self
                .map
                .units
                .iter()
                .enumerate()
                .map(|(index, unit)| {
                    let compiled = self
                        .compiled
                        .as_ref()
                        .and_then(|map| map.units.get(index));
                    let (controls, modes) = compiled.map_or_else(
                        || (unit.controls.len(), unit.modes.len()),
                        |unit| (unit.controls.len(), unit.modes.len()),
                    );
                    Row {
                        kind: RowKind::Unit,
                        index,
                        label: unit.id.clone(),
                        hint: format!(
                            "{} · preset {} · {} controls · {} modes",
                            unit.deck
                                .map_or_else(|| "global".to_owned(), |deck| format!("deck{}", deck + 1)),
                            unit.preset.as_deref().unwrap_or("—"),
                            controls,
                            modes
                        ),
                    }
                })
                .collect(),
            Stage::Controls | Stage::Actions => self
                .compiled_unit()
                .map(|unit| {
                    unit.controls
                        .iter()
                        .enumerate()
                        .map(|(index, control)| {
                            let endpoints = endpoint_labels(unit, control);
                            let hint = if self.stage == Stage::Actions {
                                self.selected_mode_name()
                                    .and_then(|mode| self.action_for(&mode, &control.slot))
                                    .unwrap_or_else(|| "—".to_owned())
                            } else if endpoints.is_empty() {
                                format!("{:?} · unbound", control.shape)
                            } else {
                                format!("{:?} · {}", control.shape, endpoints.join(" + "))
                            };
                            Row {
                                kind: if self.stage == Stage::Actions {
                                    RowKind::Bind
                                } else {
                                    RowKind::Control
                                },
                                index,
                                label: control.slot.clone(),
                                hint,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Stage::Modes => self
                .compiled_unit()
                .map(|unit| {
                    unit.modes
                        .iter()
                        .enumerate()
                        .map(|(index, mode)| {
                            let binds = mode.binds.iter().flatten().count();
                            Row {
                                kind: RowKind::Mode,
                                index,
                                label: mode.name.clone(),
                                hint: format!(
                                    "{}{} bind{} · {}",
                                    if index == unit.start { "start · " } else { "" },
                                    binds,
                                    if binds == 1 { "" } else { "s" },
                                    self.mode_source(&mode.name)
                                ),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Stage::Selectors => self
                .compiled_unit()
                .map(|unit| {
                    unit.selectors
                        .iter()
                        .enumerate()
                        .map(|(index, selector)| Row {
                            kind: RowKind::Selector,
                            index,
                            label: endpoint_label(&selector.endpoint),
                            hint: match selector.behavior {
                                crate::map::SelectorBehavior::Trigger { mode } => {
                                    format!("trigger → {}", unit.modes[mode].name)
                                }
                                crate::map::SelectorBehavior::Cycle => "cycle".to_owned(),
                            },
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Stage::Save => Vec::new(),
        }
    }

    fn details(&self) -> Vec<(String, String)> {
        match self.stage {
            Stage::Setup => vec![
                ("file".to_owned(), self.path.clone().unwrap_or_else(|| "<none>".to_owned())),
                (
                    "device".to_owned(),
                    self.device().unwrap_or_else(|| "<none>".to_owned()),
                ),
            ],
            Stage::Units => {
                let Some(unit) = self.unit() else {
                    return Vec::new();
                };
                vec![
                    ("unit".to_owned(), unit.id.clone()),
                    (
                        "deck".to_owned(),
                        unit.deck
                            .map_or_else(|| "global".to_owned(), |deck| format!("deck{}", deck + 1)),
                    ),
                    (
                        "preset".to_owned(),
                        unit.preset.clone().unwrap_or_else(|| "—".to_owned()),
                    ),
                    (
                        "start".to_owned(),
                        unit.start.clone().unwrap_or_else(|| "default".to_owned()),
                    ),
                    (
                        "modes".to_owned(),
                        self.mode_names().join(" | "),
                    ),
                ]
            }
            Stage::Controls | Stage::Actions => {
                let Some(unit) = self.compiled_unit() else {
                    return Vec::new();
                };
                let Some(control) = unit.controls.get(self.selected_control) else {
                    return Vec::new();
                };
                let mut details = vec![
                    ("slot".to_owned(), control.slot.clone()),
                    ("shape".to_owned(), format!("{:?}", control.shape)),
                    (
                        "endpoints".to_owned(),
                        {
                            let labels = endpoint_labels(unit, control);
                            if labels.is_empty() {
                                "unbound".to_owned()
                            } else {
                                labels.join(" + ")
                            }
                        },
                    ),
                ];
                if let Some(mode) = self.selected_mode_name() {
                    details.push((
                        "action".to_owned(),
                        self.action_for(&mode, &control.slot)
                            .unwrap_or_else(|| "—".to_owned()),
                    ));
                    if let Some(on) = self.current_on() {
                        details.push(("on".to_owned(), format!("{on:?}").to_lowercase()));
                    }
                }
                details
            }
            Stage::Modes => {
                let Some(name) = self.selected_mode_name() else {
                    return Vec::new();
                };
                let Some(unit) = self.compiled_unit() else {
                    return Vec::new();
                };
                if unit.modes.get(self.selected_mode).is_none() {
                    return Vec::new();
                }
                vec![
                    ("mode".to_owned(), name.clone()),
                    ("source".to_owned(), self.mode_source(&name)),
                    (
                        "start".to_owned(),
                        if self.selected_mode == unit.start {
                            "yes".to_owned()
                        } else {
                            "no".to_owned()
                        },
                    ),
                    (
                        "binds".to_owned(),
                        unit.controls
                            .iter()
                            .filter_map(|control| {
                                self.action_for(&name, &control.slot)
                                    .map(|action| format!("{}: {action}", control.slot))
                            })
                            .collect::<Vec<_>>()
                            .join(" | "),
                    ),
                ]
            }
            Stage::Selectors => {
                let Some(unit) = self.compiled_unit() else {
                    return Vec::new();
                };
                let Some(selector) = unit.selectors.get(self.selected_selector) else {
                    return Vec::new();
                };
                vec![
                    ("endpoint".to_owned(), endpoint_label(&selector.endpoint)),
                    (
                        "behavior".to_owned(),
                        match selector.behavior {
                            crate::map::SelectorBehavior::Trigger { mode } => {
                                format!("trigger → {}", unit.modes[mode].name)
                            }
                            crate::map::SelectorBehavior::Cycle => "cycle".to_owned(),
                        },
                    ),
                ]
            }
            Stage::Save => vec![(
                "validation".to_owned(),
                self.error
                    .clone()
                    .unwrap_or_else(|| "valid".to_owned()),
            )],
        }
    }

    fn selected_endpoints(&self) -> Vec<(EventKind, u8, u8)> {
        let Some(unit) = self.compiled_unit() else {
            return Vec::new();
        };
        match self.stage {
            Stage::Controls | Stage::Actions => {
                let Some(control) = unit.controls.get(self.selected_control) else {
                    return Vec::new();
                };
                control
                    .endpoints
                    .iter()
                    .map(|&index| unit.endpoints[index].key())
                    .collect()
            }
            Stage::Selectors => unit
                .selectors
                .get(self.selected_selector)
                .map(|selector| vec![selector.endpoint.key()])
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn buttons(&self) -> Vec<Button> {
        if self.capture.is_some() {
            return vec![Button::Confirm, Button::Cancel, Button::Mode, Button::Prev, Button::Next];
        }
        let mut buttons = match self.stage {
            Stage::Setup => vec![Button::File, Button::Device],
            Stage::Units => vec![Button::New, Button::Clone, Button::Delete],
            Stage::Controls => vec![Button::Learn, Button::Delete],
            Stage::Modes => vec![
                Button::New,
                Button::Delete,
                Button::Up,
                Button::Down,
                Button::Start,
            ],
            Stage::Selectors => vec![Button::Trigger, Button::Cycle, Button::Delete],
            Stage::Actions => vec![Button::Edit, Button::On],
            Stage::Save => vec![Button::Save],
        };
        // Stage navigation sits at the right end, like cfdisk's persistent buttons.
        buttons.extend([Button::Prev, Button::Next]);
        buttons
    }

    // ---- internals --------------------------------------------------------

    fn unit(&self) -> Option<&RawUnit> {
        self.map.units.get(self.selected_unit)
    }

    fn compiled_unit(&self) -> Option<&crate::map::Unit> {
        self.compiled.as_ref()?.units.get(self.selected_unit)
    }

    fn preset(&self) -> Option<&Preset> {
        let name = self.unit()?.preset.as_deref()?;
        self.presets.get(name)
    }

    fn preset_mode(&self, name: &str) -> Option<&RawMode> {
        self.preset()?
            .modes
            .iter()
            .find(|mode| mode.name == name)
    }

    fn preset_has_mode(&self, name: &str) -> bool {
        self.preset_mode(name).is_some()
    }

    fn mode_source(&self, name: &str) -> String {
        self.mode_source_unit(self.selected_unit, name)
    }

    fn mode_source_unit(&self, unit_index: usize, name: &str) -> String {
        if self
            .map
            .units
            .get(unit_index)
            .is_some_and(|unit| unit.modes.iter().any(|mode| mode.name == name))
        {
            "unit".to_owned()
        } else if self
            .map
            .units
            .get(unit_index)
            .and_then(|unit| unit.preset.as_deref())
            .and_then(|preset| self.presets.get(preset))
            .is_some_and(|preset| preset.modes.iter().any(|mode| mode.name == name))
        {
            "preset".to_owned()
        } else {
            "—".to_owned()
        }
    }

    fn action_for(&self, mode: &str, slot: &str) -> Option<String> {
        self.action_for_unit(self.selected_unit, mode, slot)
    }

    fn action_for_unit(&self, unit_index: usize, mode: &str, slot: &str) -> Option<String> {
        let unit = self.map.units.get(unit_index)?;
        if let Some(raw_mode) = unit.modes.iter().find(|candidate| candidate.name == mode) {
            // A unit mode replaces the preset mode; do not resurrect a preset bind the unit
            // dropped.
            return raw_mode
                .binds
                .iter()
                .find(|bind| bind.slot == slot)
                .map(|bind| bind.action.clone());
        }
        unit.preset
            .as_deref()
            .and_then(|name| self.presets.get(name))
            .and_then(|preset| preset.modes.iter().find(|candidate| candidate.name == mode))
            .and_then(|mode| mode.binds.iter().find(|bind| bind.slot == slot))
            .map(|bind| bind.action.clone())
    }

    fn on_for_unit(&self, unit_index: usize, mode: &str, slot: &str) -> Option<Edge> {
        let unit = self.map.units.get(unit_index)?;
        if let Some(raw_mode) = unit.modes.iter().find(|candidate| candidate.name == mode) {
            return raw_mode
                .binds
                .iter()
                .find(|bind| bind.slot == slot)?
                .on;
        }
        unit.preset
            .as_deref()
            .and_then(|name| self.presets.get(name))
            .and_then(|preset| preset.modes.iter().find(|candidate| candidate.name == mode))
            .and_then(|mode| mode.binds.iter().find(|bind| bind.slot == slot))?
            .on
    }

    fn unique_unit_id(&self, base: String) -> String {
        if !self.map.units.iter().any(|unit| unit.id == base) {
            return base;
        }
        for n in 2.. {
            let candidate = format!("{base}-{n}");
            if !self.map.units.iter().any(|unit| unit.id == candidate) {
                return candidate;
            }
        }
        unreachable!()
    }

    fn recompile(&mut self) {
        let text = match toml::to_string(&self.map) {
            Ok(text) => text,
            Err(err) => {
                self.error = Some(err.to_string());
                self.compiled = None;
                return;
            }
        };
        match Map::from_toml_str_at(&text, &self.base) {
            Ok(map) => {
                self.compiled = Some(map);
                self.error = None;
            }
            Err(err) => {
                self.compiled = None;
                self.error = Some(err.to_string());
            }
        }
    }

    fn with_map(
        &mut self,
        mutate: impl FnOnce(&mut MapFile) -> Result<(), String>,
    ) -> Result<(), String> {
        let old = self.map.clone();
        if let Err(err) = mutate(&mut self.map) {
            self.map = old;
            self.recompile();
            return Err(err);
        }
        self.recompile();
        if self.compiled.is_none() {
            let error = self
                .error
                .clone()
                .unwrap_or_else(|| "the map is not valid".to_owned());
            self.map = old;
            self.recompile();
            return Err(error);
        }
        self.dirty = true;
        self.clamp_selection();
        Ok(())
    }
}

fn parent_dir(path: &str) -> PathBuf {
    Path::new(path)
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn validate_name(what: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(format!("{what} `{value}` must not contain whitespace"));
    }
    Ok(())
}

fn endpoint_label(endpoint: &crate::map::Endpoint) -> String {
    if let Some(lsb) = endpoint.lsb {
        return format!(
            "cc14 ch{} #{}/#{}",
            endpoint.channel, endpoint.id, lsb
        );
    }
    let kind = match endpoint.kind {
        EventKind::Cc => "cc",
        EventKind::Note => "note",
        EventKind::Bend => "bend",
    };
    let mode = match endpoint.mode {
        RelMode::Abs => "",
        RelMode::Rel1 => " rel1",
        RelMode::Rel2 => " rel2",
        RelMode::Rel3 => " rel3",
    };
    format!("{kind}{mode} ch{} #{}", endpoint.channel, endpoint.id)
}

fn endpoint_labels(unit: &crate::map::Unit, control: &crate::map::Control) -> Vec<String> {
    control
        .endpoints
        .iter()
        .map(|&index| {
            let endpoint = &unit.endpoints[index];
            match endpoint.role {
                crate::map::EndpointRole::Touch => format!("touch {}", endpoint_label(endpoint)),
                crate::map::EndpointRole::Turn => format!("turn {}", endpoint_label(endpoint)),
                crate::map::EndpointRole::Main => endpoint_label(endpoint),
            }
        })
        .collect()
}

fn control_shape_for(expected: ControlShape, candidate: &Candidate) -> ControlShape {
    if expected == ControlShape::JogWheel {
        return ControlShape::JogWheel;
    }
    match (expected, candidate.kind) {
        (ControlShape::Button, EventKind::Note) => ControlShape::Button,
        (ControlShape::Button, EventKind::Cc) => ControlShape::Button,
        (ControlShape::Knob, EventKind::Cc | EventKind::Bend) => ControlShape::Knob,
        (ControlShape::BipolKnob, EventKind::Cc | EventKind::Bend) => ControlShape::BipolKnob,
        (ControlShape::RelKnob, EventKind::Cc) => ControlShape::RelKnob,
        _ => expected,
    }
}

fn raw_control(
    slot: &str,
    shape: ControlShape,
    candidate: &Candidate,
    touch: Option<&Candidate>,
) -> Result<RawControl, String> {
    let mode = if candidate.kind == EventKind::Cc {
        Some(candidate.mode)
    } else {
        None
    };
    if shape == ControlShape::JogWheel {
        let touch = touch.ok_or_else(|| "the jog touch was not captured".to_owned())?;
        if candidate.kind != EventKind::Cc || candidate.mode == RelMode::Abs {
            return Err("the jog turn must be a relative CC".to_owned());
        }
        return Ok(RawControl {
            slot: slot.to_owned(),
            kind: Some(ControlShape::JogWheel),
            event_type: None,
            lsb: None,
            mode: None,
            reverse: None,
            channel: None,
            id: None,
            touch: Some(RawEndpoint {
                kind: touch.kind,
                lsb: None,
                mode: if touch.kind == EventKind::Cc {
                    Some(touch.mode)
                } else {
                    None
                },
                reverse: None,
                channel: touch.channel,
                id: touch.id,
            }),
            turn: Some(RawEndpoint {
                kind: EventKind::Cc,
                lsb: None,
                mode: Some(candidate.mode),
                reverse: None,
                channel: candidate.channel,
                id: candidate.id,
            }),
            target: None,
        });
    }
    Ok(RawControl {
        slot: slot.to_owned(),
        kind: Some(shape),
        event_type: Some(candidate.kind),
        lsb: None,
        mode,
        reverse: None,
        channel: Some(candidate.channel),
        id: Some(candidate.id),
        touch: None,
        turn: None,
        target: None,
    })
}

fn empty_bind(slot: &str) -> RawModeBind {
    RawModeBind {
        slot: slot.to_owned(),
        action: String::new(),
        down: None,
        deck: None,
        min: None,
        max: None,
        curve: None,
        step: None,
        seconds: None,
        beats: None,
        chain: None,
        fx: None,
        param: None,
        on: None,
    }
}

fn ensure_unit_mode(unit: &mut RawUnit, name: &str, preset_mode: Option<&RawMode>) {
    if unit.modes.iter().any(|mode| mode.name == name) {
        return;
    }
    let mut mode = preset_mode.cloned().unwrap_or_else(|| RawMode {
        name: name.to_owned(),
        label: None,
        binds: Vec::new(),
    });
    mode.name = name.to_owned();
    unit.modes.push(mode);
}

fn clone_id(old: &str, old_deck: DeckId, new_deck: DeckId) -> String {
    let old_prefix = format!("deck{}", old_deck + 1);
    let new_prefix = format!("deck{}", new_deck + 1);
    if let Some(rest) = old.strip_prefix(&format!("{old_prefix}.")) {
        return format!("{new_prefix}.{rest}");
    }
    if let Some(rest) = old.strip_prefix(&old_prefix) {
        return format!("{new_prefix}{rest}");
    }
    format!("{new_prefix}.{old}")
}

fn clone_unit_raw(
    source: &RawUnit,
    target_deck: DeckId,
    delta: i16,
    id: String,
) -> Result<RawUnit, String> {
    let old_deck = source.deck.ok_or_else(|| "no source deck".to_owned())?;
    let mut unit = source.clone();
    unit.id = id;
    unit.deck = Some(target_deck);
    for control in &mut unit.controls {
        if let Some(channel) = control.channel {
            control.channel = Some(shift_channel(channel, delta)?);
        }
        if let Some(touch) = &mut control.touch {
            touch.channel = shift_channel(touch.channel, delta)?;
        }
        if let Some(turn) = &mut control.turn {
            turn.channel = shift_channel(turn.channel, delta)?;
        }
    }
    for selector in &mut unit.selectors {
        selector.channel = shift_channel(selector.channel, delta)?;
    }
    for mode in &mut unit.modes {
        for bind in &mut mode.binds {
            if bind.deck == Some(old_deck) {
                bind.deck = Some(target_deck);
            }
            if let Some(chain) = &bind.chain {
                bind.chain = Some(rewrite_chain(chain, old_deck, target_deck));
            }
            bind.action = rewrite_action_target(&bind.action, old_deck, target_deck);
        }
    }
    Ok(unit)
}

fn shift_channel(channel: u8, delta: i16) -> Result<u8, String> {
    let value = i16::from(channel) + delta;
    if !(0..=15).contains(&value) {
        return Err(format!(
            "channel {channel} shifted by {delta:+} leaves the MIDI range"
        ));
    }
    Ok(value as u8)
}

fn rewrite_chain(token: &str, old: DeckId, new: DeckId) -> String {
    let old_number = old + 1;
    let new_number = new + 1;
    let old = old_number.to_string();
    let new = new_number.to_string();
    let forms = [
        (format!("deck{old}"), format!("deck{new}")),
        (format!("d{old}"), format!("deck{new}")),
        (old.clone(), format!("deck{new}")),
    ];
    for (from, to) in forms {
        if token == from {
            return to;
        }
        if let Some(rest) = token.strip_prefix(&format!("{from}/")) {
            return format!("{to}/{rest}");
        }
    }
    token.to_owned()
}

fn rewrite_action_target(action: &str, old: DeckId, new: DeckId) -> String {
    let mut words = action.split_whitespace();
    let Some(first) = words.next() else {
        return action.to_owned();
    };
    let rewritten = rewrite_chain(first, old, new);
    if rewritten != first {
        let rest = action[first.len()..].trim_start();
        if rest.is_empty() {
            rewritten
        } else {
            format!("{rewritten} {rest}")
        }
    } else {
        action.to_owned()
    }
}

/// The action names the simple action picker offers. Kept as a function so the CLI does not depend
/// on the registry's static layout.
pub fn action_names() -> Vec<&'static str> {
    ACTIONS.iter().map(|spec| spec.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::MapFile;

    fn guide(text: &str) -> Guide {
        Guide::from_toml_str(text, "guide-test.toml", 2).expect("guide map")
    }

    fn map_of(guide: &Guide) -> MapFile {
        MapFile::from_toml_str(&guide.to_toml_string().unwrap()).unwrap()
    }

    fn note(channel: u8, key: u8) -> Event {
        Event::NoteOn {
            channel,
            key,
            velocity: 100,
        }
    }

    fn cc(channel: u8, controller: u8, value: u8) -> Event {
        Event::ControlChange {
            channel,
            controller,
            value,
        }
    }

    fn node_index(guide: &Guide, kind: TreeNodeKind) -> usize {
        guide
            .tree()
            .iter()
            .position(|node| node.kind == kind)
            .unwrap_or_else(|| panic!("missing node {kind:?}"))
    }

    #[test]
    fn the_empty_tree_starts_with_deck_groups_and_add_rows() {
        let guide = Guide::empty(2).unwrap();
        let tree = guide.tree();
        assert!(tree
            .iter()
            .any(|node| matches!(node.kind, TreeNodeKind::Deck { deck: Some(0) })));
        assert!(tree
            .iter()
            .any(|node| matches!(node.kind, TreeNodeKind::Deck { deck: Some(1) })));
        assert!(tree
            .iter()
            .any(|node| matches!(node.kind, TreeNodeKind::Deck { deck: None })));
        assert_eq!(
            tree.iter()
                .filter(|node| matches!(node.kind, TreeNodeKind::AddUnit { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn tree_new_unit_selects_the_unit_and_exposes_slots_and_modes() {
        let mut guide = Guide::empty(2).unwrap();
        guide.tree_new_unit("touchpad", 0, None).unwrap();
        assert!(matches!(
            guide.selected_tree_kind(),
            Some(TreeNodeKind::Unit { unit: 0 })
        ));
        let tree = guide.tree();
        assert!(tree
            .iter()
            .any(|node| matches!(node.kind, TreeNodeKind::Control { unit: 0, control: 0 })));
        assert!(tree
            .iter()
            .any(|node| matches!(node.kind, TreeNodeKind::Mode { unit: 0, mode: 0 })));
        assert!(tree
            .iter()
            .any(|node| matches!(node.kind, TreeNodeKind::Action { unit: 0, mode: 0, control: 0 })));
    }

    #[test]
    fn tree_learning_a_control_updates_the_tree_hint() {
        let mut guide = Guide::empty(2).unwrap();
        guide.tree_new_unit("touchpad", 0, None).unwrap();
        let kind = TreeNodeKind::Control {
            unit: 0,
            control: 0,
        };
        let index = node_index(&guide, kind);
        guide.select_tree_row(index);
        guide.tree_begin_learn().unwrap();
        guide.feed(&note(0, 16));
        guide.tree_confirm_learn().unwrap();
        let tree = guide.tree();
        let control = tree
            .iter()
            .find(|node| node.kind == kind)
            .expect("control node");
        assert!(control.hint.contains("note ch0 #16"), "{}", control.hint);
    }

    #[test]
    fn tree_action_editing_uses_the_selected_action_node() {
        let mut guide = Guide::empty(2).unwrap();
        guide.tree_new_unit("touchpad", 0, None).unwrap();
        let kind = TreeNodeKind::Action {
            unit: 0,
            mode: 0,
            control: 0,
        };
        let index = node_index(&guide, kind);
        guide.select_tree_row(index);
        assert_eq!(guide.tree_action_text().as_deref(), Some("hotcue trigger 1"));
        guide.tree_set_action("play".to_owned()).unwrap();
        let tree = guide.tree();
        let action = tree
            .iter()
            .find(|node| node.kind == kind)
            .expect("action node");
        assert!(action.label.contains("pad1 → play"), "{}", action.label);
    }

    #[test]
    fn tree_selector_capture_and_behavior_edit_update_the_node() {
        let mut guide = Guide::empty(2).unwrap();
        guide.tree_new_unit("touchpad", 0, None).unwrap();
        guide
            .tree_begin_selector(RawSelectorBehavior::Trigger, Some("hotcue".to_owned()))
            .unwrap();
        guide.feed(&note(0, 24));
        guide.tree_confirm_learn().unwrap();
        let kind = TreeNodeKind::Selector {
            unit: 0,
            selector: 0,
        };
        assert!(guide.tree().iter().any(|node| node.kind == kind));
        let index = node_index(&guide, kind);
        guide.select_tree_row(index);
        guide
            .tree_set_selector_behavior(RawSelectorBehavior::Cycle, None)
            .unwrap();
        let tree = guide.tree();
        let selector = tree
            .iter()
            .find(|node| node.kind == kind)
            .expect("selector node");
        assert!(selector.hint.contains("cycle"), "{}", selector.hint);
    }

    #[test]
    fn tree_clone_selects_the_new_unit() {
        let mut guide = Guide::empty(2).unwrap();
        guide.tree_new_unit("transport", 0, None).unwrap();
        guide.tree_clone(1).unwrap();
        assert_eq!(
            guide.selected_tree_kind(),
            Some(TreeNodeKind::Unit { unit: 1 })
        );
        assert_eq!(guide.map.units.len(), 2);
        assert_eq!(guide.map.units[1].id, "deck2.transport");
    }

    #[test]
    fn a_jog_control_has_separate_press_and_turn_rows() {
        let text = r#"
[[unit]]
id = "deck1.jog"
preset = "jog"
deck = 1

[[unit.control]]
slot = "jog"
kind = "JogWheel"
press = { type = "note", channel = 0, id = 80 }
turn = { type = "cc", mode = "rel2", channel = 0, id = 6 }
"#;
        let mut guide = guide(text);
        let press = TreeNodeKind::ControlPart {
            unit: 0,
            control: 0,
            part: ControlPart::Press,
        };
        let turn = TreeNodeKind::ControlPart {
            unit: 0,
            control: 0,
            part: ControlPart::Turn,
        };
        assert!(guide.tree().iter().any(|node| node.kind == press));
        assert!(guide.tree().iter().any(|node| node.kind == turn));

        let index = node_index(&guide, turn);
        guide.select_tree_row(index);
        assert!(guide.tree_view().buttons.contains(&TreeButton::Learn));
        assert_eq!(guide.tree_endpoints().len(), 1);
        assert!(guide
            .tree_details()
            .iter()
            .any(|(key, value)| key == "part" && value == "turn"));
    }

    #[test]
    fn a_global_preset_does_not_get_a_deck() {
        let mut guide = guide("");
        assert!(guide.preset_is_global("browser"));
        guide.new_global_unit("browser", None).unwrap();
        let map = map_of(&guide);
        assert_eq!(map.units[0].deck, None);
        assert_eq!(map.units[0].id, "global.browser");

        // Even an old call that passes a deck must not attach it to a global preset.
        guide.new_unit("output", 0, None).unwrap();
        let map = map_of(&guide);
        assert_eq!(map.units[1].deck, None);
        assert_eq!(map.units[1].id, "global.output");
    }

    #[test]
    fn a_new_unit_takes_its_slots_and_modes_from_a_preset() {
        let mut guide = guide("");
        guide.new_unit("touchpad", 0, None).unwrap();
        assert_eq!(guide.mode_names().len(), 4);
        assert_eq!(guide.stage(), Stage::Controls);
        let map = map_of(&guide);
        assert_eq!(map.units.len(), 1);
        assert_eq!(map.units[0].preset.as_deref(), Some("touchpad"));
        assert_eq!(map.units[0].deck, Some(0));
    }

    #[test]
    fn learning_a_note_writes_the_endpoint() {
        let mut guide = guide("");
        guide.new_unit("touchpad", 0, None).unwrap();
        guide.begin_learn().unwrap();
        assert_eq!(guide.feed(&note(0, 16)), GuideStep::Captured);
        guide.confirm_learn().unwrap();
        let map = map_of(&guide);
        let control = &map.units[0].controls[0];
        assert_eq!(control.slot, "pad1");
        assert_eq!(control.event_type, Some(EventKind::Note));
        assert_eq!(control.channel, Some(0));
        assert_eq!(control.id, Some(16));
        assert_eq!(control.kind, Some(ControlShape::Button));
    }

    #[test]
    fn a_one_cc_jog_learns_press_and_wheel() {
        let mut guide = guide("");
        guide.new_unit("jog", 0, None).unwrap();
        guide.begin_learn().unwrap();
        guide.feed(&note(0, 80));
        guide.confirm_learn().unwrap();
        guide.feed(&cc(0, 6, 1));
        guide.feed(&cc(0, 6, 127));
        guide.confirm_learn().unwrap();

        let map = map_of(&guide);
        let control = &map.units[0].controls[0];
        assert_eq!(control.slot, "jog");
        assert_eq!(control.kind, Some(ControlShape::JogWheel));
        assert_eq!(control.touch.as_ref().map(|endpoint| endpoint.id), Some(80));
        assert_eq!(control.turn.as_ref().map(|endpoint| endpoint.id), Some(6));
        assert_eq!(
            control.turn.as_ref().and_then(|endpoint| endpoint.mode),
            Some(RelMode::Rel2)
        );
    }

    #[test]
    fn two_adjacent_cc_values_promote_the_encoder_to_relative() {
        let mut guide = guide("");
        guide.new_unit("mixer", 0, None).unwrap();
        guide.begin_learn().unwrap();
        guide.feed(&cc(0, 7, 1));
        guide.feed(&cc(0, 7, 127));
        let candidate = guide.candidate().expect("candidate");
        assert_eq!(candidate.mode, RelMode::Rel2);
        guide.confirm_learn().unwrap();
        let map = map_of(&guide);
        let control = &map.units[0].controls[0];
        assert_eq!(control.slot, "fader");
        assert_eq!(control.mode, Some(RelMode::Rel2));
        assert_eq!(control.kind, Some(ControlShape::Knob));
    }

    #[test]
    fn a_cc_pad_release_is_not_mistaken_for_a_relative_encoder() {
        let mut guide = guide("");
        guide.new_unit("touchpad", 0, None).unwrap();
        guide.begin_learn().unwrap();
        guide.feed(&cc(0, 16, 127));
        guide.feed(&cc(0, 16, 0));
        let candidate = guide.candidate().expect("candidate");
        assert_eq!(candidate.mode, RelMode::Abs);
        guide.confirm_learn().unwrap();
        let map = map_of(&guide);
        assert_eq!(map.units[0].controls[0].mode, Some(RelMode::Abs));
    }

    #[test]
    fn a_selector_is_learned_with_its_behavior_and_target() {
        let text = r#"
[[unit]]
id = "u"
deck = 1
[[unit.mode]]
name = "m"
"#;
        let mut guide = guide(text);
        guide.set_stage(Stage::Selectors);
        guide
            .begin_selector(RawSelectorBehavior::Trigger, Some("m".to_owned()))
            .unwrap();
        guide.feed(&note(0, 2));
        guide.confirm_learn().unwrap();
        let map = map_of(&guide);
        let selector = &map.units[0].selectors[0];
        assert_eq!(selector.channel, 0);
        assert_eq!(selector.id, 2);
        assert_eq!(selector.behavior, RawSelectorBehavior::Trigger);
        assert_eq!(selector.target.as_deref(), Some("m"));
    }

    #[test]
    fn cloning_to_another_deck_shifts_channels_and_rewrites_deck_and_chain() {
        let text = r#"
[[unit]]
id = "deck1.pad"
deck = 1
[[unit.control]]
slot = "knob"
type = "cc"
channel = 0
id = 1
[[unit.selector]]
type = "note"
channel = 0
id = 2
behavior = "trigger"
target = "m"
[[unit.mode]]
name = "m"
[[unit.mode.bind]]
slot = "knob"
action = "fx param"
deck = 1
chain = "deck2"
fx = "eq"
param = "low"
min = -1.0
max = 1.0
"#;
        let mut guide = guide(text);
        guide.clone_unit(1).unwrap();
        let map = map_of(&guide);
        assert_eq!(map.units.len(), 2);
        let cloned = &map.units[1];
        assert_eq!(cloned.id, "deck2.pad");
        assert_eq!(cloned.deck, Some(1));
        assert_eq!(cloned.controls[0].channel, Some(1));
        assert_eq!(cloned.selectors[0].channel, 1);
        let bind = &cloned.modes[0].binds[0];
        assert_eq!(bind.deck, Some(1));
        assert_eq!(bind.chain.as_deref(), Some("deck2"));
        assert_eq!(bind.action, "fx param");
    }

    #[test]
    fn clone_to_an_arbitrary_deck_uses_the_channel_delta() {
        let text = r#"
[[unit]]
id = "deck1.pad"
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
action = "play"
"#;
        let mut guide = guide(text);
        guide.clone_unit(2).unwrap();
        let map = map_of(&guide);
        let cloned = &map.units[1];
        assert_eq!(cloned.id, "deck3.pad");
        assert_eq!(cloned.deck, Some(2));
        assert_eq!(cloned.controls[0].channel, Some(2));
    }

    #[test]
    fn removing_a_mode_rewrites_an_explicit_order_even_without_a_preset() {
        let text = r#"
[[unit]]
id = "u"
deck = 1
mode_order = ["a", "b"]
[[unit.mode]]
name = "a"
[[unit.mode]]
name = "b"
"#;
        let mut guide = guide(text);
        guide.set_stage(Stage::Modes);
        guide.select_row(1);
        guide.remove_selected().unwrap();
        let map = map_of(&guide);
        assert_eq!(map.units[0].mode_order.as_deref(), Some(["a".to_owned()].as_slice()));
        assert_eq!(map.units[0].modes.len(), 1);
    }

    #[test]
    fn cloning_onto_an_occupied_deck_reports_the_existing_unit() {
        let text = r#"
[[unit]]
id = "deck1.pad"
deck = 1
[[unit.mode]]
name = "m"
[[unit]]
id = "deck2.pad"
deck = 2
[[unit.mode]]
name = "m"
"#;
        let mut guide = guide(text);
        let error = guide.clone_unit(1).expect_err("deck 1 is occupied");
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn modes_can_be_added_reordered_and_removed_from_a_preset() {
        let mut guide = guide("");
        guide.new_unit("touchpad", 0, None).unwrap();
        guide.set_stage(Stage::Modes);
        guide.add_mode("mine".to_owned()).unwrap();
        assert_eq!(guide.mode_names().len(), 5);
        // Move the new mode one step up in the cycle order.
        guide.move_mode(-1).unwrap();
        let names = guide.mode_names();
        assert_eq!(names[3], "mine");
        // Remove a preset-inherited mode; that materializes an explicit whitelist.
        guide.select_row(1); // loop
        guide.remove_selected().unwrap();
        assert!(!guide.mode_names().contains(&"loop".to_owned()));
        let map = map_of(&guide);
        assert!(map.units[0].mode_order.is_some());
    }

    #[test]
    fn editing_a_preset_mode_materializes_its_other_binds() {
        let mut guide = guide("");
        guide.new_unit("touchpad", 0, None).unwrap();
        guide.set_stage(Stage::Actions);
        // pad1 in the hotcue mode.
        guide.set_action("play".to_owned()).unwrap();
        let map = map_of(&guide);
        let hotcue = map.units[0]
            .modes
            .iter()
            .find(|mode| mode.name == "hotcue")
            .expect("unit override");
        assert_eq!(hotcue.binds.len(), 8, "the preset's other pads are copied");
        assert_eq!(hotcue.binds[0].action, "play");
    }

    #[test]
    fn a_device_is_written_into_meta() {
        let mut guide = guide("");
        guide.set_device(Some("DDJ-400".to_owned()));
        let text = guide.to_toml_string().unwrap();
        assert!(text.contains("device = \"DDJ-400\""), "{text}");
        MapFile::from_toml_str(&text).expect("an empty map with a device still parses");
        guide.set_device(None);
        assert!(!guide.to_toml_string().unwrap().contains("DDJ-400"));
    }

    #[test]
    fn a_guide_session_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!(
            "hypermixx-guide-roundtrip-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("map.toml");
        let path_str = path.display().to_string();
        let mut guide = Guide::new(Some(&path_str), 2).unwrap();
        guide.new_unit("transport", 0, None).unwrap();
        guide.set_device(Some("DDJ-400".to_owned()));
        guide.clone_unit(1).unwrap();
        guide.save().unwrap();
        let reopened = Guide::new(Some(&path_str), 2).unwrap();
        assert_eq!(reopened.device().as_deref(), Some("DDJ-400"));
        assert_eq!(reopened.map.units.len(), 2);
        assert_eq!(reopened.map.units[1].id, "deck2.transport");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_shipped_maps_open_in_the_guide() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for name in ["profile/midi.toml", "profile/midi-map.toml"] {
            let path = root.join(name);
            let text = std::fs::read_to_string(&path).expect("shipped map");
            let mut guide = Guide::from_toml_str(&text, &path.display().to_string(), 2)
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            assert!(!guide.view().rows.is_empty());
            guide.set_stage(Stage::Controls);
            assert!(
                !guide.view().rows.is_empty(),
                "{name}: the first unit should expose its controls"
            );
        }
        // The big example's first unit is the touchpad; its preset slots and modes are visible.
        let path = root.join("profile/midi-map.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut guide =
            Guide::from_toml_str(&text, &path.display().to_string(), 2).unwrap();
        guide.set_stage(Stage::Controls);
        let labels: Vec<String> = guide
            .view()
            .rows
            .iter()
            .map(|row| row.label.clone())
            .collect();
        assert!(labels.contains(&"pad1".to_owned()), "{labels:?}");
        guide.set_stage(Stage::Modes);
        let modes: Vec<String> = guide
            .view()
            .rows
            .iter()
            .map(|row| row.label.clone())
            .collect();
        assert!(modes.contains(&"hotcue".to_owned()), "{modes:?}");
        assert!(modes.contains(&"loop".to_owned()), "{modes:?}");
    }

    #[test]
    fn the_old_two_part_file_is_refused() {
        let error = Guide::from_toml_str("device = \"x\"\nmap = \"m.toml\"\n", "g.toml", 2)
            .expect_err("old shape");
        assert!(error.contains("two-part"), "{error}");
    }
}
