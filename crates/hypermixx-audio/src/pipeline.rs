//! Producer thread + cpal output: the only place the engine touches a sound card.
//!
//! Topology:
//!   CLI --Command--> [producer thread: deck.process_block] --f32--> ring --f32--> [cpal callback]
//!
//! The two ends of the ring are split to their threads at construction, so the audio callback only
//! touches the ring buffer: no locks, no allocation, no decoding.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use rtrb::{Consumer, Producer};

use crate::command::{Command, CommandResponse};
use crate::deck::Deck;
use crate::ringbuf::{fill_with_silence_on_underrun, pop_samples, push_samples, AudioRingBuffer};
use crate::source::{decode_file, PcmPool, Source};
use crate::{
    BLOCK_SAMPLES, BLOCK_SIZE, CHANNELS, OUTPUT_RING_CAPACITY, OUTPUT_RING_SAMPLES, PREFILL_FRAMES,
    SAMPLE_RATE,
};

/// Sleep between produced blocks: deliberately shorter than the block's audio duration (90% of
/// 5.33ms) so the producer keeps a margin in the ring instead of drifting into underrun.
const BLOCK_PACE: Duration =
    Duration::from_nanos(BLOCK_SIZE as u64 * 900_000_000 / SAMPLE_RATE as u64);

/// Owns the audio stream, the producer thread and the deck.
pub struct AudioPipeline {
    deck: Arc<Mutex<Option<Deck>>>,
    shutdown: Arc<AtomicBool>,
    producer: Option<JoinHandle<()>>,
    /// The cpal stream must outlive the producer thread; dropping it stops the sound card.
    _stream: Option<cpal::Stream>,
}

impl AudioPipeline {
    /// Spins up the producer thread and the audio output.
    ///
    /// With no usable output device the engine still runs — blocks keep being produced into the
    /// ring (which simply overflows), so the CLI stays usable on headless machines.
    pub fn start(command_rx: Receiver<Command>, response_tx: Sender<CommandResponse>) -> Self {
        let deck = Arc::new(Mutex::new(None::<Deck>));
        let shutdown = Arc::new(AtomicBool::new(false));

        let (mut producer, consumer) = AudioRingBuffer::new(OUTPUT_RING_CAPACITY).split();
        fill_ring_with_silence(&mut producer, PREFILL_FRAMES);

        let (stream, drain) = Self::open_output_stream(consumer);
        if stream.is_none() {
            eprintln!("[audio] headless mode: blocks are produced into a silent sink");
        }

        let thread_deck = Arc::clone(&deck);
        let thread_shutdown = Arc::clone(&shutdown);
        let producer_thread = std::thread::Builder::new()
            .name("hypermixx-producer".into())
            .spawn(move || {
                Self::producer_loop(
                    producer,
                    drain,
                    thread_deck,
                    command_rx,
                    response_tx,
                    thread_shutdown,
                )
            })
            .expect("failed to spawn producer thread");

        Self {
            deck,
            shutdown,
            producer: Some(producer_thread),
            _stream: stream,
        }
    }

