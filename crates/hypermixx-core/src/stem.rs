//! Stems: the parallel streams a separated track exposes.
//!
//! A track is loaded as **one** stream. `Command::SetStems` replaces it with four — the model's
//! four sources — and from then on the deck has four parallel streams that share one transport
//! (one clock, one loop range, one tempo). See `hypermixx-audio`'s `flow` module for why they are
//! one engine rather than four.
//!
//! Everything here is protocol: names, the payload a separator hands over, and the small amount of
//! state a front-end reads back. No engine or analysis logic knows these types mean anything more
//! than "index 0..3".

use crate::source::Shared;

/// One of the four sources a separated track is split into.
///
/// The order is the model's output order and **is** the index order of every per-stem array in the
/// engine ([`Stem::index`]), so it is fixed and must match [`StemSet::stems`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stem {
    Drums,
    Bass,
    Other,
    Vocals,
}

impl Stem {
    /// How many stems a separated track has.
    pub const COUNT: usize = 4;
    /// Every stem, in model/index order.
    pub const ALL: [Stem; Stem::COUNT] = [Stem::Drums, Stem::Bass, Stem::Other, Stem::Vocals];

    /// This stem's slot in every per-stem array.
    pub const fn index(self) -> usize {
        match self {
            Stem::Drums => 0,
            Stem::Bass => 1,
            Stem::Other => 2,
            Stem::Vocals => 3,
        }
    }

    /// The stem at `index`, or `None` outside `0..Stem::COUNT`.
    pub const fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Stem::Drums),
            1 => Some(Stem::Bass),
            2 => Some(Stem::Other),
            3 => Some(Stem::Vocals),
            _ => None,
        }
    }

    /// The canonical lowercase name — what commands, TOML and log lines use.
    pub const fn name(self) -> &'static str {
        match self {
            Stem::Drums => "drums",
            Stem::Bass => "bass",
            Stem::Other => "other",
            Stem::Vocals => "vocals",
        }
    }

    /// Parses a command/token name. Accepts the canonical name and a few obvious aliases.
    pub fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "drums" | "drum" | "d" => Some(Stem::Drums),
            "bass" | "b" => Some(Stem::Bass),
            "other" | "o" | "rest" => Some(Stem::Other),
            "vocals" | "vocal" | "vox" | "acapella" | "a" => Some(Stem::Vocals),
            _ => None,
        }
    }
}

impl std::fmt::Display for Stem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A separated track: four frame-aligned PCM streams, in [`Stem::ALL`] order.
///
/// Built by a separator (offline, on a worker thread) and handed to the engine by
/// [`Command::SetStems`](crate::Command::SetStems). The streams are expected to be the *same
/// length* as the mix they came from — the engine's four streams share one clock, so a short
/// stream would silently diverge.
#[derive(Clone)]
pub struct StemSet {
    pub stems: [Shared; Stem::COUNT],
}

impl StemSet {
    pub fn new(stems: [Shared; Stem::COUNT]) -> Self {
        Self { stems }
    }


    /// The stream for one stem.
    pub fn get(&self, stem: Stem) -> Shared {
        Shared::clone(&self.stems[stem.index()])
    }

    /// Total frames of the set (stem 0's length; the four are frame-aligned by contract).
    pub fn total_frames(&self) -> u64 {
        self.stems[0].total_frames()
    }

    /// Whether every stream reports the same length. A separator should refuse to hand over a set
    /// that fails this, but the check belongs where the result is built, not here.
    pub fn is_frame_aligned(&self) -> bool {
        let frames = self.total_frames();
        self.stems.iter().all(|s| s.total_frames() == frames)
    }
}

// `Source` is a trait object with no `Debug`, so derive cannot cover this. The lengths are what
// actually matters when one of these appears in a log line or a failed assertion.
impl std::fmt::Debug for StemSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StemSet")
            .field("stems", &Stem::COUNT)
            .field("frames", &self.total_frames())
            .finish()
    }
}

/// What a front-end reads back about a channel's stems.
///
/// `ready` is the honest answer to "is the audio actually separated yet": installing stems is a
/// source swap that lands a few blocks after the command, so the intent and the audio can differ
/// for a moment.
#[derive(Clone, Debug, PartialEq)]
pub struct StemStatus {
    pub ready: bool,
    /// Per-stem level, as a **bipolar fader position** (`-1.0` = exact silence, `0.0` = unity).
    pub level: [f32; Stem::COUNT],
    pub mute: [bool; Stem::COUNT],
    /// The solo set as a bitmask of `1 << Stem::index()`; `0` = nothing soloed.
    pub solo: u8,
}

impl Default for StemStatus {
    /// The state of a track that has no stems: nothing to report, and a silent set of levels would
    /// read as "all muted", so the defaults are unity.
    fn default() -> Self {
        Self {
            ready: false,
            level: [0.0; Stem::COUNT],
            mute: [false; Stem::COUNT],
            solo: 0,
        }
    }
}

