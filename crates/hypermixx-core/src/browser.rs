//! Browser actions a controller can drive. The engine never sees these; the front-end consumes
//! them. Kept in `core` so MIDI does not need to depend on the library or the TUI.

use crate::DeckId;

/// One navigation/load action in the library browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserAction {
    Up,
    Down,
    Open,
    Exit,
    /// Load the highlighted track, optionally into a specific deck.
    Load {
        deck: Option<DeckId>,
    },
    Refresh,
}
