//! Output destinations: what a mixer drives, and how a front-end reports it.
//!
//! An output is a *logical* destination (`name`, `role`) that is bound to a physical device by a
//! pattern. The binding can fail or be impossible — the device is not plugged in, or it does not
//! accept the engine's rate — and that is a state rather than an error: the engine keeps running,
//! the destination is listed as unavailable, and `output refresh` retries the binding. A DJ who
//! unplugs the headphones mid-set should not lose the master.

use serde::Deserialize;

/// Whether an output carries the mix or the cue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputRole {
    /// The summed mix — what the room hears.
    #[default]
    Master,
    /// The cue bus (the headphone destination).
    Cue,
}

impl OutputRole {
    pub fn name(self) -> &'static str {
        match self {
            OutputRole::Master => "master",
            OutputRole::Cue => "cue",
        }
    }
}

/// Why an output is (or is not) audible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputState {
    /// A stream is open and the device is being fed.
    Live,
    /// The host has no output device at all (a headless machine or a container). The engine still
    /// runs and the destination accepts and discards audio.
    Sink,
    /// A device was named but not found. Plug it in, then `output refresh`.
    Missing,
    /// The device exists but does not accept the rate this output is allowed to use. Warned about
    /// and left unrouted rather than silently resampled: a rate mismatch that is papered over is a
    /// track playing at the wrong speed.
    Disabled,
}

impl OutputState {
    /// The word a front-end shows. Kept here so every front-end agrees on it.
    pub fn label(self) -> &'static str {
        match self {
            OutputState::Live => "live",
            OutputState::Sink => "sink",
            OutputState::Missing => "missing",
            OutputState::Disabled => "disabled",
        }
    }

    /// Whether audio reaches the device on this state.
    pub fn is_live(self) -> bool {
        self == OutputState::Live
    }
}

/// One destination's binding, as a front-end reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputStatus {
    pub name: String,
    pub role: OutputRole,
    pub state: OutputState,
    /// The resolved cpal device name, when one was found.
    pub device: Option<String>,
    /// `(left, right)` device channel indices actually in use.
    pub channels: (u16, u16),
    pub gain: f32,
    /// Blocks handed to this output, and blocks whose tail the ring could not take.
    pub writes: u64,
    pub overruns: u64,
}

/// A command addressed to the mixer's output set rather than to a signal path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputOp {
    /// Report every destination's binding.
    List,
    /// Retry every unavailable destination, and report the result. This is what a user does after
    /// plugging a device in: `output refresh`, then `output list` shows it live.
    Refresh,
}