    /// The producer thread body: drain commands, then push one block into the output ring.
    fn producer_loop(
        mut producer: Producer<f32>,
        mut drain: Option<Consumer<f32>>,
        deck: Arc<Mutex<Option<Deck>>>,
        command_rx: Receiver<Command>,
        response_tx: Sender<CommandResponse>,
        shutdown: Arc<AtomicBool>,
    ) {
        let mut block = [0.0f32; BLOCK_SAMPLES];
        'engine: loop {
            loop {
                match command_rx.try_recv() {
                    Ok(Command::Quit) => {
                        shutdown.store(true, Ordering::Relaxed);
                        let _ = response_tx.send(CommandResponse::Ok);
                        break 'engine;
                    }
                    Ok(command) => Self::handle_command(command, &deck, &response_tx),
                    Err(TryRecvError::Empty) => break,
                    // CLI gone: shut down instead of spinning forever.
                    Err(TryRecvError::Disconnected) => break 'engine,
                }
            }
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            // Pace on ring room, not on the clock: the callback drains at the device rate, so
            // producing only when a whole block fits keeps the playhead locked to wall time
            // instead of drifting ~10% fast while the ring has slack. With no sound card there is
            // no consumer at all, so `drain` plays that role and the deck never stalls.
            if drain.is_none() && producer.slots() < BLOCK_SAMPLES {
                std::thread::sleep(BLOCK_PACE);
                continue;
            }

            {
                let mut guard = lock(&deck);
                match guard.as_mut() {
                    // A paused or exhausted deck still writes a fully populated silent block.
                    Some(deck) => {
                        deck.process_block(&mut block);
                    }
                    None => block.fill(0.0),
                };
            }
            push_samples(&mut producer, &block);
            if let Some(sink) = drain.as_mut() {
                let mut discard = [0.0f32; BLOCK_SAMPLES];
                pop_samples(sink, &mut discard);
            }
            std::thread::sleep(BLOCK_PACE);
        }
    }

    /// Applies one command (everything except `Quit`, which ends the loop).
    fn handle_command(
        command: Command,
        deck: &Arc<Mutex<Option<Deck>>>,
        response_tx: &Sender<CommandResponse>,
    ) {
        match command {
            Command::Load { path } => {
                // Decoding is slow, so it gets its own thread; audio keeps running meanwhile.
                let deck = Arc::clone(deck);
                let worker_tx = response_tx.clone();
                let spawned = std::thread::Builder::new()
                    .name("hypermixx-decode".into())
                    .spawn(move || match decode_file(&path) {
                        Ok(decoded) => {
                            let total_frames = decoded.total_frames;
                            let pool: Arc<dyn Source> = Arc::new(PcmPool::from_decoded(decoded));
                            *lock(&deck) = Some(Deck::new(pool));
                            let _ = worker_tx.send(CommandResponse::Loaded { total_frames });
                        }
                        Err(err) => {
                            let _ =
                                worker_tx.send(CommandResponse::Error(format!("{path}: {err}")));
                        }
                    });
                if let Err(err) = spawned {
                    reject(
                        response_tx,
                        &format!("could not start decoder thread: {err}"),
                    );
                }
            }
            Command::Play => with_deck(deck, response_tx, |deck| {
                deck.play();
                CommandResponse::Ok
            }),
            Command::Pause => with_deck(deck, response_tx, |deck| {
                deck.pause();
                CommandResponse::Ok
            }),
            Command::Jump { target_frame } => with_deck(deck, response_tx, move |deck| {
                deck.jump(target_frame);
                CommandResponse::Ok
            }),
            Command::GetState => {
                let guard = lock(deck);
                match guard.as_ref() {
                    Some(deck) => {
                        let _ = response_tx.send(CommandResponse::State {
                            current_frame: deck.current_frame(),
                            playing: deck.is_playing(),
                        });
                    }
                    None => reject(response_tx, NO_TRACK),
                }
            }
            Command::Quit => {
                // The producer loop intercepts `Quit` before calling this function; stopping the
                // engine from here would be a silent no-op, so nothing to do.
            }
        }
    }

    /// Opens the default output device, preferring 48kHz stereo f32.
    ///
    /// Returns the stream (if any) plus the ring's read end when it could stay unused: handing the
    /// consumer back lets the producer thread drain the ring itself instead of stalling forever.
    fn open_output_stream(
        mut consumer: Consumer<f32>,
    ) -> (Option<cpal::Stream>, Option<Consumer<f32>>) {
        let host = cpal::default_host();
        let Some(device) = host.default_output_device() else {
            eprintln!("[audio] no output device found");
            return (None, Some(consumer));
        };
        let name = device.name().unwrap_or_else(|_| "audio device".into());

        let config = match preferred_config(&device) {
            Some(config) => config,
            None => match device.default_output_config() {
                Ok(config) => config,
                Err(err) => {
                    eprintln!("[audio] {name}: cannot read default config ({err})");
                    return (None, Some(consumer));
                }
            },
        };
        let (rate, channels) = (config.sample_rate().0, config.channels() as usize);
        if rate != SAMPLE_RATE || channels != CHANNELS {
            eprintln!(
                "[audio] warning: device pinned to {rate}Hz/{channels}ch but the engine produces \
                 {SAMPLE_RATE}Hz/{CHANNELS}ch, so playback timing will be off"
            );
        }
        let stream_config = config.config();
        eprintln!(
            "[audio] {name}: {}/{channels}ch/{:?}",
            stream_config.sample_rate.0,
            config.sample_format()
        );

        let errors = |err: cpal::StreamError| eprintln!("[audio] stream error: {err}");
        let built = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    fill_with_silence_on_underrun(&mut consumer, data);
                },
                errors,
                None,
            ),
            cpal::SampleFormat::I16 => {
                // Pre-sized outside the callback: the real-time thread must not allocate.
                let mut scratch = vec![0.0f32; OUTPUT_RING_SAMPLES];
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        grow(&mut scratch, data.len());
                        let (buffer, _spare) = scratch.split_at_mut(data.len());
                        fill_with_silence_on_underrun(&mut consumer, buffer);
                        for (out, sample) in data.iter_mut().zip(&*buffer) {
                            *out = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                        }
                    },
                    errors,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let mut scratch = vec![0.0f32; OUTPUT_RING_SAMPLES];
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                        grow(&mut scratch, data.len());
                        let (buffer, _spare) = scratch.split_at_mut(data.len());
                        fill_with_silence_on_underrun(&mut consumer, buffer);
                        for (out, sample) in data.iter_mut().zip(&*buffer) {
                            *out = ((sample.clamp(-1.0, 1.0) + 1.0) * 0.5 * f32::from(u16::MAX))
                                as u16;
                        }
                    },
                    errors,
                    None,
                )
            }
            format => {
                eprintln!("[audio] unsupported sample format {format:?}");
                return (None, Some(consumer));
            }
        };
        let stream = match built {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("[audio] {name}: failed to build stream ({err})");
                return (None, None);
            }
        };
        if let Err(err) = stream.play() {
            eprintln!("[audio] {name}: failed to start stream ({err})");
            return (None, None);
        }
        (Some(stream), None)
    }

    /// Handle to the deck, for consumers that want to inspect transport state without going
    /// through the command channel (a future UI or mixer takes this).
    pub fn deck(&self) -> &Arc<Mutex<Option<Deck>>> {
        &self.deck
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(worker) = self.producer.take() {
            let _ = worker.join();
        }
    }
}

