//! MIDI input mapping for Hypermixx.
//!
//! The crate is deliberately independent of the engine, the audio device and the terminal: it
//! depends on `hypermixx-core` for the [`Command`](hypermixx_core::Command) protocol and on
//! `midir` for port I/O, nothing else. That keeps its logic pure and testable without hardware:
//!
//! ```text
//! bytes ──msg::Decoder──► Event ──router::Router──► Vec<Command> ──MergeBuffer──► command_tx
//!                                  ▲
//!                          map::Map (units + presets) + Router state
//! ```
//!
//! - [`msg`] parses the byte stream (NoteOn/Off, CC, PitchBend; drops clock/sysex).
//! - [`map`] is the TOML mapping file: units, slots, selectors and mode tables.
//! - [`preset`] holds built-in slot layouts and default mode tables; user files may replace them.
//! - [`router`] owns the active mode per unit and routes each endpoint to its slot.
//! - [`translate`] applies one binding (soft-takeover, relative encoders, press/release edges).
//! - [`ports`] enumerates and opens `midir` inputs, forwarding [`msg::Event`]s.

pub mod guide;
pub mod map;
pub mod msg;
pub mod ports;
pub mod preset;
pub mod router;
pub mod translate;

pub use guide::{Candidate, Column, Entered, Guide, GuideStep, Nav, Row, View};
pub use map::{
    manifest, Action, ActionSpec, BindingSpec, Encoding, FxParamRef, Map, MapError, Meta, SlotKind,
    SlotRole, Target, ACTIONS,
};
pub use msg::{Decoder, Event};
pub use ports::{list_ports, Input, PortInfo, Received};
pub use router::{Feed, ModeEvent, Router, RouterCommand, RouterState, UnitModeState};
pub use translate::{apply, default_norm, BindState, MergeBuffer, MergeKey};
