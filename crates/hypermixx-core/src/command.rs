//! The command / response protocol between the CLI and the audio pipeline.
//!
//! Commands carry no decode or analysis work — the CLI owns file IO and the analyser, then hands
//! the *results* (a loaded source, a compiled [`TrackAnalysis`]) to the producer. Everything here
//! must be executable by the pipeline's producer thread with no further I/O.

use crate::analysis::TrackAnalysis;
use crate::deck::HotCue;
use crate::deck::{DeckId, DeckState};
use crate::output::{OutputOp, OutputStatus};
use crate::source::Shared;
use crate::stem::{Stem, StemOp, StemSet};

/// Which FX chain a command addresses. `Deck` is the per-deck insert chain the mixer owns; a deck
/// itself knows nothing about FX, which keeps the transport logic free of audio effects.
///
/// `Stem` addresses **one stream's** insert chain. With stems installed a deck has one flow chain
/// per stream (its `flow_fx`), so the old "merged index space" (flow slots then deck slots) has no
/// single meaning any more: a chain is named outright. On a track without stems the two are the
/// same thing, because there is only one stream and its chain is the one the config describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FxChainId {
    /// The chain belonging to one deck (a mono-style insert on that deck's signal).
    Deck(DeckId),
    /// One stem's insert chain — a per-stream insert inside that deck.
    Stem { deck: DeckId, stem: Stem },
    /// The summed output chain, before the master limiter.
    Master,
}

impl FxChainId {
    /// A stable label for logs, errors and the front-end's slot book.
    pub fn label(&self) -> String {
        match self {
            // Deck numbers are 1-based everywhere a user reads them; the id itself is 0-based.
            FxChainId::Deck(deck_id) => crate::deck_label(*deck_id),
            FxChainId::Stem { deck, stem } => format!("{}/{stem}", crate::deck_label(*deck)),
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

/// One parameter of an FX slot, as the front-end needs to draw it.
#[derive(Clone, Debug, PartialEq)]
pub struct FxParamStatus {
    pub name: String,
    pub value: f32,
    /// Display/travel range, when the effect declares one. `None` means the front-end should show
    /// the value as text rather than draw a bar it cannot map.
    pub range: Option<(f32, f32)>,
    /// Musical length of this parameter, when it is a beat division. The encoding is deliberately
    /// signed and integral so a UI can step it without floating-point comparisons:
    ///
    /// * `n > 0`  → `n` beats (`1`, `2`, `4`, ...)
    /// * `n < 0`  → `1 / -n` beat (`-2` = half a beat, `-4` = a quarter, `-32` = a 1/32)
    /// * `0`      → this parameter is not a beat division (the field is still present, so every
    ///              effect has the same shape)
    ///
    /// The value itself stays `f32`: the DSP still takes a fractional beat length.
    pub beats: i32,
}

impl FxParamStatus {
    /// The parameter's beat length, if it is a beat division.
    pub fn beat_len(&self) -> Option<f64> {
        match self.beats {
            n if n > 0 => Some(f64::from(n)),
            n if n < 0 => Some(1.0 / f64::from(-n)),
            _ => None,
        }
    }

    /// The nearest signed beat code for `value`: positive whole beats, negative reciprocal beats.
    ///
    /// Non-standard values snap to the nearest standard division rather than returning `0`; only a
    /// non-positive or non-finite value means "not a beat length".
    pub fn beats_code(value: f32) -> i32 {
        if !value.is_finite() || value <= 0.0 {
            return 0;
        }
        if value >= 1.0 {
            return value.round().max(1.0) as i32;
        }
        let denominator = (1.0 / value).round().max(1.0);
        -(denominator as i32)
    }

    /// The beat length after one ×2 (`double = true`) or ÷2 (`double = false`) press, clamped to
    /// this parameter's declared range. `None` when the parameter is not a beat division.
    pub fn scaled_beat_value(&self, double: bool) -> Option<f32> {
        let length = self.beat_len()?;
        let next = if double { length * 2.0 } else { length / 2.0 };
        let next = match self.range {
            Some((min, max)) => next.clamp(f64::from(min), f64::from(max)),
            None => next,
        };
        Some(next as f32)
    }
}

/// One FX slot as reported back to the caller.
#[derive(Clone, Debug, PartialEq)]
pub struct FxSlotStatus {
    pub index: usize,
    /// Stable identity of the *position*, independent of what is in it (`eq`, `filter`, `fx`, ...).
    /// A front-end finds its knob's slot by role, so swapping `filter` for `moog` in that slot does
    /// not move the control out from under the user.
    pub role: String,
    /// What is actually in the slot right now.
    pub kind: String,
    pub enabled: bool,
    /// Which flows this slot is routed to — `None` for a slot that is not in a flow chain (a
    /// knob/pad/master slot, which sees one bus). An empty list is a valid answer: the slot is
    /// mounted but routed to no flow, so it processes nothing.
    pub flows: Option<Vec<Stem>>,
    /// Parameters, in the order the effect declares them.
    pub params: Vec<FxParamStatus>,
}

/// How `loop out` and loop edits snap to the beat grid: the granularity of the out offset,
/// measured in beats from the loop's in point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoopQuantum {
    /// Whole beats (1.0).
    #[default]
    Beat,
    /// Half beats (0.5).
    Half,
    /// Quarter beats (0.25).
    Quarter,
    /// Eighth beats (0.125).
    Eighth,
}

impl LoopQuantum {
    /// The quantum's length in beats.
    pub fn beats(self) -> f64 {
        match self {
            LoopQuantum::Beat => 1.0,
            LoopQuantum::Half => 0.5,
            LoopQuantum::Quarter => 0.25,
            LoopQuantum::Eighth => 0.125,
        }
    }

    /// Parses a CLI token.
    pub fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "beat" | "b" | "1" => Some(LoopQuantum::Beat),
            "half" | "1/2" | "2" => Some(LoopQuantum::Half),
            "quarter" | "1/4" | "4" => Some(LoopQuantum::Quarter),
            "eighth" | "1/8" | "8" => Some(LoopQuantum::Eighth),
            _ => None,
        }
    }
}