const NO_TRACK: &str = "no track loaded, use `load <path>` first";

/// Grows a reusable real-time scratch buffer. Reallocating here is a cold path: the buffer starts
/// out as large as the whole output ring, which any sane callback fits into.
fn grow(scratch: &mut Vec<f32>, needed: usize) {
    if scratch.len() < needed {
        scratch.resize(needed, 0.0);
    }
}

/// Picks a 48kHz stereo config if the device advertises one.
fn preferred_config(device: &cpal::Device) -> Option<cpal::SupportedStreamConfig> {
    let ranges = device.supported_output_configs().ok()?;
    ranges
        .filter(|range| {
            range.channels() as usize == CHANNELS
                && range.min_sample_rate().0 <= SAMPLE_RATE
                && range.max_sample_rate().0 >= SAMPLE_RATE
        })
        .max_by_key(|range| range.sample_format() == cpal::SampleFormat::F32)
        .map(|range| range.with_sample_rate(cpal::SampleRate(SAMPLE_RATE)))
}

/// Pre-fills the output ring with silence so the callback never starts from empty.
fn fill_ring_with_silence(producer: &mut Producer<f32>, frames: usize) {
    let silence = [0.0f32; BLOCK_SAMPLES];
    let mut remaining = frames * CHANNELS;
    while remaining > 0 {
        let written = push_samples(producer, &silence[..remaining.min(BLOCK_SAMPLES)]);
        if written == 0 {
            break; // ring is full; the prefill target is already met
        }
        remaining -= written;
    }
}

fn with_deck(
    deck: &Arc<Mutex<Option<Deck>>>,
    response_tx: &Sender<CommandResponse>,
    apply: impl FnOnce(&mut Deck) -> CommandResponse,
) {
    let mut guard = lock(deck);
    match guard.as_mut() {
        Some(deck) => {
            let _ = response_tx.send(apply(deck));
        }
        None => reject(response_tx, NO_TRACK),
    }
}

fn reject(response_tx: &Sender<CommandResponse>, reason: &str) {
    let _ = response_tx.send(CommandResponse::Error(reason.into()));
}

/// Locks without ever panicking on poisoning: a panic elsewhere must not kill audio.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
