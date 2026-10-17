//! [`Output`]: one sound-card stream plus the ring buffer that feeds it.
//!
//! This is the only module in the crate that names a cpal type, which is what keeps the rest of the
//! engine device-agnostic: the mixer produces blocks, an `Output` decides how they reach hardware.
//! Each output owns its own ring and its own stream, so "main + headphones" is two of these and
//! neither can starve the other.
//!
//! The pull model matters for correctness. The producer thread writes one block per tick into a ring
//! sized for a handful of blocks; the cpal callback drains it at the device clock. If the ring fills
//! (device slower than us) new samples are dropped, if it empties the callback pads with silence.
//! Either way neither side ever waits on the other, which is the whole reason a scratch buffer is
//! pre-allocated outside the callback: real-time code may not call `malloc`.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Consumer, Producer};

use hypermixx_core::{OutputRole, OutputState, OutputStatus};

use super::bus::Bus;
use super::config::OutputConfig;
use crate::ringbuf::{fill_with_silence_on_underrun, push_samples, AudioRingBuffer};
use crate::{CHANNELS, OUTPUT_RING_CAPACITY, PREFILL_FRAMES};

/// Identifies one output. Stable for the life of a `Mixer`; `MixerConfig` assigns them.
pub type OutputId = u8;

/// Per-destination buffer sizes, from the live config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputTuning {
    /// Ring capacity in frames. Bigger = more slack, more latency.
    pub ring_frames: usize,
    /// Frames of silence pushed before the stream starts, so the device never waits on the first
    /// block. Capped at half the ring.
    pub prefill_frames: usize,
}

impl Default for OutputTuning {
    fn default() -> Self {
        Self {
            ring_frames: OUTPUT_RING_CAPACITY,
            prefill_frames: PREFILL_FRAMES,
        }
    }
}

/// Why an output could not be opened.
#[derive(Clone, Debug, PartialEq)]
pub enum OutputError {
    /// The host has no output device at all.
    NoDevice,
    /// The device exists but offers no format this module can feed.
    UnsupportedFormat(String),
    /// cpal refused to build or start the stream.
    Stream(String),
}

impl OutputError {
    pub fn message(&self) -> String {
        match self {
            OutputError::NoDevice => "no output device".into(),
            OutputError::UnsupportedFormat(what) => {
                format!("device offers no usable format (engine wants {what})")
            }
            OutputError::Stream(what) => format!("stream: {what}"),
        }
    }
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for OutputError {}

/// One hardware destination: a cpal stream, its ring, and the channel mapping into it.
///
/// `_stream` is a field and never read: dropping it stops the sound card, which is exactly the
/// lifetime rule the mixer relies on — an `Output` in the vec is an output that is playing.
pub struct Output {
    pub id: OutputId,
    pub name: String,
    /// Whether this destination carries the mix or the cue (from the config, so a refresh can keep
    /// reporting it).
    pub role: OutputRole,
    /// `(left, right)` device channel indices this output writes to. `(0, 1)` on any stereo card;
    /// a 4-channel interface can carry main on `(0, 1)` and cue on `(2, 3)`.
    pub channels: (u16, u16),
    /// Frames per block, from the mixer, so the scratch buffers are sized once.
    block_frames: usize,
    producer: Option<Producer<f32>>,
    /// Some(_) only while the stream is live; a sink output has `None` and discards writes.
    _stream: Option<cpal::Stream>,
    /// Device-shaped staging buffer for [`Output::write`], sized at construction so a write never
    /// allocates.
    frame: Vec<f32>,
    device_channels: usize,
    /// Whether audio reaches a device, and if not, why. Writes are counted and dropped for every
    /// non-live state.
    state: OutputState,
    /// The cpal device name this output's stream lives on, `None` when none was found. Two outputs
    /// on the same device are the mixer's business to detect — the server (PipeWire's graph,
    /// ALSA's dmix) sums two streams with no limiter in sight.
    device: Option<String>,
    /// Output trim from [`OutputConfig::gain`] — the field existed but was never applied, so
    /// two destinations could not differ in level no matter what the config said.
    gain: f32,
    /// Producer-side overruns: blocks whose tail could not fit in the ring and was dropped.
    /// With the pipeline's room-gated production this stays at zero; a non-zero value means the
    /// device drained slower than the mixer produced.
    overruns: u64,
    writes: u64,
}

impl Output {
    /// Opens `cfg` against the default output device.
    ///
    /// No device at all is not an error: a headless machine (or a CI container) gets a sink output
    /// that accepts and discards audio, so the engine stays runnable and testable everywhere. A
    /// device that exists but cannot be fed is an error, because then the user should hear something
    /// and will not.
    pub fn open(
        cfg: &OutputConfig,
        sample_rate: u32,
        block_frames: usize,
    ) -> Result<Self, OutputError> {
        Self::open_with(cfg, sample_rate, block_frames, crate::mixer::OutputTuning::default())
    }

