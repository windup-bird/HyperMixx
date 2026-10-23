//! Front-end preferences: the parts of the TUI that are a matter of taste rather than a machine
//! fact (the waveform backend, its three colours, image refresh target).
//!
//! Deliberately forgiving: a typo in a colour must not stop the music. Unknown keys are reported as
//! notes, invalid values fall back to the built-in default, and a missing file is simply "use the
//! defaults". The file lives next to the profile that names it (`profile/preference.toml` by
//! default).

use std::fs;

use hypermixx_core::KeyFormat;
use hypermixx_library::SourceKind;
use ratatui::style::Color;

/// Where the TUI looks for preferences when no profile points at another folder.
pub const DEFAULT_PATH: &str = "profile/preference.toml";

/// Which waveform renderer to use. `Auto` resolves to Kitty when the terminal advertises Kitty
/// graphics, otherwise Braille; an explicit choice overrides detection (except under tmux/screen,
/// where passthrough is not implemented yet).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WaveformBackend {
    #[default]
    Auto,
    Kitty,
    Braille,
}

impl WaveformBackend {
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "kitty" => Some(Self::Kitty),
            "braille" => Some(Self::Braille),
            _ => None,
        }
    }
}

/// The three waveform band colours. Defaults match the historical braille palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaveformColors {
    pub low: Color,
    pub mid: Color,
    pub high: Color,
}

impl Default for WaveformColors {
    fn default() -> Self {
        Self {
            low: Color::Blue,
            mid: Color::Green,
            high: Color::White,
        }
    }
}

/// Everything the waveform view needs from preferences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaveformSettings {
    pub backend: WaveformBackend,
    /// Target refresh for the image backends. Braille follows the UI tick.
    pub refresh_hz: u16,
    /// Cell size in pixels, when the terminal cannot report it (notably Windows). `None` asks
    /// crossterm, then falls back to 8×16.
    pub cell: Option<(u16, u16)>,
    /// Band crossovers, in Hz. Raising `high_hz` keeps the white high band out of the body.
    pub low_hz: u16,
    pub high_hz: u16,
    pub colors: WaveformColors,
}

impl Default for WaveformSettings {
    fn default() -> Self {
        Self {
            backend: WaveformBackend::Auto,
            refresh_hz: 60,
            cell: None,
            low_hz: 200,
            high_hz: 4_000,
            colors: WaveformColors::default(),
        }
    }
}

/// One export format a source category may read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryFormat {
    Rekordbox,
    Serato,
    VirtualDj,
}

impl LibraryFormat {
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "rkb" | "rekordbox" => Some(Self::Rekordbox),
            "serato" => Some(Self::Serato),
            "vdj" | "virtualdj" => Some(Self::VirtualDj),
            _ => None,
        }
    }

    pub fn kind(self) -> SourceKind {
        match self {
            Self::Rekordbox => SourceKind::Rekordbox,
            Self::Serato => SourceKind::Serato,
            Self::VirtualDj => SourceKind::VirtualDj,
        }
    }

}

/// Library discovery settings. A category that is absent or empty contributes no sources; this is
/// deliberate so a machine with no local rekordbox install does not scan the whole home directory.
#[derive(Clone, Debug, PartialEq)]
pub struct LibrarySettings {
    /// Export formats to look for in local user locations (`~/Music`, `~/.VirtualDJ`, ...).
    pub local: Vec<LibraryFormat>,
    /// Export formats to look for on automatically discovered/mounted USB media.
    pub usb: Vec<LibraryFormat>,
    /// Plain folders to index.
    pub folders: Vec<String>,
    /// Override the decoder's gapless start trim. `None` probes each file; `Some(0.0)` disables it.
    pub decoder_start_ms: Option<f64>,
    /// Extra correction added after the source and decoder compensation. Positive shifts later.
    pub grid_offset_ms: f64,
    /// The Rekordbox ANLZ clock offset for MP3 files.
    pub rkb_mp3_offset_ms: f64,
    /// Serato tag clock offset. The validated sample needs `-16.5` relative to RB/VDJ.
    pub serato_offset_ms: f64,
    /// VirtualDJ ANLZ clock offset.
    pub vdj_offset_ms: f64,
}

