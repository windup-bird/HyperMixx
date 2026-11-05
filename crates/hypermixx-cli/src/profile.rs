//! The one configuration file: where it lives, how it is created, and how its sections are read.
//!
//! A config is a single TOML file with `[mixer]`, `[live]`, `[preference]` and `[controller]`
//! sections. `--config <file>` reads exactly that file; without it the platform default is used, and
//! on the first start the shipped template is copied there. An existing file is never overwritten,
//! and a read-only config directory does not stop the engine: the built-in template runs instead.
//!
//! All file IO stays in this front-end crate — the audio engine only ever reads text. In particular
//! the template is the single source of truth for `--print-config`, the first-start copy and the
//! engine's built-in topology (`reference_toml()` embeds the same bytes).
//!
//! `[controller].map` is a **file pointer** to a unit map, resolved relative to this config's
//! folder. Leaving it out means no controller input, which is the shipped state.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use hypermixx_audio::{reference_toml, MixerConfig};
use hypermixx_core::LiveConfig;

/// The shipped template. `--print-config` emits it and the first-start copy writes it verbatim.
pub fn template() -> &'static str {
    reference_toml()
}

/// The shipped unit-map template (`profile/mapping.toml`), embedded so `--guide` has a map to edit
/// on a machine with no checkout. It is written only when the guide opens a map that does not exist
/// yet, never at startup — the first start still creates exactly one file, the config.
pub const MAP_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../profile/mapping.toml"
));

/// The `[controller]` section: a pointer to one unit-map file, plus an optional port override.
/// Parsed by hand because this crate does not depend on serde; two keys do not warrant one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ControllerSection {
    map: Option<String>,
    device: Option<String>,
}

/// What the session needs from `[controller]`, with `map` already resolved to a usable path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Controller {
    pub map: Option<String>,
    pub device: Option<String>,
}

/// One parsed config, and the path it came from.
#[derive(Debug)]
pub struct Config {
    pub path: PathBuf,
    pub mixer: MixerConfig,
    pub live: LiveConfig,
    pub controller: Controller,
    /// Notes worth showing at startup (a first-start copy, clamped live values, ...).
    pub notes: Vec<String>,
}

/// The platform's config path. `HYPERMIXX_CONFIG` wins when set, which is also how a test or a
/// Termux/proot session names its own location.
pub fn default_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HYPERMIXX_CONFIG") {
        return Some(PathBuf::from(path));
    }
    let dir = if cfg!(windows) {
        std::env::var_os("APPDATA").map(|base| PathBuf::from(base).join("Hypermixx"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
            })
            .map(|base| base.join("hypermixx"))
    }?;
    Some(dir.join("config.toml"))
}

/// Loads `explicit`, or the platform default. Only the *default* path is created on a first start:
/// a path the user named must exist, so a typo is an error rather than a silently copied template.
pub fn load_or_create(explicit: Option<&str>) -> Result<Config, String> {
    let path = match explicit {
        Some(path) => PathBuf::from(path),
        None => default_path().ok_or_else(|| {
            "cannot find the platform config directory (set HYPERMIXX_CONFIG)".to_owned()
        })?,
    };
    let mut notes = Vec::new();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound && explicit.is_none() => {
            match first_copy(&path) {
                Ok(()) => notes.push(format!("created {}", path.display())),
                Err(err) => notes.push(format!(
                    "{}: {err}; running on the built-in template",
                    path.display()
                )),
            }
            template().to_owned()
        }
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    parse(&path, &text, notes)
}

/// Writes the template once. The caller has already established the file is absent.
fn first_copy(path: &Path) -> Result<(), String> {
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    std::fs::write(path, template()).map_err(|err| err.to_string())
}

/// Splits the sections. Each keeps its own schema and validation: the mixer's own parser reads
/// `[mixer]` (or the whole text for a standalone topology), `LiveConfig` deserializes `[live]`,
/// and preferences are read from `[preference]` by `preference::parse`.
fn parse(path: &Path, text: &str, mut notes: Vec<String>) -> Result<Config, String> {
    let value: toml::Value =
        toml::from_str(text).map_err(|err| format!("{}: {err}", path.display()))?;
    let mixer = MixerConfig::from_toml_str(text)
        .map_err(|err| format!("{}: {}", path.display(), err.message()))?;

    let live = match value.get("live") {
        Some(section) => {
            let parsed: LiveConfig = section
                .clone()
                .try_into()
                .map_err(|err| format!("{}: [live]: {err}", path.display()))?;
            let live = parsed.clone().sanitised();
            if live != parsed {
                notes.push(format!("{}: some live values were clamped", path.display()));
            }
            live
        }
        None => LiveConfig::default(),
    };

    let section = match value.get("controller") {
        Some(section) => controller_section(section)
            .map_err(|err| format!("{}: [controller]: {err}", path.display()))?,
        None => ControllerSection::default(),
    };
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let controller = Controller {
        map: section.map.map(|map| resolve_against(dir, &map)),
        device: section.device,
    };

    Ok(Config {
        path: path.to_path_buf(),
        mixer,
        live,
        controller,
        notes,
    })
}

