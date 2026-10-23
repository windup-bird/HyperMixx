//! Musical key: pitch class + mode, with traditional and Camelot renderings.

use serde::{Deserialize, Serialize};

/// Major or minor tonality.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyMode {
    Major,
    Minor,
}

/// Notation style a key is rendered in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyFormat {
    /// `"C"`, `"Am"`, `"F#"`.
    #[default]
    Traditional,
    /// Camelot wheel, e.g. `"8B"`, `"8A"`.
    Camelot,
}

/// A key detection result: pitch class, mode and confidence.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Key {
    /// 0 = C, 1 = C#, ..., 11 = B.
    pub pc: u8,
    pub mode: KeyMode,
    /// 0.0–1.0, from the analyser.
    pub confidence: f32,
}

const PITCH_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

impl Key {
    /// Standard name: `"C"`, `"Am"`, `"F#"`.
    pub fn traditional(&self) -> String {
        let root = PITCH_NAMES[self.pc as usize % 12];
        match self.mode {
            KeyMode::Major => root.to_owned(),
            KeyMode::Minor => format!("{root}m"),
        }
    }

    /// Camelot wheel label: majors land on `B`, minors on `A`.
    ///
    /// The number walks the circle of fifths from C = 8, so each +7 semitones is +1. A minor
    /// shares its relative major's number (A minor = C major = 8 → `"8A"`).
    pub fn camelot(&self) -> String {
        let anchor_pc = match self.mode {
            KeyMode::Major => self.pc as usize % 12,
            // The relative major sits three semitones above the minor tonic.
            KeyMode::Minor => (self.pc as usize + 3) % 12,
        };
        let number = (anchor_pc * 7 + 8) % 12;
        let number = if number == 0 { 12 } else { number };
        let letter = match self.mode {
            KeyMode::Major => 'B',
            KeyMode::Minor => 'A',
        };
        format!("{number}{letter}")
    }

    /// Renders the key in the requested notation.
    pub fn format(&self, fmt: KeyFormat) -> String {
        match fmt {
            KeyFormat::Traditional => self.traditional(),
            KeyFormat::Camelot => self.camelot(),
        }
    }

    /// Both notations at once, for UIs that show `"Am (8A)"`.
    pub fn both(&self) -> (String, String) {
        (self.traditional(), self.camelot())
    }

    /// Parses a key as written in a tag or a DJ library: `"Am"`, `"A minor"`, `"F#"`,
    /// `"8A"`, `"08B"`. Unknown text yields `None` rather than a guessed key.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        if let Some(key) = parse_camelot(text) {
            return Some(key);
        }
        parse_traditional(text)
    }
}

fn parse_camelot(text: &str) -> Option<Key> {
    let bytes = text.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let letter = bytes[bytes.len() - 1].to_ascii_uppercase();
    if letter != b'A' && letter != b'B' {
        return None;
    }
    let number: u8 = text[..text.len() - 1].trim().parse().ok()?;
    if !(1..=12).contains(&number) {
        return None;
    }
    // Camelot majors walk the circle of fifths from C = 8B; minors share the relative major's
    // number, three semitones below the major tonic.
    let major_pc = (i32::from(number) - 8) * 7;
    let major_pc = major_pc.rem_euclid(12) as u8;
    let (pc, mode) = if letter == b'B' {
        (major_pc, KeyMode::Major)
    } else {
        (
            (i32::from(major_pc) - 3).rem_euclid(12) as u8,
            KeyMode::Minor,
        )
    };
    Some(Key {
        pc,
        mode,
        confidence: 1.0,
    })
}

fn parse_traditional(text: &str) -> Option<Key> {
    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    let lower = compact.to_ascii_lowercase();
    let (root, mode) = if let Some(root) = lower.strip_suffix("minor") {
        (root, KeyMode::Minor)
    } else if let Some(root) = lower.strip_suffix("min") {
        (root, KeyMode::Minor)
    } else if let Some(root) = lower.strip_suffix('m') {
        (root, KeyMode::Minor)
    } else if let Some(root) = lower.strip_suffix("major") {
        (root, KeyMode::Major)
    } else if let Some(root) = lower.strip_suffix("maj") {
        (root, KeyMode::Major)
    } else {
        (lower.as_str(), KeyMode::Major)
    };
    let pc = match root {
        "c" => 0,
        "c#" | "db" => 1,
        "d" => 2,
        "d#" | "eb" => 3,
        "e" => 4,
        "f" => 5,
        "f#" | "gb" => 6,
        "g" => 7,
        "g#" | "ab" => 8,
        "a" => 9,
        "a#" | "bb" => 10,
        "b" | "cb" => 11,
        _ => return None,
    };
    Some(Key {
        pc,
        mode,
        confidence: 1.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(pc: u8, mode: KeyMode) -> Key {
        Key {
            pc,
            mode,
            confidence: 1.0,
        }
    }

    #[test]
    fn traditional_names() {
        assert_eq!(key(0, KeyMode::Major).traditional(), "C");
        assert_eq!(key(9, KeyMode::Minor).traditional(), "Am");
        assert_eq!(key(6, KeyMode::Major).traditional(), "F#");
    }

    #[test]
    fn camelot_matches_the_wheel() {
        // C = 8B, Am = 8A, G = 9B, Em = 9A, F# = 2B, D major = 10B.
        assert_eq!(key(0, KeyMode::Major).camelot(), "8B");
        assert_eq!(key(9, KeyMode::Minor).camelot(), "8A");
        assert_eq!(key(7, KeyMode::Major).camelot(), "9B");
        assert_eq!(key(4, KeyMode::Minor).camelot(), "9A");
        assert_eq!(key(6, KeyMode::Major).camelot(), "2B");
        assert_eq!(key(2, KeyMode::Major).camelot(), "10B");
    }

    #[test]
    fn format_and_both_agree_with_direct_calls() {
        let a = key(9, KeyMode::Minor);
        assert_eq!(a.format(KeyFormat::Camelot), a.camelot());
        assert_eq!(a.both(), ("Am".into(), "8A".into()));
    }

    #[test]
    fn parses_traditional_and_camelot_text() {
        assert_eq!(Key::parse("Am"), Some(key(9, KeyMode::Minor)));
        assert_eq!(Key::parse("A minor"), Some(key(9, KeyMode::Minor)));
        assert_eq!(Key::parse("F#"), Some(key(6, KeyMode::Major)));
        assert_eq!(Key::parse("Bbm"), Some(key(10, KeyMode::Minor)));
        assert_eq!(Key::parse("8A"), Some(key(9, KeyMode::Minor)));
        assert_eq!(Key::parse("08B"), Some(key(0, KeyMode::Major)));
        assert_eq!(Key::parse("12A"), Some(key(1, KeyMode::Minor)));
        assert_eq!(Key::parse("not-a-key"), None);
    }
}
