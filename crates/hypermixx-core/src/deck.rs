//! Deck identity and the state snapshot the producer reports.

use crate::command::KeylockMode;
use crate::stem::StemStatus;

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
    /// The grid BPM **at the audible position** (`bpm_at_frame`), 0.0 without a grid. Multiplying
    /// this by [`DeckState::tempo`] gives the front-end's "current BPM": the track's local tempo as
    /// it is actually playing. Deliberately not derived from `playing_rate` — a nudge or a phase
    /// correction is a transient, not a tempo.
    pub bpm_at_frame: f32,
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
    /// The tempo fader position derived from `tempo` and `tempo_range`
    /// (`clamp((tempo - 1) / range, -1, 1)`).
    pub tempo_fader: f32,
    /// The fader's full-deflection range (`0.1` = ±10%).
    pub tempo_range: f32,
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
    /// The cue point in source frames. Defaults to the loaded origin (0); `cue set` moves it.
    pub cue_frame: u64,
    /// Whether a hand is on the platter (`vinyl` touch). While it is, the deck is paused and a wheel
    /// turn moves the playhead instead of bending the tempo.
    pub vinyl: bool,
    /// The active keylock profile.
    pub keylock: KeylockMode,
    /// Pitch shift in semitones. Always 0 today — the streaming engine has no pitch axis.
    pub key_shift: i32,
    /// The per-stem state of this deck: whether the audio is separated yet, and the level, mute and
    /// solo of each stem. Defaults describe a track with no stems, so a front-end can render it
    /// unconditionally.
    pub stems: StemStatus,
}
