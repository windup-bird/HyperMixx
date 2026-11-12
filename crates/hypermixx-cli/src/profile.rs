//! The one configuration file: where it lives, how it is created, and how its sections are read.
//!
//! A config is a single TOML file with `[mixer]`, `[live]`, `[preference]` and `[controller]`
//! sections. `--config <file>` reads exactly that file; without it the platform default is used, and
//! on the first start the shipped template is copied there. An existing file is never overwritten,
//! and a read-only config directory does not stop the engine: the built-in template runs instead.
//!
//! All file IO stays in this front-end crate — the audio engine only ever reads text. In particular
//! the template is the single source of truth for the first-start copy and the
//! engine's built-in topology (`reference_toml()` embeds the same bytes).
//!
//! `[controller].map` is a **file pointer** to a unit map, resolved relative to this config's
//! folder. Leaving it out means no controller input, which is the shipped state.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use hypermixx_audio::{reference_toml, MixerConfig};
use hypermixx_core::{Backend, LiveConfig};
use hypermixx_midi::map::DeckRoutes;

/// The shipped template. The first-start copy writes it verbatim.
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
    id: Option<String>,
    map: Option<String>,
    device: Option<String>,
}

/// One controller instance. `id` is the label a `[[multideck.deck]] source` refers to; `map` is
/// already resolved to a usable path. `[[controller]]` may declare any number of these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Controller {
    pub id: String,
    pub map: Option<String>,
    pub device: Option<String>,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            id: "default".to_owned(),
            map: None,
            device: None,
        }
    }
}

/// One controller ready to attach: its resolved port, unit map and its own slice of the face →
/// deck table. Both front-ends attach these as separate MIDI sessions.
#[derive(Clone, Debug)]
pub struct ControllerSession {
    pub id: String,
    pub port: Option<String>,
    pub map: String,
    pub routes: DeckRoutes,
}

/// The `[preference.analysis]` section: which backend runs which analysis path, and whether a
/// browser load analyses a track that has no imported/cached grid yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnalysisBackends {
    pub beats: Backend,
    pub key: Backend,
    /// Run the analyser after a browser load when neither the imported library nor the analysis
    /// cache has a grid. Off by default: analysis takes seconds and should be an explicit choice.
    pub auto_analyse: bool,
}

fn analysis_section(value: &toml::Value) -> Result<AnalysisBackends, String> {
    let table = value
        .as_table()
        .ok_or_else(|| "must be a table".to_owned())?;
    let mut section = AnalysisBackends::default();
    for (key, value) in table {
        match key.as_str() {
            "beats" | "key" => {
                let text = value
                    .as_str()
                    .ok_or_else(|| format!("`{key}` must be a string"))?
                    .trim()
                    .to_owned();
                let backend = Backend::parse(&text).ok_or_else(|| {
                    format!("`{key} = \"{text}\"` is not a backend (stratum | timestretch)")
                })?;
                if key == "beats" {
                    section.beats = backend;
                } else {
                    section.key = backend;
                }
            }
            "auto_analyse" => match value.as_bool() {
                Some(auto) => section.auto_analyse = auto,
                None => return Err(format!("`{key}` must be true | false")),
            },
            other => {
                return Err(format!(
                    "unknown key `{other}` (beats | key | auto_analyse)"
                ))
            }
        }
    }
    Ok(section)
}

/// One parsed config, and the path it came from.
#[derive(Debug)]
pub struct Config {
    pub path: PathBuf,
    pub mixer: MixerConfig,
    pub live: LiveConfig,
    /// The first (or only) controller, kept for the front-ends' single-input paths.
    pub controller: Controller,
    /// Every declared controller. `[[controller]]` instances each own a port, a unit map and their
    /// own slice of the face → deck table.
    pub controllers: Vec<Controller>,
    /// Which backend runs the beat and the key analysis path.
    pub analysis: AnalysisBackends,
    /// Notes worth showing at startup (a first-start copy, clamped live values, ...).
    pub notes: Vec<String>,
}

/// Builds the controller-face → physical-deck table from `[[multideck.deck]] source`.
///
/// `source` is `<controller-id>.<face>[.mirror]` (an `Am` spelling also means the mirror). Passing
/// a `controller` id keeps only that controller's sources, so each `[[controller]]` instance gets
/// its own table and two devices cannot address the same deck unless their sources say so.
pub fn deck_routes(mixer: &MixerConfig, controller: Option<&str>) -> DeckRoutes {
    let mut routes = DeckRoutes::default();
    let mut faces: std::collections::HashMap<String, (Option<u8>, Option<u8>)> =
        std::collections::HashMap::new();
    for (index, channel) in mixer.channels.iter().enumerate() {
        let Some(source) = channel.source.as_deref() else {
            continue;
        };
        let Some((owner, face, mirror)) = parse_source(source) else {
            continue;
        };
        if let Some(want) = controller {
            if owner.as_deref().is_some_and(|owner| owner != want) {
                continue;
            }
        }
        let entry = faces.entry(face).or_insert((None, None));
        if mirror {
            entry.1 = Some(index as u8);
        } else {
            entry.0 = Some(index as u8);
        }
    }
    for (face, (normal, mirror)) in faces {
        if let Some(normal) = normal {
            routes.insert(&face, normal, mirror);
        }
    }
    routes
}

