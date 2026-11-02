//! Profiles: one file that says which topology, which live settings and which MIDI input to use.
//!
//! The `profile/` folder holds everything — the profile itself, the toml files it points at, and any
//! other named profile (`profile/dj.toml`, run with `--profile dj`). A profile is a **pointer file**,
//! not another schema to keep in step: the mixer topology, the live settings and the MIDI input each
//! keep their own file and their own validation, which is what lets `mixer reload` and `live reload`
//! keep working on the file they were told about.
//!
//! **Every relative path resolves against the folder of the file that contains it.** So
//! `profile/profile.toml`'s `mixer = "mixer.toml"` means `profile/mixer.toml`, and a named profile
//! sitting next to the sets it names needs no `../` arithmetic. One rule, wherever a path appears.
//!
//! `[preference]` is accepted and **reserved**: the front-end's own settings will live there, and
//! until they exist a key in it is reported rather than silently ignored.

use std::path::{Path, PathBuf};

/// Where a profile is looked for when `--profile` is not given.
pub const AMBIENT: &str = "profile/profile.toml";
/// Where the folder defaults live, used when a profile names nothing (or there is no profile).
pub const DEFAULT_MIXER: &str = "profile/mixer.toml";
pub const DEFAULT_LIVE: &str = "profile/live.toml";
pub const DEFAULT_MIDI: &str = "profile/controller.toml";
/// The pre-rename default, read when `controller.toml` is not there. Keeping both is what lets an
/// existing folder keep working without a migration step.
pub const LEGACY_MIDI: &str = "profile/midi.toml";
/// Where a bare profile *name* is resolved: the folder itself, so `--profile dj` is
/// `profile/dj.toml` next to the files it names.
pub const PROFILES_DIR: &str = "profile";
/// The mapping file used when nothing names one (the historical default, in the working directory).
pub const DEFAULT_MAP: &str = "midi-map.toml";

/// The MIDI input a profile asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MidiSettings {
    /// A port name substring (case-insensitive, first match wins) or an index from `controller ports`.
    /// `None` means no input at startup — the TUI can still pick one at runtime.
    pub device: Option<String>,
    /// The mapping file. Already resolved against the folder of the file that named it.
    pub map: Option<String>,
}

/// The controller file to read when nothing names one: `controller.toml` if it exists, the legacy
/// `midi.toml` otherwise. A folder that predates the rename keeps working untouched.
pub fn default_midi_path() -> &'static str {
    if Path::new(DEFAULT_MIDI).is_file() {
        DEFAULT_MIDI
    } else {
        LEGACY_MIDI
    }
}

/// The **mapping file** to open when a front-end has a port but no file: the shipped one when it is
/// there, and otherwise the historical working-directory default. One definition, used by
/// `--guide`, the TUI's controller pickers and the session's fallback, so the four cannot drift.
pub fn default_map_path() -> &'static str {
    let shipped = default_midi_path();
    if Path::new(shipped).is_file() {
        shipped
    } else {
        DEFAULT_MAP
    }
}

/// What a profile names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    /// The mixer topology, already resolved. `None` = the folder default that already applies.
    pub mixer: Option<String>,
    /// This machine's settings, already resolved.
    pub live: Option<String>,
    /// The MIDI input, already resolved.
    pub midi: MidiSettings,
}

