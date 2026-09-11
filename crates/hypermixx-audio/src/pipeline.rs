//! Producer thread + cpal output: the only place the engine touches a sound card.
//!
//! Topology:
//!   CLI --Command--> [producer thread: deck0 + deck1 -> mix] --f32--> ring --f32--> [cpal callback]
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

use crate::command::{Command, CommandResponse, DeckState};
use crate::deck::Deck;
use crate::ringbuf::{fill_with_silence_on_underrun, pop_samples, push_samples, AudioRingBuffer};
use crate::source::{decode_file, PcmPool, Source};
use crate::{
    BLOCK_SAMPLES, BLOCK_SIZE, CHANNELS, DECK_COUNT, DECK_MIX_GAIN, OUTPUT_RING_CAPACITY,
    OUTPUT_RING_SAMPLES, PREFILL_FRAMES, SAMPLE_RATE,
};

/// Sleep between produced blocks: deliberately shorter than the block's audio duration (90% of
/// 5.33ms) so the producer keeps a margin in the ring instead of drifting into underrun.
const BLOCK_PACE: Duration =
    Duration::from_nanos(BLOCK_SIZE as u64 * 900_000_000 / SAMPLE_RATE as u64);

/// Owns the audio stream, the producer thread and the decks.
pub struct AudioPipeline {
    /// Fixed set of decks indexed by `deck_id`. Only the producer thread and the commands it
    /// handles take these locks, so the audio callback never does.
    decks: Vec<Arc<Mutex<Deck>>>,
    shutdown: Arc<AtomicBool>,
    producer: Option<JoinHandle<()>>,
    /// The cpal stream must outlive the producer thread; dropping it stops the sound card.
    _stream: Option<cpal::Stream>,
}

impl AudioPipeline {
    /// Spins up the producer thread and the audio output.
    ///
    /// With no usable output device the engine still runs — blocks keep being produced and drained
    /// internally, so the CLI stays usable on headless machines.
    pub fn start(command_rx: Receiver<Command>, response_tx: Sender<CommandResponse>) -> Self {
        let decks: Vec<Arc<Mutex<Deck>>> = (0..DECK_COUNT)
            .map(|_| Arc::new(Mutex::new(Deck::new(Arc::new(PcmPool::empty())))))
            .collect();
        let shutdown = Arc::new(AtomicBool::new(false));

        let (mut producer, consumer) = AudioRingBuffer::new(OUTPUT_RING_CAPACITY).split();
        fill_ring_with_silence(&mut producer, PREFILL_FRAMES);

        let (stream, drain) = Self::open_output_stream(consumer);
        if stream.is_none() {
            eprintln!("[audio] headless mode: blocks are produced into a silent sink");
        }

        let thread_decks = decks.clone();
        let thread_shutdown = Arc::clone(&shutdown);
        let producer_thread = std::thread::Builder::new()
            .name("hypermixx-producer".into())
            .spawn(move || {
                Self::producer_loop(
                    producer,
                    drain,
                    thread_decks,
                    command_rx,
                    response_tx,
                    thread_shutdown,
                )
            })
            .expect("failed to spawn producer thread");

        Self {
            decks,
            shutdown,
            producer: Some(producer_thread),
            _stream: stream,
        }
    }

    /// The deck behind `deck_id`, for consumers that inspect a transport directly (a future UI or
    /// mixer) instead of going through the command channel.
    pub fn deck(&self, deck_id: usize) -> Option<&Arc<Mutex<Deck>>> {
        self.decks.get(deck_id)
    }

    /// Number of decks the pipeline mixes.
    pub fn deck_count(&self) -> usize {
        self.decks.len()
    }

