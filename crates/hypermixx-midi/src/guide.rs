//! The map editor's model: three columns over the file, plus the state machine that learns an
//! endpoint.
//!
//! The columns are built from the **file**, not from the compiled map, so a half-finished map
//! still renders (and can be fixed) instead of disappearing behind a compile error. Compilation is
//! only used to report whether the current file is valid.

use std::path::{Path, PathBuf};

use crate::map::{
    Encoding, EndpointRole, EventKind, Map, MapFile, RawControl, RawEndpoint, RawModeBind, RawUnit,
    SlotKind, SlotRole,
};
use crate::msg::Event;
use crate::preset::{Preset, PresetRegistry, PresetScope};

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
    NewDeck,
}

/// Which kind of row a deck-column entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeckKind {
    Global,
    Deck(usize),
    New,
}

/// One deck-column row.
struct DeckEntry {
    row: Row,
    kind: DeckKind,
}

/// A unit row, with the id it points at (`None` for the trailing `add unit`).
struct UnitEntry {
    row: Row,
    unit: Option<usize>,
}

/// A slot row: one control, including mode keys.
struct SlotEntry {
    row: Row,
    slot: Option<usize>,
}

/// One rendered row of a column.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub label: String,
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
    /// Lines for the info pane.
    pub details: Vec<(String, String)>,
    /// Endpoints to highlight in the raw monitor.
    pub endpoints: Vec<(EventKind, u8, u8)>,
    pub capturing: bool,
    pub candidate: Option<Candidate>,
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
    /// Decks created from the `new` row that have no unit yet. They exist for this session only;
    /// the first unit added to one is what makes it appear in the file.
    extra_decks: std::collections::BTreeSet<usize>,
    nav: Nav,
    capture: Option<Capture>,
    dirty: bool,
    message: Option<String>,
}

impl Guide {
    /// An empty session that has not loaded anything.
    pub fn empty(decks: usize) -> Result<Self, String> {
        Self::from_map_file(
            MapFile {
                meta: None,
                units: Vec::new(),
            },
            PathBuf::from("."),
            None,
            decks,
        )
    }