/// An in-loop edit: the range is recomputed as a whole and stored in one shot, so the feed's
/// mapping never sees a half-moved range. `virtual_pos` never moves — only `loop_range` does.
///
/// `Halve`/`Double` are the dedicated relative length keys (the DJ ÷2/×2 buttons): they scale
/// the *current* length, clamped to the domain **1/32 beat ..= 64 beats** — below the
/// quantization floor on purpose (a loop roll may want a 32nd) and above it so a runaway double
/// can't swallow the track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LoopEditOp {
    /// Keep `in`, move `out` to `in + beats` (beats may be fractional, down to one quantum).
    Length { beats: f64 },
    /// Halve the running length (`in` fixed); floors at 1/32 beat.
    Halve,
    /// Double the running length (`in` fixed); caps at 64 beats.
    Double,
    /// Shift `in` and `out` together by whole beats.
    Move { beats: i64 },
    /// Move only `in`; the length changes with it.
    In { beats: i64 },
    /// Move only `out`; the length changes with it.
    Out { beats: i64 },
}

/// A musical length, exact for every loop a DJ uses: `4` beats, `1/2` beat, `1/4` beat.
///
/// The denominator is a power of two, so a length converts to frames without drift. A `u64` beat
/// count could not express the sub-beat loops a controller's beat-loop pads send.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BeatLen {
    num: u32,
    den: u32,
}

impl BeatLen {
    /// The length in whole beats.
    pub const fn beats(num: u32) -> Self {
        Self { num, den: 1 }
    }

    /// `numerator/denominator` beats. The denominator must be a non-zero power of two.
    pub const fn fraction(num: u32, den: u32) -> Option<Self> {
        if num == 0 || den == 0 || !den.is_power_of_two() {
            return None;
        }
        Some(Self { num, den })
    }

    /// Parses `4`, `1/2`, `3/4`. Integers only, so `0.25` is rejected rather than guessed.
    pub fn parse(token: &str) -> Option<Self> {
        let token = token.trim();
        match token.split_once('/') {
            Some((num, den)) => {
                let num: u32 = num.trim().parse().ok()?;
                let den: u32 = den.trim().parse().ok()?;
                Self::fraction(num, den)
            }
            None => {
                let num: u32 = token.parse().ok()?;
                (num > 0).then(|| Self::beats(num))
            }
        }
    }

    pub const fn numerator(self) -> u32 {
        self.num
    }

    pub const fn denominator(self) -> u32 {
        self.den
    }

    /// The number of whole beats in the length.
    pub const fn whole_beats(self) -> u64 {
        (self.num / self.den) as u64
    }

    /// The fraction of a beat left after the whole beats.
    pub fn fraction_of_beat(self) -> f64 {
        f64::from(self.num % self.den) / f64::from(self.den)
    }

    /// The length as a `f64` beat count.
    pub fn to_beats(self) -> f64 {
        f64::from(self.num) / f64::from(self.den)
    }

    /// `4` or `1/4`, the spelling a map file and a session line use.
    pub fn label(self) -> String {
        if self.den == 1 {
            self.num.to_string()
        } else {
            format!("{}/{}", self.num, self.den)
        }
    }
}