    /// The producer thread body: drain commands, then push one mixed block into the output ring.
    fn producer_loop(
        mut producer: Producer<f32>,
        mut drain: Option<Consumer<f32>>,
        decks: Vec<Arc<Mutex<Deck>>>,
        command_rx: Receiver<Command>,
        response_tx: Sender<CommandResponse>,
        shutdown: Arc<AtomicBool>,
    ) {
        let mut mix = [0.0f32; BLOCK_SAMPLES];
        let mut deck_block = [0.0f32; BLOCK_SAMPLES];
        'engine: loop {
            loop {
                match command_rx.try_recv() {
                    Ok(Command::Quit) => {
                        shutdown.store(true, Ordering::Relaxed);
                        let _ = response_tx.send(CommandResponse::Ok);
                        break 'engine;
                    }
                    Ok(command) => Self::handle_command(command, &decks, &response_tx),
                    Err(TryRecvError::Empty) => break,
                    // CLI gone: shut down instead of spinning forever.
                    Err(TryRecvError::Disconnected) => break 'engine,
                }
            }
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            // Pace on ring room, not on the clock: the callback drains at the device rate, so
            // producing only when a whole block fits keeps both playheads locked to wall time
            // instead of drifting ~10% fast while the ring has slack. With no sound card there is
            // no consumer at all, so `drain` plays that role and the decks never stall.
            if drain.is_none() && producer.slots() < BLOCK_SAMPLES {
                std::thread::sleep(BLOCK_PACE);
                continue;
            }

            mix.fill(0.0);
            for deck in &decks {
                // A paused or exhausted deck writes a fully populated silent block, so every deck
                // advances one block per tick and both transports share the same clock.
                lock(deck).process_block(&mut deck_block);
                for (mixed, sample) in mix.iter_mut().zip(deck_block) {
                    *mixed += sample;
                }
            }
            for sample in &mut mix {
                *sample *= DECK_MIX_GAIN;
            }
            push_samples(&mut producer, &mix);
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
        decks: &[Arc<Mutex<Deck>>],
        response_tx: &Sender<CommandResponse>,
    ) {
        match command {
            Command::Load { deck_id, path } => {
                let Some(target) = decks.get(deck_id).cloned() else {
                    return unknown_deck(response_tx, deck_id, decks.len());
                };
                // Decoding is slow, so it gets its own thread; audio keeps running meanwhile.
                let worker_tx = response_tx.clone();
                let spawned = std::thread::Builder::new()
                    .name(format!("hypermixx-decode-{deck_id}"))
                    .spawn(move || match decode_file(&path) {
                        Ok(decoded) => {
                            let total_frames = decoded.total_frames;
                            let pool: Arc<dyn Source> = Arc::new(PcmPool::from_decoded(decoded));
                            let deck = Deck::new(pool);
                            // Replacing the deck resets its transport and joins the old warm-up
                            // thread on drop; the other deck keeps playing untouched.
                            *lock(&target) = deck;
                            let _ = worker_tx.send(CommandResponse::Loaded {
                                deck_id,
                                total_frames,
                            });
                        }
                        Err(err) => {
                            let _ = worker_tx.send(CommandResponse::Error(format!(
                                "deck {deck_id}, {path}: {err}"
                            )));
                        }
                    });
                if let Err(err) = spawned {
                    reject(
                        response_tx,
                        &format!("could not start decoder thread: {err}"),
                    );
                }
            }
            Command::Play { deck_id } => {
                on_deck(decks, deck_id, response_tx, |deck| {
                    deck.play();
                    CommandResponse::Ok
                });
            }
            Command::Pause { deck_id } => {
                on_deck(decks, deck_id, response_tx, |deck| {
                    deck.pause();
                    CommandResponse::Ok
                });
            }
            Command::Jump {
                deck_id,
                target_frame,
            } => {
                on_deck(decks, deck_id, response_tx, move |deck| {
                    deck.jump(target_frame);
                    CommandResponse::Ok
                });
            }
            Command::BeatJump { deck_id, beats } => {
                on_deck(decks, deck_id, response_tx, move |deck| {
                    deck.beatjump(beats);
                    CommandResponse::Ok
                });
            }
            Command::SetAnalysis { deck_id, analysis } => {
                let Some(deck) = decks.get(deck_id) else {
                    return unknown_deck(response_tx, deck_id, decks.len());
                };
                lock(deck).set_analysis(analysis);
                let _ = response_tx.send(CommandResponse::Ok);
            }
            Command::GetState { deck_id } => {
                let Some(deck) = decks.get(deck_id) else {
                    return unknown_deck(response_tx, deck_id, decks.len());
                };
                let _ = response_tx.send(CommandResponse::State(state_of(deck_id, deck)));
            }
            Command::GetAllStates => {
                // One pass over every deck: all frames in the answer come from the same block, so
                // differences between decks are free of sampling skew.
                let states = decks
                    .iter()
                    .enumerate()
                    .map(|(deck_id, deck)| state_of(deck_id, deck))
                    .collect();
                let _ = response_tx.send(CommandResponse::States(states));
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
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(worker) = self.producer.take() {
            let _ = worker.join();
        }
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

/// Grows a reusable real-time scratch buffer. Reallocating here is a cold path: the buffer starts
/// out as large as the whole output ring, which any sane callback fits into.
fn grow(scratch: &mut Vec<f32>, needed: usize) {
    if scratch.len() < needed {
        scratch.resize(needed, 0.0);
    }
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

/// Reads one deck's transport. Called from the producer thread only.
fn state_of(deck_id: usize, deck: &Arc<Mutex<Deck>>) -> DeckState {
    let deck = lock(deck);
    DeckState {
        deck_id,
        current_frame: deck.current_frame(),
        playing: deck.is_playing(),
        total_frames: deck.total_frames(),
        bpm: deck.bpm(),
        key: deck.key().map(|k| k.name()),
    }
}

/// Runs `apply` on a loaded deck, answering with `Error` for a bad id or an empty deck.
fn on_deck(
    decks: &[Arc<Mutex<Deck>>],
    deck_id: usize,
    response_tx: &Sender<CommandResponse>,
    apply: impl FnOnce(&mut Deck) -> CommandResponse,
) {
    let Some(deck) = decks.get(deck_id) else {
        return unknown_deck(response_tx, deck_id, decks.len());
    };
    if lock(deck).total_frames() == 0 {
        return reject(
            response_tx,
            &format!("deck {deck_id} holds no track, use `load {deck_id} <path>`"),
        );
    }
    let _ = response_tx.send(apply(&mut lock(deck)));
}

fn unknown_deck(response_tx: &Sender<CommandResponse>, deck_id: usize, count: usize) {
    let last = count.saturating_sub(1);
    reject(
        response_tx,
        &format!("unknown deck {deck_id}, valid ids are 0..={last}"),
    );
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
