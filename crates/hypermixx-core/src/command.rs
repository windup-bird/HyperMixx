//! The command / response protocol between the CLI and the audio pipeline.
//!
//! Commands carry no decode or analysis work — the CLI owns file IO and the analyser, then hands
//! the *results* (a loaded source, a compiled [`TrackAnalysis`]) to the producer. Everything here
//! must be executable by the pipeline's producer thread with no further I/O.

use crate::analysis::TrackAnalysis;
use crate::deck::{DeckId, DeckState};
use crate::source::Shared;

/// Which analyser backend to use for a track.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    /// Prefer stratum, fall back to timestretch.
    #[default]
    Auto,
    /// stratum-dsp only.
    Stratum,
    /// timestretch's own analyser only.
    Timestretch,
}

/// A command sent to the pipeline's producer thread.
pub enum Command {
    /// Installs a decoded `source` into `deck_id`, cued at frame 0.
    Load {
        deck_id: DeckId,
        source: Shared,
        /// Optional caller-supplied constant grid (deterministic testing / beat-match).
        analysis: Option<TrackAnalysis>,
    },
    Play {
        deck_id: DeckId,
    },
    Pause {
        deck_id: DeckId,
    },
    /// Sample-accurate seek, in frames.
    Jump {
        deck_id: DeckId,
        target_frame: u64,
    },
    /// Phase-preserving seek of whole beats; negative goes back.
    BeatJump {
        deck_id: DeckId,
        beats: i64,
    },
    /// Sets the tempo rate (0.25 = quarter speed, 1.0 = unity, 4.0 = quadruple).
    SetRate {
        deck_id: DeckId,
        rate: f32,
    },
    /// Switches the time-stretch profile ("tape", "keylock", "wide").
    SetProfile {
        deck_id: DeckId,
        profile: String,
    },
    /// Publishes a compiled analysis (beat grid + key + bpm) to a deck.
    SetAnalysis {
        deck_id: DeckId,
        analysis: TrackAnalysis,
    },
    /// One deck's state. Use [`Command::GetAllStates`] when comparing decks: two separate
    /// `GetState` commands are answered one production block apart.
    GetState {
        deck_id: DeckId,
    },
    /// Every deck's state, sampled inside the same production block.
    GetAllStates,
    Quit,
}

/// The reply to a [`Command`].
#[derive(Debug, PartialEq)]
pub enum CommandResponse {
    /// A deck finished loading: carries the track length.
    Loaded {
        deck_id: DeckId,
        total_frames: u64,
    },
    State(DeckState),
    /// One answer holding every deck, sampled in the same block (skew-free comparison).
    States(Vec<DeckState>),
    Ok,
    Error(String),
}

// `Command` carries a `Shared` (a trait object with no `Debug`), so derive can't cover it. This
// manual impl names each variant and its scalars, which is all tracing needs.
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::Load {
                deck_id, analysis, ..
            } => f
                .debug_struct("Load")
                .field("deck_id", deck_id)
                .field("has_analysis", &analysis.is_some())
                .finish(),
            Command::Play { deck_id } => f.debug_struct("Play").field("deck_id", deck_id).finish(),
            Command::Pause { deck_id } => {
                f.debug_struct("Pause").field("deck_id", deck_id).finish()
            }
            Command::Jump {
                deck_id,
                target_frame,
            } => f
                .debug_struct("Jump")
                .field("deck_id", deck_id)
                .field("target_frame", target_frame)
                .finish(),
            Command::BeatJump { deck_id, beats } => f
                .debug_struct("BeatJump")
                .field("deck_id", deck_id)
                .field("beats", beats)
                .finish(),
            Command::SetRate { deck_id, rate } => f
                .debug_struct("SetRate")
                .field("deck_id", deck_id)
                .field("rate", rate)
                .finish(),
            Command::SetProfile { deck_id, profile } => f
                .debug_struct("SetProfile")
                .field("deck_id", deck_id)
                .field("profile", profile)
                .finish(),
            Command::SetAnalysis { deck_id, .. } => f
                .debug_struct("SetAnalysis")
                .field("deck_id", deck_id)
                .finish(),
            Command::GetState { deck_id } => f
                .debug_struct("GetState")
                .field("deck_id", deck_id)
                .finish(),
            Command::GetAllStates => f.write_str("GetAllStates"),
            Command::Quit => f.write_str("Quit"),
        }
    }
}
