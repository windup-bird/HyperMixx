//! Command / response protocol between the CLI and the audio pipeline.

/// A command sent to the pipeline's producer thread.
#[derive(Debug)]
pub enum Command {
    Load { path: String },
    Play,
    Pause,
    Jump { target_frame: u64 },
    GetState,
    Quit,
}

/// The reply to a [`Command`].
#[derive(Debug, PartialEq)]
pub enum CommandResponse {
    Loaded { total_frames: u64 },
    State { current_frame: u64, playing: bool },
    Ok,
    Error(String),
}
