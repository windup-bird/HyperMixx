//! Command / response protocol between the CLI and the audio pipeline.

use crate::beatgrid::TrackAnalysis;

/// A command sent to the pipeline's producer thread.
#[derive(Debug)]
pub enum Command {
    /// Decodes `path` into `deck_id`. When `bpm` is `Some`, a constant-tempo grid is built
    /// immediately and analysis is skipped (a deterministic grid for testing/beat-matching);
    /// when `None`, an async analysis job is expected to fill the grid via [`Command::SetAnalysis`].
    Load {
        deck_id: usize,
        path: String,
        bpm: Option<f32>,
    },
    Play {
        deck_id: usize,
    },
    Pause {
        deck_id: usize,
    },
    /// Sample-accurate seek, in frames.
    Jump {
        deck_id: usize,
        target_frame: u64,
    },
    /// Phase-preserving seek of whole beats; negative goes back.
    BeatJump {
        deck_id: usize,
        beats: i64,
    },
    /// Sets the tempo rate (0.25 = quarter speed, 1.0 = unity, 4.0 = quadruple).
    SetRate {
        deck_id: usize,
        rate: f32,
    },
    /// Switches the time-stretch profile ("tape", "keylock", "wide").
    SetProfile {
        deck_id: usize,
        profile: String,
    },
    /// Publishes analysis (beat grid + key + bpm) to a deck.
    SetAnalysis {
        deck_id: usize,
        analysis: TrackAnalysis,
    },
    /// One deck's state. Use [`Command::GetAllStates`] when comparing decks: two separate
    /// `GetState` commands are answered one production block apart.
    GetState {
        deck_id: usize,
    },
    /// Every deck's state, sampled inside the same production block.
    GetAllStates,
    Quit,
}

/// One deck's transport, as observed by the producer thread.
#[derive(Debug, Clone, PartialEq)]
pub struct DeckState {
    pub deck_id: usize,
    pub current_frame: u64,
    pub playing: bool,
    /// 0 while the deck holds no audio.
    pub total_frames: u64,
    /// 0.0 while the deck has no beat grid.
    pub bpm: f32,
    /// Detected key name (e.g. `"Am"`), or `None` without analysis.
    pub key: Option<String>,
}

/// The reply to a [`Command`].
#[derive(Debug, PartialEq)]
pub enum CommandResponse {
    /// `analyzed` is true when the deck already carries a grid (constant-bpm load), so the caller
    /// should not launch an async analysis that would overwrite it.
    Loaded {
        deck_id: usize,
        total_frames: u64,
        analyzed: bool,
    },
    State(DeckState),
    States(Vec<DeckState>),
    Ok,
    Error(String),
}