    /// Opens a file, or starts empty when the path does not exist yet.
    pub fn new(path: Option<&str>, decks: usize) -> Result<Self, String> {
        match path {
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
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
            extra_decks: std::collections::BTreeSet::new(),
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

    fn with_map<T>(
        &mut self,
        edit: impl FnOnce(&mut MapFile) -> Result<T, String>,
    ) -> Result<T, String> {
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
        self.extra_decks.clear();
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
        let id = id
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| match deck {
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
        let cloned = copy_unit(&self.map.units[index], target_deck, channel_offset)?;
        if self.map.units.iter().any(|unit| unit.id == cloned.id) {
            return Err(format!(
                "unit `{}` already exists; delete it first or pick another deck",
                cloned.id
            ));
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

    /// The next deck number after every deck already shown or configured.
    pub fn next_deck(&self) -> usize {
        self.deck_entries()
            .iter()
            .filter_map(|entry| match entry.kind {
                DeckKind::Deck(deck) => Some(deck + 1),
                DeckKind::Global | DeckKind::New => None,
            })
            .max()
            .unwrap_or(0)
            .max(self.decks)
    }

    /// Selects a fresh empty deck. It becomes part of the file only when its first unit is added.
    pub fn start_deck(&mut self) -> usize {
        let target = self.next_deck();
        self.extra_decks.insert(target);
        self.select_deck(target);
        self.message = Some(format!("deck{} — add a unit to persist it", target + 1));
        target
    }

    /// Copies every unit of `source` to the next fresh deck, shifting channels by the deck distance.
    pub fn clone_deck(&mut self, source: usize) -> Result<usize, String> {
        let target = self.next_deck();
        if target <= source {
            return Err("there is no newer deck to clone into".to_owned());
        }
        let offset = target as i16 - source as i16;
        let indexes: Vec<usize> = self
            .map
            .units
            .iter()
            .enumerate()
            .filter(|(_, unit)| unit.deck.map(usize::from) == Some(source))
            .map(|(index, _)| index)
            .collect();
        if indexes.is_empty() {
            return Err(format!("deck{} has no units to clone", source + 1));
        }
        let mut clones = Vec::with_capacity(indexes.len());
        for index in indexes {
            let cloned = copy_unit(&self.map.units[index], target, offset)?;
            if self.map.units.iter().any(|unit| unit.id == cloned.id)
                || clones
                    .iter()
                    .any(|unit: &crate::map::RawUnit| unit.id == cloned.id)
            {
                return Err(format!(
                    "unit `{}` already exists on deck{}",
                    cloned.id,
                    target + 1
                ));
            }
            clones.push(cloned);
        }
        let count = clones.len();
        self.with_map(|map| {
            map.units.extend(clones);
            Ok(())
        })?;
        self.extra_decks.insert(target);
        self.select_deck(target);
        self.message = Some(format!(
            "cloned deck{} → deck{} ({count} unit{}, channels {offset:+})",
            source + 1,
            target + 1,
            if count == 1 { "" } else { "s" }
        ));
        Ok(target)
    }

    /// Instantiates a preset in the deck/global group the deck column points at.
    pub fn add_unit_selected(&mut self, preset: &str) -> Result<(), String> {
        let decks = self.deck_entries();
        let kind = decks
            .get(self.nav.deck)
            .map(|entry| entry.kind)
            .ok_or_else(|| "select a deck first".to_owned())?;
        match kind {
            DeckKind::Global => self.new_unit(preset, None, None),
            DeckKind::Deck(deck) => self.new_unit(preset, Some(deck), None),
            DeckKind::New => Err("choose a new deck first".to_owned()),
        }
    }

    /// The deck/global label the columns currently point at.
    pub fn selected_group_label(&self) -> String {
        self.deck_entries()
            .get(self.nav.deck)
            .map(|entry| entry.row.label.clone())
            .unwrap_or_else(|| "global".to_owned())
    }

    /// Deletes the selected unit, or unbinds the selected slot from the unit.
    pub fn delete_selected(&mut self) -> Result<(), String> {
        match self.nav.focus {
            ColumnFocus::Deck => Err("select a unit or slot to delete".to_owned()),
            ColumnFocus::Unit => {
                let unit = self
                    .selected_unit_key()
                    .ok_or_else(|| "select a unit to delete".to_owned())?;
                self.with_map(|map| {
                    map.units.remove(unit);
                    Ok(())
                })?;
                self.nav.unit = self.nav.unit.saturating_sub(1);
                self.clamp_nav();
                self.message = Some("unit deleted".to_owned());
                Ok(())
            }
            ColumnFocus::Slot => {
                let (unit, control) = self
                    .current_slot_key()
                    .ok_or_else(|| "select a slot to delete".to_owned())?;
                let slot = self
                    .unit_slots(unit)
                    .into_iter()
                    .nth(control)
                    .ok_or_else(|| "the slot disappeared".to_owned())?;
                let name = slot.name.clone();
                if !self.map.units[unit]
                    .controls
                    .iter()
                    .any(|control| control.slot == name)
                {
                    return Err(format!("`{name}` comes from the preset; nothing to delete"));
                }
                self.with_map(|map| {
                    let unit = &mut map.units[unit];
                    unit.controls.retain(|control| control.slot != name);
                    for mode in &mut unit.modes {
                        mode.binds.retain(|bind| bind.slot != name);
                    }
                    Ok(())
                })?;
                self.clamp_nav();
                self.message = Some(format!("unbound `{name}`"));
                Ok(())
            }
        }
    }

    /// Built-in presets for the group the unit column points at.
    pub fn add_unit_presets(&self) -> Vec<String> {
        let decks = self.deck_entries();
        match decks.get(self.nav.deck).map(|entry| entry.kind) {
            Some(DeckKind::Global) => self.preset_names_for(None),
            Some(DeckKind::Deck(deck)) => self.preset_names_for(Some(deck)),
            Some(DeckKind::New) | None => Vec::new(),
        }
    }

    /// Built-in presets that can be instantiated in `deck` (`None` = global).
    pub fn preset_names_for(&self, deck: Option<usize>) -> Vec<String> {
        let mut names: Vec<String> = self
            .presets
            .builtin_names()
            .filter(|name| {
                let scope = self.presets.scope(name).unwrap_or_default();
                if deck.is_some() {
                    scope.takes_deck()
                } else {
                    scope.takes_global()
                }
            })
            .map(str::to_owned)
            .collect();
        names.sort();
        names
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
                let control = raw
                    .controls
                    .iter()
                    .find(|control| control.slot == slot.slot);
                slots.push(SlotInfo {
                    name: slot.slot.clone(),
                    kind: control
                        .and_then(|control| control.kind)
                        .unwrap_or(slot.kind),
                    role: control.map(|control| control.role).unwrap_or(slot.role),
                    target: control
                        .and_then(|control| control.target.clone())
                        .or_else(|| slot.target.clone()),
                    endpoints: control.map(raw_endpoints).unwrap_or_default(),
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
            });
        }
        slots
    }

    #[cfg(test)]
    fn unit_mode_names_for_test(&self, unit: usize) -> Vec<String> {
        self.unit_modes(unit)
            .into_iter()
            .map(|(name, _, _)| name)
            .collect()
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
        let name = raw.start.clone().or_else(|| {
            self.preset_of_unit(unit)
                .and_then(|preset| preset.start.clone())
        })?;
        self.unit_modes(unit)
            .iter()
            .position(|(mode, _, _)| *mode == name)
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

    fn bind_action(&self, unit: usize, mode: &str, slot: &str) -> Option<String> {
        self.raw_bind(unit, mode, slot)?.action
    }

    // ---- columns ---------------------------------------------------------

    /// Deck-column rows: `global`, every deck, then the trailing `new` row.
    fn deck_entries(&self) -> Vec<DeckEntry> {
        let mut decks: Vec<usize> = (0..self.decks).collect();
        for unit in &self.map.units {
            if let Some(deck) = unit.deck {
                decks.push(usize::from(deck));
            }
        }
        decks.extend(self.extra_decks.iter().copied());
        decks.sort_unstable();
        decks.dedup();

        let mut rows = vec![DeckEntry {
            row: Row {
                label: "global".to_owned(),
                muted: false,
            },
            kind: DeckKind::Global,
        }];
        for deck in decks {
            rows.push(DeckEntry {
                row: Row {
                    label: deck_label(Some(deck)),
                    muted: false,
                },
                kind: DeckKind::Deck(deck),
            });
        }
        rows.push(DeckEntry {
            row: Row {
                label: "new".to_owned(),
                muted: false,
            },
            kind: DeckKind::New,
        });
        rows
    }

    /// The units of the focused deck group, with the trailing `add unit` row. The `new` row has no
    /// units: choosing it opens the new-deck menu instead.
    fn unit_entries(&self) -> Vec<UnitEntry> {
        let decks = self.deck_entries();
        let selected = decks.get(self.nav.deck.min(decks.len().saturating_sub(1)));
        let deck = match selected.map(|entry| entry.kind) {
            Some(DeckKind::Deck(deck)) => Some(deck),
            Some(DeckKind::Global) => None,
            Some(DeckKind::New) | None => return Vec::new(),
        };
        let mut rows: Vec<UnitEntry> = self
            .map
            .units
            .iter()
            .enumerate()
            .filter(|(_, unit)| unit.deck.map(usize::from) == deck)
            .map(|(index, unit)| UnitEntry {
                row: Row {
                    label: unit.id.clone(),
                    muted: false,
                },
                unit: Some(index),
            })
            .collect();
        rows.push(UnitEntry {
            row: Row {
                label: "add unit".to_owned(),
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
            rows.push(SlotEntry {
                row: Row {
                    label: slot.name.clone(),
                    muted: false,
                },
                slot: Some(index),
            });
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
    }

    fn current_slot(&self) -> Option<SlotInfo> {
        let unit = self.selected_unit_key()?;
        let slot = self.slot_entries().get(self.nav.slot)?.slot?;
        self.unit_slots(unit).into_iter().nth(slot)
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
            }
            ColumnFocus::Unit => {
                self.nav.unit = next;
                self.nav.slot = 0;
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

    /// `Enter`: drill into the focused column, open the `new`/`add unit` menu, or learn the
    /// focused slot.
    pub fn enter(&mut self) -> Result<Entered, String> {
        match self.nav.focus {
            ColumnFocus::Deck => {
                let decks = self.deck_entries();
                match decks.get(self.nav.deck).map(|entry| entry.kind) {
                    Some(DeckKind::New) => Ok(Entered::NewDeck),
                    Some(_) => {
                        self.nav.focus = ColumnFocus::Unit;
                        Ok(Entered::Moved)
                    }
                    None => Ok(Entered::Moved),
                }
            }
            ColumnFocus::Unit => {
                let decks = self.deck_entries();
                if decks.get(self.nav.deck).map(|entry| entry.kind) == Some(DeckKind::New) {
                    return Ok(Entered::NewDeck);
                }
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

    /// Whether the slot column points at a real slot.
    pub fn has_slot(&self) -> bool {
        self.current_slot_key().is_some()
    }

    /// Points the deck column at one deck index.
    pub fn select_deck(&mut self, deck: usize) {
        if let Some(index) = self
            .deck_entries()
            .iter()
            .position(|entry| entry.kind == DeckKind::Deck(deck))
        {
            self.nav.deck = index;
            self.nav.unit = 0;
            self.nav.slot = 0;
            self.nav.focus = ColumnFocus::Unit;
            self.clamp_nav();
        }
    }

    /// Points every column at one unit.
    pub fn select_unit(&mut self, unit: usize) {
        let decks = self.deck_entries();
        let deck = self
            .map
            .units
            .get(unit)
            .and_then(|unit| unit.deck.map(usize::from));
        let wanted = deck.map_or(DeckKind::Global, DeckKind::Deck);
        if let Some(index) = decks.iter().position(|entry| entry.kind == wanted) {
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
            .position(|entry| entry.slot == Some(index))
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
            decks: self
                .deck_entries()
                .into_iter()
                .map(|entry| entry.row)
                .collect(),
            units: self
                .unit_entries()
                .into_iter()
                .map(|entry| entry.row)
                .collect(),
            slots: self
                .slot_entries()
                .into_iter()
                .map(|entry| entry.row)
                .collect(),
            nav: self.nav,
            details: self.details(),
            endpoints: self.endpoints(),
            capturing: self.capture.is_some(),
            candidate: self.candidate().copied(),
        }
    }

    fn details(&self) -> Vec<(String, String)> {
        let mut details = Vec::new();
        let decks = self.deck_entries();
        let Some(entry) = decks.get(self.nav.deck) else {
            return details;
        };
        if entry.kind == DeckKind::New {
            details.push((
                "new deck".to_owned(),
                "clone a deck or start a new one".to_owned(),
            ));
            return details;
        }
        details.push(("deck".to_owned(), entry.row.label.clone()));
        if let Some(unit) = self.selected_unit_key() {
            let raw = &self.map.units[unit];
            details.push((
                "unit".to_owned(),
                format!(
                    "{}{}",
                    raw.id,
                    raw.preset
                        .as_deref()
                        .map_or_else(String::new, |preset| format!(" · preset {preset}"))
                ),
            ));
            let modes = self.unit_modes(unit);
            let names = modes
                .iter()
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            details.push((
                "modes".to_owned(),
                if modes.is_empty() {
                    "0".to_owned()
                } else {
                    format!("{} · {names}", modes.len())
                },
            ));
            if let Some(start) = self.unit_start(unit) {
                if let Some((name, _, _)) = modes.get(start) {
                    details.push(("start".to_owned(), name.clone()));
                }
            }
            let slots = self.unit_slots(unit);
            let bound = slots
                .iter()
                .filter(|slot| !slot.endpoints.is_empty())
                .count();
            details.push((
                "slots".to_owned(),
                format!("{} · {} bound", slots.len(), bound),
            ));
        } else {
            let units = self
                .map
                .units
                .iter()
                .filter(|unit| {
                    unit.deck.map(usize::from)
                        == match entry.kind {
                            DeckKind::Deck(deck) => Some(deck),
                            DeckKind::Global => None,
                            DeckKind::New => None,
                        }
                })
                .count();
            details.push(("units".to_owned(), units.to_string()));
        }
        if let Some(slot) = self.current_slot() {
            details.push((
                "slot".to_owned(),
                match slot.role {
                    SlotRole::Action => format!("{:?} · {}", slot.kind, slot.name),
                    _ => format!("{} · {}", mode_key_hint(&slot), slot.name),
                },
            ));
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
                if let Some(unit) = self.selected_unit_key() {
                    for (mode, _, _) in self.unit_modes(unit) {
                        if slot.kind == SlotKind::JogWheel {
                            let Some(bind) = self.raw_bind(unit, &mode, &slot.name) else {
                                details.push((format!("action {mode}"), "—".to_owned()));
                                continue;
                            };
                            let press = bind
                                .press
                                .or_else(|| bind.action.clone())
                                .unwrap_or_else(|| "—".to_owned());
                            let turn = bind.turn.or(bind.action).unwrap_or_else(|| "—".to_owned());
                            details.push((format!("action {mode} press"), press));
                            details.push((format!("action {mode} turn"), turn));
                        } else {
                            let action = self
                                .bind_action(unit, &mode, &slot.name)
                                .unwrap_or_else(|| "—".to_owned());
                            details.push((format!("action {mode}"), action));
                        }
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
        slot.endpoints
            .iter()
            .flat_map(|(_, endpoint)| endpoint_keys(endpoint))
            .collect()
    }

    // ---- learn -----------------------------------------------------------

    pub fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }

    pub fn candidate(&self) -> Option<&Candidate> {
        self.capture
            .as_ref()
            .and_then(|capture| capture.candidate.as_ref())
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
                match unit
                    .controls
                    .iter_mut()
                    .find(|control| control.slot == slot_name)
                {
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
            match unit
                .controls
                .iter_mut()
                .find(|control| control.slot == slot_name)
            {
                Some(existing) => *existing = raw,
                None => unit.controls.push(raw),
            }
            Ok(())
        })?;
        self.select_slot(unit, &slot_name);
        Ok(())
    }
}

/// Copies one unit to `target_deck`, shifting every endpoint channel by `channel_offset`.
fn copy_unit(
    unit: &crate::map::RawUnit,
    target_deck: usize,
    channel_offset: i16,
) -> Result<crate::map::RawUnit, String> {
    let Some(old_deck) = unit.deck else {
        return Err("a global unit has no deck to clone to".to_owned());
    };
    let target = target_deck as u8;
    if old_deck == target {
        return Err(format!(
            "`{}` is already on deck {}",
            unit.id,
            target_deck + 1
        ));
    }
    let mut cloned = unit.clone();
    cloned.id = clone_id(&unit.id, old_deck, target);
    cloned.deck = Some(target);
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
    Ok(cloned)
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
        let dir =
            std::env::temp_dir().join(format!("hypermixx-guide-{}-{}", std::process::id(), name));
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
        assert_eq!(labels, vec!["global", "deck1", "deck2", "new"]);
        assert_eq!(view.units.len(), 1, "only the add row");
        assert!(view.units[0].muted);
        assert!(view.slots.is_empty());
    }

    #[test]
    fn a_deck_preset_needs_a_deck_and_a_global_one_refuses_it() {
        let mut guide = Guide::empty(2).unwrap();
        assert!(guide.new_unit("mixer", None, None).is_err());
        assert!(guide.new_unit("outputfader", Some(0), None).is_err());
        guide.new_unit("outputfader", None, None).unwrap();
        assert_eq!(guide.map.units[0].deck, None);
        assert_eq!(guide.map.units[0].id, "global.outputfader");
    }

    #[test]
    fn a_global_only_preset_refuses_a_deck() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("browser", None, None).unwrap();
        assert_eq!(guide.map.units[0].id, "global.browser");
        assert!(guide.new_unit("browser", Some(0), None).is_err());
    }

    #[test]
    fn the_slot_column_lists_the_preset_slots_without_synthetic_halves() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("jog", Some(0), None).unwrap();
        let labels: Vec<String> = guide
            .view()
            .slots
            .into_iter()
            .map(|row| row.label)
            .collect();
        assert_eq!(labels, vec!["jog"]);
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
        assert_eq!(
            guide.feed_at(&cc(0, 9, 64), Some(1_000)),
            GuideStep::Captured
        );
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
        guide.new_unit("browser", None, None).unwrap();
        let mut guide = at_slot(guide, 0, "choose");
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
        let labels: Vec<String> = guide
            .view()
            .decks
            .into_iter()
            .map(|row| row.label)
            .collect();
        assert_eq!(
            labels,
            vec!["global", "deck1", "deck2", "deck3", "deck4", "new"]
        );
        guide.new_unit("transport", Some(3), None).unwrap();
        assert_eq!(guide.map.units[0].id, "deck4.transport");
    }

    #[test]
    fn the_pad_preset_has_the_four_documented_modes_and_stem_pads() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("pad", Some(0), None).unwrap();
        let modes = guide.unit_mode_names_for_test(0);
        assert_eq!(modes, vec!["hotcue", "loop", "beatjump", "stems"]);
        for slot in ["pad1", "pad2", "pad3", "pad4"] {
            assert!(
                guide
                    .raw_bind(0, "stems", slot)
                    .and_then(|bind| bind.action)
                    .is_some(),
                "{slot} should toggle a stem"
            );
        }
        for slot in ["pad5", "pad6", "pad7", "pad8"] {
            assert!(
                guide
                    .raw_bind(0, "stems", slot)
                    .and_then(|bind| bind.action)
                    .is_none(),
                "{slot} should be empty in stems mode"
            );
        }
    }

    #[test]
    fn unbound_manual_loop_and_jump_presets_compile() {
        let mut guide = Guide::empty(1).unwrap();
        guide.new_unit("manualloop", Some(0), None).unwrap();
        guide.new_unit("manualjump", Some(0), None).unwrap();
        assert!(guide.error.is_none(), "{:?}", guide.error);
        assert_eq!(guide.map.units.len(), 2);
    }

    #[test]
    fn the_deckfx_presets_compile_before_the_mixer_exposes_fx_names() {
        let mut guide = Guide::empty(2).unwrap();
        for preset in ["deckfx1", "deckfx2", "deckfx3"] {
            guide.new_unit(preset, Some(0), None).unwrap();
        }
        assert!(guide.error.is_none(), "{:?}", guide.error);
        assert_eq!(guide.map.units.len(), 3);
    }

    #[test]
    fn delete_removes_a_unit_or_unbinds_a_slot() {
        let mut guide = Guide::empty(1).unwrap();
        guide.new_unit("transport", Some(0), None).unwrap();
        guide.select_slot(0, "play");
        guide.begin_learn().unwrap();
        guide.feed(&note(0, 16));
        guide.confirm_learn().unwrap();
        guide.delete_selected().unwrap();
        assert!(
            guide.map.units[0]
                .controls
                .iter()
                .all(|control| control.slot != "play"),
            "the slot control should be gone"
        );
        assert!(guide.error.is_none(), "{:?}", guide.error);

        guide.select_unit(0);
        guide.delete_selected().unwrap();
        assert!(guide.map.units.is_empty());
    }

    #[test]
    fn a_new_deck_starts_empty_and_gets_a_unit_from_a_preset() {
        let mut guide = Guide::empty(2).unwrap();
        guide.move_column(-1);
        // Walk to the trailing `new` row.
        for _ in 0..guide.view().decks.len() {
            if guide.selected_group_label() == "new" {
                break;
            }
            guide.move_cursor(1);
        }
        assert_eq!(guide.selected_group_label(), "new");
        guide.move_cursor(-1);
        let target = guide.start_deck();
        assert_eq!(target, 2);
        assert_eq!(guide.selected_group_label(), "deck3");
        guide.add_unit_selected("transport").unwrap();
        assert_eq!(guide.map.units[0].id, "deck3.transport");
    }

    #[test]
    fn cloning_a_deck_copies_every_unit_and_shifts_the_deck_distance() {
        let mut guide = Guide::empty(2).unwrap();
        guide.new_unit("transport", Some(0), None).unwrap();
        guide.new_unit("jog", Some(0), None).unwrap();
        guide.select_slot(1, "jog");
        guide.begin_learn().unwrap();
        guide.feed(&note(0, 80));
        guide.confirm_learn().unwrap();
        guide.feed(&cc(0, 6, 1));
        guide.feed(&cc(0, 6, 127));
        guide.confirm_learn().unwrap();
        let target = guide.clone_deck(0).unwrap();
        assert_eq!(target, 2);
        assert_eq!(
            guide
                .map
                .units
                .iter()
                .filter(|unit| unit.deck == Some(2))
                .count(),
            2
        );
        let jog = guide
            .map
            .units
            .iter()
            .find(|unit| unit.deck == Some(2) && unit.preset.as_deref() == Some("jog"))
            .expect("cloned jog");
        assert_eq!(jog.id, "deck3.jog");
        let turn = jog
            .controls
            .iter()
            .find(|control| control.slot == "jog")
            .and_then(|control| control.endpoint.as_ref())
            .and_then(|endpoints| {
                endpoints
                    .as_slice()
                    .iter()
                    .find(|e| e.role == EndpointRole::Turn)
            })
            .expect("turn endpoint");
        assert_eq!(turn.channel, 2);
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
        assert_eq!(guide.preset_scope("outputfader"), PresetScope::Global);
        assert_eq!(guide.preset_scope("browser"), PresetScope::Global);
        assert_eq!(guide.preset_scope("mixer"), PresetScope::Deck);
    }

    #[test]
    fn the_shipped_maps_open_in_the_guide() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut checked = 0;
        for name in ["profile/mapping.toml"] {
            let path = root.join(name);
            if !path.exists() {
                continue;
            }
            let guide = Guide::new(Some(&path.display().to_string()), 2)
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            assert!(guide.error.is_none(), "{name}: {:?}", guide.error);
            assert!(!guide.view().decks.is_empty());
            checked += 1;
        }
        assert!(checked > 0, "no map to open in the repository");
    }
}