    /// As [`Output::open`], with the ring and prefill sizes spelled out (they come from the live
    /// config: how much slack a machine's device needs is a property of that machine).
    pub fn open_with(
        cfg: &OutputConfig,
        sample_rate: u32,
        block_frames: usize,
        tuning: OutputTuning,
    ) -> Result<Self, OutputError> {
        let host = cpal::default_host();
        // The config's own rate allow-list is checked first: it is a mistake in the file, and it is
        // worth reporting as such whether or not a device happens to be plugged in.
        if !cfg.allows_rate(sample_rate) {
            eprintln!(
                "[audio] {}: the engine runs at {sample_rate}Hz, which its `sample_rates` does not \
                 allow ({}); disabling it",
                cfg.name,
                describe_rates(&cfg.sample_rates)
            );
            return Ok(Self::placeholder(cfg, block_frames, OutputState::Disabled, None));
        }
        // A named device that is not there, or a device that cannot take the rate we need, are
        // *states*, not errors: the engine keeps running with the destination unrouted and a
        // `output refresh` retries it. Only a device that claims to work and then fails to build a
        // stream is an error.
        let found = match resolve_device(&host, cfg.device.as_deref()) {
            DeviceLookup::Default(None) => {
                eprintln!("[audio] {}: no output device, writing to a silent sink", cfg.name);
                return Ok(Self::placeholder(cfg, block_frames, OutputState::Sink, None));
            }
            DeviceLookup::Named(None) => {
                eprintln!(
                    "[audio] {}: no device matches `{}` yet; `output refresh` retries",
                    cfg.name,
                    cfg.device.as_deref().unwrap_or_default()
                );
                return Ok(Self::placeholder(cfg, block_frames, OutputState::Missing, None));
            }
            DeviceLookup::Default(Some(device)) | DeviceLookup::Named(Some(device)) => device,
        };
        let device_name = found.name().unwrap_or_else(|_| "audio device".into());

        // The device has to offer the engine's rate. Refused rather than papered over: a device
        // pinned to 48k while the engine produces 44.1k plays every track 8.8% slow, which is worse
        // than silence because it sounds intentional.
        let Some(config) = preferred_config(&found, sample_rate) else {
            let offered = describe_device_rates(&found);
            eprintln!(
                "[audio] {}: {device_name} does not accept {sample_rate}Hz (offers {offered}); \
                 disabling it",
                cfg.name
            );
            return Ok(Self::placeholder(cfg, block_frames, OutputState::Disabled, Some(device_name)));
        };
        let (device_channels, format) = (config.channels() as usize, config.sample_format());
        eprintln!(
            "[audio] {}: {device_name} @ {}/{device_channels}ch/{format:?}",
            cfg.name,
            config.sample_rate().0
        );
        let mapping = cfg.channel_pair(device_channels);
        let mut output = Self {
            id: cfg.id,
            name: cfg.name.clone(),
            role: cfg.role,
            channels: mapping,
            block_frames,
            producer: None,
            _stream: None,
            frame: vec![0.0; block_frames * device_channels.max(1)],
            device_channels: device_channels.max(1),
            state: OutputState::Live,
            device: Some(device_name.clone()),
            gain: cfg.gain_or_unity(),
            overruns: 0,
            writes: 0,
        };

        let ring_frames = tuning.ring_frames.max(block_frames);
        let (mut producer, consumer) = AudioRingBuffer::new(ring_frames * CHANNELS).split();
        // Prefill: the callback starts draining the instant the stream is built. Without a
        // cushion the first seconds are a race between the producer's first blocks and the
        // device, and every lost race is a silence gap the listener hears as a click. Half a
        // ring of silence buys the producer a standing head start; the mix simply starts
        // ~46 ms later, which nothing can hear.
        let prefill = (tuning.prefill_frames * CHANNELS).min(ring_frames * CHANNELS / 2);
        if prefill > 0 {
            let silence = vec![0.0f32; prefill];
            push_samples(&mut producer, &silence);
        }
        output.producer = Some(producer);
        let stream = build_stream(&found, &config, consumer, &cfg.name)?;
        output._stream = Some(stream);
        Ok(output)
    }

