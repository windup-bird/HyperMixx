//! The map editor's model: a tree over the file, plus the state machine that learns an endpoint.
//!
//! The tree is built from the **file**, not from the compiled map, so a half-finished map still
//! renders (and can be fixed) instead of disappearing behind a compile error. Compilation is only
//! used to report whether the current file is valid.

use std::path::{Path, PathBuf};

use crate::map::{
    Edge, Encoding, EndpointRole, EventKind, Map, MapFile, RawControl, RawEndpoint, RawMode,
    RawModeBind, RawUnit, SlotKind, SlotRole,
};
use crate::msg::Event;
use crate::preset::{Preset, PresetRegistry, PresetScope};

/// The four-way buttons the bottom bar shows. Connect/Open/Save/Exit are always appended.
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
            TreeButton::NewSelector => "New mode key",
            TreeButton::Delete => "Del",
            TreeButton::Up => "Up",
            TreeButton::Down => "Down",
            TreeButton::SetStart => "Start",
            TreeButton::Edit => "Edit",
            TreeButton::On => "On",
            TreeButton::Confirm => "Confirm",
            TreeButton::Cancel => "Cancel",
            TreeButton::Mode => "Encoding",
            TreeButton::Connect => "Connect",
            TreeButton::Open => "Open",
            TreeButton::Save => "Save",
            TreeButton::Exit => "Exit",
        }
    }

    /// One short line under the button row, so the focused action explains itself.
    pub fn hint(self) -> &'static str {
        match self {
            TreeButton::Learn => "learn the selected slot's endpoint",
            TreeButton::NewUnit => "add a unit from a preset",
            TreeButton::Clone => "copy this unit to another deck",
            TreeButton::NewMode => "add a mode to this unit",
            TreeButton::NewSelector => "add a mode key (trigger or cycle)",
            TreeButton::Delete => "delete the selected item",
            TreeButton::Up => "move the mode up",
            TreeButton::Down => "move the mode down",
            TreeButton::SetStart => "mark this mode as the start mode",
            TreeButton::Edit => "edit the selected action or mode key",
            TreeButton::On => "toggle press / release edge",
            TreeButton::Confirm => "confirm the learned endpoint",
            TreeButton::Cancel => "cancel learning",
            TreeButton::Mode => "cycle absolute / two-point / offset / complement / direction",
            TreeButton::Connect => "choose the MIDI input",
            TreeButton::Open => "open a map file",
            TreeButton::Save => "save the map",
            TreeButton::Exit => "quit the guide",
        }
    }
}

/// One half of a compound slot (currently a jog wheel). The slot stays a single control so press and
/// turn share `vinyl` and the touch state; the tree only shows them separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlPart {
    Press,
    Turn,
}

impl ControlPart {
    pub fn label(self) -> &'static str {
        match self {
            ControlPart::Press => "press",
            ControlPart::Turn => "turn",
        }
    }

    fn role(self) -> EndpointRole {
        match self {
            ControlPart::Press => EndpointRole::Press,
            ControlPart::Turn => EndpointRole::Turn,
        }
    }
}

/// Which column has the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    /// global / deck1 / deck2 …
    Deck,
    /// the units of that deck, then `add unit`
    Unit,
    /// the slots of that unit
    Slot,
}

impl Column {
    pub fn label(self) -> &'static str {
        match self {
            Column::Deck => "deck",
            Column::Unit => "unit",
            Column::Slot => "slot",
        }
    }
}

/// Where the cursor is: one index per column, plus which column has focus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nav {
    pub deck: usize,
    pub unit: usize,
    pub slot: usize,
    /// The mode the info pane is showing.
    pub mode: usize,
    pub focus: ColumnFocus,
}

/// `Column` is the renderer-facing name; this is the stored default (`Deck`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColumnFocus {
    #[default]
    Deck,
    Unit,
    Slot,
}

impl ColumnFocus {
    pub fn column(self) -> Column {
        match self {
            ColumnFocus::Deck => Column::Deck,
            ColumnFocus::Unit => Column::Unit,
            ColumnFocus::Slot => Column::Slot,
        }
    }
}

/// What `enter` did, so the front-end can open a preset menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entered {
    Moved,
    Learning,
    AddUnit,
}

/// A unit row, with the id it points at (`None` for the trailing `add unit`).
struct UnitEntry {
    row: Row,
    unit: Option<usize>,
}

/// A slot row: a control, a jog half, or a mode key.
struct SlotEntry {
    row: Row,
    slot: Option<usize>,
    part: Option<ControlPart>,
}

/// One rendered row of a column.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    pub hint: String,
    /// Drawn dim: the trailing `add unit` row.
    pub muted: bool,
}

/// A read-only snapshot for the renderer.
#[derive(Clone, Debug)]
pub struct View {
    pub path: Option<String>,
    pub device: Option<String>,
    pub dirty: bool,
    pub message: Option<String>,
    pub error: Option<String>,
    pub decks: Vec<Row>,
    pub units: Vec<Row>,
    pub slots: Vec<Row>,
    pub nav: Nav,
    /// Lines for the right-hand info card.
    pub details: Vec<(String, String)>,
    /// Endpoints to highlight in the raw monitor.
    pub endpoints: Vec<(EventKind, u8, u8)>,
    pub buttons: Vec<TreeButton>,
    pub capturing: bool,
    pub candidate: Option<Candidate>,
    /// `(index, count, name)` of the mode the info pane shows, when a unit is selected.
    pub mode: Option<(usize, usize, String)>,
}

/// A detected endpoint, before it is committed to a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub kind: EventKind,
    pub channel: u8,
    pub id: u8,
    pub encoding: Encoding,
    /// The LSB controller when the control turned out to be a 14-bit pair.
    pub lsb: Option<u8>,
}

impl Candidate {
    pub fn label(&self) -> String {
        let kind = match self.kind {
            EventKind::Cc => "cc",
            EventKind::Note => "note",
            EventKind::Bend => "bend",
        };
        if let Some(lsb) = self.lsb {
            return format!("cc14 ch{} #{}/#{}", self.channel + 1, self.id, lsb);
        }
        let encoding = match self.encoding {
            Encoding::Absolute => "",
            Encoding::Offset64 => " offset64",
            Encoding::TwosComplement => " twos_complement",
            Encoding::Direction => " direction",
            Encoding::TwoPoint => " two_point",
        };
        format!("{kind}{encoding} ch{} #{}", self.channel, self.id)
    }
}

/// What `Guide::feed` did with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuideStep {
    Ignored,
    Captured,
    RelativeDetected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JogPart {
    Press,
    Turn,
}

#[derive(Clone, Debug)]
struct SelectorCapture {
    role: SlotRole,
    target: Option<String>,
}

#[derive(Clone, Debug)]
struct Capture {
    unit: usize,
    slot: String,
    kind: SlotKind,
    part: JogPart,
    press: Option<Candidate>,
    selector: Option<SelectorCapture>,
    candidate: Option<Candidate>,
    last_cc: Option<u8>,
    /// The previous CC, for spotting a 14-bit MSB/LSB pair: `(controller, at_ms)`.
    previous_cc: Option<(u8, u64)>,
    /// Both `1` and `127` were seen: a two-point encoder.
    two_point: bool,
    /// A value outside `{0, 1, 127}` was seen: not a two-point encoder.
    swept: bool,
}

/// One slot in a unit, as the tree shows it.
#[derive(Clone, Debug)]
struct SlotInfo {
    name: String,
    kind: SlotKind,
    role: SlotRole,
    target: Option<String>,
    /// The raw endpoints bound to it, as `(role, endpoint)`.
    endpoints: Vec<(EndpointRole, RawEndpoint)>,
    from_preset: bool,
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
    decks: usize,
    nav: Nav,
    capture: Option<Capture>,
    dirty: bool,
    message: Option<String>,
}

impl Guide {
    /// An empty session that has not loaded anything.
    pub fn empty(decks: usize) -> Result<Self, String> {
        Self::from_map_file(MapFile {
            meta: None,
            units: Vec::new(),
        }, PathBuf::from("."), None, decks)
    }

