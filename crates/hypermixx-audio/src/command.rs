//! Command / response protocol between the CLI and the audio pipeline.

use crate::beatgrid::TrackAnalysis;

/// A command sent to the pipeline's producer thread.
#[derive(Debug)]
pub enum Command {
    /// Decodes `path` into `deck_id`.
    Load {
        deck_id: usize,
        path: String,
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
    Loaded { deck_id: usize, total_frames: u64 },
    State(DeckState),
    States(Vec<DeckState>),
    Ok,
    Error(String),
}
