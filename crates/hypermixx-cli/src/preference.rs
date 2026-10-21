//! Front-end preferences: the parts of the TUI that are a matter of taste rather than a machine
//! fact (the waveform backend, its three colours, image refresh target).
//!
//! Deliberately forgiving: a typo in a colour must not stop the music. Unknown keys are reported as
//! notes, invalid values fall back to the built-in default, and a missing file is simply "use the
//! defaults". The file lives next to the profile that names it (`profile/preference.toml` by
//! default).

use std::fs;

use ratatui::style::Color;

/// Where the TUI looks for preferences when no profile points at another folder.
pub const DEFAULT_PATH: &str = "profile/preference.toml";

/// Which waveform renderer to use. `Auto` resolves in the order Kitty → Sixel → Braille from the
/// terminal's environment; an explicit choice overrides detection (except under tmux/screen, where
/// passthrough is not implemented yet).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WaveformBackend {
    #[default]
    Auto,
    Kitty,
    Sixel,
    Braille,
}

impl WaveformBackend {
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "kitty" => Some(Self::Kitty),
            "sixel" => Some(Self::Sixel),
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

/// The TUI's preferences. Kept as one struct so adding a tab/theme later does not change every
/// call site.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Preference {
    pub waveform: WaveformSettings,
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
            other => notes.push(format!("unknown preference section `[{other}]` ignored")),
        }
    }
    Ok((preference, notes))
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
                    "waveform.backend must be auto | kitty | sixel | braille; using the default"
                        .to_owned(),
                ),
            },
            "refresh_hz" => match value.as_integer() {
                Some(hz) => {
                    let clamped = hz.clamp(10, 240) as u16;
                    if hz != i64::from(clamped) {
                        notes.push(format!(
                            "waveform.refresh_hz clamped from {hz} to {clamped} (10..=240)"
                        ));
                    }
                    waveform.refresh_hz = clamped;
                }
                None => notes.push("waveform.refresh_hz must be an integer".to_owned()),
            },
            "cell" => match parse_cell(value) {
                Some(cell) => waveform.cell = Some(cell),
                None => notes.push(
                    "waveform.cell must be \"WxH\" or [width, height] in pixels; ignoring".to_owned(),
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
            backend = "sixel"
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
        assert_eq!(preference.waveform.backend, WaveformBackend::Sixel);
        assert_eq!(preference.waveform.refresh_hz, 30);
        assert_eq!(preference.waveform.cell, Some((10, 20)));
        assert_eq!(preference.waveform.low_hz, 150);
        assert_eq!(preference.waveform.high_hz, 6000);
        assert_eq!(preference.waveform.colors.low, Color::Rgb(255, 0, 0));
        assert_eq!(
            preference.waveform.colors.mid,
            Color::Rgb(0x12, 0x34, 0x56)
        );
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
        assert_eq!(preference.waveform.refresh_hz, 240);
        assert_eq!(preference.waveform.colors.low, Color::Blue);
        assert!(notes.iter().any(|note| note.contains("backend")));
        assert!(notes.iter().any(|note| note.contains("refresh_hz")));
        assert!(notes.iter().any(|note| note.contains("waveform.low")));
        assert!(notes.iter().any(|note| note.contains("waveform.cell")));
        assert!(notes.iter().any(|note| note.contains("unknown waveform key")));
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
    fn a_missing_file_falls_back_to_defaults() {
        let (preference, notes) =
            load("/tmp/hypermixx-preference-that-does-not-exist.toml").unwrap();
        assert_eq!(preference, Preference::default());
        assert!(notes.is_empty());
    }
}