impl Default for LibrarySettings {
    fn default() -> Self {
        Self {
            local: Vec::new(),
            usb: Vec::new(),
            folders: Vec::new(),
            decoder_start_ms: None,
            grid_offset_ms: 0.0,
            rkb_mp3_offset_ms: 0.0,
            serato_offset_ms: -16.5,
            vdj_offset_ms: 0.0,
        }
    }
}

impl LibrarySettings {
    pub fn local_kinds(&self) -> Vec<SourceKind> {
        self.local.iter().map(|format| format.kind()).collect()
    }

    pub fn usb_kinds(&self) -> Vec<SourceKind> {
        self.usb.iter().map(|format| format.kind()).collect()
    }
}

/// Display choices that apply to more than one panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplaySettings {
    /// How keys are rendered. `Traditional` is the "classic" spelling (`Am`).
    pub key_format: KeyFormat,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            key_format: KeyFormat::Camelot,
        }
    }
}

/// The TUI's preferences. Kept as one struct so adding a tab/theme later does not change every
/// call site.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preference {
    pub waveform: WaveformSettings,
    pub library: LibrarySettings,
    pub display: DisplaySettings,
}

/// Reads a preference file. A missing file is not an error: the defaults apply.
pub fn load(path: &str) -> Result<(Preference, Vec<String>), String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Preference::default(), Vec::new()))
        }
        Err(err) => return Err(format!("{path}: {err}")),
    };
    parse(&text).map_err(|err| format!("{path}: {err}"))
}

/// Parses the file body. Unknown keys are notes, not failures.
pub fn parse(text: &str) -> Result<(Preference, Vec<String>), String> {
    let value: toml::Value = toml::from_str(text).map_err(|err| err.to_string())?;
    let table = value
        .as_table()
        .ok_or_else(|| "preferences are a table of sections".to_owned())?;
    let mut preference = Preference::default();
    let mut notes = Vec::new();
    for (key, value) in table {
        match key.as_str() {
            "waveform" => {
                let table = value
                    .as_table()
                    .ok_or_else(|| "`waveform` must be a table".to_owned())?;
                parse_waveform(table, &mut preference.waveform, &mut notes);
            }
            "library" => {
                let table = value
                    .as_table()
                    .ok_or_else(|| "`library` must be a table".to_owned())?;
                parse_library(table, &mut preference.library, &mut notes);
            }
            "display" => {
                let table = value
                    .as_table()
                    .ok_or_else(|| "`display` must be a table".to_owned())?;
                parse_display(table, &mut preference.display, &mut notes);
            }
            other => notes.push(format!("unknown preference section `[{other}]` ignored")),
        }
    }
    Ok((preference, notes))
}

fn parse_display(
    table: &toml::value::Table,
    display: &mut DisplaySettings,
    notes: &mut Vec<String>,
) {
    for (key, value) in table {
        match key.as_str() {
            "key" => match value.as_str().map(str::trim).map(str::to_ascii_lowercase).as_deref() {
                Some("camelot" | "camlot") => display.key_format = KeyFormat::Camelot,
                Some("classic" | "traditional") => display.key_format = KeyFormat::Traditional,
                _ => notes.push("display.key must be camelot | classic".to_owned()),
            },
            other => notes.push(format!("unknown display key `{other}` ignored")),
        }
    }
}

