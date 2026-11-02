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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

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
    /// Device buffer in frames, or `None` for whatever the backend chooses. On ALSA the backend
    /// default is a 25 ms period and a 100 ms buffer; `Some(512)` asks for a 128-frame period
    /// (~2.9 ms) and a 512-frame buffer (~11.6 ms).
    ///
    /// Best effort: a PCM path that refuses a fixed buffer (ALSA's `dmix`/`plug` behind
    /// `sysdefault`, i.e. most USB interfaces) gets the backend default instead of a failed build.
    /// Latency is a preference; silence is not.
    pub device_buffer_frames: Option<usize>,
}

impl Default for OutputTuning {
    fn default() -> Self {
        Self {
            ring_frames: OUTPUT_RING_CAPACITY,
            prefill_frames: PREFILL_FRAMES,
            device_buffer_frames: None,
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

/// What a device's callbacks look like, shared between the audio callback and the producer thread.
///
/// The size matters beyond diagnostics: cpal's ALSA backend hands the callback *all* the PCM
/// buffer's free space, so a ring smaller than one callback underruns on every callback and the
/// padded silence makes the mix crawl.
#[derive(Debug, Default)]
struct CallbackStats {
    /// Biggest chunk the device has asked for, in frames.
    frames: AtomicUsize,
    /// Callbacks that were short of ring.
    starved: AtomicU64,
    /// Samples that had to be padded with silence.
    padded: AtomicU64,
}

impl CallbackStats {
    fn record(&self, frames: usize) {
        self.frames.fetch_max(frames, Ordering::Relaxed);
    }

    fn padded(&self, samples: usize) {
        if samples > 0 {
            self.starved.fetch_add(1, Ordering::Relaxed);
            self.padded.fetch_add(samples as u64, Ordering::Relaxed);
        }
    }
}

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
    /// What the device's callbacks look like, written from the audio thread (see [`CallbackStats`]).
    callback: Arc<CallbackStats>,
    /// This output's ring capacity in frames, so a callback can be compared with it.
    ring_frames: usize,
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
        Self::open_with(
            cfg,
            sample_rate,
            block_frames,
            crate::mixer::OutputTuning::default(),
        )
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
        if !cfg.allows_engine_rate(sample_rate) {
            eprintln!(
                "[audio] {}: the engine runs at {sample_rate}Hz, which its `sample_rates` does not \
                 allow ({}); disabling it",
                cfg.name,
                describe_rates(&cfg.sample_rates)
            );
            return Ok(Self::placeholder(
                cfg,
                block_frames,
                OutputState::Disabled,
                None,
            ));
        }
        // A named device that is not there, or a device that cannot take the rate we need, are
        // *states*, not errors: the engine keeps running with the destination unrouted and a
        // `output refresh` retries it. Only a device that claims to work and then fails to build a
        // stream is an error.
        let found = match resolve_device(&host, cfg.device.as_deref()) {
            DeviceLookup::Default(None) => {
                eprintln!(
                    "[audio] {}: no output device, writing to a silent sink",
                    cfg.name
                );
                return Ok(Self::placeholder(
                    cfg,
                    block_frames,
                    OutputState::Sink,
                    None,
                ));
            }
            DeviceLookup::Named { device: None, seen } => {
                // The usual cause is not "unplugged": cpal's ALSA enumeration only lists PCMs it
                // can open right now, so a card the sound server (PipeWire/PulseAudio) currently
                // holds is absent from the list rather than reported as busy. Saying so is what
                // turns "no sound and no reason" into an actionable line.
                eprintln!(
                    "[audio] {}: no output device matches `{}` ({} device(s) visible; it may be \
                     unplugged or held by the sound server — a `default`/`pipewire` destination goes \
                     through the server instead); `output refresh` retries",
                    cfg.name,
                    cfg.device.as_deref().unwrap_or_default(),
                    seen
                );
                return Ok(Self::placeholder(
                    cfg,
                    block_frames,
                    OutputState::Missing,
                    None,
                ));
            }
            DeviceLookup::Default(Some(device))
            | DeviceLookup::Named {
                device: Some(device),
                ..
            } => device,
        };
        let device_name = found.name().unwrap_or_else(|_| "audio device".into());

        // The device has to offer the engine's rate, in a format we can actually write. Refused
        // rather than papered over: a device pinned to 48k while the engine produces 44.1k plays
        // every track 8.8% slow, which is worse than silence because it sounds intentional. A
        // device that is 24-bit-only (many USB DJ controllers) is refused *here*, with the reason,
        // instead of being handed a format `build_stream` cannot feed and turning into a sink later.
        let Some(config) = preferred_config(&found, sample_rate, cfg.wanted_device_channels())
        else {
            eprintln!(
                "[audio] {}: {device_name} {}; disabling it",
                cfg.name,
                unsupported_message(&found, sample_rate)
            );
            return Ok(Self::placeholder(
                cfg,
                block_frames,
                OutputState::Disabled,
                Some(device_name),
            ));
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
            callback: Arc::new(CallbackStats::default()),
            ring_frames: 0,
        };

        let ring_frames = tuning.ring_frames.max(block_frames);
        output.ring_frames = ring_frames;
        let ring_channels = output.device_channels.max(1);
        // The latency hint is a *preference*. Some paths (ALSA's `dmix`/`plug` behind
        // `sysdefault`/`default`, which is how a USB DJ controller is usually reached) reject a
        // fixed buffer outright, and failing the build for that would turn a working device into a
        // silent sink. So the fixed request is retried once with the backend's own default — which
        // needs a fresh ring, because the failed attempt's callback took the consumer with it.
        let mut device_buffer_frames = tuning.device_buffer_frames;
        let stream = loop {
            let (mut producer, consumer) =
                AudioRingBuffer::with_channels(ring_frames, ring_channels).split();
            // Prefill: the callback starts draining the instant the stream is built. Without a
            // cushion the first seconds are a race between the producer's first blocks and the
            // device, and every lost race is a silence gap the listener hears as a click. Half a
            // ring of silence buys the producer a standing head start; the mix simply starts
            // ~46 ms later, which nothing can hear.
            let prefill =
                (tuning.prefill_frames * ring_channels).min(ring_frames * ring_channels / 2);
            if prefill > 0 {
                let silence = vec![0.0f32; prefill];
                push_samples(&mut producer, &silence);
            }
            output.producer = Some(producer);
            match build_stream(
                &found,
                &config,
                consumer,
                &cfg.name,
                device_buffer_frames,
                Arc::clone(&output.callback),
                output.device_channels,
            ) {
                Ok(stream) => break stream,
                Err(err) if device_buffer_frames.is_some() => {
                    eprintln!(
                        "[audio] {}: fixed device buffer rejected ({err}); using the backend's \
                         default instead",
                        cfg.name
                    );
                    device_buffer_frames = None;
                }
                Err(err) => return Err(err),
            }
        };
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
            callback: Arc::new(CallbackStats::default()),
            ring_frames: 0,
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
            starved: self.starved_callbacks(),
            padded: self.padded_samples(),
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

    /// Frames of room in this output, in the device's channel layout. `None` for a sink.
    pub fn free_frames(&self) -> Option<usize> {
        self.free_samples()
            .map(|free| free / self.device_channels.max(1))
    }

    /// Producer-side overruns since construction: writes whose whole block could not fit in the
    /// ring. Each event is counted once, regardless of how many samples were dropped. Callback-side
    /// underruns (silence padding) are *not* counted anywhere today — a padded gap and a genuinely
    /// silent block look identical from the consumer side, so this counter is the honest half of the
    /// story.
    pub fn overruns(&self) -> u64 {
        self.overruns
    }

    /// `Some(frames)` when the device's callback asks for more frames than the ring can hold.
    ///
    /// That is not a tuning preference: the callback gets `avail_frames` from ALSA, so a ring
    /// smaller than one callback underruns *every* callback, and the padded silence makes the mix
    /// play slow and broken. The mixer reopens the destination with a ring that fits (see
    /// [`Mixer::widen_output_rings`](super::Mixer::widen_output_rings)).
    pub fn ring_shortfall(&self) -> Option<usize> {
        if !self.is_live() {
            return None;
        }
        let wanted = self.callback.frames.load(Ordering::Relaxed);
        shortfall(self.ring_frames, self.block_frames, wanted)
    }

    /// Callbacks that ran out of ring and were padded with silence. A non-zero, growing count means
    /// the mix is gapped: the ring is too small for the device's chunk, or the producer is late.
    pub fn starved_callbacks(&self) -> u64 {
        self.callback.starved.load(Ordering::Relaxed)
    }

    /// Samples of padding the device has been given, across every starved callback.
    pub fn padded_samples(&self) -> u64 {
        self.callback.padded.load(Ordering::Relaxed)
    }

    /// The ring this output was built with, in frames.
    pub fn ring_frames(&self) -> usize {
        self.ring_frames
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
    /// A pattern was configured: the matching device, or nothing plus how many were visible (the
    /// reason a "missing" device deserves a better line than "not found").
    Named {
        device: Option<cpal::Device>,
        seen: usize,
    },
}

/// How many times a named device is looked for before it is called missing, and the pause between
/// tries. Three attempts means two pauses — 10 ms in the worst case, well inside the output ring's
/// slack — and covers the sound server releasing a card it has just suspended.
const DEVICE_LOOKUP_ATTEMPTS: usize = 3;
const DEVICE_LOOKUP_PAUSE: std::time::Duration = std::time::Duration::from_millis(5);

/// Resolves a `device` pattern against the host's outputs: an all-digit pattern is an **index** into
/// the enumeration, anything else is a **case-insensitive name substring** (first match wins).
/// No pattern means the system default device.
///
/// The enumeration is the only place a device name comes from, so `output list` and the resolution
/// agree by construction. Two details matter more than they look:
///
/// * The **names are collected first, with every device dropped as it is read**. cpal's ALSA backend
///   opens a PCM pair per device while iterating, and holding all of them alive truncates its own
///   list: an alias early in the hint order (`samplerate`, `speex`, `upmix`, … all slave to the same
///   card) keeps the card's `hw` PCM, so the real entries for that card (`sysdefault:CARD=…`,
///   `front:CARD=…`) then fail to open and are silently skipped. Resolving a device by name used to
///   fail precisely for the destinations the list had just shown.
/// * The lookup is retried briefly because the same enumeration drops PCMs another client holds: a
///   card the sound server has just suspended stays absent for a moment, and without the retry a
///   correctly configured destination would boot into silence.
fn resolve_device(host: &cpal::Host, pattern: Option<&str>) -> DeviceLookup {
    let Some(pattern) = pattern.map(str::trim).filter(|p| !p.is_empty()) else {
        return DeviceLookup::Default(host.default_output_device());
    };
    let mut seen = 0;
    for attempt in 0..DEVICE_LOOKUP_ATTEMPTS {
        let names = output_device_names(host);
        seen = names.len();
        if let Some(index) = match_device(&names, pattern) {
            return DeviceLookup::Named {
                device: open_device_at(host, index),
                seen,
            };
        }
        if attempt + 1 < DEVICE_LOOKUP_ATTEMPTS {
            std::thread::sleep(DEVICE_LOOKUP_PAUSE);
        }
    }
    DeviceLookup::Named { device: None, seen }
}

/// The host's output device names, in enumeration order, **without keeping any device open**.
fn output_device_names(host: &cpal::Host) -> Vec<String> {
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };
    devices.filter_map(|device| device.name().ok()).collect()
}

/// Opens the `index`-th device of [`output_device_names`]' list, dropping every device before it
/// before the target is opened (see [`resolve_device`] for why that is not optional).
fn open_device_at(host: &cpal::Host, index: usize) -> Option<cpal::Device> {
    let devices = host.output_devices().ok()?;
    // The same `name()` filter as the snapshot, so the two enumerations cannot disagree on an index.
    devices
        .filter(|device| device.name().is_ok())
        // `nth` drops each skipped device before asking for the next one.
        .nth(index)
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

/// One host output device, as a front-end reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The PCM name the host uses (`sysdefault:CARD=III`, `hw:3,0`, `USB Headset`).
    pub name: String,
    /// The card's own description, when the host has one (`Numark Party Mix III`). A USB interface
    /// is usually named after its card *id* in the PCM name, which is exactly what a user cannot
    /// recognise in a list.
    pub description: String,
    /// Whether this is the host's default output.
    pub is_default: bool,
    /// The rate span the device advertises (`44100Hz..192000Hz`), or a note when it will not say.
    pub rates: String,
    /// Whether the device accepts the rate the caller asked about.
    pub accepts: Option<bool>,
    /// The widest channel count any config of this device offers. A 4-channel interface can carry
    /// the master on one stereo pair and the cue on another, so the front-end needs this to offer
    /// the choice.
    pub channels: u16,
}

/// Enumerates the host's output devices with the rates each one accepts.
///
/// No stream is opened: this is what lets the controller guide answer "will this device take the
/// engine's rate?" before a session exists. Empty when the host has no outputs at all (a container
/// or a headless machine), which is a normal answer rather than an error.
pub fn output_devices(wanted_rate: Option<u32>) -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default = host
        .default_output_device()
        .map(|device| device_name(&device));
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };
    let mut out: Vec<DeviceInfo> = devices
        .map(|device| {
            let name = device_name(&device);
            let channels = device
                .supported_output_configs()
                .map(|configs| configs.map(|config| config.channels()).max().unwrap_or(0))
                .unwrap_or(0);
            let accepts = wanted_rate.map(|rate| {
                device
                    .supported_output_configs()
                    .map(|ranges| {
                        // Same predicate the stream build uses: a rate is only "accepted" if it can
                        // be fed in a format we can write. A 24-bit-only PCM must not show `✓` and
                        // then disable itself.
                        ranges.into_iter().any(|range| {
                            playable_format(range.sample_format())
                                && range.min_sample_rate().0 <= rate
                                && range.max_sample_rate().0 >= rate
                        })
                    })
                    .unwrap_or(false)
            });
            let description = card_description(&name);
            DeviceInfo {
                is_default: default.as_deref() == Some(name.as_str()),
                rates: describe_device_rates(&device),
                name,
                description,
                accepts,
                channels,
            }
        })
        .collect();
    out.truncate(64);
    out
}

