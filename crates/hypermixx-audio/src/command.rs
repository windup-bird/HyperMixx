//! Command / response protocol between the CLI and the audio pipeline.

/// A command sent to the pipeline's producer thread.
#[derive(Debug)]
pub enum Command {
    /// Decodes `path` into `deck_id`. `bpm` overrides the tempo used to build the beat grid.
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
}

/// The reply to a [`Command`].
#[derive(Debug, PartialEq)]
pub enum CommandResponse {
    Loaded {
        deck_id: usize,
        total_frames: u64,
        bpm: f32,
    },
    State(DeckState),
    States(Vec<DeckState>),
    Ok,
    Error(String),
}
