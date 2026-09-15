//! Deck identity and the state snapshot the producer reports.

/// Identifies one of the pipeline's decks. Kept as a plain `u8` so commands can carry it by value
/// and the producer can index its fixed deck array directly.
pub type DeckId = u8;

/// One deck's transport, as observed by the producer thread.
#[derive(Clone, Debug, PartialEq)]
pub struct DeckState {
    pub deck_id: DeckId,
    pub current_frame: u64,
    pub playing: bool,
    /// 0 while the deck holds no audio.
    pub total_frames: u64,
    /// 0.0 while the deck has no beat grid.
    pub bpm: f32,
    /// Detected key name (e.g. `"Am"`), or `None` without analysis.
    pub key: Option<String>,
}