impl std::fmt::Display for BeatLen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

/// One loop command. The whole family rides a single [`Command::Loop`] variant so the deck's
/// loop state machine stays one match arm.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LoopOp {
    /// Arm a manual loop-in: quantize `in` to the beat and prepare the looper.
    In,
    /// Set the quantized `out` and engage: the prepared looper becomes `playing`.
    Out,
    /// Drop an armed loop-in (a pending looper).
    Cancel,
    /// Leave the active loop; a slip loop drops its shadow and resumes `playing`.
    Exit,
    /// Beat loop: with no loop engaged, open an N-beat loop at the quantized current beat; while
    /// looping, re-time the current loop's `out` to `in + N` beats (halve/double), in place.
    Beats(BeatLen),
    /// Toggle a loop using the deck's `loop_beats`: no loop opens one, an active loop exits.
    Toggle,
    /// Arm a slip loop: `playing` keeps its slip clock, `shadow` is the audible loop.
    SlipIn,
    /// Engage the armed slip loop.
    SlipOut,
    /// Slip beat loop: audible loop, `playing` continues silently underneath.
    SlipBeats(BeatLen),
    /// Slip roll: a slip beat loop whose shadow reads backwards.
    SlipRoll(BeatLen),
    /// Edit the active loop's range in place — no flow change.
    Edit(LoopEditOp),
    /// Set the deck's out-point quantization granularity.
    SetQuantum(LoopQuantum),
}

/// Which analyser backend to use for a track. Configured directly (`[preference.analysis]` in
/// config.toml) — there is no auto fallback: the user names the backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    /// timestretch's own analyser (tracker + kick-band corroboration). The default.
    #[default]
    Timestretch,
    /// stratum-dsp only.
    Stratum,
}

impl Backend {
    /// Parses a config/CLI token.
    pub fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "timestretch" => Some(Backend::Timestretch),
            "stratum" => Some(Backend::Stratum),
            _ => None,
        }
    }

    /// The config/CLI token for this backend.
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Timestretch => "timestretch",
            Backend::Stratum => "stratum",
        }
    }
}

/// How a phase correction closes the gap: the `sync phase` / `sync phaselock` argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseMode {
    /// One flow switch that lands the follower on the leader's phase. Nothing keeps it there —
    /// this is a correction, not a controller.
    Instant,
    /// Close the gap at a fixed slope over `t_seconds` (default 2.0).
    Linear,
    /// PI controller with integral anti-windup and a ±5% output clamp.
    Pid,
}

impl PhaseMode {
    /// Parses a CLI token.
    pub fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "instant" | "jump" => Some(PhaseMode::Instant),
            "linear" | "lin" => Some(PhaseMode::Linear),
            "pid" | "pi" | "pll" => Some(PhaseMode::Pid),
            _ => None,
        }
    }

    /// A stable label for logs, state readouts and errors.
    pub fn label(&self) -> &'static str {
        match self {
            PhaseMode::Instant => "instant",
            PhaseMode::Linear => "linear",
            PhaseMode::Pid => "pid",
        }
    }
}

/// One `sync` command. `phase` includes `tempo`; `phaselock` includes `tempolock`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SyncOp {
    /// One-shot tempo match: the target's rate becomes `leader_bpm / own_bpm`.
    Tempo,
    /// `Tempo`, then a phase correction (`t_seconds` belongs to [`PhaseMode::Linear`]).
    Phase {
        mode: PhaseMode,
        t_seconds: Option<f64>,
    },
    /// Bidirectional shared tempo: one `group_bpm` both decks derive their rate from.
    TempoLock,
    /// One-way shared tempo — the follower tracks the leader — plus a phase correction.
    PhaseLock {
        mode: PhaseMode,
        t_seconds: Option<f64>,
    },
    /// Make the *target* deck the sync leader (target-first grammar: `deck0 sync set-leader`).
    SetLeader,
    /// Drop the lock and the phase correction and any nudge; the tempo stays where it is.
    Unlock,
}

/// One `nudge` command: a temporary rate bend that changes phase while it runs and leaves
/// [`SyncOp::Tempo`]'s tempo untouched when it ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NudgeOp {
    /// Start bending by `delta` (a rate, `0.04` = 4% fast). `seconds` releases it on its own;
    /// `None` holds until [`NudgeOp::Stop`].
    ///
    /// Absolute: the bend becomes `delta`, whatever it was. What a *button* wants.
    Start { delta: f32, seconds: Option<f64> },
    /// Add `delta` to the running bend (clamped to the engine's limit) and release it `seconds`
    /// after the last message. What a **wheel** wants: turning faster bends harder instead of
    /// bending for longer, and the two directions subtract from each other.
    Bend { delta: f32, seconds: f64 },
    /// Release the bend (ramps back to zero, it is not cut off).
    Stop,
}

