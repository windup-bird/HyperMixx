//! The command / response protocol between the CLI and the audio pipeline.
//!
//! Commands carry no decode or analysis work — the CLI owns file IO and the analyser, then hands
//! the *results* (a loaded source, a compiled [`TrackAnalysis`]) to the producer. Everything here
//! must be executable by the pipeline's producer thread with no further I/O.

use crate::analysis::TrackAnalysis;
use crate::deck::{DeckId, DeckState};
use crate::source::Shared;

/// Which FX chain a command addresses. `Deck` is the per-deck insert chain the mixer owns; a deck
/// itself knows nothing about FX, which keeps the transport logic free of audio effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxChainId {
    /// The chain belonging to one deck (a mono-style insert on that deck's signal).
    Deck(DeckId),
    /// The summed output chain, before the master limiter.
    Master,
}

impl FxChainId {
    /// A stable label for logs and errors.
    pub fn label(&self) -> String {
        match self {
            FxChainId::Deck(deck_id) => format!("deck{deck_id}"),
            FxChainId::Master => "master".into(),
        }
    }
}

/// Names one slot inside a chain: `(chain, index)`. Indices are assigned by [`Command::AddFx`]
/// and shift down when an earlier slot is removed, which is acceptable for a live-coded front-end
/// and avoids the lifetime bookkeeping stable ids would cost on the audio thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FxSlotRef {
    pub chain: FxChainId,
    pub index: usize,
}

/// One FX slot as reported back to the caller.
#[derive(Clone, Debug, PartialEq)]
pub struct FxSlotStatus {
    pub index: usize,
    pub kind: String,
    pub enabled: bool,
    /// `(name, value)` pairs, in the order the effect declares them.
    pub params: Vec<(String, f32)>,
}

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

    // ---- FX ---------------------------------------------------------------
    // Everything below is applied by the producer thread at a block boundary; `Box<dyn Fx>` is
    // moved in/out of a chain there, so the audio path never takes a lock for it.
    /// Appends a new effect instance to the end of a chain ("eq", "filter", "gain", "limiter", ...).
    AddFx {
        chain: FxChainId,
        kind: String,
    },
    /// Drops the effect in `index`, shifting later slots down.
    RemoveFx {
        chain: FxChainId,
        index: usize,
    },
    /// Bypasses or engages a slot. Enabling resets the instance so it can't carry stale state.
    SetFxEnabled {
        slot: FxSlotRef,
        enabled: bool,
    },
    /// Sets one named parameter. The value lands on the audio thread through a smoothing filter,
    /// so sweeping a cutoff never clicks.
    SetFxParam {
        slot: FxSlotRef,
        name: String,
        value: f32,
    },
    /// Fires the effect's one-shot hook (re-trigger an envelope, restart a sweep).
    FxTrigger {
        slot: FxSlotRef,
    },
    /// Held pad: engages the slot for as long as the pad is down, restoring the previous
    /// engagement on [`Command::PadRelease`] (momentary FX such as a beat gate or a roll).
    PadPress {
        slot: FxSlotRef,
    },
    PadRelease {
        slot: FxSlotRef,
    },
    /// Reports every slot in a chain with its parameters, so a UI can build its controls.
    ListFx {
        chain: FxChainId,
    },
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
    /// A slot was appended; carries its assigned index and canonical kind name.
    FxAdded {
        chain: FxChainId,
        index: usize,
        kind: String,
    },
    /// The answer to [`Command::ListFx`].
    FxListed {
        chain: FxChainId,
        slots: Vec<FxSlotStatus>,
    },
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
            Command::AddFx { chain, kind } => {
                f.debug_struct("AddFx").field("chain", chain).field("kind", kind).finish()
            }
            Command::RemoveFx { chain, index } => f
                .debug_struct("RemoveFx")
                .field("chain", chain)
                .field("index", index)
                .finish(),
            Command::SetFxEnabled { slot, enabled } => f
                .debug_struct("SetFxEnabled")
                .field("slot", slot)
                .field("enabled", enabled)
                .finish(),
            Command::SetFxParam { slot, name, value } => f
                .debug_struct("SetFxParam")
                .field("slot", slot)
                .field("name", name)
                .field("value", value)
                .finish(),
            Command::FxTrigger { slot } => f.debug_struct("FxTrigger").field("slot", slot).finish(),
            Command::PadPress { slot } => f.debug_struct("PadPress").field("slot", slot).finish(),
            Command::PadRelease { slot } => {
                f.debug_struct("PadRelease").field("slot", slot).finish()
            }
            Command::ListFx { chain } => f.debug_struct("ListFx").field("chain", chain).finish(),
            Command::Quit => f.write_str("Quit"),
        }
    }
}
