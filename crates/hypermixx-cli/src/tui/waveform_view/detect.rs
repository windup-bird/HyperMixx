//! Backend and cell-size detection.
//!
//! Environment hints are deliberately used before any terminal query: the TUI is already inside
//! raw mode when this runs, and a probe that races the event loop is worse than a conservative
//! fallback. Every image backend still has braille underneath it, so a wrong guess costs quality,
//! not correctness. A future round can add a DA1/Kitty query for terminals whose environment is
//! silent.
//!
//! One hint is a *transport* fact rather than an identity fact: on Windows every console program is
//! hosted through ConPTY, whose VT parser discards SOS/PM/APC strings outright — and kitty graphics
//! travels in an APC string. The terminal may well be WezTerm (so `kitty_hint()` is true) and still
//! never see a byte of it, which shows up as a blank waveform rather than a slow one. `docs/WAVEFORM.md`
//! has the measurement; the guard below is the cheap half of the fix (the probe is the other).

use crate::preference::WaveformBackend;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Braille,
    Kitty,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Braille => "braille",
            Self::Kitty => "kitty",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellPx {
    pub width: u16,
    pub height: u16,
}

impl Default for CellPx {
    fn default() -> Self {
        Self {
            width: 8,
            height: 16,
        }
    }
}

/// What the process can learn about the terminal from its environment. Split out from
/// [`resolve`] so the decision table is testable without a real terminal (or a real Windows).
#[derive(Clone, Copy, Debug, Default)]
pub struct Hints {
    pub tmux: bool,
    pub dumb: bool,
    pub kitty: bool,
    /// The output crosses ConPTY: true for a native Windows console session.
    pub conpty: bool,
}

impl Hints {
    /// The hints this process actually has.
    pub fn from_env() -> Self {
        Self {
            tmux: env("TMUX").is_some() || env("STY").is_some(),
            dumb: env("TERM").is_some_and(|term| term == "dumb"),
            kitty: kitty_hint(),
            conpty: cfg!(windows),
        }
    }
}

/// Resolves `requested` against `hints`: the image backend to use and, when the choice had to be
/// overruled, the one-line reason to put in the log.
pub fn decide(requested: WaveformBackend, hints: Hints) -> (Backend, Option<String>) {
    let backend = match requested {
        WaveformBackend::Braille => Backend::Braille,
        WaveformBackend::Kitty => Backend::Kitty,
        WaveformBackend::Auto => {
            if hints.kitty {
                Backend::Kitty
            } else {
                Backend::Braille
            }
        }
    };
    if backend == Backend::Braille {
        return (backend, None);
    }
    if hints.tmux {
        return (
            Backend::Braille,
            Some("tmux/screen passthrough is not enabled; using braille".to_owned()),
        );
    }
    if hints.dumb {
        return (
            Backend::Braille,
            Some("TERM=dumb; using braille".to_owned()),
        );
    }
    if hints.conpty {
        // Not the terminal's fault and not our escape sequences' fault: the bytes are consumed
        // before they reach it. `wezterm ssh` to a Linux/macOS host is the way to get images back.
        return (
            Backend::Braille,
            Some(
                "Windows consoles are hosted through ConPTY, which drops the APC strings kitty \
                 graphics travels in; using braille (see docs/WAVEFORM.md)"
                    .to_owned(),
            ),
        );
    }
    (backend, None)
}

/// Resolves the requested preference against this process's environment.
pub fn resolve(requested: WaveformBackend) -> (Backend, Option<String>) {
    decide(requested, Hints::from_env())
}

/// The terminal's cell size in pixels, as reported by crossterm's window-size query. Falls back to
/// the common 8×16 cell when the terminal reports a character grid only (many do).
pub fn cell_px() -> CellPx {
    if let Ok(size) = crossterm::terminal::window_size() {
        if size.columns > 0 && size.rows > 0 && size.width > 0 && size.height > 0 {
            let width = (size.width / size.columns).clamp(1, 64);
            let height = (size.height / size.rows).clamp(1, 64);
            // A 1×1 "cell" is a terminal reporting a nonsense pixel grid, not a real font.
            if width >= 4 && height >= 6 {
                return CellPx { width, height };
            }
        }
    }
    CellPx::default()
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn kitty_hint() -> bool {
    env("KITTY_WINDOW_ID").is_some()
        || env("GHOSTTY_RESOURCES_DIR").is_some()
        || env("WEZTERM_EXECUTABLE").is_some()
        || env("TERM").is_some_and(|term| term.contains("kitty"))
        || env("TERM_PROGRAM").is_some_and(|program| {
            matches!(
                program.as_str(),
                "kitty" | "ghostty" | "WezTerm" | "wezterm"
            )
        })
        || env("KONSOLE_VERSION").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_choices_are_honoured() {
        assert_eq!(resolve(WaveformBackend::Braille).0, Backend::Braille);
        // This depends only on the request unless tmux/dumb is set; the test environment is
        // ordinarily neither.
        if env("TMUX").is_none() && env("STY").is_none() && env("TERM").as_deref() != Some("dumb") {
            assert_eq!(resolve(WaveformBackend::Kitty).0, Backend::Kitty);
        }
    }

    #[test]
    fn a_conpty_session_falls_back_to_braille() {
        let conpty = Hints {
            kitty: true,
            conpty: true,
            ..Hints::default()
        };
        // `auto` would otherwise pick kitty because the terminal *is* WezTerm.
        let (backend, note) = decide(WaveformBackend::Auto, conpty);
        assert_eq!(backend, Backend::Braille);
        assert!(
            note.as_deref().is_some_and(|note| note.contains("ConPTY")),
            "{note:?}"
        );
        // An explicit request is overruled too, because the bytes cannot arrive either way —
        // but it says why instead of silently ignoring the file.
        let (backend, note) = decide(WaveformBackend::Kitty, conpty);
        assert_eq!(backend, Backend::Braille);
        assert!(note.is_some());

        // A terminal that is not behind ConPTY keeps its images.
        let native = Hints {
            kitty: true,
            ..Hints::default()
        };
        assert_eq!(
            decide(WaveformBackend::Auto, native),
            (Backend::Kitty, None)
        );
        // And brailles stays braille, with nothing to report.
        assert_eq!(
            decide(WaveformBackend::Braille, conpty),
            (Backend::Braille, None)
        );
    }

    #[test]
    fn a_mux_or_a_dumb_terminal_wins_over_the_hint() {
        let muxed = Hints {
            kitty: true,
            tmux: true,
            ..Hints::default()
        };
        assert_eq!(decide(WaveformBackend::Auto, muxed).0, Backend::Braille);
        let dumb = Hints {
            kitty: true,
            dumb: true,
            ..Hints::default()
        };
        assert_eq!(decide(WaveformBackend::Auto, dumb).0, Backend::Braille);
    }

    #[test]
    fn the_cell_size_is_never_zero() {
        let cell = cell_px();
        assert!(cell.width > 0 && cell.height > 0);
    }
}
