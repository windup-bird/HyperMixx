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

use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Consumer, Producer};

use super::bus::Bus;
use super::config::OutputConfig;
use crate::ringbuf::{fill_with_silence_on_underrun, push_samples, AudioRingBuffer};
use crate::{CHANNELS, OUTPUT_RING_CAPACITY};

/// Identifies one output. Stable for the life of a `Mixer`; `MixerConfig` assigns them.
pub type OutputId = u8;

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
    /// True when nothing is connected: writes are counted and dropped.
    sink: bool,
    underruns: u64,
    writes: u64,
}

impl Output {
    /// Opens `cfg` against the default output device.
    ///
    /// No device at all is not an error: a headless machine (or a CI container) gets a sink output
    /// that accepts and discards audio, so the engine stays runnable and testable everywhere. A
    /// device that exists but cannot be fed is an error, because then the user should hear something
    /// and will not.
    pub fn open(cfg: &OutputConfig, sample_rate: u32, block_frames: usize) -> Result<Self, OutputError> {
        let host = cpal::default_host();
        let Some(device) = host.default_output_device() else {
            eprintln!("[audio] {}: no output device, writing to a silent sink", cfg.name);
            return Ok(Self::sink(cfg, block_frames));
        };
        let device_name = device.name().unwrap_or_else(|_| "audio device".into());
        let config = match preferred_config(&device, sample_rate) {
            Some(config) => config,
            None => match device.default_output_config() {
                Ok(config) => config,
                Err(err) => return Err(OutputError::Stream(format!("{device_name}: {err}"))),
            },
        };
        let (rate, device_channels, format) = (
            config.sample_rate().0,
            config.channels() as usize,
            config.sample_format(),
        );
        if rate != sample_rate {
            eprintln!(
                "[audio] {}: device pinned to {rate}Hz but the engine produces {sample_rate}Hz, \
                 so timing will be off",
                cfg.name
            );
        }
        eprintln!(
            "[audio] {}: {}/{device_channels}ch/{format:?}",
            cfg.name, config.sample_rate().0
        );
        let mapping = cfg.channel_pair(device_channels);
        let mut output = Self {
            id: cfg.id,
            name: cfg.name.clone(),
            channels: mapping,
            block_frames,
            producer: None,
            _stream: None,
            frame: vec![0.0; block_frames * device_channels.max(1)],
            device_channels: device_channels.max(1),
            sink: false,
            underruns: 0,
            writes: 0,
        };

        let (producer, consumer) = AudioRingBuffer::new(OUTPUT_RING_CAPACITY).split();
        output.producer = Some(producer);
        let stream = build_stream(&device, &config, consumer, &cfg.name)?;
        output._stream = Some(stream);
        Ok(output)
    }

    /// An output that accepts audio and throws it away — the headless case, and the default for
    /// tests that only care about the mix.
    pub fn sink(cfg: &OutputConfig, block_frames: usize) -> Self {
        Self {
            id: cfg.id,
            name: cfg.name.clone(),
            channels: (0, 1),
            block_frames,
            producer: None,
            _stream: None,
            frame: vec![0.0; block_frames],
            device_channels: CHANNELS,
            sink: true,
            underruns: 0,
            writes: 0,
        }
    }

    /// A named sink with the default mapping, for tests and for "no output configured".
    pub fn null(id: OutputId, block_frames: usize) -> Self {
        Self::sink(&OutputConfig::main(id, "null"), block_frames)
    }

    pub fn is_sink(&self) -> bool {
        self.sink
    }

    /// Blocks written since construction. Together with [`Output::underruns`] this says whether a
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