    /// An output that accepts audio and throws it away — the headless case, and the default for
    /// tests that only care about the mix.
    pub fn sink(cfg: &OutputConfig, block_frames: usize) -> Self {
        Self::placeholder(cfg, block_frames, OutputState::Sink, None)
    }

    /// A destination that takes audio and keeps nothing: the headless case, a device that is not
    /// there, or one that cannot take the engine's rate. The three are told apart by `state`.
    pub fn placeholder(
        cfg: &OutputConfig,
        block_frames: usize,
        state: OutputState,
        device: Option<String>,
    ) -> Self {
        Self {
            id: cfg.id,
            name: cfg.name.clone(),
            role: cfg.role,
            channels: (0, 1),
            block_frames,
            producer: None,
            _stream: None,
            frame: vec![0.0; block_frames],
            device_channels: CHANNELS,
            state,
            device,
            gain: cfg.gain_or_unity(),
            overruns: 0,
            writes: 0,
        }
    }

    /// A named sink with the default mapping, for tests and for "no output configured".
    pub fn null(id: OutputId, block_frames: usize) -> Self {
        Self::sink(&OutputConfig::master(id, "null"), block_frames)
    }

    /// Whether a device is actually being fed.
    pub fn is_live(&self) -> bool {
        self.state.is_live()
    }

    /// Why this output is not live, or `Live`.
    pub fn state(&self) -> OutputState {
        self.state
    }

    /// What a front-end reports for this destination.
    pub fn status(&self) -> OutputStatus {
        OutputStatus {
            name: self.name.clone(),
            role: self.role,
            state: self.state,
            device: self.device.clone(),
            channels: self.channels,
            gain: self.gain,
            writes: self.writes,
            overruns: self.overruns,
        }
    }

    pub fn role(&self) -> OutputRole {
        self.role
    }

    /// The cpal device name this output's stream lives on, or `None` for a sink.
    pub fn device(&self) -> Option<&str> {
        self.device.as_deref()
    }

    /// The trim applied on write, from [`OutputConfig::gain`].
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Blocks written since construction. Together with [`Output::overruns`] this says whether a
    /// device is keeping up.
    pub fn writes(&self) -> u64 {
        self.writes
    }

    /// Samples the ring can still take, or `None` for a sink (which takes anything and keeps nothing).
    pub fn free_samples(&self) -> Option<usize> {
        self.producer.as_ref().map(Producer::slots)
    }

    /// Frames of room in this output, assuming a stereo-shaped ring. `None` for a sink.
    pub fn free_frames(&self) -> Option<usize> {
        self.free_samples().map(|free| free / self.device_channels.max(1))
    }

