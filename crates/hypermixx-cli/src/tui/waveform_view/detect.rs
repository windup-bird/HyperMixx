//! Backend and cell-size detection.
//!
//! Environment hints are deliberately used before any terminal query: the TUI is already inside
//! raw mode when this runs, and a probe that races the event loop is worse than a conservative
//! fallback. Every image backend still has braille underneath it, so a wrong guess costs quality,
//! not correctness. A future round can add a DA1/Kitty query for terminals whose environment is
//! silent.

use crate::preference::WaveformBackend;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Braille,
    Kitty,
    Sixel,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Braille => "braille",
            Self::Kitty => "kitty",
            Self::Sixel => "sixel",
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

/// Resolves the requested preference against the terminal's environment.
pub fn resolve(requested: WaveformBackend) -> (Backend, Option<String>) {
    let tmux = env("TMUX").is_some() || env("STY").is_some();
    let dumb = env("TERM").is_some_and(|term| term == "dumb");
    let backend = match requested {
        WaveformBackend::Braille => Backend::Braille,
        WaveformBackend::Kitty => Backend::Kitty,
        WaveformBackend::Sixel => Backend::Sixel,
        WaveformBackend::Auto => {
            if kitty_hint() {
                Backend::Kitty
            } else if sixel_hint() {
                Backend::Sixel
            } else {
                Backend::Braille
            }
        }
    };
    if backend == Backend::Braille {
        return (backend, None);
    }
    // Kitty and Ghostty do not implement sixel. If sixel was forced there, use the graphics
    // protocol the terminal actually has rather than drawing an image it will ignore.
    if backend == Backend::Sixel && kitty_only() {
        return (
            Backend::Kitty,
            Some("this terminal does not implement sixel; using kitty graphics".to_owned()),
        );
    }
    if tmux {
        return (
            Backend::Braille,
            Some("tmux/screen passthrough is not enabled; using braille".to_owned()),
        );
    }
    if dumb {
        return (
            Backend::Braille,
            Some("TERM=dumb; using braille".to_owned()),
        );
    }
    (backend, None)
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

/// Terminals that implement Kitty graphics but not sixel. WezTerm is deliberately excluded: it
/// supports sixel too, so an explicit sixel request there is honoured.
fn kitty_only() -> bool {
    env("KITTY_WINDOW_ID").is_some()
        || env("GHOSTTY_RESOURCES_DIR").is_some()
        || env("KONSOLE_VERSION").is_some()
        || env("TERM").is_some_and(|term| term.contains("kitty"))
        || env("TERM_PROGRAM").is_some_and(|program| {
            matches!(program.as_str(), "kitty" | "ghostty")
        })
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

fn sixel_hint() -> bool {
    // Windows Terminal 1.22+ carries sixel through ConPTY. WezTerm also supports it, but Kitty is
    // preferred above so it only reaches here when the Kitty hint failed.
    env("WT_SESSION").is_some()
        || env("TERM").is_some_and(|term| {
            term.contains("sixel") || term.contains("foot") || term.contains("mlterm")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_choices_are_honoured() {
        assert_eq!(resolve(WaveformBackend::Braille).0, Backend::Braille);
        // These depend only on the request unless tmux/dumb is set; the test environment is
        // ordinarily neither.
        if env("TMUX").is_none() && env("STY").is_none() && env("TERM").as_deref() != Some("dumb") {
            assert_eq!(resolve(WaveformBackend::Kitty).0, Backend::Kitty);
            let (backend, note) = resolve(WaveformBackend::Sixel);
            if kitty_only() {
                // Kitty/Ghostty have graphics but no sixel; an explicit sixel request must not
                // leave a blank deck.
                assert_eq!(backend, Backend::Kitty);
                assert!(note.is_some_and(|note| note.contains("sixel")));
            } else {
                assert_eq!(backend, Backend::Sixel);
            }
        }
    }

    #[test]
    fn the_cell_size_is_never_zero() {
        let cell = cell_px();
        assert!(cell.width > 0 && cell.height > 0);
    }
}