    /// Callback-side starves, as counted by the ring consumer's padding. Best-effort: an underrun is
    /// detected by the callback writing silence, which is also what a genuinely silent block looks
    /// like, so treat this as an upper bound.
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// Hands one block to the device.
    ///
    /// `bus` is stereo; it is expanded to the device's channel count at [`Output::channels`], which
    /// is why this takes the bus rather than samples — the mapping is the output's business.
    pub fn write(&mut self, bus: &Bus) {
        if self.sink || self.producer.is_none() {
            self.writes += 1;
            return;
        }
        let frames = bus.frames().min(self.block_frames);
        if frames == 0 {
            return;
        }
        // De-interleave -> remap -> interleave into the device-shaped frame buffer, then one push.
        self.frame[..frames * self.device_channels].fill(0.0);
        let (l, r) = (self.channels.0 as usize, self.channels.1 as usize);
        for i in 0..frames {
            let base = i * self.device_channels;
            match (l < self.device_channels, r < self.device_channels) {
                (true, true) => {
                    self.frame[base + l] = bus.l[i];
                    self.frame[base + r] = bus.r[i];
                }
                // A mono destination gets the sum, not the left channel: a cue on a 1ch device must
                // still tell you what is happening on both sides.
                _ => {
                    self.frame[base] = (bus.l[i] + bus.r[i]) * 0.5;
                }
            }
        }
        let len = frames * self.device_channels;
        if let Some(producer) = self.producer.as_mut() {
            let written = push_samples(producer, &self.frame[..len]);
            if written < len {
                self.underruns += 1;
            }
        }
        self.writes += 1;
    }

    /// The scratch block size this output was built for.
    pub fn block_frames(&self) -> usize {
        self.block_frames
    }
}

/// Asks the device for `sample_rate` stereo, preferring native f32.
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

    /// Total blocks accepted, across every output — a liveness check for tests.
    pub fn writes(&self) -> u64 {
        self.items.iter().map(Output::writes).sum()
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
                channels: self.channels,
                role: super::config::OutputRole::Main,
                gain: 1.0,
            },
            self.block_frames,
        )
    }
}

// A shared handle so a future UI can read meters without borrowing the mixer.
impl Output {
    pub fn arc_sink(id: OutputId, block_frames: usize) -> Arc<Output> {
        Arc::new(Output::null(id, block_frames))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BLOCK_SIZE;

    fn bus(left: f32, right: f32) -> Bus {
        let mut b = Bus::new(BLOCK_SIZE);
        b.fill_from(left, right);
        b
    }

    #[test]
    fn a_sink_counts_writes_and_holds_nothing() {
        let mut out = Output::null(0, BLOCK_SIZE);
        assert!(out.is_sink());
        out.write(&bus(1.0, -1.0));
        out.write(&bus(1.0, -1.0));
        assert_eq!(out.writes(), 2);
        assert_eq!(out.underruns(), 0);
    }

    #[test]
    fn writing_a_short_or_empty_bus_is_harmless() {
        let mut out = Output::null(0, BLOCK_SIZE);
        out.write(&Bus::new(0));
        out.write(&Bus::new(4));
        assert!(out.writes() >= 1);
    }

    #[test]
    fn outputs_are_addressed_by_position() {
        let mut set = Outputs::new();
        assert_eq!(set.push(Output::null(0, BLOCK_SIZE)), 0);
        assert_eq!(set.push(Output::null(1, BLOCK_SIZE)), 1);
        assert_eq!(set.len(), 2);
        set.write_many(&[0, 1, 99], &bus(0.5, 0.5));
        assert_eq!(set.writes(), 2);
        assert_eq!(set.get(1).unwrap().name, "null");
        assert!(set.get(2).is_none());
    }

    #[test]
    fn an_empty_output_set_is_not_an_error() {
        let mut set = Outputs::new();
        assert!(set.is_empty());
        set.write_many(&[], &bus(1.0, 1.0));
        assert_eq!(set.writes(), 0);
    }

    /// The device may be absent (CI) or present (a real card). Either way `open` must not panic, and
    /// must never hand back an output that silently produces nothing while claiming to be live.
    #[test]
    fn open_either_succeeds_or_reports_a_real_failure() {
        let cfg = OutputConfig::main(0, "probe");
        match Output::open(&cfg, crate::SAMPLE_RATE, BLOCK_SIZE) {
            Ok(out) => {
                // A sink means "no device"; anything else means a started stream.
                assert_eq!(out.is_sink(), out._stream.is_none(), "sink/stream disagree");
            }
            Err(err) => assert!(!err.message().is_empty()),
        }
    }

    #[test]
    fn channel_pair_falls_back_to_mono_mapping() {
        let mut cfg = OutputConfig::main(0, "cue");
        cfg.channels = (2, 3);
        // A 2ch device cannot honour (2,3); it must clamp rather than index out of bounds.
        assert_eq!(cfg.channel_pair(2), (0, 1));
        assert_eq!(cfg.channel_pair(4), (2, 3));
        assert_eq!(cfg.channel_pair(1), (0, 1), "a 1ch device writes the sum to ch0");
    }
}