impl StemStatus {
    /// Whether `stem` is in the solo set.
    pub fn is_soloed(&self, stem: Stem) -> bool {
        self.solo & (1 << stem.index()) != 0
    }

    /// Whether anything is soloed at all (which is what makes the solo set meaningful).
    pub fn any_solo(&self) -> bool {
        self.solo != 0
    }

    /// The position the engine is actually applying to `stem`: the level while it is audible, and
    /// `-1.0` (exact silence) while a mute or someone else's solo is masking it.
    pub fn effective_position(&self, stem: Stem) -> f32 {
        let audible = if self.any_solo() {
            self.is_soloed(stem)
        } else {
            !self.mute[stem.index()]
        };
        if audible {
            self.level[stem.index()]
        } else {
            -1.0
        }
    }
}

/// One `stem` command, for a channel that has stems.
///
/// Mute and solo are kept as *separate facts* rather than folded into the level: unmuting has to
/// restore the level the DJ had set, and soloing one stem must not destroy the others' settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StemOp {
    /// Set one stem's level (bipolar fader position, `0.0` = unity).
    Level { stem: Stem, position: f32 },
    /// Mute/unmute one stem. Solo, if any, still wins.
    Mute { stem: Stem, on: bool },
    /// Add/remove one stem from the solo set.
    Solo { stem: Stem, on: bool },
    /// All four audible at unity, no mute, no solo.
    Clear,
    /// A named arrangement (acapella, instrumental, …).
    Preset(StemPreset),
}

/// A named per-stem arrangement, as mute flags — the DJ's own levels survive a preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StemPreset {
    /// Everything, nothing muted. Same as [`StemOp::Clear`].
    Full,
    /// Vocals only.
    Acapella,
    /// Everything but vocals.
    Instrumental,
    /// Drums only.
    DrumsOnly,
    /// Bass only.
    BassOnly,
}

impl StemPreset {
    /// Parses a command token.
    pub fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "full" | "all" | "reset" => Some(StemPreset::Full),
            "acapella" | "a-capella" | "vocals-only" | "vocals_only" => Some(StemPreset::Acapella),
            "instrumental" | "inst" | "no-vocals" | "karaoke" => Some(StemPreset::Instrumental),
            "drums" | "drums-only" | "drums_only" => Some(StemPreset::DrumsOnly),
            "bass" | "bass-only" | "bass_only" => Some(StemPreset::BassOnly),
            _ => None,
        }
    }

    /// The mute flags this preset means, indexed like [`Stem::ALL`].
    pub const fn mute(self) -> [bool; Stem::COUNT] {
        // Order: drums, bass, other, vocals.
        match self {
            StemPreset::Full => [false, false, false, false],
            StemPreset::Acapella => [true, true, true, false],
            StemPreset::Instrumental => [false, false, false, true],
            StemPreset::DrumsOnly => [false, true, true, true],
            StemPreset::BassOnly => [true, false, true, true],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_parse_round_trip() {
        for (i, stem) in Stem::ALL.iter().enumerate() {
            assert_eq!(stem.index(), i);
            assert_eq!(Stem::from_index(i), Some(*stem));
            assert_eq!(Stem::parse(stem.name()), Some(*stem));
        }
        assert_eq!(Stem::from_index(Stem::COUNT), None);
        assert_eq!(Stem::parse("guitar"), None);
        assert_eq!(Stem::parse("VOCALS"), Some(Stem::Vocals));
    }

    #[test]
    fn presets_mute_the_right_stems() {
        assert_eq!(StemPreset::Full.mute(), [false, false, false, false]);
        let acapella = StemPreset::Acapella.mute();
        assert!(!acapella[Stem::Vocals.index()]);
        assert!(acapella[Stem::Drums.index()] && acapella[Stem::Bass.index()]);
        let inst = StemPreset::Instrumental.mute();
        assert!(inst[Stem::Vocals.index()] && !inst[Stem::Drums.index()]);
        assert_eq!(StemPreset::parse("karaoke"), Some(StemPreset::Instrumental));
        assert_eq!(StemPreset::parse("nope"), None);
    }

    #[test]
    fn effective_position_combines_level_mute_and_solo() {
        let mut status = StemStatus {
            level: [0.0, 0.5, -0.25, 0.0],
            ..Default::default()
        };
        // No mute, no solo: the levels themselves.
        assert_eq!(status.effective_position(Stem::Bass), 0.5);
        // A mute masks that stem only.
        status.mute[Stem::Bass.index()] = true;
        assert_eq!(status.effective_position(Stem::Bass), -1.0);
        assert_eq!(status.effective_position(Stem::Drums), 0.0);
        // Solo overrides everybody's mute, including its own target's.
        status.solo = 1 << Stem::Vocals.index();
        assert_eq!(status.effective_position(Stem::Vocals), 0.0);
        assert_eq!(status.effective_position(Stem::Drums), -1.0);
        assert_eq!(status.effective_position(Stem::Bass), -1.0);
        assert!(status.any_solo() && status.is_soloed(Stem::Vocals));
    }
}
