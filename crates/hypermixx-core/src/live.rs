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

/// Keylock STFT window defaults: 2048/512 at 44.1 kHz is ~46 ms of read-ahead.
pub const DEFAULT_KEYLOCK_BLOCK: usize = 2048;
pub const DEFAULT_KEYLOCK_INTERVAL: usize = 512;
/// Smallest useful keylock FFT window.
pub const MIN_KEYLOCK_BLOCK: usize = 256;
/// Largest keylock FFT window: more fidelity, but ~186 ms of read-ahead at 44.1 kHz.
pub const MAX_KEYLOCK_BLOCK: usize = 8192;
/// Smallest STFT hop. Must stay below half the block.
pub const MIN_KEYLOCK_INTERVAL: usize = 32;
/// The most threads a config may ask for. Past any real machine's core count, so a typo cannot reach
/// the runtime.
pub const MAX_THREADS: usize = 256;

/// The bundled model's window, in seconds (its ONNX export pins 343 980 samples at 44.1 kHz). Used
/// only to *describe* the streaming cadence in `live show` — the runtime owns the real number.
const SEGMENT_SECONDS: f64 = 7.8;

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

/// The keylock engine's STFT window, a machine/latency tradeoff rather than a per-track choice.
///
/// A bigger block resolves bass and dense material better, at the cost of more read-ahead before
/// the engine can produce sound. The interval is the hop: `block / 4` is the DJ preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct KeylockSettings {
    /// FFT block length, in frames. Clamped to a power of two in `256 ..= 8192`.
    pub block: usize,
    /// STFT hop, in frames. Clamped to a power of two in `32 ..= block / 2`.
    pub interval: usize,
}

impl Default for KeylockSettings {
    fn default() -> Self {
        Self {
            block: DEFAULT_KEYLOCK_BLOCK,
            interval: DEFAULT_KEYLOCK_INTERVAL,
        }
    }
}

/// Everything the engine asks the separator for: the model, the session it runs in, and the two
/// dials that change the produced audio.
///
/// All of it lives in `live.toml` rather than on a command line, because that is what it is: a
/// property of *this machine* (how many cores to leave for playback, how much memory there is, which
/// model the operator settled on), not of the track being separated.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StemSettings {
    /// Model name, from the table in `hypermixx-stems` (`stem model list`).
    pub model: String,
    /// Execution provider: `cpu` or `cuda` (`gpu` is accepted as a spelling of the latter).
    pub provider: String,
    /// Window overlap, `0.0 ..= 0.5`: lower is faster and weights window edges more.
    pub overlap: f32,
    /// Threads the separator may use. `0` = the runtime decides (every core).
    ///
    /// On a small device this is not a performance knob but a **playback** one: ONNX Runtime takes
    /// every core by default, and a separation that saturates the machine starves the audio callback —
    /// the mix stutters *while* the stems are being prepared. `2` leaves room for the music.
    pub threads: usize,
    /// Trade throughput for peak memory: on HTDemucs, ONNX Runtime's constant folding materialises
    /// several gigabytes of index tensors at load, which does not fit a small device. Off is the
    /// fastest setting and needs ~5.6 GB; on is the default and needs ~2.1 GB for ~11% less speed.
    pub low_memory: bool,
    /// Ensemble shifts: `1` is one pass (and the only setting that **streams**: blocks land in the
    /// deck as the model finishes them). `2` averages a second, shifted run for ~2× the time, but the
    /// runtime cannot shift a stream, so that path separates the whole track before installing it.
    pub shifts: usize,
}

impl Default for StemSettings {
    fn default() -> Self {
        Self {
            model: "htdemucs".to_owned(),
            provider: "cpu".to_owned(),
            overlap: 0.25,
            shifts: 1,
            threads: 0,
            low_memory: true,
        }
    }
}

