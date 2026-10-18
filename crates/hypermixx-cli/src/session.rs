//! What a front-end session is handed: the engine, plus everything the *user* chose.
//!
//! A struct rather than a list of arguments because both front-ends take the same set (the REPL
//! simply has no use for the MIDI fields — it attaches its input before the session starts), and
//! because a new setting should be one field here rather than a tenth parameter in two places.

use crossbeam_channel::{Receiver, Sender};

use hypermixx_audio::Command;
use hypermixx_core::{Backend, LiveConfig};

use crate::notices::NoticeTx;
use crate::response::LogLine;

/// Everything a session needs that is not the engine itself.
pub struct Session {
    pub command_tx: Sender<Command>,
    pub backend: Backend,
    /// How many decks the engine owns (asked of it, not assumed).
    pub decks: usize,
    pub notice_tx: NoticeTx,
    pub notice_rx: Receiver<LogLine>,
    /// `--config`'s path: what `mixer reload` falls back to.
    pub topology: Option<String>,
    /// The profile this session was started with, for `profile show`. `None` = folder defaults.
    pub profile: Option<String>,
    /// This machine's settings, already validated.
    pub live: LiveConfig,
    /// Where the live settings came from, for `live reload`.
    pub live_source: Option<String>,
    /// `--midi` / `--midi-map`, for the front-end that can pick them at runtime.
    pub midi_port: Option<String>,
    pub midi_map: String,
}
