//! Live settings: what depends on *this machine* rather than on the music.
//!
//! Kept apart from the mixer topology on purpose. A topology is a design decision (which chains,
//! which destinations, which curves) and gets edited as one; live settings are device facts — the
//! rate the sound card runs at, how much buffer the ring holds, where the stem model came from — and
//! are read once at startup.
//!
//! Everything here is validated in one place ([`LiveConfig::sanitised`]) so a nonsense file degrades
//! to something that runs (with a message) rather than to silence or a panic on the audio thread.

use serde::Deserialize;

/// The defaults, in one place: the audio engine and the shipped `profile/live.toml` agree by
/// construction.
pub const DEFAULT_SAMPLE_RATE: u32 = 44_100;
/// Output ring capacity, in frames (~93 ms at 44.1 kHz).
pub const DEFAULT_RING_FRAMES: usize = 4096;
/// Silence pre-filled into each ring before its stream starts, so the device is never waiting on the
/// producer's first block. Capped at half the ring.
pub const DEFAULT_PREFILL_FRAMES: usize = 2048;
/// Smallest ring worth opening: two blocks is the least that can absorb any jitter at all.
pub const MIN_RING_FRAMES: usize = 512;
/// Largest ring: past this, the latency a user hears is worse than any dropout it prevents.
pub const MAX_RING_FRAMES: usize = 65_536;

/// Device-facing settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AudioSettings {
    /// The rate the engine renders at *and* asks every device for. Material is resampled to it at
    /// load time, so this is the engine's clock, not a per-device setting.
    pub sample_rate: u32,
    /// Output ring capacity in frames. Bigger = more slack for a slow device, more latency.
    pub ring_frames: usize,
    /// Frames of silence written into each ring before its stream starts.
    pub prefill_frames: usize,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            ring_frames: DEFAULT_RING_FRAMES,
            prefill_frames: DEFAULT_PREFILL_FRAMES,
        }
    }
}

/// Where the stem model comes from and how it is run.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StemSettings {
    /// Model name, from the table in `hypermixx-stems` (`stem model list`).
    pub model: String,
    /// Execution provider: `cpu` or `cuda` (`gpu` is accepted as a spelling of the latter).
    pub provider: String,
    /// Window overlap, `0.0 ..= 0.5`: lower is faster and weights window edges more.
    pub overlap: f32,
    /// Ensemble shifts: `1` is one pass, `2` averages a second, shifted run for ~2× the time.
    pub shifts: usize,
    /// Frames per separation chunk.
    ///
    /// **Reserved**: today a track is separated in one job and installed when it is done. The number
    /// is here because the model is already windowed (HTDemucs takes 7.8 s at a time), so chunked
    /// separation with progressive install is a matter of feeding the deck as each chunk lands —
    /// and that setting belongs in this file rather than in a code constant.
    pub chunk_seconds: f32,
}

impl Default for StemSettings {
    fn default() -> Self {
        Self {
            model: "htdemucs".to_owned(),
            provider: "cpu".to_owned(),
            overlap: 0.25,
            shifts: 1,
            chunk_seconds: 0.0,
        }
    }
}

/// Everything that depends on the machine rather than on the music.
#[derive(Clone, Debug, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LiveConfig {
    pub audio: AudioSettings,
    pub stems: StemSettings,
}

impl LiveConfig {
    /// Clamps what can be clamped and resets what cannot, so a nonsense file still runs. Each
    /// substitution is the caller's to report: this is a *value* type and has no logger.
    pub fn sanitised(mut self) -> Self {
        let audio = &mut self.audio;
        // A rate no device will take is worse than the default: the output would be disabled and the
        // material resampled for nothing.
        if !(8_000..=384_000).contains(&audio.sample_rate) {
            audio.sample_rate = DEFAULT_SAMPLE_RATE;
        }
        audio.ring_frames = audio.ring_frames.clamp(MIN_RING_FRAMES, MAX_RING_FRAMES);
        audio.prefill_frames = audio.prefill_frames.min(audio.ring_frames / 2);
        let stems = &mut self.stems;
        if !stems.overlap.is_finite() {
            stems.overlap = StemSettings::default().overlap;
        }
        stems.overlap = stems.overlap.clamp(0.0, 0.5);
        stems.shifts = stems.shifts.clamp(1, 2);
        if !stems.chunk_seconds.is_finite() || stems.chunk_seconds < 0.0 {
            stems.chunk_seconds = 0.0;
        }
        if stems.model.trim().is_empty() {
            stems.model = StemSettings::default().model;
        }
        if stems.provider.trim().is_empty() {
            stems.provider = StemSettings::default().provider;
        }
        self
    }

    /// A one-line-per-key report, for `live show`.
    pub fn report(&self) -> Vec<String> {
        vec![
            format!(
                "audio  sample_rate {}Hz  ring {} frames  prefill {} frames",
                self.audio.sample_rate, self.audio.ring_frames, self.audio.prefill_frames
            ),
            format!(
                "stems  model {}  provider {}  overlap {:.2}  shifts {}  chunk {}",
                self.stems.model,
                self.stems.provider,
                self.stems.overlap,
                self.stems.shifts,
                if self.stems.chunk_seconds > 0.0 {
                    format!("{:.1}s", self.stems.chunk_seconds)
                } else {
                    "whole track".to_owned()
                }
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let live = LiveConfig::default();
        assert_eq!(live.audio.sample_rate, DEFAULT_SAMPLE_RATE);
        assert_eq!(live.audio.ring_frames, DEFAULT_RING_FRAMES);
        assert_eq!(live.stems.provider, "cpu", "a machine without a GPU never needs a flag");
        assert_eq!(live.stems.chunk_seconds, 0.0, "whole tracks today");
        assert_eq!(live.report().len(), 2);
        assert!(live.report()[0].contains("44100Hz"));
    }

    #[test]
    fn nonsense_is_clamped_rather_than_fatal() {
        let live = LiveConfig {
            audio: AudioSettings {
                sample_rate: 7,
                ring_frames: 99,
                prefill_frames: 999_999,
            },
            stems: StemSettings {
                overlap: 5.0,
                shifts: 99,
                chunk_seconds: -1.0,
                model: "  ".to_owned(),
                ..Default::default()
            },
        }
        .sanitised();
        assert_eq!(live.audio.sample_rate, DEFAULT_SAMPLE_RATE, "an impossible rate is replaced");
        assert_eq!(live.audio.ring_frames, MIN_RING_FRAMES, "clamped up");
        assert!(live.audio.prefill_frames <= live.audio.ring_frames / 2, "prefill cannot fill the ring");
        assert_eq!(live.stems.overlap, 0.5, "clamped to the useful range");
        assert_eq!(live.stems.shifts, 2, "two passes is the most worth offering");
        assert_eq!(live.stems.chunk_seconds, 0.0);
        assert_eq!(live.stems.model, "htdemucs");
        // A huge ring is clamped down as well, and a NaN never survives.
        let live = LiveConfig {
            audio: AudioSettings {
                ring_frames: usize::MAX,
                ..Default::default()
            },
            stems: StemSettings {
                overlap: f32::NAN,
                ..Default::default()
            },
        }
        .sanitised();
        assert_eq!(live.audio.ring_frames, MAX_RING_FRAMES);
        assert_eq!(live.stems.overlap, StemSettings::default().overlap);
    }
}