/// The conventional name of a unit-map file, used by the TUI's pickers when no config path is at
/// hand. A real map path always comes from `[controller].map` or `map_path_for`.
pub fn default_map_path() -> &'static str {
    "mapping.toml"
}

/// The unit-map file next to one specific config: the guide's default target and the path a session
/// shows. `[controller].map` overrides it, but a config that names no map simply opens no input.
pub fn map_path_for(config_path: &Path) -> String {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("mapping.toml")
        .to_string_lossy()
        .into_owned()
}

/// The `[[mixer.output]]` array of a one-file config. The TUI's device pickers read it; a missing
/// or malformed section means "no outputs declared" rather than an error.
pub fn output_tables(doc: &toml_edit::DocumentMut) -> Option<&toml_edit::ArrayOfTables> {
    doc.get("mixer")?
        .as_table()?
        .get("output")?
        .as_array_of_tables()
}

/// The same array, mutable, creating `[mixer]` and the array when the config omits them. Used by
/// the settings screens, which write a device back into the config the session is running on.
pub fn output_tables_mut(
    doc: &mut toml_edit::DocumentMut,
) -> Result<&mut toml_edit::ArrayOfTables, String> {
    if doc.get("mixer").is_none() {
        doc.insert("mixer", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let mixer = doc
        .get_mut("mixer")
        .and_then(|item| item.as_table_mut())
        .ok_or_else(|| "`mixer` is not a table".to_owned())?;
    if mixer.get("output").is_none() {
        mixer.insert(
            "output",
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }
    mixer
        .get_mut("output")
        .and_then(|item| item.as_array_of_tables_mut())
        .ok_or_else(|| "`[[mixer.output]]` is not an array of tables".to_owned())
}

fn controller_section(value: &toml::Value) -> Result<ControllerSection, String> {
    let table = value
        .as_table()
        .ok_or_else(|| "must be a table".to_owned())?;
    let mut section = ControllerSection::default();
    for (key, value) in table {
        let text = value
            .as_str()
            .ok_or_else(|| format!("`{key}` must be a string"))?
            .trim()
            .to_owned();
        match key.as_str() {
            "map" => section.map = Some(text),
            "device" => section.device = Some(text),
            other => return Err(format!("unknown key `{other}` (map | device)")),
        }
    }
    Ok(section)
}

/// A path written inside the config: absolute stays absolute, relative joins the config's folder.
fn resolve_against(dir: &Path, path: &str) -> String {
    let path = Path::new(path.trim());
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    dir.join(path).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hypermixx-config-{}-{name}", std::process::id()))
    }

    #[test]
    fn the_template_parses_into_a_usable_config() {
        let cfg = parse(Path::new("config.toml"), template(), Vec::new()).expect("parses");
        assert_eq!(cfg.mixer.channels.len(), 2, "the reference topology has two decks");
        assert_eq!(cfg.mixer.outputs.len(), 2);
        assert!(!cfg.live.autogain.enabled, "auto-gain is off in the template");
        assert_eq!(cfg.controller, Controller::default());
    }

    #[test]
    fn sections_are_read_independently_and_map_is_a_resolved_pointer() {
        let dir = temp("sections");
        let path = dir.join("config.toml");
        let text = r#"
[mixer]
[[mixer.channel]]
side = "center"

[live]
[live.autogain]
enabled = true
target_db = -9.0

[preference]
[preference.waveform]
backend = "braille"

[controller]
map = "mapping.toml"
device = "DDJ-400"
"#;
        let cfg = parse(&path, text, Vec::new()).expect("parses");
        assert_eq!(cfg.mixer.channels.len(), 1);
        assert!(cfg.live.autogain.enabled);
        assert_eq!(cfg.live.autogain.target_db, -9.0);
        assert_eq!(
            cfg.controller.map,
            Some(dir.join("mapping.toml").to_string_lossy().into_owned())
        );
        assert_eq!(cfg.controller.device.as_deref(), Some("DDJ-400"));
    }

    #[test]
    fn an_existing_file_is_read_not_overwritten() {
        let path = temp("existing.toml");
        std::fs::write(&path, "[mixer]\n[[mixer.channel]]\nside = \"center\"\n").unwrap();
        let cfg = load_or_create(Some(path.to_string_lossy().as_ref())).expect("reads");
        assert_eq!(cfg.mixer.channels.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[mixer]\n[[mixer.channel]]\nside = \"center\"\n",
            "loading must not rewrite the user's file"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_explicit_config_is_an_error() {
        let path = temp("absent.toml");
        std::fs::remove_file(&path).ok();
        assert!(load_or_create(Some(path.to_string_lossy().as_ref())).is_err());
    }

    #[test]
    fn first_copy_creates_the_parent_directories() {
        let dir = temp("copy");
        let path = dir.join("nested/config.toml");
        std::fs::remove_dir_all(&dir).ok();
        first_copy(&path).expect("copies");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), template());
        std::fs::remove_dir_all(&dir).ok();
    }
}
