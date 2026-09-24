//! MIDI input mapping for Hypermixx.
//!
//! The crate is deliberately independent of the engine, the audio device and the terminal: it
//! depends on `hypermixx-core` for the [`Command`](hypermixx_core::Command) protocol and on
//! `midir` for port I/O, nothing else. That keeps its logic pure and testable without hardware:
//!
//! ```text
//! bytes ──msg::Decoder──► Event ──translate::translate──► Vec<Command> ──MergeBuffer──► command_tx
//!                                  ▲
//!                          map::Map (TOML) + TranslateState
//! ```
//!
//! - [`msg`] parses the byte stream (NoteOn/Off, CC, PitchBend; drops clock/sysex).
//! - [`map`] is the TOML mapping file plus the action registry the guide generates from.
//! - [`translate`] is the pure event → command function with soft-takeover and relative encoders.
//! - [`ports`] enumerates and opens `midir` inputs, forwarding [`msg::Event`]s.

pub mod guide;
pub mod map;
pub mod msg;
pub mod ports;
pub mod translate;

pub use guide::{describe, event_label, BindStatus, Candidate, Guide, GuideStep};
pub use map::{
    manifest, Action, ActionSpec, BindingSpec, Map, MapError, Meta, Target, ACTIONS,
};
pub use msg::{Decoder, Event};
pub use ports::{list_ports, Input, PortInfo, Received};
pub use translate::{translate, MergeBuffer, MergeKey, TranslateState};