/// The short card description behind a PCM name, on Linux only.
///
/// ALSA names a USB interface after its card id (`sysdefault:CARD=III`), so the list would otherwise
/// show `III` where the user is looking for "Party Mix". `/proc/asound/cards` has the long name; on
/// any other platform (and for PCMs that do not name a card) this returns an empty string and the
/// front-end simply shows the PCM name alone.
fn card_description(pcm_name: &str) -> String {
    let Some((_, after)) = pcm_name.split_once("CARD=") else {
        return String::new();
    };
    let card_id = after.split(',').next().unwrap_or(after);
    let Ok(text) = std::fs::read_to_string("/proc/asound/cards") else {
        return String::new();
    };
    // Each card is two lines: ` 2 [III            ]: USB-Audio - Numark Party Mix III` then an
    // indented detail line. The id sits in the brackets *before* the colon.
    for line in text.lines() {
        let Some((head, rest)) = line.split_once(':') else {
            continue;
        };
        let Some((_, bracketed)) = head.split_once('[') else {
            continue;
        };
        let id = bracketed.split(']').next().unwrap_or("").trim();
        if id != card_id {
            continue;
        }
        let Some((_, description)) = rest.split_once(" - ") else {
            continue;
        };
        return description.trim().to_owned();
    }
    String::new()
}