/// One `vinyl` command: a DJ wheel, which is two things at once (a platter sensor and a jog wheel)
/// and therefore resolved by the deck, which is the only thing that knows whether the platter is
/// held right now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VinylOp {
    /// A hand went on the platter: pause, remembering whether the deck was playing. Pressing twice
    /// without a release is ignored, so a repeat sensor cannot stack pauses.
    Touch,
    /// The hand came off: resume if the touch is what paused it.
    Release,
    /// Arm the next touch as a **slip** vinyl: `playing` keeps its slip clock, and the audible
    /// scratch is a Tape shadow. Release drops the shadow; `playing` is already at the slip
    /// position.
    Slip,
    /// The wheel turned by `ticks` (signed — negative is anticlockwise). **Held** (a hand on the
    /// platter): the playhead moves by `ticks × frames_per_tick`. **Not held**: it is a pitch bend,
    /// which is what a wheel does on a deck when nobody is touching the record.
    ///
    /// `frames_per_tick` comes from the binding, so the sensitivity is per controller: a wheel with
    /// 60 detents per revolution wants a very different number from one that only reports ±1 slowly.
    Turn { ticks: i32, frames_per_tick: f32 },
}

/// One `cue` command. The cue point defaults to frame 0 (the loaded origin) and `Set` can move
/// it; `Smart` folds the two-button DJ gesture into one command that the *engine* resolves, so a
/// front-end never has to guess the deck's transport from a possibly-stale state snapshot.
/// One hot-cue operation. Slots are 0-based; invalid slots are ignored by the deck.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotCueOp {
    Set(u8),
    Trigger(u8),
    Clear(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CueOp {
    /// Jump to the cue point, then play. Unconditional: it neither moves the cue point nor asks the
    /// transport anything, which is what `cue play` means.
    Play,
    /// Jump to the cue point and pause (the release edge of a held cue button).
    Back,
    /// Move the cue point to the current frame.
    Set,
    /// `Back` while playing, `Set` while paused — resolved by the deck itself.
    Smart,
    /// The **press edge** of a held cue button, resolved by the deck against its own transport:
    ///
    /// * playing → a back cue: jump to the cue point and pause. The transport was already running, so
    ///   there is nothing to start — no preview overlapping the back cue.
    /// * paused → the playhead becomes the cue point, and the deck plays it. Finding a spot and
    ///   cueing it is one gesture, and holding the button is what makes it audible.
    ///
    /// One op rather than a pair of commands because the decision must be atomic with the jump it
    /// causes: two commands could be split by another one arriving in between.
    Hold,
    /// The whole gesture of a cue **button**, for a front-end that has no release edge (a typed
    /// command, a clicking UI): each `Button` is the next edge of that button — the first is the
    /// press ([`CueOp::Hold`]), the next is the release ([`CueOp::Back`]), and so on.
    ///
    /// The deck holds the state, for the same reason it resolves `Smart` and `Hold` itself: a
    /// front-end that tracked it would be guessing at a transport it can only see stale snapshots of.
    Button,
}

/// The keylock profile a deck runs: whether the time-stretch corrects pitch while the tempo moves.
///
/// The keylock STFT window is a machine setting (`[keylock]` in `live.toml`), not a runtime mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeylockMode {
    /// Pure varispeed: pitch follows tempo (Tape backend).
    Off,
    /// Keylock with the configured STFT window (Signalsmith; `[keylock]` in live.toml).
    On,
}

impl KeylockMode {
    /// Parses a CLI token.
    pub fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "off" | "tape" | "false" | "0" => Some(KeylockMode::Off),
            "on" | "keylock" | "true" | "1" => Some(KeylockMode::On),
            _ => None,
        }
    }

    /// A stable label for state readouts and errors.
    pub fn label(&self) -> &'static str {
        match self {
            KeylockMode::Off => "off",
            KeylockMode::On => "on",
        }
    }
}