fn parse_library(
    table: &toml::value::Table,
    library: &mut LibrarySettings,
    notes: &mut Vec<String>,
) {
    for (key, value) in table {
        match key.as_str() {
            "local" | "usb" => {
                let Some(values) = value.as_array() else {
                    notes.push(format!("library.{key} must be an array of formats"));
                    continue;
                };
                let target = if key == "local" {
                    &mut library.local
                } else {
                    &mut library.usb
                };
                target.clear();
                for value in values {
                    match value.as_str().and_then(LibraryFormat::parse) {
                        Some(format) => target.push(format),
                        None => notes.push(format!(
                            "library.{key} entries must be rkb | serato | vdj"
                        )),
                    }
                }
            }
            "folder" | "folders" => {
                let Some(values) = value.as_array() else {
                    notes.push("library.folders must be an array of paths".to_owned());
                    continue;
                };
                library.folders.clear();
                for value in values {
                    match value
                        .as_str()
                        .map(str::trim)
                        .filter(|path| !path.is_empty())
                    {
                        Some(path) => library.folders.push(path.to_owned()),
                        None => {
                            notes.push("library.folders entries must be non-empty strings".into())
                        }
                    }
                }
            }
            "decoder_start_ms" => match value {
                toml::Value::String(text) if text.eq_ignore_ascii_case("auto") => {
                    library.decoder_start_ms = None;
                }
                _ => match number(value) {
                    Some(ms) if ms.is_finite() => library.decoder_start_ms = Some(ms),
                    _ => notes.push(
                        "library.decoder_start_ms must be \"auto\" or a finite number".to_owned(),
                    ),
                },
            },
            "grid_offset_ms" => match number(value) {
                Some(ms) if ms.is_finite() => library.grid_offset_ms = ms,
                _ => notes.push("library.grid_offset_ms must be a finite number".to_owned()),
            },
            "rkb_mp3_offset_ms" | "rekordbox_mp3_offset_ms" => match number(value) {
                Some(ms) if ms.is_finite() => library.rkb_mp3_offset_ms = ms,
                _ => notes.push("library.rkb_mp3_offset_ms must be a finite number".to_owned()),
            },
            "serato_offset_ms" => match number(value) {
                Some(ms) if ms.is_finite() => library.serato_offset_ms = ms,
                _ => notes.push("library.serato_offset_ms must be a finite number".to_owned()),
            },
            "vdj_offset_ms" | "virtualdj_offset_ms" => match number(value) {
                Some(ms) if ms.is_finite() => library.vdj_offset_ms = ms,
                _ => notes.push("library.vdj_offset_ms must be a finite number".to_owned()),
            },
            other => notes.push(format!("unknown library key `{other}` ignored")),
        }
    }
}

fn number(value: &toml::Value) -> Option<f64> {
    value
        .as_float()
        .or_else(|| value.as_integer().map(|value| value as f64))
}

