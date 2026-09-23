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
    /// The slip clock: the position playback would be at with no loop, advanced monotonically
    /// while a loop wraps around underneath it. Equals `current_frame` when no loop is engaged.
    pub virtual_frame: u64,
    /// The active loop as `(in, out)` source frames, half-open, or `None`.
    pub loop_range: Option<(u64, u64)>,
    /// A manual loop-in point waiting for its out press, as the quantized `in` frame.
    pub loop_in_armed: Option<u64>,
    /// The stable tempo: what the DJ's fader, `sync tempo` and a lock's group recompute write.
    /// This is the rate that survives `sync unlock`.
    pub tempo: f32,
    /// The temporary rate stacked on top of `tempo`: phase correction plus nudge. `0.0` when
    /// nothing is correcting.
    pub nudgerate: f32,
    /// `tempo + nudgerate` — the rate the deck actually plays at.
    pub playing_rate: f32,
    /// This deck's tempo follows the group's shared BPM.
    pub lock: bool,
    /// The active phase correction, as a label (`"instant"`/`"linear"`/`"pid"`), or `None`.
    pub align: Option<String>,
    /// The current nudge bend, a rate (also part of `nudgerate`), `0.0` when idle.
    pub nudge: f32,
    /// The deck this one tracks, or `None` when it leads or stands alone.
    pub sync_leader: Option<DeckId>,
    /// The pair's sync mode: `"free"`, `"tempolock"` or `"phaselock"`.
    pub sync_mode: String,
    /// The shared BPM the group's tempos derive from (`0.0` while free).
    pub group_bpm: f32,
}