/// Which mixer fader a [`Command::SetFader`] addresses.
///
/// The value domain belongs to the target, not the protocol: bipolar positions are
/// `-1.0..=1.0`, sends and bus levels are `0.0..=1.0`. The mixer's own setters clamp and sanitise
/// the value, so a mapping layer never has to duplicate that knowledge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaderTarget {
    /// One channel's **per-stream** level: the fader between a stream's inserts and the sum. With
    /// stems installed this writes every stem's level at once, because one control still has to
    /// work on a stem deck; a single stem is addressed through [`FaderTarget::Stem`].
    Flow(DeckId),
    /// One stem's level, as a bipolar fader position (`-1.0` = exact silence, `0.0` = unity).
    Stem { deck: DeckId, stem: Stem },
    /// One channel's **channel fader** — the one `[channel] fader` places before or after the
    /// channel `chain`. This is what a mixer's fader is mapped to.
    Deck(DeckId),
    /// One channel's **trim**: `0.0` is 0 dB, `1.0` is +6 dB, `-1.0` is silence. The only level a
    /// channel has that can boost, and therefore the only one that is a setting rather than a
    /// performance control.
    Gain(DeckId),
    /// One channel's cue send, linear `0.0..=1.0` (a send level, not a bipolar fader).
    CueSend(DeckId),
    /// The single crossfader, broadcast to every channel (`-1.0` hard left .. `1.0` hard right).
    Crossfader,
    /// The master bus fader.
    Master,
    /// The cue bus fader.
    Cue,
}

impl FaderTarget {
    /// Whether the target is a bipolar position (`-1.0..=1.0`, `0.0` = unity) rather than a
    /// linear level (`0.0..=1.0`). A mapping layer uses this to pick the default value range.
    pub fn is_bipolar(self) -> bool {
        match self {
            FaderTarget::CueSend(..) => false,
            FaderTarget::Flow(..)
            | FaderTarget::Stem { .. }
            | FaderTarget::Deck(..)
            | FaderTarget::Gain(..)
            | FaderTarget::Crossfader
            | FaderTarget::Master
            | FaderTarget::Cue => true,
        }
    }
}