fn parse_waveform(
    table: &toml::value::Table,
    waveform: &mut WaveformSettings,
    notes: &mut Vec<String>,
) {
    for (key, value) in table {
        match key.as_str() {
            "backend" => match value.as_str().and_then(WaveformBackend::parse) {
                Some(backend) => waveform.backend = backend,
                None => notes.push(
                    "waveform.backend must be auto | kitty | braille; using the default"
                        .to_owned(),
                ),
            },
            "refresh_hz" => match value.as_integer() {
                Some(hz) => {
                    let clamped = hz.clamp(10, 60) as u16;
                    if hz != i64::from(clamped) {
                        notes.push(format!(
                            "waveform.refresh_hz clamped from {hz} to {clamped} (10..=60)"
                        ));
                    }
                    waveform.refresh_hz = clamped;
                }
                None => notes.push("waveform.refresh_hz must be an integer".to_owned()),
            },
            "cell" => match parse_cell(value) {
                Some(cell) => waveform.cell = Some(cell),
                None => notes.push(
                    "waveform.cell must be \"WxH\" or [width, height] in pixels; ignoring"
                        .to_owned(),
                ),
            },
            "low_hz" | "high_hz" => {
                let (slot, range) = if key == "low_hz" {
                    (&mut waveform.low_hz, 20..=2_000)
                } else {
                    (&mut waveform.high_hz, 500..=20_000)
                };
                match value.as_integer().and_then(|hz| u16::try_from(hz).ok()) {
                    Some(hz) if range.contains(&hz) => *slot = hz,
                    _ => notes.push(format!(
                        "waveform.{key} must be {}..={} Hz",
                        range.start(),
                        range.end()
                    )),
                }
            }
            "low" | "mid" | "high" => {
                let slot = match key.as_str() {
                    "low" => &mut waveform.colors.low,
                    "mid" => &mut waveform.colors.mid,
                    _ => &mut waveform.colors.high,
                };
                match value.as_str().and_then(parse_color) {
                    Some(color) => *slot = color,
                    None => notes.push(format!(
                        "waveform.{key} must be #rrggbb or #rgb; using the default"
                    )),
                }
            }
            other => notes.push(format!("unknown waveform key `{other}` ignored")),
        }
    }
    if waveform.high_hz <= waveform.low_hz.saturating_add(100) {
        notes.push(
            "waveform.high_hz must be at least low_hz + 100; using the default 200/4000".to_owned(),
        );
        waveform.low_hz = 200;
        waveform.high_hz = 4_000;
    }
}

/// `8x16` or `[8, 16]`, both in pixels.
fn parse_cell(value: &toml::Value) -> Option<(u16, u16)> {
    let (width, height) = match value {
        toml::Value::String(text) => {
            let (width, height) = text.split_once('x').or_else(|| text.split_once('X'))?;
            (width.trim().parse().ok()?, height.trim().parse().ok()?)
        }
        toml::Value::Array(values) if values.len() == 2 => (
            values[0].as_integer()?.try_into().ok()?,
            values[1].as_integer()?.try_into().ok()?,
        ),
        _ => return None,
    };
    if !(1..=64).contains(&width) || !(1..=64).contains(&height) {
        return None;
    }
    Some((width, height))
}