/// A device's name, or a stand-in when the backend will not say.
fn device_name(device: &cpal::Device) -> String {
    device.name().unwrap_or_else(|_| "audio device".into())
}

/// `[44100]` / `[44100, 48000]` / `any` — for a warning that has to be readable.
fn describe_rates(rates: &[u32]) -> String {
    if rates.is_empty() {
        return "defaults to the engine's rate".to_owned();
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

/// Asks the device for `sample_rate` with at least `wanted_channels`, preferring native f32 and
/// the smallest channel count that can hold the configured mapping.
///
/// Unlike the old version this does **not** fall back to the device's default config: a device that
/// cannot run at the engine's rate is refused (and the output disabled), because a silent fallback
/// to another rate plays everything at the wrong speed.
///
/// The format filter is not cosmetic. cpal enumerates I8/U8/I32/U32/F64 too, `build_stream` feeds
/// only F32/I16/U16, and picking an unplayable format used to turn a working device into a silent
/// sink with no message. A 24-bit-only interface (S24_3LE) is exactly this case when it is opened
/// through a raw `hw:` PCM instead of ALSA's `plug`/`sysdefault` layer.
fn preferred_config(
    device: &cpal::Device,
    sample_rate: u32,
    wanted_channels: usize,
) -> Option<cpal::SupportedStreamConfig> {
    let ranges = device.supported_output_configs().ok()?;
    let wanted = wanted_channels.max(CHANNELS) as u16;
    ranges
        .filter(|range| {
            range.channels() >= wanted
                && playable_format(range.sample_format())
                && range.min_sample_rate().0 <= sample_rate
                && range.max_sample_rate().0 >= sample_rate
        })
        // Smallest channel count first (a 4-channel cue should not open 8 channels just because the
        // card offers them), then native f32, which avoids a per-sample quantisation step.
        .min_by_key(|range| {
            (
                range.channels(),
                u8::from(range.sample_format() != cpal::SampleFormat::F32),
            )
        })
        .map(|range| range.with_sample_rate(cpal::SampleRate(sample_rate)))
}

/// The sample formats [`build_stream`] can feed.
fn playable_format(format: cpal::SampleFormat) -> bool {
    matches!(
        format,
        cpal::SampleFormat::F32 | cpal::SampleFormat::I16 | cpal::SampleFormat::U16
    )
}

/// Why [`preferred_config`] found nothing, in the device's own terms: the rate, the channels or the
/// format. Silence with a reason beats a silent sink.
fn unsupported_message(device: &cpal::Device, sample_rate: u32) -> String {
    let Ok(ranges) = device.supported_output_configs() else {
        return format!("will not say what it accepts (engine wants {sample_rate}Hz)");
    };
    let mut formats: Vec<String> = Vec::new();
    let mut rate_ok = false;
    for range in ranges {
        if range.min_sample_rate().0 <= sample_rate && range.max_sample_rate().0 >= sample_rate {
            rate_ok = true;
            let format = format!("{:?}", range.sample_format());
            if !formats.contains(&format) {
                formats.push(format);
            }
        }
    }
    if rate_ok {
        format!(
            "offers {sample_rate}Hz only in formats we cannot play ({}); open it through a \
             `sysdefault`/`plug` PCM or the sound server",
            formats.join(", ")
        )
    } else {
        format!(
            "does not accept {sample_rate}Hz (offers {})",
            describe_device_rates(device)
        )
    }
}

/// Clamps a requested device buffer to the range the backend advertises, so a value that is too
/// small or too large becomes the nearest legal one instead of a stream-build failure.
fn clamp_device_buffer(requested: usize, supported: &cpal::SupportedBufferSize) -> u32 {
    let frames = match supported {
        cpal::SupportedBufferSize::Range { min, max } => {
            requested.clamp(*min as usize, *max as usize)
        }
        cpal::SupportedBufferSize::Unknown => requested,
    };
    frames.min(u32::MAX as usize) as u32
}

/// Whether a ring of `ring_frames` can serve a callback that asked for `wanted` frames.
///
/// One block of slack on top of the callback: the producer writes whole blocks, so a ring that fits
/// exactly one callback has no room for the next block between callbacks. `wanted == 0` means no
/// callback has fired yet (nothing to decide).
fn shortfall(ring_frames: usize, block_frames: usize, wanted: usize) -> Option<usize> {
    (wanted > 0 && wanted + block_frames > ring_frames).then_some(wanted)
}

/// The ring to reopen with when a device's callback wanted `wanted` frames: that callback plus four
/// blocks of slack, never smaller than the configured ring.
///
/// The cap is a runaway guard, not a tuning value: a device that wants more than two seconds of
/// buffer is not a device this engine can play through, and growing forever would eat the session.
pub(crate) fn grown_ring(wanted: usize, block_frames: usize, configured: usize) -> usize {
    const MAX_RING_FRAMES: usize = 131_072;
    (wanted + 4 * block_frames)
        .max(configured)
        .min(MAX_RING_FRAMES)
}

/// Builds and starts the stream. The consumer is moved into the callback, which is why the ring's two
/// ends are owned rather than shared.
///
/// `callback_frames` is written by the audio callback with the size of the chunk the device asked
/// for. cpal's ALSA backend passes ALSA's `avail_frames` — everything the PCM buffer has room for —
/// so this is the device's own period/buffer, and a ring smaller than it starves on every callback.
#[allow(clippy::too_many_arguments)]
fn build_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut consumer: Consumer<f32>,
    name: &str,
    device_buffer_frames: Option<usize>,
    callback: Arc<CallbackStats>,
    channels: usize,
) -> Result<cpal::Stream, OutputError> {
    let mut stream_config = config.config();
    if let Some(requested) = device_buffer_frames {
        let frames = clamp_device_buffer(requested, config.buffer_size());
        stream_config.buffer_size = cpal::BufferSize::Fixed(frames);
        eprintln!("[audio] {name}: device buffer {frames} frames (fixed)");
    }
    let name = name.to_owned();
    let on_error = move |err: cpal::StreamError| eprintln!("[audio] {name}: stream error: {err}");
    // Each callback records its own size: the ring-growth check reads the maximum, so a device that
    // asks for its whole PCM buffer once (the first callback after `play`) is sized for.
    let channels = channels.max(1);
    let built = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let stats = Arc::clone(&callback);
            device.build_output_stream(
                &stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    stats.record(data.len() / channels);
                    let padded = fill_with_silence_on_underrun(&mut consumer, data);
                    stats.padded(padded);
                },
                on_error,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            // Sized here, outside the real-time callback.
            let mut scratch: Vec<f32> = vec![0.0; data_capacity(config)];
            let stats = Arc::clone(&callback);
            device.build_output_stream(
                &stream_config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    stats.record(data.len() / channels);
                    grow(&mut scratch, data.len());
                    let (buffer, _) = scratch.split_at_mut(data.len());
                    let padded = fill_with_silence_on_underrun(&mut consumer, buffer);
                    stats.padded(padded);
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
            let stats = Arc::clone(&callback);
            device.build_output_stream(
                &stream_config,
                move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                    stats.record(data.len() / channels);
                    grow(&mut scratch, data.len());
                    let (buffer, _) = scratch.split_at_mut(data.len());
                    let padded = fill_with_silence_on_underrun(&mut consumer, buffer);
                    stats.padded(padded);
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
    stream
        .play()
        .map_err(|err| OutputError::Stream(err.to_string()))?;
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

    /// Every device the host lists must actually resolve. Resolving used to collect *all* devices
    /// first, which kept one ALSA PCM per alias open: `samplerate`/`speex`/`upmix`/`default` slave to
    /// the same card, so that card's real entries (`sysdefault:CARD=…`, `front:CARD=…`) failed to
    /// open and were silently dropped from the list being searched. The destination a user had just
    /// picked in `output` was then reported `missing` — and the engine played into a sink.
    #[test]
    fn every_listed_device_resolves_by_its_own_name() {
        let host = cpal::default_host();
        let names = output_device_names(&host);
        if names.len() < 2 {
            // A headless host (or one with a single PCM) has no ordering to disagree about.
            return;
        }
        let mut unresolved = Vec::new();
        let mut checked = 0;
        for (index, name) in names.iter().enumerate() {
            // Skip anything that has vanished from a fresh *names-only* snapshot: another client (the
            // sound server, a test holding a stream open) grabbing a device in the meantime is not
            // what this pins. The bug this test exists for truncated the list *while opening* the
            // devices, which does not affect this snapshot.
            if !output_device_names(&host)
                .iter()
                .any(|current| current == name)
            {
                continue;
            }
            checked += 1;
            match resolve_device(&host, Some(name)) {
                DeviceLookup::Named {
                    device: Some(_), ..
                } => {}
                DeviceLookup::Named { device: None, seen } => {
                    unresolved.push(format!("`{name}` (#{index}, {seen} visible)"))
                }
                DeviceLookup::Default(_) => unreachable!("a pattern never takes the default path"),
            }
        }
        assert!(
            unresolved.is_empty(),
            "{} of {} listed device(s) do not resolve: {}",
            unresolved.len(),
            checked,
            unresolved.join(", ")
        );
    }

    /// The formats [`build_stream`] can feed; the enumeration offers more than that.
    #[test]
    fn only_formats_the_stream_can_feed_are_accepted() {
        use cpal::SampleFormat as F;
        for format in [F::F32, F::I16, F::U16] {
            assert!(playable_format(format), "{format:?}");
        }
        for format in [F::I8, F::U8, F::I32, F::U32, F::F64] {
            assert!(!playable_format(format), "{format:?} cannot be fed");
        }
    }

    /// A device whose callback is bigger than the ring starves on every callback: the mix plays
    /// slow and broken. The check has to fire on the callback size, not on the configured ring.
    #[test]
    fn a_callback_bigger_than_the_ring_asks_for_a_bigger_ring() {
        // The PCH measurement that started this: 1024-frame ring, ~2032-frame callbacks.
        assert_eq!(shortfall(1024, 256, 2032), Some(2032));
        assert_eq!(shortfall(1024, 256, 768), None, "fits with slack");
        assert_eq!(shortfall(1024, 256, 0), None, "no callback yet");
        assert_eq!(
            shortfall(1024, 256, 769),
            Some(769),
            "exactly a callback but no room for the next block"
        );
    }

    #[test]
    fn the_grown_ring_holds_the_callback_and_never_shrinks_below_the_config() {
        assert_eq!(grown_ring(2032, 256, 1024), 2032 + 1024);
        assert_eq!(grown_ring(512, 256, 4096), 4096, "never shrinks");
        // A pathological device cannot make the engine allocate unlimited frames.
        assert_eq!(grown_ring(usize::MAX / 2, 256, 1024), 131_072);
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
        assert_eq!(
            out.state(),
            OutputState::Disabled,
            "a rate mismatch disables the output"
        );
        assert!(wrong_rate.allows_engine_rate(48_000));
        assert!(!wrong_rate.allows_engine_rate(SAMPLE_RATE));
    }

    /// The engine rate is allowed by default; anything else is not.
    #[test]
    fn the_default_rate_allow_list_is_the_engine_rate() {
        let cfg = OutputConfig::master(0, "x");
        assert!(
            cfg.sample_rates.is_empty(),
            "the default is empty, not a literal 44.1k"
        );
        assert!(cfg.allows_engine_rate(SAMPLE_RATE));
        assert!(
            cfg.allows_engine_rate(48_000),
            "an empty list follows the engine's rate"
        );
    }

    #[test]
    fn a_requested_device_buffer_is_clamped_to_the_backend_range() {
        let range = cpal::SupportedBufferSize::Range {
            min: 128,
            max: 2048,
        };
        assert_eq!(clamp_device_buffer(512, &range), 512);
        assert_eq!(
            clamp_device_buffer(64, &range),
            128,
            "too small is raised to the minimum"
        );
        assert_eq!(
            clamp_device_buffer(9999, &range),
            2048,
            "too large is lowered to the maximum"
        );
        assert_eq!(
            clamp_device_buffer(512, &cpal::SupportedBufferSize::Unknown),
            512,
            "an unknown range accepts the request"
        );
    }

    /// The device may be absent (CI) or present (a real card). Either way `open` must not panic, and
    /// must never hand back an output that silently produces nothing while claiming to be live.
    #[test]
    fn open_either_succeeds_or_reports_a_real_failure() {
        let cfg = OutputConfig::master(0, "probe");
        match Output::open(&cfg, crate::SAMPLE_RATE, BLOCK_SIZE) {
            Ok(out) => {
                // A sink means "no device"; anything else means a started stream.
                assert_eq!(
                    !out.is_live(),
                    out._stream.is_none(),
                    "state/stream disagree"
                );
            }
            Err(err) => assert!(!err.message().is_empty()),
        }
    }
}

#[cfg(test)]
mod device_probe {
    use super::*;

    /// Prints what the host offers: `cargo test -p hypermixx-audio output_devices_probe --
    /// --ignored --nocapture`. A debugging aid for "my interface is not in the list".
    #[test]
    #[ignore]
    fn output_devices_probe() {
        let devices = output_devices(Some(hyper_mixx_rate()));
        println!("{} output device(s):", devices.len());
        for device in &devices {
            println!(
                "  {}{}  [{}]  accepts={:?}  channels={}  rates={}",
                device.name,
                if device.is_default { "  [default]" } else { "" },
                device.description,
                device.accepts,
                device.channels,
                device.rates
            );
        }
        let host = cpal::default_host();
        println!("host: {:?}", host.id());
        if let Ok(all) = host.output_devices() {
            let names: Vec<String> = all.map(|device| device_name(&device)).collect();
            println!("cpal sees {} device(s)", names.len());
        }
    }

    fn hyper_mixx_rate() -> u32 {
        44_100
    }
}