/// A command sent to the pipeline's producer thread.
pub enum Command {
    /// Installs a decoded `source` into `deck_id`, cued at frame 0.
    Load {
        deck_id: DeckId,
        source: Shared,
        /// Optional caller-supplied constant grid (deterministic testing / beat-match).
        analysis: Option<TrackAnalysis>,
        /// The track's loudness (the average level of its loudest section, dBFS), measured by the
        /// front-end while decoding. Auto-gain derives its gain from this at play time, so toggling
        /// auto-gain or changing its target applies to whatever is already loaded.
        loudness_db: Option<f32>,
    },
    Play {
        deck_id: DeckId,
    },
    Pause {
        deck_id: DeckId,
    },
    /// Flips play/pause in one command, so a single button (or bare `play`) is a toggle without
    /// the front-end having to know the current transport.
    TogglePlay {
        deck_id: DeckId,
    },
    /// The cue family: play from / return to / set the cue point.
    Cue {
        deck_id: DeckId,
        op: CueOp,
    },
    /// The hot-cue family: set, trigger, or clear one slot.
    HotCue {
        deck_id: DeckId,
        op: HotCueOp,
    },
    /// Seeds the deck with imported memory/hot cues, normally immediately after `Load`.
    SetCues {
        deck_id: DeckId,
        memory: Option<u64>,
        hot: Vec<HotCue>,
    },
    /// A DJ wheel: platter touch, release, or a turn.
    Vinyl {
        deck_id: DeckId,
        op: VinylOp,
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
    /// Jump by the deck's `jump_beats`, positive for forward and negative for backward.
    BeatJumpStep {
        deck_id: DeckId,
        direction: i8,
    },
    /// Double or halve the deck's persistent `jump_beats` step.
    ScaleJumpBeats {
        deck_id: DeckId,
        halve: bool,
    },
    /// Double or halve the deck's persistent `loop_beats`, retiming an active loop in place.
    ScaleLoopBeats {
        deck_id: DeckId,
        halve: bool,
    },
    /// Sets the tempo (0.25 = quarter speed, 1.0 = unity, 4.0 = quadruple).
    SetTempo {
        deck_id: DeckId,
        tempo: f32,
    },
    /// Sets the tempo fader position, `-1.0..=1.0`; the deck derives `tempo = 1 + position * range`.
    SetTempoFader {
        deck_id: DeckId,
        position: f32,
    },
    /// Sets the fader's full-deflection range (`0.1` = ±10%). Only changes the mapping — the
    /// running tempo stays where it is.
    SetTempoRange {
        deck_id: DeckId,
        range: f32,
    },
    /// The loop family: manual in/out, beat loops, exit, in-loop edits and quantization.
    Loop {
        deck_id: DeckId,
        op: LoopOp,
    },
    /// Beat-sync against another deck: tempo matching, shared-tempo locks and phase correction.
    Sync {
        deck_id: DeckId,
        op: SyncOp,
    },
    /// A temporary rate bend to nudge the beat into alignment by hand.
    Nudge {
        deck_id: DeckId,
        op: NudgeOp,
    },
    /// Sets one mixer fader. The mixer clamps the value per target kind; the only failure is an
    /// unknown deck.
    SetFader {
        target: FaderTarget,
        value: f32,
    },
    /// Installs a separated track: four frame-aligned streams, one per [`Stem`].
    ///
    /// A source swap at the current position, so it is seamless (the same jump machinery a `Jump`
    /// uses). Until the swap lands the deck keeps playing the plain mix — read
    /// [`DeckState::stems`] to know which of the two the audio is.
    SetStems {
        deck_id: DeckId,
        stems: StemSet,
    },
    /// Per-stem level, mute, solo or a named arrangement. Only meaningful once stems are installed;
    /// the mixer answers with an error on a track that has none.
    Stem {
        deck_id: DeckId,
        op: StemOp,
    },
    /// Replaces the engine's auto-gain preference (from `live.toml`). Applies immediately to every
    /// loaded track: the measurement lives on the deck, the gain is derived here.
    SetAutogain {
        enabled: bool,
        target_db: f32,
    },
    /// Switches the keylock profile (`off` = tape, `on` = the configured keylock window).
    SetKeylock {
        deck_id: DeckId,
        mode: KeylockMode,
    },
    /// Flips a deck's keylock profile in the engine, so a single pad is a latch and a front-end
    /// never has to keep a stale mirror of the deck's state.
    ToggleKeylock {
        deck_id: DeckId,
    },
    /// Flips one channel's cue send between off (`0`) and full (`1`). The engine owns the value
    /// because the `fader cuesend` path can change it too.
    ToggleCueSend {
        deck_id: DeckId,
    },
    /// Sets the pitch shift in semitones. A **placeholder**: the streaming time-stretch engine has
    /// no pitch axis yet, so the deck stores the value and reports it, but nothing audibly moves.
    SetKey {
        deck_id: DeckId,
        semitones: i32,
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
    /// As [`Command::GetState`], but the answer is meant to be rendered as a per-stem report. The
    /// payload is identical; the variant exists so a front-end does not have to remember which
    /// request a [`CommandResponse::State`] came from.
    GetStemState {
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
    /// Flips a slot's a/b state in the engine, which is where the truth is: a front-end that tracked
    /// it locally would go wrong the first time anything else changed the slot.
    ToggleFx {
        slot: FxSlotRef,
    },
    /// Sets one named parameter. The value lands on the audio thread through a smoothing filter,
    /// so sweeping a cutoff never clicks.
    SetFxParam {
        slot: FxSlotRef,
        name: String,
        value: f32,
    },
    /// Routes a flow slot to a set of flows. Only the `flowfx` bank has flows; `eq`, `filter`, the
    /// post-merge banks and master are single-bus and refuse this.
    ///
    /// An empty list routes the slot to no flow — it stays mounted (index, role and kind unchanged)
    /// but processes nothing, which is the difference between "off" and "not routed here". Narrowing
    /// the set cuts a ringing tail on the flows that leave; widening resets the instances that
    /// enter, so no flow ever resumes with another flow's history.
    SetFxFlows {
        slot: FxSlotRef,
        flows: Vec<Stem>,
    },
    /// Replaces the effect in a slot, keeping the position, the role and the on/off state. This is
    /// how a fixed slot's effect changes at run time (`filter` → `moog`, a pad bank retargeted
    /// between echo and reverb) without renumbering anything a map is bound to.
    ///
    /// Parameters whose names the new effect also declares keep their value; the rest take the new
    /// effect's defaults. A new instance is prepared at the engine's rate before its first block.
    SetFxKind {
        slot: FxSlotRef,
        kind: String,
    },
    /// Sets the parameter at `index` in the effect's declared parameter order. A mutable FX slot
    /// uses this form because its parameter names change when the effect is swapped.
    SetFxParamIndex {
        slot: FxSlotRef,
        index: usize,
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
    /// As [`Command::ListFx`], but meant to be polled: a front-end that shows a live knob value
    /// asks this every few ticks and renders the answer *without* echoing it to the response log.
    /// The payload is identical; the variant exists so the two uses stay distinguishable.
    GetFxState {
        chain: FxChainId,
    },
    /// The output destinations: list them, or retry the ones that could not be bound.
    Output {
        op: OutputOp,
    },
    /// Rebuilds the mixer's topology from a config file's text.
    ///
    /// The text, not a path: file IO belongs to the front-end, which is also the only layer that
    /// knows where its user keeps configs. What arrives here is parsed, validated and applied, or
    /// refused wholesale — a reload that half-applies would be worse than one that does not.
    MixerReload {
        text: String,
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
    /// The answer to [`Command::GetStemState`]: the same snapshot, to be shown per stem.
    Stems(DeckState),
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
    /// The answer to [`Command::GetFxState`]: a live read, not a command echo.
    FxState {
        chain: FxChainId,
        slots: Vec<FxSlotStatus>,
    },
    /// The answer to [`Command::Output`].
    Outputs(Vec<OutputStatus>),
    Ok,
    Error(String),
}

// `Command` carries a `Shared` (a trait object with no `Debug`), so derive can't cover it. This
// manual impl names each variant and its scalars, which is all tracing needs.
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::Load {
                deck_id,
                analysis,
                loudness_db,
                ..
            } => f
                .debug_struct("Load")
                .field("deck_id", deck_id)
                .field("has_analysis", &analysis.is_some())
                .field("loudness_db", loudness_db)
                .finish(),
            Command::Play { deck_id } => f.debug_struct("Play").field("deck_id", deck_id).finish(),
            Command::Pause { deck_id } => {
                f.debug_struct("Pause").field("deck_id", deck_id).finish()
            }
            Command::TogglePlay { deck_id } => f
                .debug_struct("TogglePlay")
                .field("deck_id", deck_id)
                .finish(),
            Command::Cue { deck_id, op } => f
                .debug_struct("Cue")
                .field("deck_id", deck_id)
                .field("op", op)
                .finish(),
            Command::HotCue { deck_id, op } => f
                .debug_struct("HotCue")
                .field("deck_id", deck_id)
                .field("op", op)
                .finish(),
            Command::SetCues {
                deck_id,
                memory,
                hot,
            } => f
                .debug_struct("SetCues")
                .field("deck_id", deck_id)
                .field("memory", memory)
                .field("hot", &hot.len())
                .finish(),
            Command::Vinyl { deck_id, op } => f
                .debug_struct("Vinyl")
                .field("deck_id", deck_id)
                .field("op", op)
                .finish(),
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
            Command::BeatJumpStep { deck_id, direction } => f
                .debug_struct("BeatJumpStep")
                .field("deck_id", deck_id)
                .field("direction", direction)
                .finish(),
            Command::ScaleJumpBeats { deck_id, halve } => f
                .debug_struct("ScaleJumpBeats")
                .field("deck_id", deck_id)
                .field("halve", halve)
                .finish(),
            Command::ScaleLoopBeats { deck_id, halve } => f
                .debug_struct("ScaleLoopBeats")
                .field("deck_id", deck_id)
                .field("halve", halve)
                .finish(),
            Command::SetTempo { deck_id, tempo } => f
                .debug_struct("SetTempo")
                .field("deck_id", deck_id)
                .field("tempo", tempo)
                .finish(),
            Command::SetTempoFader { deck_id, position } => f
                .debug_struct("SetTempoFader")
                .field("deck_id", deck_id)
                .field("position", position)
                .finish(),
            Command::SetTempoRange { deck_id, range } => f
                .debug_struct("SetTempoRange")
                .field("deck_id", deck_id)
                .field("range", range)
                .finish(),
            Command::Loop { deck_id, op } => f
                .debug_struct("Loop")
                .field("deck_id", deck_id)
                .field("op", op)
                .finish(),
            Command::Sync { deck_id, op } => f
                .debug_struct("Sync")
                .field("deck_id", deck_id)
                .field("op", op)
                .finish(),
            Command::Nudge { deck_id, op } => f
                .debug_struct("Nudge")
                .field("deck_id", deck_id)
                .field("op", op)
                .finish(),
            Command::SetFader { target, value } => f
                .debug_struct("SetFader")
                .field("target", target)
                .field("value", value)
                .finish(),
            Command::SetAutogain {
                enabled,
                target_db,
            } => f
                .debug_struct("SetAutogain")
                .field("enabled", enabled)
                .field("target_db", target_db)
                .finish(),
            Command::SetKeylock { deck_id, mode } => f
                .debug_struct("SetKeylock")
                .field("deck_id", deck_id)
                .field("mode", mode)
                .finish(),
            Command::ToggleKeylock { deck_id } => f
                .debug_struct("ToggleKeylock")
                .field("deck_id", deck_id)
                .finish(),
            Command::ToggleCueSend { deck_id } => f
                .debug_struct("ToggleCueSend")
                .field("deck_id", deck_id)
                .finish(),
            Command::SetKey { deck_id, semitones } => f
                .debug_struct("SetKey")
                .field("deck_id", deck_id)
                .field("semitones", semitones)
                .finish(),
            Command::SetAnalysis { deck_id, .. } => f
                .debug_struct("SetAnalysis")
                .field("deck_id", deck_id)
                .finish(),
            Command::SetStems { deck_id, stems } => f
                .debug_struct("SetStems")
                .field("deck_id", deck_id)
                .field("frames", &stems.total_frames())
                .finish(),
            Command::Stem { deck_id, op } => f
                .debug_struct("Stem")
                .field("deck_id", deck_id)
                .field("op", op)
                .finish(),
            Command::GetState { deck_id } => f
                .debug_struct("GetState")
                .field("deck_id", deck_id)
                .finish(),
            Command::GetStemState { deck_id } => f
                .debug_struct("GetStemState")
                .field("deck_id", deck_id)
                .finish(),
            Command::GetAllStates => f.write_str("GetAllStates"),
            Command::AddFx { chain, kind } => f
                .debug_struct("AddFx")
                .field("chain", chain)
                .field("kind", kind)
                .finish(),
            Command::RemoveFx { chain, index } => f
                .debug_struct("RemoveFx")
                .field("chain", chain)
                .field("index", index)
                .finish(),
            Command::ToggleFx { slot } => f.debug_struct("ToggleFx").field("slot", slot).finish(),
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
            Command::SetFxParamIndex { slot, index, value } => f
                .debug_struct("SetFxParamIndex")
                .field("slot", slot)
                .field("index", index)
                .field("value", value)
                .finish(),
            Command::SetFxFlows { slot, flows } => f
                .debug_struct("SetFxFlows")
                .field("slot", slot)
                .field("flows", flows)
                .finish(),
            Command::SetFxKind { slot, kind } => f
                .debug_struct("SetFxKind")
                .field("slot", slot)
                .field("kind", kind)
                .finish(),
            Command::FxTrigger { slot } => f.debug_struct("FxTrigger").field("slot", slot).finish(),
            Command::PadPress { slot } => f.debug_struct("PadPress").field("slot", slot).finish(),
            Command::PadRelease { slot } => {
                f.debug_struct("PadRelease").field("slot", slot).finish()
            }
            Command::ListFx { chain } => f.debug_struct("ListFx").field("chain", chain).finish(),
            Command::GetFxState { chain } => {
                f.debug_struct("GetFxState").field("chain", chain).finish()
            }
            Command::Output { op } => f.debug_struct("Output").field("op", op).finish(),
            Command::MixerReload { text } => f
                .debug_struct("MixerReload")
                .field("bytes", &text.len())
                .finish(),
            Command::Quit => f.write_str("Quit"),
        }
    }
}

#[cfg(test)]
mod fx_param_tests {
    use super::FxParamStatus;

    fn beat(value: f32) -> FxParamStatus {
        FxParamStatus {
            name: "time".to_owned(),
            value,
            range: Some((1.0 / 32.0, 4.0)),
            beats: FxParamStatus::beats_code(value),
        }
    }

    #[test]
    fn positive_beats_are_whole_and_negative_beats_are_reciprocals() {
        assert_eq!(FxParamStatus::beats_code(1.0), 1);
        assert_eq!(FxParamStatus::beats_code(2.0), 2);
        assert_eq!(FxParamStatus::beats_code(0.5), -2);
        assert_eq!(FxParamStatus::beats_code(0.25), -4);
        assert_eq!(FxParamStatus::beats_code(1.0 / 32.0), -32);
        assert_eq!(FxParamStatus::beats_code(0.0), 0);
    }

    #[test]
    fn the_buttons_double_and_halve_and_stay_inside_the_range() {
        let half = beat(0.5);
        assert_eq!(half.scaled_beat_value(true), Some(1.0), "1/2 x2 = 1");
        assert_eq!(half.scaled_beat_value(false), Some(0.25), "1/2 /2 = 1/4");

        let one = beat(1.0);
        assert_eq!(one.scaled_beat_value(false), Some(0.5), "1 /2 = 1/2");

        let top = beat(4.0);
        assert_eq!(top.scaled_beat_value(true), Some(4.0), "clamped at 4 beats");

        let bottom = beat(1.0 / 32.0);
        assert_eq!(
            bottom.scaled_beat_value(false),
            Some(1.0 / 32.0),
            "clamped at 1/32"
        );
        assert_eq!(
            FxParamStatus {
                name: "mix".to_owned(),
                value: 0.5,
                range: Some((0.0, 1.0)),
                beats: 0,
            }
            .scaled_beat_value(true),
            None
        );
    }
}