    /// Producer-side overruns since construction: samples the ring could not take and were
    /// dropped. Callback-side underruns (silence padding) are *not* counted anywhere today — a
    /// padded gap and a genuinely silent block look identical from the consumer side, so this
    /// counter is the honest half of the story.
    pub fn overruns(&self) -> u64 {
        self.overruns
    }

    /// Hands one block to the device.
    ///
    /// `bus` is stereo; it is expanded to the device's channel count at [`Output::channels`], which
    /// is why this takes the bus rather than samples — the mapping is the output's business.
    pub fn write(&mut self, bus: &Bus) {
        if !self.state.is_live() || self.producer.is_none() {
            self.writes += 1;
            return;
        }
        let frames = bus.frames().min(self.block_frames);
        if frames == 0 {
            return;
        }
        // De-interleave -> remap -> trim -> interleave into the device-shaped frame buffer, then
        // one push. The trim is the output's own gain, applied here because it is per-destination
        // (a quiet pair of speakers, a hot headphone amp), not per-signal.
        self.frame[..frames * self.device_channels].fill(0.0);
        let (l, r) = (self.channels.0 as usize, self.channels.1 as usize);
        let gain = self.gain;
        for i in 0..frames {
            let base = i * self.device_channels;
            match (l < self.device_channels, r < self.device_channels) {
                (true, true) => {
                    self.frame[base + l] = bus.l[i] * gain;
                    self.frame[base + r] = bus.r[i] * gain;
                }
                // A mono destination gets the sum, not the left channel: a cue on a 1ch device must
                // still tell you what is happening on both sides.
                _ => {
                    self.frame[base] = (bus.l[i] + bus.r[i]) * 0.5 * gain;
                }
            }
        }
        let len = frames * self.device_channels;
        if let Some(producer) = self.producer.as_mut() {
            let written = push_samples(producer, &self.frame[..len]);
            if written < len {
                self.overruns += 1;
            }
        }
        self.writes += 1;
    }