/// `#rrggbb`, `#rgb`, or the same without the leading `#`.
fn parse_color(text: &str) -> Option<Color> {
    let hex = text.trim().trim_start_matches('#');
    match hex.len() {
        3 => {
            let mut channels = [0u8; 3];
            for (index, ch) in hex.chars().enumerate() {
                let value = ch.to_digit(16)? as u8;
                channels[index] = value * 17;
            }
            Some(Color::Rgb(channels[0], channels[1], channels[2]))
        }
        6 => {
            let value = u32::from_str_radix(hex, 16).ok()?;
            Some(Color::Rgb(
                (value >> 16) as u8,
                (value >> 8) as u8,
                value as u8,
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_defaults() {
        let (preference, notes) = parse("").unwrap();
        assert_eq!(preference, Preference::default());
        assert!(notes.is_empty());
        assert_eq!(preference.waveform.refresh_hz, 60);
    }

    #[test]
    fn backend_and_colours_are_read() {
        let (preference, notes) = parse(
            r##"
            [waveform]
            backend = "kitty"
            refresh_hz = 30
            cell = "10x20"
            low_hz = 150
            high_hz = 6000
            low = "#f00"
            mid = "#123456"
            high = "ffffff"
            "##,
        )
        .unwrap();
        assert_eq!(preference.waveform.backend, WaveformBackend::Kitty);
        assert_eq!(preference.waveform.refresh_hz, 30);
        assert_eq!(preference.waveform.cell, Some((10, 20)));
        assert_eq!(preference.waveform.low_hz, 150);
        assert_eq!(preference.waveform.high_hz, 6000);
        assert_eq!(preference.waveform.colors.low, Color::Rgb(255, 0, 0));
        assert_eq!(preference.waveform.colors.mid, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(preference.waveform.colors.high, Color::Rgb(255, 255, 255));
        assert!(notes.is_empty());
    }

    #[test]
    fn bad_values_are_notes_not_failures() {
        let (preference, notes) = parse(
            r#"
            [waveform]
            backend = "crt"
            refresh_hz = 1000
            low = "not-a-colour"
            cell = "huge"
            extra = 1
            "#,
        )
        .unwrap();
        assert_eq!(preference.waveform.backend, WaveformBackend::Auto);
        assert_eq!(preference.waveform.refresh_hz, 60);
        assert_eq!(preference.waveform.colors.low, Color::Blue);
        assert!(notes.iter().any(|note| note.contains("backend")));
        assert!(notes.iter().any(|note| note.contains("refresh_hz")));
        assert!(notes.iter().any(|note| note.contains("waveform.low")));
        assert!(notes.iter().any(|note| note.contains("waveform.cell")));
        assert!(notes
            .iter()
            .any(|note| note.contains("unknown waveform key")));
    }

    #[test]
    fn crossovers_must_be_ordered() {
        let (preference, notes) = parse(
            r#"
            [waveform]
            low_hz = 3000
            high_hz = 4000
            "#,
        )
        .unwrap();
        assert_eq!(preference.waveform.low_hz, 200);
        assert_eq!(preference.waveform.high_hz, 4000);
        assert!(notes.iter().any(|note| note.contains("low_hz")));
    }

    #[test]
    fn library_sources_are_selected_by_category() {
        let (preference, notes) = parse(
            r#"
            [library]
            local = ["rkb", "serato"]
            usb = ["rkb", "vdj"]
            folders = ["/music", " /more "]
            "#,
        )
        .unwrap();
        assert_eq!(
            preference.library.local,
            vec![LibraryFormat::Rekordbox, LibraryFormat::Serato]
        );
        assert_eq!(
            preference.library.usb,
            vec![LibraryFormat::Rekordbox, LibraryFormat::VirtualDj]
        );
        assert_eq!(
            preference.library.folders,
            vec!["/music".to_owned(), "/more".to_owned()]
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn calibration_values_are_configurable() {
        let (preference, notes) = parse(
            r#"
            [library]
            decoder_start_ms = "auto"
            grid_offset_ms = -12.5
            rkb_mp3_offset_ms = 17.25
            serato_offset_ms = -16.5
            vdj_offset_ms = 1.25
            "#,
        )
        .unwrap();
        assert_eq!(preference.library.decoder_start_ms, None);
        assert_eq!(preference.library.grid_offset_ms, -12.5);
        assert_eq!(preference.library.rkb_mp3_offset_ms, 17.25);
        assert_eq!(preference.library.serato_offset_ms, -16.5);
        assert_eq!(preference.library.vdj_offset_ms, 1.25);
        let defaults = LibrarySettings::default();
        assert_eq!(defaults.rkb_mp3_offset_ms, 0.0);
        assert_eq!(defaults.serato_offset_ms, -16.5);
        assert_eq!(defaults.vdj_offset_ms, 0.0);
        assert!(notes.is_empty());

        let (preference, _) = parse("[library]\ndecoder_start_ms = 0.0").unwrap();
        assert_eq!(preference.library.decoder_start_ms, Some(0.0));
    }

    #[test]
    fn key_display_defaults_to_camelot_and_can_be_classic() {
        let (preference, notes) = parse("").unwrap();
        assert_eq!(preference.display.key_format, KeyFormat::Camelot);
        assert!(notes.is_empty());
        let (preference, notes) = parse("[display]\nkey = \"classic\"").unwrap();
        assert_eq!(preference.display.key_format, KeyFormat::Traditional);
        assert!(notes.is_empty());
    }

    #[test]
    fn a_missing_file_falls_back_to_defaults() {
        let (preference, notes) =
            load("/tmp/hypermixx-preference-that-does-not-exist.toml").unwrap();
        assert_eq!(preference, Preference::default());
        assert!(notes.is_empty());
    }
}