impl Profile {
    /// Parses a profile found in `dir`. Unknown sections and non-string paths are errors — a profile
    /// that quietly ignored `[mixr]` would run the default topology while looking like it had not.
    ///
    /// `midi` is a **path** to a map file. The device lives in that map's `[meta].device`; there is
    /// no second settings shape to keep in step.
    pub fn from_toml_str(text: &str, dir: &Path) -> Result<(Self, Vec<String>), String> {
        let value: toml::Value = toml::from_str(text).map_err(|err| err.to_string())?;
        let table = value
            .as_table()
            .ok_or_else(|| "a profile is a table of sections".to_owned())?;
        let mut profile = Self::default();
        let mut notes = Vec::new();
        for (key, value) in table {
            match key.as_str() {
                "mixer" => profile.mixer = path_value(key, value, dir)?,
                "live" => profile.live = path_value(key, value, dir)?,
                "controller" | "midi" => match value {
                    toml::Value::String(path) => {
                        let path = resolve_against(dir, path.trim());
                        let (settings, midi_notes) = load_midi(&path)?;
                        profile.midi = settings;
                        notes.extend(midi_notes);
                    }
                    toml::Value::Table(_) => {
                        return Err(
                            "inline `[midi]` is no longer supported; point `midi` at a map file                              (the device lives in that file's `[meta].device`)"
                                .to_owned(),
                        )
                    }
                    _ => return Err("`midi` must be a map file path".to_owned()),
                },
                "preference" => {
                    // Reserved. An empty table is exactly what an unimplemented section looks like.
                    let table = value
                        .as_table()
                        .ok_or_else(|| "`preference` must be a table".to_owned())?;
                    if !table.is_empty() {
                        notes.push(format!(
                            "preference key(s) {} are not used yet — the front-end will define them",
                            table.keys().cloned().collect::<Vec<_>>().join(", ")
                        ));
                    }
                }
                other => {
                    return Err(format!(
                        "unknown section `{other}` (mixer | live | controller | preference)"
                    ))
                }
            }
        }
        Ok((profile, notes))
    }
}

/// Reads and parses a profile file.
pub fn load(path: &str) -> Result<(Profile, Vec<String>), String> {
    // Every failure names the file: a profile is chosen by name, so "No such file" without it says
    // nothing about which name was wrong.
    let text = std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
    let dir = parent_dir(path);
    Profile::from_toml_str(&text, &dir).map_err(|err| format!("{path}: {err}"))
}

/// Reads a `profile/midi.toml`-shaped map file.
///
/// There is exactly one shape: a map file (`[[unit]]`, optional `[meta]`). The device is
/// `[meta].device`; `--controller` still overrides it. The old two-part settings table (`device` +
/// `map`) is refused with a migration message rather than silently ignored.
pub fn load_midi(path: &str) -> Result<(MidiSettings, Vec<String>), String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
    let value: toml::Value = toml::from_str(&text).map_err(|err| format!("{path}: {err}"))?;
    let table = value
        .as_table()
        .ok_or_else(|| format!("{path}: a midi file must be a map table"))?;
    if table.contains_key("device") || table.contains_key("map") {
        return Err(format!(
            "{path}: the old two-part midi file is no longer supported; keep one map file and put              the port in `[meta].device`"
        ));
    }
    let map = hypermixx_midi::Map::from_toml_str_at(&text, &parent_dir(path))
        .map_err(|err| format!("{path}: {err}"))?;
    let device = map.meta.as_ref().and_then(|meta| meta.device.clone());
    let mut notes = Vec::new();
    if device.is_none() {
        notes.push(
            "`[meta].device` is not set; no controller input opens until `--controller` or F9"
                .to_owned(),
        );
    }
    Ok((
        MidiSettings {
            device,
            map: Some(path.to_owned()),
        },
        notes,
    ))
}

/// Where `--profile <arg>` points: a bare name is a file in the `profile/` folder, anything that
/// looks like a path is used as it stands.
pub fn resolve(arg: &str) -> String {
    if arg.ends_with(".toml") || arg.contains('/') {
        return arg.to_owned();
    }
    format!("{PROFILES_DIR}/{arg}.toml")
}

/// A path as written in a file found in `dir`: absolute stays absolute, relative joins `dir`.
fn resolve_against(dir: &Path, path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    dir.join(path).to_string_lossy().into_owned()
}