    /// The scratch block size this output was built for.
    pub fn block_frames(&self) -> usize {
        self.block_frames
    }
}

/// What a device pattern resolved to.
enum DeviceLookup {
    /// No pattern was configured: the host's default device, if it has one.
    Default(Option<cpal::Device>),
    /// A pattern was configured: the matching device, or nothing.
    Named(Option<cpal::Device>),
}

/// Resolves a `device` pattern against the host's outputs: an all-digit pattern is an **index** into
/// the enumeration, anything else is a **case-insensitive name substring** (first match wins).
/// No pattern means the system default device.
///
/// The enumeration is the only place a device name comes from, so `output list` and the resolution
/// agree by construction.
fn resolve_device(host: &cpal::Host, pattern: Option<&str>) -> DeviceLookup {
    let Some(pattern) = pattern.map(str::trim).filter(|p| !p.is_empty()) else {
        return DeviceLookup::Default(host.default_output_device());
    };
    let mut devices: Vec<(String, cpal::Device)> = host
        .output_devices()
        .map(|devices| devices.map(|d| (device_name(&d), d)).collect())
        .unwrap_or_default();
    let names: Vec<String> = devices.iter().map(|(name, _)| name.clone()).collect();
    DeviceLookup::Named(match_device(&names, pattern).map(|index| devices.swap_remove(index).1))
}

/// Which device a pattern names, given the names in enumeration order: all digits is an index,
/// anything else a case-insensitive substring (first match wins). Pure, so the rule is testable
/// without a sound card.
fn match_device(names: &[String], pattern: &str) -> Option<usize> {
    let pattern = pattern.trim();
    if let Ok(index) = pattern.parse::<usize>() {
        return (index < names.len()).then_some(index);
    }
    let needle = pattern.to_ascii_lowercase();
    names
        .iter()
        .position(|name| name.to_ascii_lowercase().contains(&needle))
}

/// A device's name, or a stand-in when the backend will not say.
fn device_name(device: &cpal::Device) -> String {
    device.name().unwrap_or_else(|_| "audio device".into())
}

/// `[44100]` / `[44100, 48000]` / `any` — for a warning that has to be readable.
fn describe_rates(rates: &[u32]) -> String {
    if rates.is_empty() {
        return format!("defaults to {}Hz", hypermixx_core::SAMPLE_RATE);
    }
    rates
        .iter()
        .map(|rate| format!("{rate}Hz"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What rates a device offers, for the "it cannot take ours" warning.
fn describe_device_rates(device: &cpal::Device) -> String {
    let Ok(ranges) = device.supported_output_configs() else {
        return "nothing it will tell us".into();
    };
    let mut seen: Vec<u32> = Vec::new();
    for range in ranges {
        for rate in [range.min_sample_rate().0, range.max_sample_rate().0] {
            if !seen.contains(&rate) {
                seen.push(rate);
            }
        }
    }
    seen.sort_unstable();
    seen.iter()
        .map(|rate| format!("{rate}Hz"))
        .collect::<Vec<_>>()
        .join("..")
}

/// Asks the device for `sample_rate` stereo, preferring native f32.
///
/// Unlike the old version this does **not** fall back to the device's default config: a device that
/// cannot run at the engine's rate is refused (and the output disabled), because a silent fallback
/// to another rate plays everything at the wrong speed.
fn preferred_config(device: &cpal::Device, sample_rate: u32) -> Option<cpal::SupportedStreamConfig> {
    let ranges = device.supported_output_configs().ok()?;
    let wanted = CHANNELS as u16;
    ranges
        .filter(|range| {
            range.channels() == wanted
                && range.min_sample_rate().0 <= sample_rate
                && range.max_sample_rate().0 >= sample_rate
        })
        // A native f32 path avoids a per-sample quantisation step in the callback.
        .max_by_key(|range| range.sample_format() == cpal::SampleFormat::F32)
        .map(|range| range.with_sample_rate(cpal::SampleRate(sample_rate)))
}

/// Builds and starts the stream. The consumer is moved into the callback, which is why the ring's two
/// ends are owned rather than shared.
fn build_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut consumer: Consumer<f32>,
    name: &str,
) -> Result<cpal::Stream, OutputError> {
    let stream_config = config.config();
    let name = name.to_owned();
    let on_error = move |err: cpal::StreamError| eprintln!("[audio] {name}: stream error: {err}");
    let built = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            &stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                fill_with_silence_on_underrun(&mut consumer, data);
            },
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => {
            // Sized here, outside the real-time callback.
            let mut scratch: Vec<f32> = vec![0.0; data_capacity(config)];
            device.build_output_stream(
                &stream_config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    grow(&mut scratch, data.len());
                    let (buffer, _) = scratch.split_at_mut(data.len());
                    fill_with_silence_on_underrun(&mut consumer, buffer);
                    for (out, sample) in data.iter_mut().zip(&*buffer) {
                        *out = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                    }
                },
                on_error,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let mut scratch: Vec<f32> = vec![0.0; data_capacity(config)];
            device.build_output_stream(
                &stream_config,
                move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                    grow(&mut scratch, data.len());
                    let (buffer, _) = scratch.split_at_mut(data.len());
                    fill_with_silence_on_underrun(&mut consumer, buffer);
                    for (out, sample) in data.iter_mut().zip(&*buffer) {
                        *out = ((sample.clamp(-1.0, 1.0) + 1.0) * 0.5 * f32::from(u16::MAX)) as u16;
                    }
                },
                on_error,
                None,
            )
        }
        format => {
            return Err(OutputError::UnsupportedFormat(format!("{format:?}")));
        }
    };
    let stream = built.map_err(|err| OutputError::Stream(err.to_string()))?;
    stream.play().map_err(|err| OutputError::Stream(err.to_string()))?;
    Ok(stream)
}

