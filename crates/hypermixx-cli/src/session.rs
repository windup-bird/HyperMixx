//! What a front-end session is handed: the engine, plus everything the *user* chose.
//!
//! A struct rather than a list of arguments because both front-ends take the same set (the REPL
//! simply has no use for the MIDI fields — it attaches its input before the session starts), and
//! because a new setting should be one field here rather than a tenth parameter in two places.

use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender};

use hypermixx_audio::Command;
use hypermixx_core::LiveConfig;
use hypermixx_library::analyser::Backends;
use hypermixx_library::LibraryDb;
use hypermixx_midi::map::DeckRoutes;
use hypermixx_midi::{RouterCommand, RouterState};

use crate::notices::NoticeTx;
use crate::response::LogLine;

/// Everything a session needs that is not the engine itself.
pub struct Session {
    pub command_tx: Sender<Command>,
    /// Which backend runs which analysis path (`[preference.analysis]` in the config).
    pub backends: Backends,
    /// How many decks the engine owns (asked of it, not assumed).
    pub decks: usize,
    pub notice_tx: NoticeTx,
    pub notice_rx: Receiver<LogLine>,
    /// The one config file this session was started with: what `config show` and `mixer reload`
    /// fall back to.
    pub topology: Option<String>,
    /// This machine's settings, already validated.
    pub live: LiveConfig,
    /// Where the live settings came from, for `live reload`.
    pub live_source: Option<String>,
    /// The configured controller port, for the front-end that can pick another at runtime (`F9`).
    pub midi_port: Option<String>,
    pub midi_map: String,
    /// The live router's channels, when `main` attached an input before the session started (the
    /// REPL path). The TUI connects later and installs them on its own dispatcher.
    pub midi_control: Option<Sender<RouterCommand>>,
    pub midi_state: Option<Arc<Mutex<RouterState>>>,
    /// Controller face → physical deck table, built from `[[multideck.deck]] source`. Both
    /// front-ends need it to compile the normal and mirror unit maps.
    pub deck_routes: DeckRoutes,
    /// One entry per `[[controller]]` instance, each with its own port, map and face routes.
    pub controller_sessions: Vec<crate::profile::ControllerSession>,
    /// Analyse a browser-loaded track when it has neither an imported grid nor a cached analysis.
    pub auto_analyse: bool,
    /// The library cache, opened once by `main` and shared by both front-ends.
    pub library: Option<Arc<LibraryDb>>,
}