    /// Opens a file, or starts empty when the path does not exist yet.
    pub fn new(path: Option<&str>, decks: usize) -> Result<Self, String> {
        match path {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|err| format!("{path}: {err}"))?;
                let base = Path::new(path)
                    .parent()
                    .filter(|dir| !dir.as_os_str().is_empty())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                let file = MapFile::from_toml_str(&text).map_err(|err| err.to_string())?;
                Self::from_map_file(file, base, Some(path.to_owned()), decks)
            }
            None => Self::empty(decks),
        }
    }

    /// Parses text in memory; used by tests and by `open`'s dry run.
    pub fn from_toml_str(text: &str, path: &str, decks: usize) -> Result<Self, String> {
        let file = MapFile::from_toml_str(text).map_err(|err| err.to_string())?;
        let base = Path::new(path)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::from_map_file(file, base, Some(path.to_owned()), decks)
    }

    fn from_map_file(
        map: MapFile,
        base: PathBuf,
        path: Option<String>,
        decks: usize,
    ) -> Result<Self, String> {
        let presets = PresetRegistry::load(map.meta.as_ref(), &base)?;
        let mut guide = Self {
            path,
            base,
            map,
            presets,
            compiled: None,
            error: None,
            decks: decks.max(1),
            nav: Nav::default(),
            capture: None,
            dirty: false,
            message: None,
        };
        guide.recompile();
        Ok(guide)
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub fn device(&self) -> Option<String> {
        self.map.meta.as_ref().and_then(|meta| meta.device.clone())
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn note(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }

    /// Records the controller this map belongs to; `Save` writes it to `[meta].device`.
    pub fn set_device(&mut self, device: Option<String>) {
        let device = device.filter(|name| !name.trim().is_empty());
        if self.device() == device {
            return;
        }
        let meta = self.map.meta.get_or_insert_with(Default::default);
        meta.device = device;
        self.dirty = true;
        self.message = Some("device saved into [meta].device".to_owned());
    }

    /// Recompiles the file, remembering the first error for the status line.
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
                self.error = Some(err.message().to_owned());
            }
        }
    }

    fn with_map<T>(&mut self, edit: impl FnOnce(&mut MapFile) -> Result<T, String>) -> Result<T, String> {
        let out = edit(&mut self.map)?;
        self.dirty = true;
        self.recompile();
        Ok(out)
    }

    pub fn open(&mut self, path: &str) -> Result<(), String> {
        let text = std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
        let file = MapFile::from_toml_str(&text).map_err(|err| err.to_string())?;
        let base = Path::new(path)
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let presets = PresetRegistry::load(file.meta.as_ref(), &base)?;
        self.base = base;
        self.map = file;
        self.presets = presets;
        self.path = Some(path.to_owned());
        self.nav = Nav::default();
        self.capture = None;
        self.dirty = false;
        self.recompile();
        Ok(())
    }

    pub fn save(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Err("no file yet; use Save to choose a path".to_owned());
        };
        self.save_as(&path)
    }

    pub fn save_as(&mut self, path: &str) -> Result<(), String> {
        self.recompile();
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let text = toml::to_string_pretty(&self.map).map_err(|err| err.to_string())?;
        let temp = format!("{path}.tmp");
        std::fs::write(&temp, text).map_err(|err| format!("{temp}: {err}"))?;
        std::fs::rename(&temp, path).map_err(|err| format!("{path}: {err}"))?;
        self.path = Some(path.to_owned());
        self.dirty = false;
        self.message = Some(format!("saved {path}"));
        Ok(())
    }

    pub fn to_toml_string(&self) -> Result<String, String> {
        toml::to_string_pretty(&self.map).map_err(|err| err.to_string())
    }

    // ---- presets ---------------------------------------------------------

    pub fn preset_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.presets.names().map(str::to_owned).collect();
        names.sort();
        names
    }

    /// The groups a preset can be instantiated in.
    pub fn preset_scope(&self, name: &str) -> PresetScope {
        self.presets.scope(name).unwrap_or_default()
    }

    fn preset(&self, name: &str) -> Option<&Preset> {
        self.presets.get(name)
    }

    // ---- units -----------------------------------------------------------

    pub fn new_unit(
        &mut self,
        preset: &str,
        deck: Option<usize>,
        id: Option<String>,
    ) -> Result<(), String> {
        let Some(entry) = self.presets.get(preset) else {
            return Err(format!("unknown preset `{preset}`"));
        };
        let scope = entry.scope;
        if deck.is_some() && !scope.takes_deck() {
            return Err(format!("preset `{preset}` is global; it has no deck"));
        }
        if deck.is_none() && !scope.takes_global() {
            return Err(format!("preset `{preset}` needs a deck"));
        }
        let id = id.filter(|id| !id.trim().is_empty()).unwrap_or_else(|| match deck {
            Some(deck) => format!("deck{}.{preset}", deck + 1),
            None => format!("global.{preset}"),
        });
        validate_name("unit id", &id)?;
        let id = self.unique_unit_id(id);
        let unit = RawUnit {
            id,
            preset: Some(preset.to_owned()),
            deck: deck.map(|deck| deck as u8),
            start: None,
            mode_order: None,
            controls: Vec::new(),
            modes: Vec::new(),
        };
        self.with_map(|map| {
            map.units.push(unit);
            Ok(())
        })?;
        let unit = self.map.units.len() - 1;
        self.select_unit(unit);
        self.message = Some("unit created; learn its slots".to_owned());
        Ok(())
    }

    pub fn tree_new_unit(&mut self, preset: &str, deck: usize, id: Option<String>) -> Result<(), String> {
        self.new_unit(preset, Some(deck), id)
    }

    pub fn tree_new_global_unit(&mut self, preset: &str, id: Option<String>) -> Result<(), String> {
        self.new_unit(preset, None, id)
    }

    fn unique_unit_id(&self, base: String) -> String {
        if !self.map.units.iter().any(|unit| unit.id == base) {
            return base;
        }
        for suffix in 2.. {
            let candidate = format!("{base}-{suffix}");
            if !self.map.units.iter().any(|unit| unit.id == candidate) {
                return candidate;
            }
        }
        unreachable!()
    }

    /// Deep-copies a deck unit to `target_deck`, shifting every endpoint channel by `channel_offset`.
    pub fn clone_unit(&mut self, target_deck: usize, channel_offset: i16) -> Result<(), String> {
        let index = self
            .selected_unit_key()
            .ok_or_else(|| "select a unit to clone".to_owned())?;
        let source = self.map.units[index].clone();
        let Some(old_deck) = source.deck else {
            return Err("a global unit has no deck to clone to".to_owned());
        };
        let target = target_deck as u8;
        if old_deck == target {
            return Err(format!("`{}` is already on deck {}", source.id, target_deck + 1));
        }
        let mut cloned = source.clone();
        cloned.id = clone_id(&source.id, old_deck, target);
        if self.map.units.iter().any(|unit| unit.id == cloned.id) {
            return Err(format!(
                "unit `{}` already exists; delete it first or pick another deck",
                cloned.id
            ));
        }
        cloned.deck = Some(target);
        // Endpoints and per-row decks move with the unit.
        for control in &mut cloned.controls {
            let Some(one_or_many) = control.endpoint.as_mut() else {
                continue;
            };
            match one_or_many {
                crate::map::OneOrMany::One(endpoint) => {
                    endpoint.channel = shift_channel(endpoint.channel, channel_offset)?;
                }
                crate::map::OneOrMany::Many(endpoints) => {
                    for endpoint in endpoints.iter_mut() {
                        endpoint.channel = shift_channel(endpoint.channel, channel_offset)?;
                    }
                }
            }
        }
        for mode in &mut cloned.modes {
            for bind in &mut mode.binds {
                if bind.deck == Some(old_deck) {
                    bind.deck = Some(target);
                }
                bind.chain = bind
                    .chain
                    .as_deref()
                    .map(|token| rewrite_chain(token, old_deck, target));
            }
        }
        self.with_map(|map| {
            map.units.push(cloned);
            Ok(())
        })?;
        let unit = self.map.units.len() - 1;
        self.select_unit(unit);
        self.message = Some(format!(
            "cloned to deck {}; channels shifted by {channel_offset:+}",
            target_deck + 1
        ));
        Ok(())
    }

    pub fn tree_clone(&mut self, target_deck: usize, channel_offset: i16) -> Result<(), String> {
        self.clone_unit(target_deck, channel_offset)
    }

    pub fn tree_selected_unit_deck(&self) -> Option<usize> {
        let index = self.selected_unit_key()?;
        self.map.units[index].deck.map(usize::from)
    }

    /// The unit the unit column points at; `None` on the trailing `add unit` row.
    fn selected_unit_key(&self) -> Option<usize> {
        let units = self.unit_entries();
        let index = self.nav.unit.min(units.len().saturating_sub(1));
        units.get(index)?.unit
    }

    // ---- file lookups ----------------------------------------------------

    fn preset_of_unit(&self, unit: usize) -> Option<&Preset> {
        let name = self.map.units.get(unit)?.preset.as_deref()?;
        self.preset(name)
    }

    /// Every slot in a unit: preset slots first, then unit-only controls.
    fn unit_slots(&self, unit: usize) -> Vec<SlotInfo> {
        let Some(raw) = self.map.units.get(unit) else {
            return Vec::new();
        };
        let preset = self.preset_of_unit(unit);
        let mut slots: Vec<SlotInfo> = Vec::new();
        if let Some(preset) = preset {
            for slot in &preset.slots {
                let control = raw.controls.iter().find(|control| control.slot == slot.slot);
                slots.push(SlotInfo {
                    name: slot.slot.clone(),
                    kind: control.and_then(|control| control.kind).unwrap_or(slot.kind),
                    role: control.map(|control| control.role).unwrap_or(slot.role),
                    target: control
                        .and_then(|control| control.target.clone())
                        .or_else(|| slot.target.clone()),
                    endpoints: control.map(raw_endpoints).unwrap_or_default(),
                    from_preset: true,
                });
            }
        }
        for control in &raw.controls {
            if slots.iter().any(|slot| slot.name == control.slot) {
                continue;
            }
            let Some(kind) = control.kind else {
                continue;
            };
            slots.push(SlotInfo {
                name: control.slot.clone(),
                kind,
                role: control.role,
                target: control.target.clone(),
                endpoints: raw_endpoints(control),
                from_preset: false,
            });
        }
        slots
    }

    /// The merged mode list: preset order, unit replacement, unit-only modes appended.
    fn unit_modes(&self, unit: usize) -> Vec<(String, usize, bool)> {
        let Some(raw) = self.map.units.get(unit) else {
            return Vec::new();
        };
        let preset = self.preset_of_unit(unit);
        let mut modes: Vec<(String, Vec<RawModeBind>, bool)> = Vec::new();
        if let Some(preset) = preset {
            for mode in &preset.modes {
                modes.push((mode.name.clone(), mode.binds.clone(), true));
            }
        }
        for mode in &raw.modes {
            if let Some(existing) = modes.iter_mut().find(|(name, _, _)| *name == mode.name) {
                *existing = (mode.name.clone(), mode.binds.clone(), false);
            } else {
                modes.push((mode.name.clone(), mode.binds.clone(), false));
            }
        }
        if let Some(order) = raw.mode_order.clone() {
            let mut ordered = Vec::with_capacity(modes.len());
            for name in &order {
                if let Some(index) = modes.iter().position(|(mode, _, _)| mode == name) {
                    ordered.push(modes.remove(index));
                }
            }
            ordered.extend(modes);
            modes = ordered;
        }
        modes
            .into_iter()
            .map(|(name, binds, from_preset)| (name, binds.len(), from_preset))
            .collect()
    }

    fn unit_start(&self, unit: usize) -> Option<usize> {
        let raw = self.map.units.get(unit)?;
        let name = raw
            .start
            .clone()
            .or_else(|| self.preset_of_unit(unit).and_then(|preset| preset.start.clone()))?;
        self.unit_modes(unit)
            .iter()
            .position(|(mode, _, _)| *mode == name)
    }

    fn unit_mode_names(&self, unit: usize) -> Vec<String> {
        self.unit_modes(unit)
            .into_iter()
            .map(|(name, _, _)| name)
            .collect()
    }

    /// The merged bind row for one slot, before any edit.
    fn raw_bind(&self, unit: usize, mode: &str, slot: &str) -> Option<RawModeBind> {
        let raw = self.map.units.get(unit)?;
        if let Some(found) = raw
            .modes
            .iter()
            .find(|candidate| candidate.name == mode)
            .and_then(|mode| mode.binds.iter().find(|bind| bind.slot == slot))
        {
            return Some(found.clone());
        }
        self.preset_of_unit(unit)?
            .modes
            .iter()
            .find(|candidate| candidate.name == mode)?
            .binds
            .iter()
            .find(|bind| bind.slot == slot)
            .cloned()
    }

    fn bind_action(
        &self,
        unit: usize,
        mode: &str,
        slot: &str,
        part: Option<ControlPart>,
    ) -> Option<String> {
        let bind = self.raw_bind(unit, mode, slot)?;
        match part {
            None => bind.action,
            Some(ControlPart::Press) => bind.press.or(bind.action),
            Some(ControlPart::Turn) => bind.turn.or(bind.action),
        }
    }

    fn bind_on(&self, unit: usize, mode: &str, slot: &str) -> Option<Edge> {
        self.raw_bind(unit, mode, slot)?.on
    }

    /// Copies a preset mode into the unit so it can be edited without losing the other rows.
    fn ensure_unit_mode(&mut self, unit: usize, mode: &str) -> Result<(), String> {
        if self.map.units[unit]
            .modes
            .iter()
            .any(|candidate| candidate.name == mode)
        {
            return Ok(());
        }
        let from_preset = self
            .preset_of_unit(unit)
            .and_then(|preset| preset.modes.iter().find(|m| m.name == mode).cloned());
        if from_preset.is_none() {
            return Err(format!("unknown mode `{mode}`"));
        }
        let mode = from_preset.unwrap();
        self.map.units[unit].modes.push(mode);
        Ok(())
    }

    fn with_bind<T>(
        &mut self,
        unit: usize,
        mode: &str,
        slot: &str,
        edit: impl FnOnce(&mut RawModeBind) -> Result<T, String>,
    ) -> Result<T, String> {
        self.ensure_unit_mode(unit, mode)?;
        let out = {
            let modes = &mut self.map.units[unit].modes;
            let mode_spec = modes
                .iter_mut()
                .find(|candidate| candidate.name == mode)
                .ok_or_else(|| format!("unknown mode `{mode}`"))?;
            let index = match mode_spec.binds.iter().position(|bind| bind.slot == slot) {
                Some(index) => index,
                None => {
                    mode_spec.binds.push(empty_bind(slot));
                    mode_spec.binds.len() - 1
                }
            };
            edit(&mut mode_spec.binds[index])?
        };
        self.dirty = true;
        self.recompile();
        Ok(out)
    }

    // ---- columns ---------------------------------------------------------

    /// Every deck group, global last: `deck1 … deckN`, then `global`.
    fn deck_entries(&self) -> Vec<(Row, Option<usize>)> {
        let mut decks: Vec<usize> = (0..self.decks).collect();
        for unit in &self.map.units {
            if let Some(deck) = unit.deck {
                if !decks.contains(&usize::from(deck)) {
                    decks.push(usize::from(deck));
                }
            }
        }
        decks.sort_unstable();
        let mut rows: Vec<(Row, Option<usize>)> = decks
            .into_iter()
            .map(|deck| {
                let units = self
                    .map
                    .units
                    .iter()
                    .filter(|unit| unit.deck.map(usize::from) == Some(deck))
                    .count();
                (
                    Row {
                        label: deck_label(Some(deck)),
                        hint: format!("{units} unit{}", if units == 1 { "" } else { "s" }),
                        muted: false,
                    },
                    Some(deck),
                )
            })
            .collect();
        let global = self
            .map
            .units
            .iter()
            .filter(|unit| unit.deck.is_none())
            .count();
        rows.push((
            Row {
                label: "global".to_owned(),
                hint: format!("{global} unit{}", if global == 1 { "" } else { "s" }),
                muted: false,
            },
            None,
        ));
        rows
    }

    /// The units of the focused deck group, with the trailing `add unit` row.
    fn unit_entries(&self) -> Vec<UnitEntry> {
        let deck = self.deck_entries();
        let selected = deck.get(self.nav.deck.min(deck.len().saturating_sub(1)));
        let deck = selected.and_then(|(_, deck)| *deck);
        let mut rows: Vec<UnitEntry> = self
            .map
            .units
            .iter()
            .enumerate()
            .filter(|(_, unit)| unit.deck.map(usize::from) == deck)
            .map(|(index, unit)| UnitEntry {
                row: Row {
                    label: unit.id.clone(),
                    hint: unit
                        .preset
                        .clone()
                        .map_or_else(|| "no preset".to_owned(), |preset| format!("preset {preset}")),
                    muted: false,
                },
                unit: Some(index),
            })
            .collect();
        rows.push(UnitEntry {
            row: Row {
                label: "add unit".to_owned(),
                hint: if deck.is_some() {
                    "from a deck preset"
                } else {
                    "from a global preset"
                }
                .to_owned(),
                muted: true,
            },
            unit: None,
        });
        rows
    }

    /// The slots of the focused unit: controls, jog halves and mode keys.
    fn slot_entries(&self) -> Vec<SlotEntry> {
        let Some(unit) = self.selected_unit_key() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for (index, slot) in self.unit_slots(unit).into_iter().enumerate() {
            let endpoint_hint = if slot.endpoints.is_empty() {
                "unbound".to_owned()
            } else {
                slot.endpoints
                    .iter()
                    .map(|(role, endpoint)| match role {
                        EndpointRole::Main => endpoint_label(endpoint),
                        EndpointRole::Press => format!("press {}", endpoint_label(endpoint)),
                        EndpointRole::Turn => format!("turn {}", endpoint_label(endpoint)),
                    })
                    .collect::<Vec<_>>()
                    .join(" + ")
            };
            let hint = match slot.role {
                SlotRole::Action => format!("{:?} · {endpoint_hint}", slot.kind),
                _ => format!("{} · {endpoint_hint}", mode_key_hint(&slot)),
            };
            rows.push(SlotEntry {
                row: Row {
                    label: slot.name.clone(),
                    hint,
                    muted: false,
                },
                slot: Some(index),
                part: None,
            });
            if slot.kind == SlotKind::JogWheel && slot.role == SlotRole::Action {
                for part in [ControlPart::Press, ControlPart::Turn] {
                    let endpoint = slot
                        .endpoints
                        .iter()
                        .find(|(role, _)| *role == part.role())
                        .map(|(_, endpoint)| endpoint_label(endpoint));
                    rows.push(SlotEntry {
                        row: Row {
                            label: part.label().to_owned(),
                            hint: endpoint.unwrap_or_else(|| "unbound".to_owned()),
                            muted: false,
                        },
                        slot: Some(index),
                        part: Some(part),
                    });
                }
            }
        }
        rows
    }

    /// Clamps every cursor into its column. Called after anything that can shrink a list.
    fn clamp_nav(&mut self) {
        let decks = self.deck_entries().len().max(1);
        self.nav.deck = self.nav.deck.min(decks - 1);
        let units = self.unit_entries().len().max(1);
        self.nav.unit = self.nav.unit.min(units - 1);
        let slots = self.slot_entries().len();
        self.nav.slot = if slots == 0 {
            0
        } else {
            self.nav.slot.min(slots - 1)
        };
        let modes = self.current_unit_modes();
        self.nav.mode = if modes.is_empty() {
            0
        } else {
            self.nav.mode.min(modes.len() - 1)
        };
    }

    /// `(index, count, name)` of the mode the info pane shows.
    fn current_mode(&self) -> Option<(usize, usize, String)> {
        let unit = self.selected_unit_key()?;
        let modes = self.unit_modes(unit);
        if modes.is_empty() {
            return None;
        }
        let index = self.nav.mode.min(modes.len() - 1);
        Some((index, modes.len(), modes[index].0.clone()))
    }

    fn current_unit_modes(&self) -> Vec<(String, usize, bool)> {
        self.selected_unit_key()
            .map(|unit| self.unit_modes(unit))
            .unwrap_or_default()
    }

    fn current_slot(&self) -> Option<SlotInfo> {
        let unit = self.selected_unit_key()?;
        let slot = self.slot_entries().get(self.nav.slot)?.slot?;
        self.unit_slots(unit).into_iter().nth(slot)
    }

    fn current_part(&self) -> Option<ControlPart> {
        self.slot_entries().get(self.nav.slot)?.part
    }

    /// `(unit, slot index)` of the selected slot row, if the row is a slot rather than `add unit`.
    fn current_slot_key(&self) -> Option<(usize, usize)> {
        let unit = self.selected_unit_key()?;
        let slot = self.slot_entries().get(self.nav.slot)?.slot?;
        Some((unit, slot))
    }

    /// The unit id's index in the file.
    pub fn unit_index(&self, id: &str) -> Option<usize> {
        self.map.units.iter().position(|unit| unit.id == id)
    }

    /// The role of the focused slot, when a slot (not `add unit`) is selected.
    pub fn current_slot_role(&self) -> Option<SlotRole> {
        self.current_slot().map(|slot| slot.role)
    }

    pub fn nav(&self) -> Nav {
        self.nav
    }

    pub fn column(&self) -> Column {
        self.nav.focus.column()
    }

    /// Moves the cursor inside the focused column, wrapping.
    pub fn move_cursor(&mut self, delta: isize) {
        let len = match self.nav.focus {
            ColumnFocus::Deck => self.deck_entries().len(),
            ColumnFocus::Unit => self.unit_entries().len(),
            ColumnFocus::Slot => self.slot_entries().len(),
        };
        if len == 0 {
            return;
        }
        let current = match self.nav.focus {
            ColumnFocus::Deck => self.nav.deck,
            ColumnFocus::Unit => self.nav.unit,
            ColumnFocus::Slot => self.nav.slot,
        };
        let next = (current as isize + delta).rem_euclid(len as isize) as usize;
        match self.nav.focus {
            ColumnFocus::Deck => {
                self.nav.deck = next;
                self.nav.unit = 0;
                self.nav.slot = 0;
                self.nav.mode = 0;
            }
            ColumnFocus::Unit => {
                self.nav.unit = next;
                self.nav.slot = 0;
                self.nav.mode = 0;
            }
            ColumnFocus::Slot => self.nav.slot = next,
        }
        self.clamp_nav();
    }

    /// Moves focus between columns.
    pub fn move_column(&mut self, delta: isize) {
        let next = match (self.nav.focus, delta.is_negative()) {
            (ColumnFocus::Deck, true) | (ColumnFocus::Slot, false) => self.nav.focus,
            (ColumnFocus::Deck, false) => ColumnFocus::Unit,
            (ColumnFocus::Unit, true) => ColumnFocus::Deck,
            (ColumnFocus::Unit, false) => ColumnFocus::Slot,
            (ColumnFocus::Slot, true) => ColumnFocus::Unit,
        };
        self.nav.focus = next;
    }

    /// `Enter`: drill into the focused column, or learn the focused slot.
    pub fn enter(&mut self) -> Result<Entered, String> {
        match self.nav.focus {
            ColumnFocus::Deck => {
                self.nav.focus = ColumnFocus::Unit;
                Ok(Entered::Moved)
            }
            ColumnFocus::Unit => {
                if self.selected_unit_key().is_none() {
                    return Ok(Entered::AddUnit);
                }
                self.nav.focus = ColumnFocus::Slot;
                Ok(Entered::Moved)
            }
            ColumnFocus::Slot => {
                if self.current_slot_key().is_none() {
                    return Ok(Entered::Moved);
                }
                self.begin_learn()?;
                Ok(Entered::Learning)
            }
        }
    }

    /// Steps the mode the info pane shows, which is also the mode an action edit writes to.
    pub fn step_mode(&mut self, delta: isize) {
        let modes = self.current_unit_modes();
        if modes.is_empty() {
            self.message = Some("this unit has no modes".to_owned());
            return;
        }
        let next = (self.nav.mode as isize + delta).rem_euclid(modes.len() as isize) as usize;
        self.nav.mode = next;
        self.message = Some(format!("mode {}", modes[next].0));
    }

    /// Points every column at one unit.
    pub fn select_unit(&mut self, unit: usize) {
        let decks = self.deck_entries();
        let deck = self.map.units.get(unit).and_then(|unit| unit.deck.map(usize::from));
        if let Some(index) = decks.iter().position(|(_, deck_id)| *deck_id == deck) {
            self.nav.deck = index;
        }
        if let Some(index) = self
            .unit_entries()
            .iter()
            .position(|entry| entry.unit == Some(unit))
        {
            self.nav.unit = index;
        }
        self.nav.slot = 0;
        self.nav.mode = 0;
        self.nav.focus = ColumnFocus::Unit;
        self.clamp_nav();
    }

    /// Points the slot column at one slot name.
    pub fn select_slot(&mut self, unit: usize, slot: &str) {
        self.select_unit(unit);
        let slots = self.unit_slots(unit);
        let Some(index) = slots.iter().position(|candidate| candidate.name == slot) else {
            return;
        };
        if let Some(row) = self
            .slot_entries()
            .iter()
            .position(|entry| entry.slot == Some(index) && entry.part.is_none())
        {
            self.nav.slot = row;
        }
        self.nav.focus = ColumnFocus::Slot;
        self.clamp_nav();
    }

    pub fn view(&self) -> View {
        View {
            path: self.path.clone(),
            device: self.device(),
            dirty: self.dirty,
            message: self.message.clone(),
            error: self.error.clone(),
            decks: self.deck_entries().into_iter().map(|(row, _)| row).collect(),
            units: self.unit_entries().into_iter().map(|entry| entry.row).collect(),
            slots: self.slot_entries().into_iter().map(|entry| entry.row).collect(),
            nav: self.nav,
            details: self.details(),
            endpoints: self.endpoints(),
            buttons: self.buttons(),
            capturing: self.capture.is_some(),
            candidate: self.candidate().copied(),
            mode: self.current_mode(),
        }
    }

    fn details(&self) -> Vec<(String, String)> {
        let mut details = Vec::new();
        let deck = self.deck_entries();
        let deck = deck.get(self.nav.deck).map(|(row, _)| row.label.clone());
        if let Some(unit) = self.selected_unit_key() {
            let raw = &self.map.units[unit];
            details.push((
                "unit".to_owned(),
                format!(
                    "{} · {}{}",
                    raw.id,
                    deck.unwrap_or_else(|| "global".to_owned()),
                    raw.preset
                        .as_deref()
                        .map_or_else(String::new, |preset| format!(" · preset {preset}"))
                ),
            ));
        } else {
            details.push((
                "unit".to_owned(),
                format!("— · {}", deck.unwrap_or_else(|| "global".to_owned())),
            ));
        }
        if let Some((index, count, name)) = self.current_mode() {
            let start = self
                .selected_unit_key()
                .and_then(|unit| self.unit_start(unit))
                .is_some_and(|start| start == index);
            details.push((
                "mode".to_owned(),
                format!(
                    "‹ {name} ›  {}/{count}{}",
                    index + 1,
                    if start { " · start" } else { "" }
                ),
            ));
        }
        if let Some(slot) = self.current_slot() {
            let source = if slot.from_preset { "preset" } else { "unit" };
            match slot.role {
                SlotRole::Action => details.push((
                    "slot".to_owned(),
                    format!("{:?} · {} · {source}", slot.kind, slot.name),
                )),
                _ => details.push((
                    "slot".to_owned(),
                    format!("{} · {}", mode_key_hint(&slot), slot.name),
                )),
            }
            details.push((
                "endpoint".to_owned(),
                if slot.endpoints.is_empty() {
                    "unbound".to_owned()
                } else {
                    slot.endpoints
                        .iter()
                        .map(|(role, endpoint)| match role {
                            EndpointRole::Main => endpoint_label(endpoint),
                            EndpointRole::Press => format!("press {}", endpoint_label(endpoint)),
                            EndpointRole::Turn => format!("turn {}", endpoint_label(endpoint)),
                        })
                        .collect::<Vec<_>>()
                        .join(" + ")
                },
            ));
            if slot.role == SlotRole::Action {
                if let (Some(unit), Some((_, _, mode))) =
                    (self.selected_unit_key(), self.current_mode())
                {
                    let part = self.current_part();
                    if let Some(part) = part {
                        details.push(("part".to_owned(), part.label().to_owned()));
                    }
                    let action = self
                        .bind_action(unit, &mode, &slot.name, part)
                        .unwrap_or_else(|| "—".to_owned());
                    details.push(("action".to_owned(), action));
                    if let Some(edge) = self.bind_on(unit, &mode, &slot.name) {
                        details.push(("on".to_owned(), format!("{edge:?}").to_lowercase()));
                    }
                }
            }
        }
        details
    }

    fn endpoints(&self) -> Vec<(EventKind, u8, u8)> {
        let Some(slot) = self.current_slot() else {
            return Vec::new();
        };
        let part = self.current_part();
        slot.endpoints
            .iter()
            .filter(|(role, _)| part.is_none_or(|part| *role == part.role()))
            .flat_map(|(_, endpoint)| endpoint_keys(endpoint))
            .collect()
    }

    /// The command row. Deliberately short: adding a unit is `Enter` on its row, and modes are not
    /// edited in this front-end yet.
    fn buttons(&self) -> Vec<TreeButton> {
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
        let mut buttons = Vec::new();
        match self.nav.focus {
            // Cloning is about decks: only a unit that owns a deck can be copied to another one.
            ColumnFocus::Unit => {
                if self
                    .selected_unit_key()
                    .is_some_and(|unit| self.map.units[unit].deck.is_some())
                {
                    buttons.push(TreeButton::Clone);
                }
            }
            ColumnFocus::Slot => {
                if self.current_slot_key().is_some() {
                    buttons.push(TreeButton::Learn);
                }
            }
            ColumnFocus::Deck => {}
        }
        buttons.extend([
            TreeButton::Connect,
            TreeButton::Open,
            TreeButton::Save,
            TreeButton::Exit,
        ]);
        buttons
    }

    // ---- learn -----------------------------------------------------------

    pub fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }

    pub fn candidate(&self) -> Option<&Candidate> {
        self.capture.as_ref().and_then(|capture| capture.candidate.as_ref())
    }

    pub fn cancel_learn(&mut self) {
        if self.capture.take().is_some() {
            self.message = Some("learn cancelled".to_owned());
        }
    }

    /// Cycles the candidate's encoding: absolute → two_point → twos_complement → offset64 → direction.
    pub fn cycle_candidate_mode(&mut self) {
        let Some(capture) = self.capture.as_mut() else {
            self.message = Some("nothing captured yet".to_owned());
            return;
        };
        let Some(candidate) = capture.candidate.as_mut() else {
            self.message = Some("nothing captured yet".to_owned());
            return;
        };
        if candidate.kind != EventKind::Cc {
            self.message = Some("only a CC has an encoding".to_owned());
            return;
        }
        if candidate.lsb.is_some() {
            self.message = Some("a 14-bit pair is always absolute".to_owned());
            return;
        }
        candidate.encoding = match candidate.encoding {
            Encoding::Absolute => Encoding::TwoPoint,
            Encoding::TwoPoint => Encoding::TwosComplement,
            Encoding::TwosComplement => Encoding::Offset64,
            Encoding::Offset64 => Encoding::Direction,
            Encoding::Direction => Encoding::Absolute,
        };
    }

    /// Starts learning the focused slot (or the focused jog half).
    pub fn begin_learn(&mut self) -> Result<(), String> {
        let Some((unit, control)) = self.current_slot_key() else {
            return Err("select a slot to learn".to_owned());
        };
        let slots = self.unit_slots(unit);
        let Some(slot) = slots.get(control) else {
            return Err("select a slot to learn".to_owned());
        };
        let selector = match slot.role {
            SlotRole::Action => None,
            SlotRole::ModeTrigger => Some(SelectorCapture {
                role: SlotRole::ModeTrigger,
                target: slot.target.clone(),
            }),
            SlotRole::ModeCycle => Some(SelectorCapture {
                role: SlotRole::ModeCycle,
                target: None,
            }),
        };
        self.capture = Some(Capture {
            unit,
            slot: slot.name.clone(),
            kind: slot.kind,
            part: JogPart::Press,
            press: None,
            selector,
            candidate: None,
            last_cc: None,
            previous_cc: None,
            two_point: false,
            swept: false,
        });
        self.message = Some(match (slot.kind, slot.role) {
            (_, SlotRole::ModeTrigger) | (_, SlotRole::ModeCycle) => {
                "press the mode key now".to_owned()
            }
            (SlotKind::JogWheel, _) => "press the platter first".to_owned(),
            _ => format!("press or turn `{}` now", slot.name),
        });
        Ok(())
    }

    /// Consumes one inbound event while learning.
    pub fn feed(&mut self, event: &Event) -> GuideStep {
        self.feed_at(event, None)
    }

    /// As [`Guide::feed`], with the arrival time so a 14-bit MSB/LSB pair can be recognised: the two
    /// controllers of one fader arrive within a few milliseconds of each other, and one of them is
    /// the other's controller number plus 32.
    pub fn feed_at(&mut self, event: &Event, at_ms: Option<u64>) -> GuideStep {
        /// How far apart the two halves of a 14-bit pair may be and still count as one gesture.
        const PAIR_WINDOW_MS: u64 = 120;

        let Some(capture) = self.capture.as_mut() else {
            return GuideStep::Ignored;
        };
        match *event {
            Event::ControlChange {
                channel,
                controller,
                value,
            } => {
                // A second controller arriving in the same gesture is the other half of a 14-bit
                // pair when the two numbers differ by the standard 32.
                if let (Some((previous, previous_at)), Some(now)) = (capture.previous_cc, at_ms) {
                    let pair = controller.abs_diff(previous) == 32
                        && now.saturating_sub(previous_at) <= PAIR_WINDOW_MS;
                    if pair {
                        let (msb, lsb) = if controller < previous {
                            (controller, previous)
                        } else {
                            (previous, controller)
                        };
                        capture.previous_cc = None;
                        capture.last_cc = None;
                        capture.candidate = Some(Candidate {
                            kind: EventKind::Cc,
                            channel,
                            id: msb,
                            encoding: Encoding::Absolute,
                            lsb: Some(lsb),
                        });
                        return GuideStep::RelativeDetected;
                    }
                }
                capture.previous_cc = at_ms.map(|now| (controller, now));

                if value != 1 && value != 127 {
                    capture.swept = true;
                }
                if value == 1 || value == 127 {
                    let other = if value == 1 { 127 } else { 1 };
                    if capture.last_cc == Some(other) {
                        capture.two_point = true;
                    }
                }
                capture.last_cc = Some(value);
                let encoding = if capture.two_point && !capture.swept {
                    Encoding::TwoPoint
                } else {
                    Encoding::Absolute
                };
                let step = if encoding == Encoding::TwoPoint {
                    GuideStep::RelativeDetected
                } else {
                    GuideStep::Captured
                };
                capture.candidate = Some(Candidate {
                    kind: EventKind::Cc,
                    channel,
                    id: controller,
                    encoding,
                    lsb: None,
                });
                step
            }
            Event::NoteOn {
                channel,
                key,
                velocity,
            } if velocity > 0 => {
                capture.last_cc = None;
                capture.previous_cc = None;
                capture.candidate = Some(Candidate {
                    kind: EventKind::Note,
                    channel,
                    id: key,
                    encoding: Encoding::Absolute,
                    lsb: None,
                });
                GuideStep::Captured
            }
            Event::NoteOn { .. } | Event::NoteOff { .. } => GuideStep::Ignored,
            Event::PitchBend { channel, .. } => {
                capture.last_cc = None;
                capture.previous_cc = None;
                capture.candidate = Some(Candidate {
                    kind: EventKind::Bend,
                    channel,
                    id: 0,
                    encoding: Encoding::Absolute,
                    lsb: None,
                });
                GuideStep::Captured
            }
        }
    }

    pub fn confirm_learn(&mut self) -> Result<(), String> {
        let Some(capture) = self.capture.take() else {
            return Err("nothing is being learned".to_owned());
        };
        let candidate = match capture.candidate {
            Some(candidate) => candidate,
            None => {
                self.capture = Some(capture);
                return Err("press or turn a control first".to_owned());
            }
        };
        // A jog is two facts: the press, then the wheel.
        if capture.selector.is_none()
            && capture.kind == SlotKind::JogWheel
            && capture.part == JogPart::Press
            && capture.press.is_none()
        {
            let mut next = capture;
            next.press = Some(candidate);
            next.part = JogPart::Turn;
            next.candidate = None;
            next.last_cc = None;
            next.previous_cc = None;
            next.two_point = false;
            next.swept = false;
            self.capture = Some(next);
            self.message = Some("now turn the wheel one detent".to_owned());
            return Ok(());
        }
        match self.commit_capture(&capture, candidate) {
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

    fn commit_capture(&mut self, capture: &Capture, candidate: Candidate) -> Result<(), String> {
        if let Some(selector) = &capture.selector {
            if candidate.kind == EventKind::Bend
                || (candidate.kind == EventKind::Cc && candidate.encoding.is_relative())
                || candidate.lsb.is_some()
            {
                return Err("a mode key is a note or an absolute CC".to_owned());
            }
            let raw = RawControl {
                slot: capture.slot.clone(),
                kind: Some(SlotKind::Button),
                role: selector.role,
                target: selector.target.clone(),
                endpoint: Some(crate::map::OneOrMany::One(endpoint_of(
                    candidate,
                    EndpointRole::Main,
                ))),
            };
            let slot_name = capture.slot.clone();
            let unit = capture.unit;
            self.with_map(|map| {
                let unit = &mut map.units[unit];
                match unit.controls.iter_mut().find(|control| control.slot == slot_name) {
                    Some(existing) => *existing = raw,
                    None => unit.controls.push(raw),
                }
                Ok(())
            })?;
            self.select_slot(unit, &slot_name);
            return Ok(());
        }

        let press = match (capture.kind, capture.part) {
            (SlotKind::JogWheel, JogPart::Turn) => Some(
                capture
                    .press
                    .ok_or_else(|| "the platter press was not captured".to_owned())?,
            ),
            _ => None,
        };
        if capture.kind == SlotKind::JogWheel && press.is_none() {
            return Err("press the platter first".to_owned());
        }
        if capture.kind == SlotKind::JogWheel
            && (candidate.kind != EventKind::Cc || !candidate.encoding.is_relative())
        {
            return Err(
                "the jog wheel is a relative CC; turn it again (or press Encoding)".to_owned(),
            );
        }
        let raw = raw_control(&capture.slot, capture.kind, candidate, press);
        let slot_name = capture.slot.clone();
        let unit = capture.unit;
        self.with_map(|map| {
            let unit = &mut map.units[unit];
            match unit.controls.iter_mut().find(|control| control.slot == slot_name) {
                Some(existing) => *existing = raw,
                None => unit.controls.push(raw),
            }
            Ok(())
        })?;
        self.select_slot(unit, &slot_name);
        Ok(())
    }

    // ---- edits -----------------------------------------------------------

    /// The action line of the focused slot in the focused mode, if it has one.
    pub fn action_text(&self) -> Option<String> {
        let (unit, control) = self.current_slot_key()?;
        let slot = self.unit_slots(unit).into_iter().nth(control)?;
        let (_, _, mode) = self.current_mode()?;
        self.bind_action(unit, &mode, &slot.name, self.current_part())
    }

    pub fn set_action(&mut self, text: String) -> Result<(), String> {
        let (unit, control) = self
            .current_slot_key()
            .ok_or_else(|| "select a slot first".to_owned())?;
        let slot = self
            .unit_slots(unit)
            .into_iter()
            .nth(control)
            .ok_or_else(|| "the slot disappeared".to_owned())?;
        if slot.role != SlotRole::Action {
            return Err("a mode key has no action".to_owned());
        }
        let (_, _, mode) = self
            .current_mode()
            .ok_or_else(|| "the unit has no modes".to_owned())?;
        let part = self.current_part();
        let slot_name = slot.name.clone();
        let text = text.trim().to_owned();
        self.with_bind(unit, &mode, &slot_name, |bind| {
            match part {
                None => bind.action = Some(text),
                Some(ControlPart::Press) => bind.press = Some(text),
                Some(ControlPart::Turn) => bind.turn = Some(text),
            }
            Ok(())
        })
    }

    pub fn on(&self) -> Option<Edge> {
        let (unit, control) = self.current_slot_key()?;
        let slot = self.unit_slots(unit).into_iter().nth(control)?;
        let (_, _, mode) = self.current_mode()?;
        self.bind_on(unit, &mode, &slot.name)
    }

    pub fn set_on(&mut self, edge: Option<Edge>) -> Result<(), String> {
        let (unit, control) = self
            .current_slot_key()
            .ok_or_else(|| "select a slot first".to_owned())?;
        let slot = self
            .unit_slots(unit)
            .into_iter()
            .nth(control)
            .ok_or_else(|| "the slot disappeared".to_owned())?;
        let (_, _, mode) = self
            .current_mode()
            .ok_or_else(|| "the unit has no modes".to_owned())?;
        let slot_name = slot.name.clone();
        self.with_bind(unit, &mode, &slot_name, |bind| {
            bind.on = edge;
            Ok(())
        })
    }

    // ---- modes -----------------------------------------------------------

    pub fn mode_names(&self) -> Vec<String> {
        self.current_unit_modes()
            .into_iter()
            .map(|(name, _, _)| name)
            .collect()
    }

    pub fn add_mode(&mut self, name: String) -> Result<(), String> {
        let unit = self
            .selected_unit_key()
            .ok_or_else(|| "select a unit first".to_owned())?;
        let name = name.trim().to_owned();
        validate_name("mode", &name)?;
        if self.unit_mode_names(unit).contains(&name) {
            return Err(format!("mode `{name}` already exists"));
        }
        self.with_map(|map| {
            map.units[unit].modes.push(RawMode {
                name: name.clone(),
                label: None,
                binds: Vec::new(),
            });
            Ok(())
        })?;
        if let Some(index) = self
            .unit_mode_names(unit)
            .iter()
            .position(|candidate| *candidate == name)
        {
            self.nav.mode = index;
        }
        Ok(())
    }

    pub fn delete_mode(&mut self) -> Result<(), String> {
        let unit = self
            .selected_unit_key()
            .ok_or_else(|| "select a unit first".to_owned())?;
        let (index, _, name) = self
            .current_mode()
            .ok_or_else(|| "the unit has no modes".to_owned())?;
        if self.unit_modes(unit).get(index).is_some_and(|(_, _, from_preset)| !*from_preset)
            && self.unit_modes(unit).len() == 1
        {
            return Err("a unit needs at least one mode".to_owned());
        }
        if self.unit_modes(unit).get(index).is_some_and(|(_, _, from_preset)| *from_preset) {
            return Err(format!(
                "`{name}` comes from the preset; add a unit mode of the same name to override it"
            ));
        }
        self.with_map(|map| {
            let unit = &mut map.units[unit];
            unit.modes.retain(|candidate| candidate.name != name);
            if let Some(order) = &mut unit.mode_order {
                order.retain(|candidate| *candidate != name);
            }
            if unit.start.as_deref() == Some(name.as_str()) {
                unit.start = None;
            }
            Ok(())
        })?;
        self.clamp_nav();
        Ok(())
    }

    pub fn move_mode(&mut self, delta: isize) -> Result<(), String> {
        let unit = self
            .selected_unit_key()
            .ok_or_else(|| "select a unit first".to_owned())?;
        let names = self.unit_mode_names(unit);
        if names.len() < 2 {
            return Err("this unit has one mode".to_owned());
        }
        let current = self.nav.mode.min(names.len() - 1);
        let target = (current as isize + delta).clamp(0, names.len() as isize - 1) as usize;
        if target == current {
            return Ok(());
        }
        let mut order = names;
        let moved = order.remove(current);
        order.insert(target, moved);
        self.with_map(|map| {
            map.units[unit].mode_order = Some(order);
            Ok(())
        })?;
        self.nav.mode = target;
        self.clamp_nav();
        Ok(())
    }

    pub fn set_start(&mut self) -> Result<(), String> {
        let unit = self
            .selected_unit_key()
            .ok_or_else(|| "select a unit first".to_owned())?;
        let (_, _, name) = self
            .current_mode()
            .ok_or_else(|| "the unit has no modes".to_owned())?;
        self.with_map(|map| {
            map.units[unit].start = Some(name);
            Ok(())
        })
    }

    // ---- mode keys -------------------------------------------------------

    /// Adds a mode-key control and starts learning its endpoint.
    pub fn begin_mode_key(
        &mut self,
        trigger: bool,
        target: Option<String>,
    ) -> Result<(), String> {
        let unit = self
            .selected_unit_key()
            .ok_or_else(|| "select a unit first".to_owned())?;
        let role = if trigger {
            let wanted = target
                .as_deref()
                .ok_or_else(|| "a mode key needs a target mode".to_owned())?;
            if !self.unit_mode_names(unit).contains(&wanted.to_owned()) {
                return Err(format!("unknown mode `{wanted}`"));
            }
            SlotRole::ModeTrigger
        } else {
            SlotRole::ModeCycle
        };
        let name = self.unique_slot_name(unit, if trigger { "mode" } else { "mode-next" });
        self.capture = Some(Capture {
            unit,
            slot: name,
            kind: SlotKind::Button,
            part: JogPart::Press,
            press: None,
            selector: Some(SelectorCapture {
                role,
                target: target.clone(),
            }),
            candidate: None,
            last_cc: None,
            previous_cc: None,
            two_point: false,
            swept: false,
        });
        self.message = Some("press the mode key now".to_owned());
        Ok(())
    }

    /// Repoints the focused mode key.
    pub fn set_mode_key(&mut self, trigger: bool, target: Option<String>) -> Result<(), String> {
        let (unit, control) = self
            .current_slot_key()
            .ok_or_else(|| "select a mode key first".to_owned())?;
        let slot = self
            .unit_slots(unit)
            .into_iter()
            .nth(control)
            .ok_or_else(|| "unknown slot".to_owned())?;
        if slot.role == SlotRole::Action {
            return Err("that slot is not a mode key".to_owned());
        }
        let role = if trigger {
            let wanted = target
                .as_deref()
                .ok_or_else(|| "a mode key needs a target mode".to_owned())?;
            if !self.unit_mode_names(unit).contains(&wanted.to_owned()) {
                return Err(format!("unknown mode `{wanted}`"));
            }
            SlotRole::ModeTrigger
        } else {
            SlotRole::ModeCycle
        };
        let name = slot.name;
        self.with_map(|map| {
            let Some(control) = map.units[unit]
                .controls
                .iter_mut()
                .find(|candidate| candidate.slot == name)
            else {
                return Err("only a unit-owned mode key can be repointed".to_owned());
            };
            control.role = role;
            control.target = target;
            Ok(())
        })
    }

    /// Removes the focused unit's `add unit` placeholder, or the focused slot.
    pub fn delete_selected(&mut self) -> Result<(), String> {
        match self.nav.focus {
            ColumnFocus::Deck => Ok(()),
            ColumnFocus::Unit => {
                let Some(unit) = self.selected_unit_key() else {
                    return Ok(());
                };
                self.with_map(|map| {
                    map.units.remove(unit);
                    Ok(())
                })?;
                self.nav.unit = self.nav.unit.saturating_sub(1);
                self.clamp_nav();
                Ok(())
            }
            ColumnFocus::Slot => {
                let Some((unit, control)) = self.current_slot_key() else {
                    return Ok(());
                };
                let slot = self
                    .unit_slots(unit)
                    .into_iter()
                    .nth(control)
                    .ok_or_else(|| "unknown slot".to_owned())?;
                let name = slot.name;
                self.with_map(|map| {
                    map.units[unit].controls.retain(|control| control.slot != name);
                    Ok(())
                })?;
                self.clamp_nav();
                self.message = Some("slot unbound".to_owned());
                Ok(())
            }
        }
    }

    fn unique_slot_name(&self, unit: usize, base: &str) -> String {
        let taken: Vec<String> = self
            .unit_slots(unit)
            .into_iter()
            .map(|slot| slot.name)
            .collect();
        if !taken.iter().any(|name| name == base) {
            return base.to_owned();
        }
        for suffix in 2.. {
            let candidate = format!("{base}-{suffix}");
            if !taken.iter().any(|name| name == &candidate) {
                return candidate;
            }
        }
        unreachable!()
    }
}

fn empty_bind(slot: &str) -> RawModeBind {
    RawModeBind {
        slot: slot.to_owned(),
        action: None,
        press: None,
        turn: None,
        down: None,
        deck: None,
        min: None,
        max: None,
        curve: None,
        chain: None,
        fx: None,
        on: None,
    }
}

fn raw_endpoints(control: &RawControl) -> Vec<(EndpointRole, RawEndpoint)> {
    control
        .endpoint
        .as_ref()
        .map(|endpoints| {
            endpoints
                .as_slice()
                .iter()
                .map(|endpoint| (endpoint.role, endpoint.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn endpoint_keys(endpoint: &RawEndpoint) -> Vec<(EventKind, u8, u8)> {
    let mut keys = vec![(
        endpoint.midi,
        endpoint.channel,
        if endpoint.midi == EventKind::Bend {
            0
        } else {
            endpoint.id.unwrap_or(0)
        },
    )];
    if let Some(lsb) = endpoint.lsb {
        keys.push((EventKind::Cc, endpoint.channel, lsb));
    }
    keys
}

fn endpoint_label(endpoint: &RawEndpoint) -> String {
    if let Some(lsb) = endpoint.lsb {
        return format!(
            "cc14 ch{} #{}/#{}",
            endpoint.channel + 1,
            endpoint.id.unwrap_or(0),
            lsb
        );
    }
    let kind = match endpoint.midi {
        EventKind::Cc => "cc",
        EventKind::Note => "note",
        EventKind::Bend => "bend",
    };
    let encoding = match endpoint.encoding {
        Encoding::Absolute => "",
        Encoding::Offset64 => " offset64",
        Encoding::TwosComplement => " twos_complement",
        Encoding::Direction => " direction",
        Encoding::TwoPoint => " two_point",
    };
    let reverse = if endpoint.reverse { " reverse" } else { "" };
    let id = endpoint.id.map_or_else(String::new, |id| format!(" #{id}"));
    format!("{kind}{encoding}{reverse} ch{}{id}", endpoint.channel + 1)
}

fn endpoint_of(candidate: Candidate, role: EndpointRole) -> RawEndpoint {
    RawEndpoint {
        role,
        midi: candidate.kind,
        encoding: if candidate.kind == EventKind::Cc {
            candidate.encoding
        } else {
            Encoding::Absolute
        },
        lsb: candidate.lsb,
        reverse: false,
        channel: candidate.channel,
        id: (candidate.kind != EventKind::Bend).then_some(candidate.id),
    }
}

fn raw_control(
    slot: &str,
    kind: SlotKind,
    candidate: Candidate,
    press: Option<Candidate>,
) -> RawControl {
    let endpoint = if kind == SlotKind::JogWheel {
        let press = press.unwrap_or(candidate);
        crate::map::OneOrMany::Many(vec![
            endpoint_of(press, EndpointRole::Press),
            endpoint_of(candidate, EndpointRole::Turn),
        ])
    } else {
        crate::map::OneOrMany::One(endpoint_of(candidate, EndpointRole::Main))
    };
    RawControl {
        slot: slot.to_owned(),
        kind: Some(kind),
        role: SlotRole::Action,
        target: None,
        endpoint: Some(endpoint),
    }
}

fn mode_key_hint(slot: &SlotInfo) -> String {
    match slot.role {
        SlotRole::ModeTrigger => format!("trigger → {}", slot.target.as_deref().unwrap_or("?")),
        SlotRole::ModeCycle => "cycle".to_owned(),
        SlotRole::Action => String::new(),
    }
}

fn deck_label(deck: Option<usize>) -> String {
    deck.map_or_else(|| "global".to_owned(), |deck| format!("deck{}", deck + 1))
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

fn rewrite_chain(token: &str, old: u8, new: u8) -> String {
    let old = old + 1;
    let new = new + 1;
    let forms = [
        (format!("deck{old}"), format!("deck{new}")),
        (format!("d{old}"), format!("deck{new}")),
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

fn clone_id(old: &str, old_deck: u8, new_deck: u8) -> String {
    let old_prefix = format!("deck{}", old_deck + 1);
    let new_prefix = format!("deck{}", new_deck + 1);
    if let Some(rest) = old.strip_prefix(&format!("{old_prefix}.")) {
        return format!("{new_prefix}.{rest}");
    }
    if let Some(rest) = old.strip_prefix(&old_prefix) {
        return format!("{new_prefix}{rest}");
    }
    format!("{old}.{new_prefix}")
}

fn validate_name(what: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(format!("{what} `{value}` cannot contain whitespace"));
    }
    Ok(())
}

/// The action verbs the guide offers, straight from the shared table.
pub fn action_names() -> Vec<&'static str> {
    crate::map::ACTIONS.iter().map(|spec| spec.name).collect()
}
#[cfg(test)]
mod tests {
    use super::*;

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

    fn tmp_path(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "hypermixx-guide-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("map.toml").display().to_string()
    }

    /// Walks the columns to one slot and returns the guide with the slot column focused.
    fn at_slot(mut guide: Guide, unit: usize, slot: &str) -> Guide {
        guide.select_slot(unit, slot);
        guide
    }

    #[test]
    fn the_empty_columns_show_decsk_global_and_add_rows() {
        let guide = Guide::empty(2).unwrap();
        let view = guide.view();
        let labels: Vec<&str> = view.decks.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels, vec!["deck1", "deck2", "global"]);
        assert_eq!(view.units.len(), 1, "only the add row");
        assert!(view.units[0].muted);
        assert!(view.slots.is_empty());
    }

    #[test]
    fn a_deck_preset_needs_a_deck_and_a_global_one_refuses_it() {
        let mut guide = Guide::empty(2).unwrap();
        assert!(guide.new_unit("mixer", None, None).is_err());
        assert!(guide.new_unit("output", Some(0), None).is_err());
        guide.new_unit("output", None, None).unwrap();
        assert_eq!(guide.map.units[0].deck, None);
        assert_eq!(guide.map.units[0].id, "global.output");
    }

    #[test]
    fn a_both_preset_can_be_global_or_per_deck() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("browser", None, None).unwrap();
        guide.new_unit("browser", Some(0), None).unwrap();
        assert_eq!(guide.map.units[0].id, "global.browser");
        assert_eq!(guide.map.units[1].id, "deck1.browser");
        assert_eq!(guide.map.units[1].deck, Some(0));
    }

    #[test]
    fn the_slot_column_lists_the_preset_slots_and_jog_halves() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("jog", Some(0), None).unwrap();
        let labels: Vec<String> = guide
            .view()
            .slots
            .into_iter()
            .map(|row| row.label)
            .collect();
        assert_eq!(labels, vec!["jog", "press", "turn"]);
    }

    #[test]
    fn moving_the_cursor_walks_the_three_columns() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("pad", Some(0), None).unwrap();
        // Creating a unit leaves the cursor on it, so step back to the deck column first.
        guide.move_column(-1);
        assert_eq!(guide.column(), Column::Deck);
        guide.move_column(1);
        assert_eq!(guide.column(), Column::Unit);
        guide.move_column(1);
        assert_eq!(guide.column(), Column::Slot);
        assert_eq!(guide.nav().slot, 0);
        guide.move_cursor(1);
        assert_eq!(guide.nav().slot, 1);
        guide.move_column(-1);
        assert_eq!(guide.column(), Column::Unit);
    }

    #[test]
    fn enter_on_the_add_row_reports_that_a_preset_is_needed() {
        let mut guide = Guide::empty(2).unwrap();
        guide.move_column(1);
        assert_eq!(guide.enter().unwrap(), Entered::AddUnit);
    }

    #[test]
    fn learning_a_note_writes_a_button_endpoint() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("transport", Some(0), None).unwrap();
        let mut guide = at_slot(guide, 0, "play");
        assert_eq!(guide.enter().unwrap(), Entered::Learning);
        guide.feed(&note(0, 16));
        guide.confirm_learn().unwrap();

        let control = &guide.map.units[0].controls[0];
        let endpoint = control.endpoint.as_ref().unwrap().as_slice()[0].clone();
        assert_eq!(endpoint.midi, EventKind::Note);
        assert_eq!(endpoint.id, Some(16));
        assert_eq!(endpoint.channel, 0);
    }

    #[test]
    fn a_14_bit_fader_is_recognised_from_its_two_controllers() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("tempofader", Some(0), None).unwrap();
        let mut guide = at_slot(guide, 0, "fader");
        guide.begin_learn().unwrap();
        // The fader sends its MSB (#9) then its LSB (#41) a millisecond apart.
        assert_eq!(guide.feed_at(&cc(0, 9, 64), Some(1_000)), GuideStep::Captured);
        assert_eq!(
            guide.feed_at(&cc(0, 41, 10), Some(1_001)),
            GuideStep::RelativeDetected
        );
        let candidate = guide.candidate().copied().expect("candidate");
        assert_eq!(candidate.id, 9);
        assert_eq!(candidate.lsb, Some(41));
        guide.confirm_learn().unwrap();

        let control = &guide.map.units[0].controls[0];
        let endpoint = control.endpoint.as_ref().unwrap().as_slice()[0].clone();
        assert_eq!(endpoint.id, Some(9));
        assert_eq!(endpoint.lsb, Some(41));
    }

    #[test]
    fn two_controllers_far_apart_are_not_a_14_bit_pair() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("mixer", Some(0), None).unwrap();
        let mut guide = at_slot(guide, 0, "fader");
        guide.begin_learn().unwrap();
        guide.feed_at(&cc(0, 9, 64), Some(1_000));
        guide.feed_at(&cc(0, 41, 10), Some(1_500));
        let candidate = guide.candidate().copied().expect("candidate");
        assert_eq!(candidate.id, 41);
        assert_eq!(candidate.lsb, None);
    }

    #[test]
    fn learning_a_two_point_encoder_picks_that_encoding() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("browser", Some(0), None).unwrap();
        let mut guide = at_slot(guide, 0, "browse");
        guide.begin_learn().unwrap();
        guide.feed(&cc(0, 7, 1));
        assert_eq!(guide.feed(&cc(0, 7, 127)), GuideStep::RelativeDetected);
        guide.confirm_learn().unwrap();
        let control = &guide.map.units[0].controls[0];
        let endpoint = control.endpoint.as_ref().unwrap().as_slice()[0].clone();
        assert_eq!(endpoint.encoding, Encoding::TwoPoint);
    }

    #[test]
    fn learning_a_swept_cc_keeps_it_absolute() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("mixer", Some(0), None).unwrap();
        let mut guide = at_slot(guide, 0, "fader");
        guide.begin_learn().unwrap();
        guide.feed(&cc(0, 7, 1));
        guide.feed(&cc(0, 7, 64));
        guide.feed(&cc(0, 7, 127));
        guide.confirm_learn().unwrap();
        let control = &guide.map.units[0].controls[0];
        let endpoint = control.endpoint.as_ref().unwrap().as_slice()[0].clone();
        assert_eq!(endpoint.encoding, Encoding::Absolute);
    }

    #[test]
    fn a_jog_learns_press_then_wheel() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("jog", Some(0), None).unwrap();
        let mut guide = at_slot(guide, 0, "jog");
        guide.begin_learn().unwrap();
        guide.feed(&note(0, 80));
        guide.confirm_learn().unwrap();
        guide.feed(&cc(0, 6, 1));
        guide.feed(&cc(0, 6, 127));
        guide.confirm_learn().unwrap();

        let control = &guide.map.units[0].controls[0];
        let endpoints = control.endpoint.as_ref().unwrap().as_slice();
        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0].role, EndpointRole::Press);
        assert_eq!(endpoints[0].id, Some(80));
        assert_eq!(endpoints[1].role, EndpointRole::Turn);
        assert_eq!(endpoints[1].encoding, Encoding::TwoPoint);
    }

    #[test]
    fn a_jog_half_highlights_only_its_own_endpoint() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("jog", Some(0), None).unwrap();
        let mut guide = at_slot(guide, 0, "jog");
        guide.begin_learn().unwrap();
        guide.feed(&note(0, 80));
        guide.confirm_learn().unwrap();
        guide.feed(&cc(0, 6, 1));
        guide.feed(&cc(0, 6, 127));
        guide.confirm_learn().unwrap();

        guide.select_slot(0, "jog");
        guide.move_cursor(1); // press
        assert_eq!(guide.view().endpoints.len(), 1);
        guide.move_cursor(1); // turn
        assert_eq!(guide.view().endpoints.len(), 1);
    }

    #[test]
    fn editing_an_action_materializes_the_whole_mode() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("mixer", Some(0), None).unwrap();
        let mut guide = at_slot(guide, 0, "fader");
        assert_eq!(guide.action_text().as_deref(), Some("fader flow"));
        guide.set_action("fader gain".to_owned()).unwrap();

        let mode = guide.map.units[0]
            .modes
            .iter()
            .find(|mode| mode.name == "default")
            .expect("materialized mode");
        assert!(mode.binds.iter().any(|bind| bind.slot == "load"));
        let fader = mode.binds.iter().find(|bind| bind.slot == "fader").unwrap();
        assert_eq!(fader.action.as_deref(), Some("fader gain"));
    }

    #[test]
    fn the_mode_pane_steps_and_scopes_the_action_edit() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("jog", Some(0), None).unwrap();
        let mut guide = at_slot(guide, 0, "jog");
        assert_eq!(guide.view().mode.as_ref().map(|(_, _, name)| name.as_str()), Some("vinyl"));
        guide.step_mode(1);
        assert_eq!(guide.view().mode.as_ref().map(|(_, _, name)| name.as_str()), Some("slip"));
        // The slip mode binds the two jog halves separately, so the slot itself has no action.
        assert_eq!(guide.action_text(), None);
        guide.move_cursor(1); // the press half
        assert_eq!(guide.action_text().as_deref(), Some("vinyl slip"));
        guide.move_cursor(1); // the turn half
        assert_eq!(guide.action_text().as_deref(), Some("vinyl"));
    }

    #[test]
    fn a_mode_key_is_learned_and_repointed() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("pad", Some(0), None).unwrap();
        guide.begin_mode_key(true, Some("beatloop".to_owned())).unwrap();
        guide.feed(&note(0, 40));
        guide.confirm_learn().unwrap();

        let control = guide.map.units[0]
            .controls
            .iter()
            .find(|control| {
                control.role == SlotRole::ModeTrigger && control.target.as_deref() == Some("beatloop")
            })
            .expect("learned mode key");
        let slot = control.slot.clone();
        assert_eq!(control.endpoint.as_ref().unwrap().as_slice()[0].id, Some(40));

        guide.select_slot(0, &slot);
        guide.set_mode_key(false, None).expect("repoint to cycle");
        let control = guide.map.units[0]
            .controls
            .iter()
            .find(|control| control.slot == slot)
            .expect("the repointed key");
        assert_eq!(control.role, SlotRole::ModeCycle);
        assert_eq!(control.target, None);
    }

    #[test]
    fn cloning_shifts_channels_and_rewrites_the_deck() {
        let text = r#"
[[unit]]
id = "deck1.mixer"
preset = "mixer"
deck = 1

[[unit.control]]
slot = "low"
endpoint = { midi = "cc", channel = 0, id = 20 }

[[unit.mode]]
name = "mine"
[[unit.mode.bind]]
slot = "low"
action = "fx param low"
fx = "eq"
chain = "deck1"
"#;
        let mut guide = Guide::from_toml_str(text, "map.toml", 4).unwrap();
        guide.select_unit(0);
        guide.clone_unit(1, 1).unwrap();
        let cloned = &guide.map.units[1];
        assert_eq!(cloned.id, "deck2.mixer");
        assert_eq!(cloned.deck, Some(1));
        assert_eq!(
            cloned.controls[0].endpoint.as_ref().unwrap().as_slice()[0].channel,
            1
        );
        let bind = cloned
            .modes
            .iter()
            .find(|mode| mode.name == "mine")
            .unwrap()
            .binds
            .iter()
            .find(|bind| bind.slot == "low")
            .unwrap();
        assert_eq!(bind.chain.as_deref(), Some("deck2"));
    }

    #[test]
    fn a_four_deck_map_shows_four_decks() {
        let mut guide = Guide::empty(4).unwrap();
        let labels: Vec<String> = guide.view().decks.into_iter().map(|row| row.label).collect();
        assert_eq!(labels, vec!["deck1", "deck2", "deck3", "deck4", "global"]);
        guide.new_unit("transport", Some(3), None).unwrap();
        assert_eq!(guide.map.units[0].id, "deck4.transport");
    }

    #[test]
    fn a_session_round_trips_through_disk() {
        let path = tmp_path("roundtrip");
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("transport", Some(0), None).unwrap();
        let mut guide = at_slot(guide, 0, "play");
        guide.begin_learn().unwrap();
        guide.feed(&note(0, 16));
        guide.confirm_learn().unwrap();
        guide.set_device(Some("Test Controller".to_owned()));
        guide.save_as(&path).unwrap();

        let reopened = Guide::new(Some(&path), 2).unwrap();
        assert_eq!(reopened.device().as_deref(), Some("Test Controller"));
        assert_eq!(reopened.map.units.len(), 1);
        assert_eq!(
            reopened.map.units[0].controls[0]
                .endpoint
                .as_ref()
                .unwrap()
                .as_slice()[0]
                .id,
            Some(16)
        );
        assert!(reopened.error.is_none(), "{:?}", reopened.error);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn every_built_in_preset_is_offered_with_its_scope() {
        let guide = Guide::empty(2).unwrap();
        let names = guide.preset_names();
        assert!(names.contains(&"pad".to_owned()));
        assert!(names.contains(&"crossfader".to_owned()));
        assert_eq!(guide.preset_scope("output"), PresetScope::Global);
        assert_eq!(guide.preset_scope("browser"), PresetScope::Both);
        assert_eq!(guide.preset_scope("mixer"), PresetScope::Deck);
    }

    #[test]
    fn the_shipped_maps_open_in_the_guide() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for name in ["profile/midi.toml", "profile/midi-map.toml"] {
            let path = root.join(name);
            let guide = Guide::new(Some(&path.display().to_string()), 2)
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            assert!(guide.error.is_none(), "{name}: {:?}", guide.error);
            assert!(!guide.view().decks.is_empty());
        }
    }
}