/// Room for a generous callback buffer: the whole output ring, so `grow` is a cold path.
fn data_capacity(_config: &cpal::SupportedStreamConfig) -> usize {
    OUTPUT_RING_CAPACITY * CHANNELS * 8
}

fn grow(scratch: &mut Vec<f32>, needed: usize) {
    if scratch.len() < needed {
        scratch.resize(needed, 0.0);
    }
}

/// The set of outputs a mixer drives. Kept as a plain `Vec` so indices in `MixerConfig` (which is
/// what `MasterBus::outputs` and `CueBus::outputs` name) are just positions here.
#[derive(Default)]
pub struct Outputs {
    items: Vec<Output>,
}

impl Outputs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, output: Output) -> OutputId {
        self.items.push(output);
        (self.items.len() - 1) as OutputId
    }

    pub fn get(&self, index: usize) -> Option<&Output> {
        self.items.get(index)
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut Output> {
        self.items.get_mut(index)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Output> {
        self.items.iter()
    }

    /// Sends one block to every named output. An out-of-range index is skipped rather than panicking
    /// the audio thread: a mis-typed config should degrade, not kill playback.
    pub fn write_many(&mut self, indices: &[usize], bus: &Bus) {
        for index in indices {
            if let Some(output) = self.items.get_mut(*index) {
                output.write(bus);
            }
        }
    }

    /// Every destination's binding, for `output list`.
    pub fn statuses(&self) -> Vec<OutputStatus> {
        self.items.iter().map(Output::status).collect()
    }

    /// Total blocks accepted, across every output — a liveness check for tests.
    pub fn writes(&self) -> u64 {
        self.items.iter().map(Output::writes).sum()
    }

    /// Mutable access to the outputs, for a caller renumbering or swapping them.
    pub(crate) fn items_mut(&mut self) -> std::slice::IterMut<'_, Output> {
        self.items.iter_mut()
    }

    /// True when every live output can take another `frames` without dropping a sample. A mixer with
    /// no outputs (headless, or a unit test) always has room, which is what lets the producer pace
    /// itself on the clock instead of deadlocking on a device that will never drain.
    pub fn has_room(&self, frames: usize) -> bool {
        self.iter()
            .all(|output| output.free_frames().is_none_or(|free| free >= frames))
    }
}