/// `<controller>.<face>[.mirror]` — an `Am` spelling also means the mirror variant. The owner is
/// optional so a bare `A` still works on a single-controller rig.
fn parse_source(source: &str) -> Option<(Option<String>, String, bool)> {
    let mut parts: Vec<&str> = source.split('.').filter(|part| !part.is_empty()).collect();
    let explicit = parts.last() == Some(&"mirror");
    if explicit {
        parts.pop();
    }
    let face = parts.pop()?;
    let owner = (!parts.is_empty()).then(|| parts.join("."));
    let (face, mirror) = if let Some(base) = face.strip_suffix('m') {
        (!base.is_empty()).then(|| (base.to_owned(), true))?
    } else {
        (face.to_owned(), explicit)
    };
    Some((owner, face, mirror))
}

/// The platform's config path. `HYPERMIXX_CONFIG` wins when set, which is also how a test or a
/// Termux/proot session names its own location.
pub fn default_path() -> Option<PathBuf> {
    // An empty value means "not set" — joining onto it would put the config in the working
    // directory instead of the user's config directory.
    if let Some(path) = std::env::var_os("HYPERMIXX_CONFIG").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let dir = if cfg!(windows) {
        std::env::var_os("APPDATA")
            .filter(|v| !v.is_empty())
            .map(|base| PathBuf::from(base).join("Hypermixx"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|home| PathBuf::from(home).join(".config"))
            })
            .map(|base| base.join("hypermixx"))
    }?;
    Some(dir.join("config.toml"))
}

/// Loads `explicit`, or the platform default. Only the *default* path is created on a first start:
/// a path the user named must exist, so a typo is an error rather than a silently copied template.
pub fn load_or_create(explicit: Option<&str>) -> Result<Config, String> {
    let (path, create_missing) = match explicit {
        Some(path) => (PathBuf::from(path), false),
        None => (
            default_path().ok_or_else(|| {
                "cannot find the platform config directory (set HYPERMIXX_CONFIG)".to_owned()
            })?,
            true,
        ),
    };
    load_or_create_at(&path, create_missing)
}