/// The folder a file lives in, or `.` for a bare name.
fn parent_dir(path: &str) -> PathBuf {
    Path::new(path)
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// One of the profile's path keys.
fn path_value(key: &str, value: &toml::Value, dir: &Path) -> Result<Option<String>, String> {
    match string_value(key, value)? {
        Some(path) if !path.is_empty() => Ok(Some(resolve_against(dir, &path))),
        _ => Ok(None),
    }
}

/// A string-valued key, or an error naming it.
fn string_value(key: &str, value: &toml::Value) -> Result<Option<String>, String> {
    match value {
        toml::Value::String(text) => Ok(Some(text.trim().to_owned())),
        // A section header where a path belongs is the mistake a hand-written file actually makes,
        // so it gets its own message rather than "expected string, found table".
        toml::Value::Table(_) => Err(format!(
            "`{key}` takes a path or a string; `[{key}]` is a section (see profile/profile.toml)"
        )),
        _ => Err(format!("`{key}` must be a string")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<(Profile, Vec<String>), String> {
        Profile::from_toml_str(text, Path::new("profile"))
    }

    fn toml_path(path: &Path) -> String {
        format!("\"{}\"", path.display().to_string().replace('\\', "\\\\"))
    }

    fn is_path(actual: Option<&str>, expected: &Path) -> bool {
        actual.is_some_and(|path| Path::new(path) == expected)
    }

    #[test]
    fn a_profile_names_the_files_it_needs() {
        let (profile, notes) = parse(
            r#"
            mixer = "mixer.toml"
            live = "live.toml"
            "#,
        )
        .unwrap();
        // Relative to the profile's own folder, so a name alone is enough.
        assert!(is_path(
            profile.mixer.as_deref(),
            &Path::new("profile").join("mixer.toml")
        ));
        assert!(is_path(
            profile.live.as_deref(),
            &Path::new("profile").join("live.toml")
        ));
        assert!(notes.is_empty());
        assert_eq!(profile.midi, MidiSettings::default());
    }

    #[test]
    fn paths_resolve_against_the_profile_that_names_them() {
        let absolute = std::env::temp_dir().join("hypermixx-live.toml");
        let text = format!(
            "mixer = \"../sets/club.toml\"\nlive = {}\n",
            toml_path(&absolute)
        );
        let (profile, _) = Profile::from_toml_str(&text, Path::new("profile")).unwrap();
        assert!(is_path(
            profile.mixer.as_deref(),
            &Path::new("profile").join("../sets/club.toml")
        ));
        assert!(
            is_path(profile.live.as_deref(), &absolute),
            "absolute stays: {:?}",
            profile.live
        );
        // A profile at the root of the tree resolves against the root.
        let (profile, _) =
            Profile::from_toml_str("mixer = \"profile/mixer.toml\"\n", Path::new(".")).unwrap();
        assert!(is_path(
            profile.mixer.as_deref(),
            &Path::new(".").join("profile/mixer.toml")
        ));
    }

    #[test]
    fn the_controller_file_is_a_map_file_only() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let map = root.join(DEFAULT_MIDI);
        let text = format!("controller = {}\n", toml_path(&map));
        let (profile, notes) = parse(&text).unwrap();
        assert!(
            is_path(profile.midi.map.as_deref(), &map),
            "the map path is what the profile carries: {:?}",
            profile.midi.map
        );
        // The pre-rename key still parses, so an old profile keeps working.
        let legacy = parse(&format!("midi = {}\n", toml_path(&map))).unwrap().0;
        assert_eq!(legacy.midi.map, profile.midi.map);
        // The shipped map names the development controller, so there is nothing to warn about.
        let device = profile.midi.device.clone();
        assert_eq!(
            device.is_none(),
            notes.iter().any(|note| note.contains("device"))
        );

        // The old inline settings table is refused with a migration message.
        let error = parse("[midi]\ndevice = \"DDJ-400\"\nmap = \"m.toml\"\n").unwrap_err();
        assert!(
            error.contains("inline") && error.contains("map file"),
            "{error}"
        );
    }

    #[test]
    fn an_empty_profile_means_the_defaults() {
        let (profile, notes) = parse("").unwrap();
        assert_eq!(
            profile,
            Profile::default(),
            "nothing named, nothing overridden"
        );
        assert!(notes.is_empty());
        // A section that is present but empty is the same as absent.
        let (profile, notes) = parse("[preference]\n").unwrap();
        assert_eq!(profile, Profile::default());
        assert!(notes.is_empty());
    }

    #[test]
    fn the_reserved_preference_section_is_reported_but_not_fatal() {
        let (_, notes) = parse("[preference]\ntheme = \"dark\"\n").unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("theme"), "{notes:?}");
    }

    #[test]
    fn typos_and_wrong_types_are_errors() {
        assert!(parse("[mixr]\npath = \"x\"\n")
            .unwrap_err()
            .contains("mixr"));
        assert!(parse("mixer = 3\n").unwrap_err().contains("mixer"));
        // A section header where a path belongs gets a message about the spelling, not about types.
        let err = parse("[mixer]\nfile = \"x\"\n").unwrap_err();
        assert!(err.contains("mixer") && err.contains("section"), "{err}");
        assert!(parse("[midi]\nportt = \"1\"\n")
            .unwrap_err()
            .contains("inline"));
        assert!(parse("midi = 3\n").unwrap_err().contains("midi"));
    }

    #[test]
    fn a_bare_name_is_a_file_in_the_profile_folder() {
        assert_eq!(resolve("dj"), "profile/dj.toml");
        assert_eq!(resolve("dj.toml"), "dj.toml");
        assert_eq!(resolve("profile/dj.toml"), "profile/dj.toml");
        assert_eq!(resolve("/tmp/p.toml"), "/tmp/p.toml");
    }

    /// The shipped folder must be loadable: the example profile points at files that exist, and the
    /// resolution rule (every path relative to the file that names it) holds for all of them.
    ///
    /// Written against `profile/example.toml` rather than `profile/profile.toml` on purpose: the
    /// ambient profile is optional, so a test that starts with it silently tested nothing on a
    /// checkout that does not ship one. `example.toml` is tracked, so this always runs.
    #[test]
    fn the_shipped_profile_folder_loads() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        // Canonical, because that is what the profile's own resolution produces: every relative path
        // is joined onto the folder of the file that named it.
        let folder = std::fs::canonicalize(root.join(PROFILES_DIR)).expect("the profile folder");
        let root = &folder.parent().expect("the checkout root");
        for name in ["mixer.toml", "live.toml", "controller.toml", "example.toml"] {
            assert!(
                folder.join(name).is_file(),
                "profile/{name} is part of the shipped folder"
            );
        }
        let example = folder.join("example.toml");
        let (profile, notes) =
            load(&example.to_string_lossy()).expect("the shipped profile parses");
        assert!(
            notes.iter().all(|note| note.contains("device")),
            "the only note is that the map names no port: {notes:?}"
        );
        // Each pointer resolved against the profile's own folder — compared as a `Path`, so this is
        // about the rule rather than about which separator the platform uses.
        for (actual, expected) in [
            (profile.mixer.as_deref(), "mixer.toml"),
            (profile.live.as_deref(), "live.toml"),
            (profile.midi.map.as_deref(), "controller.toml"),
        ] {
            assert!(
                is_path(actual, &folder.join(expected)),
                "{actual:?} should resolve to profile/{expected}"
            );
            // ...and the file it names is really there.
            assert!(
                root.join(actual.expect("named")).is_file(),
                "{actual:?} does not exist"
            );
        }
        // The ambient profile is optional: a checkout without one runs on the folder defaults.
        let root = folder.parent().expect("the checkout root");
        if root.join(AMBIENT).is_file() {
            load(&root.join(AMBIENT).to_string_lossy()).expect("the ambient profile parses");
        }
    }

    #[test]
    fn a_map_file_carries_its_device_and_old_shapes_are_refused() {
        let dir =
            std::env::temp_dir().join(format!("hypermixx-midi-profile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("midi.toml");
        std::fs::write(
            &path,
            r#"
[meta]
device = "DDJ-400"

[[unit]]
id = "u"
deck = 1
[[unit.mode]]
name = "m"
"#,
        )
        .unwrap();
        let (settings, notes) = load_midi(&path.display().to_string()).unwrap();
        assert_eq!(settings.device.as_deref(), Some("DDJ-400"));
        assert!(settings.map.is_some());
        assert!(notes.is_empty());

        std::fs::write(
            &path,
            r#"device = "DDJ-400"
map = "m.toml"
"#,
        )
        .unwrap();
        let error = load_midi(&path.display().to_string()).unwrap_err();
        assert!(error.contains("two-part"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
