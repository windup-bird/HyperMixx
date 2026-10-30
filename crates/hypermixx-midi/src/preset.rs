//! Presets: named slot layouts and default mode tables that a `[[unit]]` can start from.
//!
//! A preset is **data, not a Rust enum**. Built-ins are TOML compiled into the binary; user files
//! are named by `[meta] presets = [...]` and may replace a built-in with the same name. The unit
//! still owns the endpoints, so the same `touchpad` preset works on any controller: the map binds
//! the note numbers.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::map::{Meta, RawMode, SlotKind, SlotRole};

const BUILTINS: &[(&str, &str)] = &[
    ("browser", include_str!("../presets/browser.toml")),
    ("crossfader", include_str!("../presets/crossfader.toml")),
    ("deckfx1", include_str!("../presets/deckfx1.toml")),
    ("deckfx2", include_str!("../presets/deckfx2.toml")),
    ("deckfx3", include_str!("../presets/deckfx3.toml")),
    ("jog", include_str!("../presets/jog.toml")),
    ("manualjump", include_str!("../presets/manualjump.toml")),
    ("manualloop", include_str!("../presets/manualloop.toml")),
    ("mixer", include_str!("../presets/mixer.toml")),
    ("outputfader", include_str!("../presets/outputfader.toml")),
    ("pad", include_str!("../presets/pad.toml")),
    ("tempofader", include_str!("../presets/tempofader.toml")),
    ("transport", include_str!("../presets/transport.toml")),
];

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PresetFile {
    #[serde(default, rename = "preset")]
    pub presets: Vec<Preset>,
}

/// Which groups a preset can be instantiated in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PresetScope {
    /// A deck unit (transport, mixer, jog, …).
    #[default]
    Deck,
    /// A global unit that is not tied to a deck (output, crossfader).
    Global,
    /// Either: `browser` exists both globally and per deck.
    Both,
}

impl PresetScope {
    /// Whether the preset may be owned by a deck.
    pub fn takes_deck(self) -> bool {
        matches!(self, PresetScope::Deck | PresetScope::Both)
    }

    /// Whether the preset may stand on its own.
    pub fn takes_global(self) -> bool {
        matches!(self, PresetScope::Global | PresetScope::Both)
    }
}

/// One named preset. Modes are the same shape a unit uses, so a unit can replace one by name.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub name: String,
    #[serde(default)]
    pub scope: PresetScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(default, rename = "slot")]
    pub slots: Vec<PresetSlot>,
    #[serde(default, rename = "mode")]
    pub modes: Vec<RawMode>,
}

/// One preset slot: its physical kind, plus the role and target when it is a mode slot.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PresetSlot {
    pub slot: String,
    #[serde(rename = "type")]
    pub kind: SlotKind,
    #[serde(default, skip_serializing_if = "is_action_role")]
    pub role: SlotRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

fn is_action_role(role: &SlotRole) -> bool {
    *role == SlotRole::Action
}

/// The presets visible to one map. Built-ins are loaded first; a user file with the same preset
/// name replaces the built-in wholesale.
#[derive(Clone, Debug, Default)]
pub struct PresetRegistry {
    presets: HashMap<String, Preset>,
    builtins: std::collections::HashSet<String>,
}

fn check_preset_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        return Err(format!(
            "preset name `{name}` must be non-empty and contain no whitespace"
        ));
    }
    Ok(())
}

impl PresetRegistry {
    pub fn load(meta: Option<&Meta>, base: &Path) -> Result<Self, String> {
        let mut presets = HashMap::new();
        let mut builtins = std::collections::HashSet::new();
        for (source, text) in BUILTINS {
            let file: PresetFile =
                toml::from_str(text).map_err(|err| format!("built-in preset `{source}`: {err}"))?;
            for preset in file.presets {
                check_preset_name(&preset.name)?;
                builtins.insert(preset.name.clone());
                presets.insert(preset.name.clone(), preset);
            }
        }
        if let Some(meta) = meta {
            for path in &meta.presets {
                let full = if Path::new(path).is_absolute() {
                    Path::new(path).to_path_buf()
                } else {
                    base.join(path)
                };
                let text = std::fs::read_to_string(&full)
                    .map_err(|err| format!("{}: {err}", full.display()))?;
                let file: PresetFile =
                    toml::from_str(&text).map_err(|err| format!("{}: {err}", full.display()))?;
                for preset in file.presets {
                    check_preset_name(&preset.name)?;
                    presets.insert(preset.name.clone(), preset);
                }
            }
        }
        Ok(Self { presets, builtins })
    }

    pub fn get(&self, name: &str) -> Option<&Preset> {
        self.presets.get(name)
    }

    /// The groups a preset may be instantiated in.
    pub fn scope(&self, name: &str) -> Option<PresetScope> {
        self.presets.get(name).map(|preset| preset.scope)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.presets.keys().map(String::as_str)
    }

    /// Built-in preset names only; the add-unit menu deliberately hides user-defined presets.
    pub fn builtin_names(&self) -> impl Iterator<Item = &str> {
        self.builtins.iter().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn builtins_parse() {
        let registry = PresetRegistry::load(None, Path::new(".")).expect("built-ins");
        for name in [
            "browser",
            "crossfader",
            "deckfx1",
            "deckfx2",
            "deckfx3",
            "jog",
            "manualjump",
            "manualloop",
            "mixer",
            "outputfader",
            "pad",
            "tempofader",
            "transport",
        ] {
            assert!(registry.get(name).is_some(), "missing built-in `{name}`");
        }
    }

    #[test]
    fn a_user_file_replaces_a_builtin_with_the_same_name() {
        let dir =
            std::env::temp_dir().join(format!("hypermixx-preset-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path: PathBuf = dir.join("my-presets.toml");
        std::fs::write(
            &path,
            r#"
[[preset]]
name = "touchpad"
start = "only"
[[preset.slot]]
slot = "mine"
type = "Button"
[[preset.mode]]
name = "only"
[[preset.mode.bind]]
slot = "mine"
action = "play"
deck = 1
"#,
        )
        .unwrap();
        let meta = Meta {
            name: None,
            device: None,
            description: None,
            presets: vec!["my-presets.toml".to_owned()],
        };
        let registry = PresetRegistry::load(Some(&meta), &dir).expect("user preset");
        let preset = registry.get("touchpad").expect("overridden touchpad");
        assert_eq!(preset.start.as_deref(), Some("only"));
        assert_eq!(preset.slots.len(), 1);
        assert_eq!(preset.slots[0].slot, "mine");
        std::fs::remove_dir_all(&dir).ok();
    }
}