/// Everything that depends on the machine rather than on the music.
#[derive(Clone, Debug, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LiveConfig {
    pub audio: AudioSettings,
    /// Keylock STFT window (read at startup; changing it requires a restart).
    pub keylock: KeylockSettings,
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
        let keylock = &mut self.keylock;
        // Signalsmith's FFT needs a power-of-two block, and the hop cannot exceed half of it.
        keylock.block = keylock
            .block
            .clamp(MIN_KEYLOCK_BLOCK, MAX_KEYLOCK_BLOCK)
            .next_power_of_two()
            .min(MAX_KEYLOCK_BLOCK);
        let max_interval = keylock.block / 2;
        keylock.interval = keylock
            .interval
            .clamp(MIN_KEYLOCK_INTERVAL, max_interval)
            .next_power_of_two()
            .min(max_interval);
        let stems = &mut self.stems;
        if !stems.overlap.is_finite() {
            stems.overlap = StemSettings::default().overlap;
        }
        stems.overlap = stems.overlap.clamp(0.0, 0.5);
        stems.shifts = stems.shifts.clamp(1, 2);
        // A thread count past any real machine is a typo, and handing ONNX Runtime a nonsense number
        // is worse than ignoring it: clamp to something a machine could have, and let 0 mean "auto".
        stems.threads = stems.threads.min(MAX_THREADS);
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
                "keylock block {} interval {} frames (~{:.0} ms read-ahead at {}Hz)",
                self.keylock.block,
                self.keylock.interval,
                self.keylock.block as f64 * 1000.0 / f64::from(self.audio.sample_rate),
                self.audio.sample_rate
            ),
            format!(
                "stems  model {}  provider {}  overlap {:.2}  shifts {}  threads {}  memory {}  {}",
                self.stems.model,
                self.stems.provider,
                self.stems.overlap,
                self.stems.shifts,
                if self.stems.threads == 0 {
                    "auto".to_owned()
                } else {
                    self.stems.threads.to_string()
                },
                if self.stems.low_memory { "low" } else { "max-speed" },
                // The block length is the model's own window stride, so it is reported rather than
                // configured: `(1 - overlap) × segment`.
                if self.stems.shifts > 1 {
                    "whole track (a shift ensemble cannot stream)".to_owned()
                } else {
                    format!(
                        "streamed in ~{:.1}s blocks",
                        (1.0 - self.stems.overlap) as f64 * SEGMENT_SECONDS
                    )
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
        assert_eq!(live.keylock, KeylockSettings::default());
        assert_eq!(live.stems.provider, "cpu", "a machine without a GPU never needs a flag");

        assert_eq!(live.report().len(), 3);
        assert!(live.report()[0].contains("44100Hz"));
        assert!(live.report()[1].contains("keylock block 2048 interval 512"));
    }

    #[test]
    fn nonsense_is_clamped_rather_than_fatal() {
        let live = LiveConfig {
            audio: AudioSettings {
                sample_rate: 7,
                ring_frames: 99,
                prefill_frames: 999_999,
            },
            keylock: KeylockSettings {
                block: 3_000,
                interval: 5_000,
            },
            stems: StemSettings {
                overlap: 5.0,
                shifts: 99,
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
        assert!(live.stems.low_memory, "a small device is the default assumption");
        // A thread count past any real machine is clamped rather than handed to the runtime.
        let silly = LiveConfig {
            stems: StemSettings {
                threads: 100_000,
                ..Default::default()
            },
            ..Default::default()
        }
        .sanitised();
        assert_eq!(silly.stems.threads, MAX_THREADS);
        // ...and `0` keeps meaning "the runtime decides", because it is not clamped up.
        assert_eq!(
            LiveConfig {
                stems: StemSettings {
                    threads: 0,
                    ..Default::default()
                },
                ..Default::default()
            }
            .sanitised()
            .stems
            .threads,
            0
        );
        // A negative chunk is nonsense, and the safe reading of nonsense is "whole track" rather
        // than "a hundred tiny chunks".
        assert_eq!(live.stems.model, "htdemucs");
        assert!(live.keylock.block.is_power_of_two());
        assert!(live.keylock.interval.is_power_of_two());
        assert!(live.keylock.interval <= live.keylock.block / 2);
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
            ..Default::default()
        }
        .sanitised();
        assert_eq!(live.audio.ring_frames, MAX_RING_FRAMES);
        assert_eq!(live.stems.overlap, StemSettings::default().overlap);
    }
}