impl Clone for Output {
    /// Only ever used to build fresh sinks in tests: a cpal stream is not cloneable, so this makes a
    /// disconnected sink with the same identity rather than pretending to share one.
    fn clone(&self) -> Self {
        Self::sink(
            &OutputConfig {
                id: self.id,
                name: self.name.clone(),
                device: self.device.clone(),
                channels: self.channels,
                role: self.role,
                sample_rates: Vec::new(),
                gain: self.gain,
            },
            self.block_frames,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BLOCK_SIZE, SAMPLE_RATE};

    fn bus(left: f32, right: f32) -> Bus {
        let mut b = Bus::new(BLOCK_SIZE);
        b.fill_from(left, right);
        b
    }

    #[test]
    fn a_sink_counts_writes_and_holds_nothing() {
        let mut out = Output::null(0, BLOCK_SIZE);
        assert!(!out.is_live());
        out.write(&bus(1.0, -1.0));
        out.write(&bus(1.0, -1.0));
        assert_eq!(out.writes(), 2);
        assert_eq!(out.overruns(), 0);
        // A short or empty bus must not panic; the sink just counts what it got.
        out.write(&Bus::new(0));
        out.write(&Bus::new(4));
        assert!(out.writes() >= 3);
    }

    #[test]
    fn outputs_are_addressed_by_position() {
        let mut set = Outputs::new();
        assert!(set.is_empty());
        assert_eq!(set.push(Output::null(0, BLOCK_SIZE)), 0);
        assert_eq!(set.push(Output::null(1, BLOCK_SIZE)), 1);
        assert_eq!(set.len(), 2);
        set.write_many(&[0, 1, 99], &bus(0.5, 0.5));
        assert_eq!(set.writes(), 2);
        assert_eq!(set.get(1).unwrap().name, "null");
        assert!(set.get(2).is_none());
        // No outputs and no targets is a no-op, not an error.
        let mut empty = Outputs::new();
        empty.write_many(&[], &bus(1.0, 1.0));
        assert_eq!(empty.writes(), 0);
    }

    /// A device pattern is a name substring, or an index when it is all digits. The rule is pure so
    /// it can be pinned without a sound card.
    #[test]
    fn device_patterns_resolve_by_index_or_name_substring() {
        let names = vec![
            "Built-in Audio Analog Stereo".to_owned(),
            "USB Headset".to_owned(),
            "Speakers (Realtek)".to_owned(),
        ];
        // An index picks that device, in enumeration order.
        assert_eq!(match_device(&names, "0"), Some(0));
        assert_eq!(match_device(&names, "2"), Some(2));
        assert_eq!(match_device(&names, "9"), None, "out of range is no match");
        // A substring matches case-insensitively, first hit wins.
        assert_eq!(match_device(&names, "usb"), Some(1));
        assert_eq!(match_device(&names, "REALTEK"), Some(2));
        assert_eq!(match_device(&names, "audio"), Some(0), "first match wins");
        assert_eq!(match_device(&names, "  usb  "), Some(1), "trimmed");
        assert_eq!(match_device(&names, "firewire"), None);
    }

    /// A destination that is configured but unavailable is a *state*, not a failed build: the mixer
    /// still comes up, the output is listed, and `refresh` reports it again.
    #[test]
    fn an_unavailable_destination_is_a_state_not_a_failure() {
        // A device pattern nothing will match.
        let ghost = OutputConfig {
            device: Some("hypermixx-no-such-device".into()),
            ..OutputConfig::master(0, "ghost")
        };
        let out = Output::open(&ghost, SAMPLE_RATE, BLOCK_SIZE).expect("builds");
        assert_eq!(out.state(), OutputState::Missing);
        assert!(!out.is_live());
        assert_eq!(out.status().name, "ghost");

        // A rate the config does not allow: the engine rate has to be in `sample_rates`.
        let wrong_rate = OutputConfig {
            sample_rates: vec![48_000],
            ..OutputConfig::master(0, "48k-only")
        };
        let out = Output::open(&wrong_rate, SAMPLE_RATE, BLOCK_SIZE).expect("builds");
        assert_eq!(out.state(), OutputState::Disabled, "a rate mismatch disables the output");
        assert!(wrong_rate.allows_rate(48_000));
        assert!(!wrong_rate.allows_rate(SAMPLE_RATE));
    }

    /// The engine rate is allowed by default; anything else is not.
    #[test]
    fn the_default_rate_allow_list_is_the_engine_rate() {
        let cfg = OutputConfig::master(0, "x");
        assert!(cfg.sample_rates.is_empty(), "the default is empty, not a literal 44.1k");
        assert!(cfg.allows_rate(SAMPLE_RATE));
        assert!(!cfg.allows_rate(48_000));
    }

    /// The device may be absent (CI) or present (a real card). Either way `open` must not panic, and
    /// must never hand back an output that silently produces nothing while claiming to be live.
    #[test]
    fn open_either_succeeds_or_reports_a_real_failure() {
        let cfg = OutputConfig::master(0, "probe");
        match Output::open(&cfg, crate::SAMPLE_RATE, BLOCK_SIZE) {
            Ok(out) => {
                // A sink means "no device"; anything else means a started stream.
                assert_eq!(!out.is_live(), out._stream.is_none(), "state/stream disagree");
            }
            Err(err) => assert!(!err.message().is_empty()),
        }
    }
}