/// The same load with the "create on first start" decision already made, so a test can exercise
/// the first-start path without touching the process-wide config environment variables.
fn load_or_create_at(path: &Path, create_missing: bool) -> Result<Config, String> {
    let mut notes = Vec::new();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound && create_missing => {
            match first_copy(path) {
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
    parse(path, &text, notes)
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

    let controller_sections: Vec<ControllerSection> = match value.get("controller") {
        Some(toml::Value::Array(entries)) => entries
            .iter()
            .map(controller_section)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("{}: [[controller]]: {err}", path.display()))?,
        Some(section) => {
            let section = controller_section(section)
                .map_err(|err| format!("{}: [controller]: {err}", path.display()))?;
            // The shipped template keeps `[controller]` with every key commented out. That is
            // "no controller configured", not one anonymous controller with no port: a phantom
            // entry would make the TUI's F9 picker think a configured list already exists and
            // silently skip the port the user just chose.
            if section.id.is_none() && section.map.is_none() && section.device.is_none() {
                Vec::new()
            } else {
                vec![section]
            }
        }
        None => Vec::new(),
    };
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut controllers = Vec::with_capacity(controller_sections.len());
    let count = controller_sections.len();
    for (index, section) in controller_sections.into_iter().enumerate() {
        let label = section.id.clone().unwrap_or_else(|| {
            if count == 1 {
                "default".to_owned()
            } else {
                format!("controller{}", index + 1)
            }
        });
        let map = section.map.map(|map| resolve_against(dir, &map));
        // A configured but unreadable map is a silent "no controller" otherwise; say exactly why.
        if let Some(map) = &map {
            match std::fs::read_to_string(map) {
                Ok(text) => {
                    let base = Path::new(map)
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."));
                    if let Err(err) = hypermixx_midi::Map::from_toml_str_at(&text, base) {
                        notes.push(format!(
                            "{}: controller `{label}`.map `{map}` does not parse: {err}",
                            path.display()
                        ));
                    }
                }
                Err(err) => notes.push(format!(
                    "{}: controller `{label}`.map `{map}` cannot be read: {err}",
                    path.display()
                )),
            }
        }
        controllers.push(Controller {
            id: section.id.unwrap_or(label),
            map,
            device: section.device,
        });
    }
    let controller = controllers.first().cloned().unwrap_or_default();

    let analysis = match value
        .get("preference")
        .and_then(|preference| preference.get("analysis"))
    {
        Some(section) => analysis_section(section)
            .map_err(|err| format!("{}: [preference.analysis]: {err}", path.display()))?,
        None => AnalysisBackends::default(),
    };

    Ok(Config {
        path: path.to_path_buf(),
        mixer,
        live,
        controller,
        controllers,
        analysis,
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
            "id" => section.id = Some(text),
            "map" => section.map = Some(text),
            "device" => section.device = Some(text),
            other => return Err(format!("unknown key `{other}` (id | map | device)")),
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
        assert!(!cfg.mixer.autogain.enabled, "auto-gain is off in the template");
        assert_eq!(cfg.controller, Controller::default());
        assert!(
            cfg.controllers.is_empty(),
            "the template's empty [controller] table means no controller, not a phantom session"
        );
    }

    #[test]
    fn an_empty_controller_table_is_no_controller_but_a_named_one_is_kept() {
        let cfg = parse(
            Path::new("config.toml"),
            "[mixer]\n[[multideck.deck]]\nside = \"center\"\n[controller]\n",
            Vec::new(),
        )
        .expect("parses");
        assert!(cfg.controllers.is_empty());

        let cfg = parse(
            Path::new("config.toml"),
            "[mixer]\n[[multideck.deck]]\nside = \"center\"\n[controller]\nid = \"ctl1\"\n",
            Vec::new(),
        )
        .expect("parses");
        assert_eq!(cfg.controllers.len(), 1);
        assert_eq!(cfg.controllers[0].id, "ctl1");
    }

    #[test]
    fn analysis_settings_include_the_auto_analyse_switch() {
        let cfg = parse(
            Path::new("config.toml"),
            "[mixer]\n[[multideck.deck]]\nside = \"center\"\n[preference.analysis]\nbeats = \"stratum\"\nkey = \"timestretch\"\nauto_analyse = true\n",
            Vec::new(),
        )
        .expect("parses");
        assert_eq!(cfg.analysis.beats, Backend::Stratum);
        assert_eq!(cfg.analysis.key, Backend::Timestretch);
        assert!(cfg.analysis.auto_analyse);

        let err = parse(
            Path::new("config.toml"),
            "[preference.analysis]\nauto_analyse = \"yes\"\n",
            Vec::new(),
        )
        .unwrap_err();
        assert!(err.contains("auto_analyse"), "{err}");
    }

    #[test]
    fn sections_are_read_independently_and_map_is_a_resolved_pointer() {
        let dir = temp("sections");
        let path = dir.join("config.toml");
        let text = r#"
[mixer]
[mixer.autogain]
enabled = true
target_db = -9.0

[[multideck.deck]]
side = "center"

[live]

[preference]
[preference.waveform]
backend = "braille"

[controller]
map = "mapping.toml"
device = "DDJ-400"
"#;
        let cfg = parse(&path, text, Vec::new()).expect("parses");
        assert_eq!(cfg.mixer.channels.len(), 1);
        assert!(cfg.mixer.autogain.enabled);
        assert_eq!(cfg.mixer.autogain.target_db, -9.0);
        assert_eq!(
            cfg.controller.map,
            Some(dir.join("mapping.toml").to_string_lossy().into_owned())
        );
        assert_eq!(cfg.controller.device.as_deref(), Some("DDJ-400"));
    }

    #[test]
    fn an_existing_file_is_read_not_overwritten() {
        let path = temp("existing.toml");
        std::fs::write(&path, "[mixer]\n[[multideck.deck]]\nside = \"center\"\n").unwrap();
        let cfg = load_or_create(Some(path.to_string_lossy().as_ref())).expect("reads");
        assert_eq!(cfg.mixer.channels.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[mixer]\n[[multideck.deck]]\nside = \"center\"\n",
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

    #[test]
    fn a_first_start_creates_the_config_and_never_overwrites_it() {
        let dir = temp("first-start");
        let path = dir.join("nested/config.toml");
        std::fs::remove_dir_all(&dir).ok();
        let cfg = load_or_create_at(&path, true).expect("first start creates the config");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), template());
        assert_eq!(cfg.path, path);
        assert!(cfg.notes.iter().any(|note| note.contains("created")));

        let edited = "[mixer]\n[[multideck.deck]]\nside = \"center\"\n";
        std::fs::write(&path, edited).unwrap();
        let again = load_or_create_at(&path, true).expect("reads the edited config");
        assert_eq!(again.mixer.channels.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_broken_controller_map_is_reported_not_silent() {
        let dir = temp("controller-map");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[mixer]\n[[multideck.deck]]\nside = \"center\"\n[controller]\nmap = \"missing.toml\"\n",
        )
        .unwrap();
        let cfg = load_or_create(Some(path.to_string_lossy().as_ref())).expect("parses");
        assert!(
            cfg.notes
                .iter()
                .any(|note| note.contains("controller") && note.contains("cannot be read")),
            "a missing map must be named: {:?}",
            cfg.notes
        );

        // Once the map is a real file, no note is produced.
        std::fs::write(dir.join("missing.toml"), MAP_TEMPLATE).unwrap();
        let ok = load_or_create(Some(path.to_string_lossy().as_ref())).expect("parses");
        assert!(ok.notes.is_empty(), "{:?}", ok.notes);
        std::fs::remove_dir_all(&dir).ok();
    }
}
