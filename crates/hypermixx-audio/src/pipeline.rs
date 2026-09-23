//! Producer thread + command channel: the boundary between "someone asked" and "the mix heard it".
//!
//! Topology:
//!   CLI ──Command──► [producer thread: Mixer::process] ──blocks──► Outputs ──► cpal ──► hardware
//!
//! Nothing in this module names a cpal type. Device opening, the rings, the sample-format conversions
//! and the real-time callbacks live in [`crate::mixer::output`], the one place a cpal type appears.
//!
//! The mixer is **built on the producer thread**, not handed to it. That is not tidiness:
//! `cpal::Stream` is deliberately not `Send`, so a mixer holding streams cannot cross a thread
//! boundary. Building here keeps every stream created and dropped on the thread that runs them, and
//! lets `start` still report a bad config synchronously — the constructor's result travels back over
//! a private channel before the first block is rendered.
//!
//! Pacing has two sources of truth, in this order: free space in the output rings (a callback draining
//! at the hardware rate is the only honest metronome, and producing into a full ring drops samples)
//! and, with no device attached, the wall clock. Sleeping a hair under a block's duration in both
//! cases is what keeps two playheads locked to real time instead of drifting while a buffer has slack.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use hypermixx_core::{Command, CommandResponse, DeckId, DeckState, Shared};

use crate::deck::Deck;
use crate::mixer::config::{MixerConfig, MixerError};
use crate::mixer::{Mixer, OutputError};
use crate::BLOCK_SIZE;
use crate::SAMPLE_RATE;

/// Why the engine could not start.
#[derive(Clone, Debug, PartialEq)]
pub enum PipelineError {
    /// The mixer's topology was invalid (unknown FX name, no channels, ...).
    Config(String),
    /// An output could not be opened.
    Device(String),
    /// The producer thread could not be spawned.
    Thread(String),
    /// The producer thread died before it reported back.
    Lost,
}

impl PipelineError {
    pub fn message(&self) -> String {
        match self {
            PipelineError::Config(what) => format!("mixer config: {what}"),
            PipelineError::Device(what) => format!("output device: {what}"),
            PipelineError::Thread(what) => format!("producer thread: {what}"),
            PipelineError::Lost => "the audio thread exited before reporting".into(),
        }
    }
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for PipelineError {}

impl From<MixerError> for PipelineError {
    fn from(err: MixerError) -> Self {
        PipelineError::Config(err.message())
    }
}

impl From<OutputError> for PipelineError {
    fn from(err: OutputError) -> Self {
        PipelineError::Device(err.message())
    }
}

/// A read-only snapshot of the mixer's output, sampled on the producer thread between blocks.
///
/// Modelled as a query rather than a pushed response: a UI wants the meters when it draws, and a
/// front-end that never asks pays nothing. The values are the fields `Mixer::process` already
/// maintains, so reading them costs no DSP work.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Meters {
    /// Peak of the last master block, linear (1.0 = full scale).
    pub master_peak: f32,
    /// Peak of the last cue block, linear.
    pub cue_peak: f32,
    /// Producer-side samples dropped because an output ring was full, summed over all outputs.
    pub overruns: u64,
}

/// Owns the producer thread and the mixer it drives.
///
/// The command and response channels belong to the pipeline rather than being passed in: a `start`
/// that hands back both ends cannot be wired wrongly, and a caller that wants to send commands has to
/// keep the pipeline alive — which is the lifetime rule the engine wants anyway.
pub struct AudioPipeline {
    producer: Option<JoinHandle<()>>,
    /// `None` once dropped, which closes the channel and ends the thread.
    cmd_tx: Option<Sender<Command>>,
    resp_rx: Receiver<CommandResponse>,
    /// Requests that need the mixer's own data, served by the producer thread. Only the deck's
    /// `Source` is fetched this way today (the CLI reads it back to analyse a track); a UI would add
    /// getters here rather than reaching into the engine.
    query_tx: Option<Sender<(Query, Sender<QueryResponse>)>>,
}

/// A read that has to happen on the mixer's thread.
enum Query {
    /// The PCM a deck is playing, for out-of-band analysis.
    DeckSource(DeckId),
    /// How many channels the mixer owns — the deck-id range a front-end should accept.
    ChannelCount,
    /// The last master/cue block peaks and the dropped-sample count.
    Meters,
}

enum QueryResponse {
    Source(Option<Shared>),
    Count(usize),
    Meters(Meters),
}

impl AudioPipeline {
    /// Builds the mixer from `cfg` at the engine rate and starts the producer thread.
    pub fn start(cfg: MixerConfig) -> Result<Self, PipelineError> {
        Self::start_at(cfg, SAMPLE_RATE)
    }

    /// As [`start`](Self::start) with an explicit rate, for a test that wants a specific clock.
    /// Production callers want [`start`](Self::start): the decoder resamples to the engine rate, so
    /// any other value would make the two disagree about what a second is.
    pub fn start_at(cfg: MixerConfig, sample_rate: u32) -> Result<Self, PipelineError> {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<Command>();
        let (resp_tx, resp_rx) = crossbeam_channel::unbounded::<CommandResponse>();
        let (query_tx, query_rx) = crossbeam_channel::unbounded::<(Query, Sender<QueryResponse>)>();
        // Carries "did the mixer build?" out of the thread that had to build it. The mixer itself
        // never crosses back — it cannot, which is why it is built on that thread in the first place.
        let (boot_tx, boot_rx) = mpsc::channel::<Result<(), PipelineError>>();

        let thread_cfg = cfg;
        let producer = std::thread::Builder::new()
            .name("hypermixx-producer".into())
            .spawn(move || {
                let mixer = match Mixer::new(thread_cfg, sample_rate) {
                    Ok(mixer) => mixer,
                    Err(err) => {
                        let _ = boot_tx.send(Err(err.into()));
                        return;
                    }
                };
                let _ = boot_tx.send(Ok(()));
                Self::producer_loop(mixer, cmd_rx, query_rx, resp_tx, sample_rate);
            })
            .map_err(|err| PipelineError::Thread(err.to_string()))?;

        // The thread answers this before its first block, so there is nothing to wait on.
        match boot_rx.recv() {
            Err(_) => Err(PipelineError::Lost),
            Ok(Err(err)) => {
                drop(cmd_tx);
                let _ = producer.join();
                Err(err)
            }
            Ok(Ok(())) => Ok(Self {
                producer: Some(producer),
                cmd_tx: Some(cmd_tx),
                resp_rx,
                query_tx: Some(query_tx),
            }),
        }
    }

    /// The command entry point. Clone it into whichever thread asks for things.
    pub fn command_tx(&self) -> Sender<Command> {
        self.cmd_tx
            .clone()
            .expect("the pipeline is shutting down")
    }

    pub fn response_rx(&self) -> &Receiver<CommandResponse> {
        &self.resp_rx
    }

    /// The PCM a deck is playing, or `None` for an empty deck or a bad id.
    ///
    /// Round-trips through the producer thread rather than reaching into the mixer: the engine's data
    /// is only ever touched by the thread that renders it, which is why there is no `deck()` accessor
    /// handing out a `&Mutex<Deck>` at all.
    pub fn deck_source(&self, deck_id: DeckId) -> Option<Shared> {
        let (answer_tx, answer_rx) = crossbeam_channel::unbounded::<QueryResponse>();
        self.query_tx
            .as_ref()?
            .send((Query::DeckSource(deck_id), answer_tx))
            .ok()?;
        match answer_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(QueryResponse::Source(source)) => source,
            Ok(_) | Err(_) => None,
        }
    }

    /// The mixer's output meters, or `None` if the engine is gone or slow to answer.
    ///
    /// Answers on the producer thread between blocks, exactly like [`deck_source`](Self::deck_source),
    /// so calling this from a 30Hz UI neither blocks nor disturbs the audio thread.
    pub fn meters(&self) -> Option<Meters> {
        let (answer_tx, answer_rx) = crossbeam_channel::unbounded::<QueryResponse>();
        self.query_tx
            .as_ref()?
            .send((Query::Meters, answer_tx))
            .ok()?;
        match answer_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(QueryResponse::Meters(meters)) => Some(meters),
            Ok(_) | Err(_) => None,
        }
    }

    /// How many channels (deck slots) the engine was configured with. A front-end validating
    /// deck ids should ask this rather than assuming a count — a custom `--config` topology may
    /// name any number of channels.
    pub fn channel_count(&self) -> Option<usize> {
        let (answer_tx, answer_rx) = crossbeam_channel::unbounded::<QueryResponse>();
        self.query_tx
            .as_ref()?
            .send((Query::ChannelCount, answer_tx))
            .ok()?;
        match answer_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(QueryResponse::Count(count)) => Some(count),
            Ok(_) | Err(_) => None,
        }
    }

    /// The producer thread body: drain commands, serve queries, render one block, keep pace.
    ///
    /// The mixer is owned outright here — no `Mutex`, no lock ordering to reason about, and a reader
    /// on another thread can only ever ask via a query, which is answered between blocks.
    fn producer_loop(
        mut mixer: Mixer,
        cmd_rx: Receiver<Command>,
        query_rx: Receiver<(Query, Sender<QueryResponse>)>,
        resp_tx: Sender<CommandResponse>,
        sample_rate: u32,
    ) {
        // Deliberately shorter than a block's audio duration, so the producer keeps a margin in the
        // rings instead of drifting into underrun.
        let pace = Duration::from_nanos(BLOCK_SIZE as u64 * 900_000_000 / sample_rate as u64);
        loop {
            let started = Instant::now();
            let mut quit = false;
            loop {
                match cmd_rx.try_recv() {
                    Ok(Command::Quit) => quit = true,
                    Ok(command) => {
                        if let Some(response) = route(&mut mixer, command) {
                            if resp_tx.send(response).is_err() {
                                return; // nobody left to answer
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        // No commands waiting — but a query may be. The first thing a front-end
                        // does after `start` is ask how many channels the engine owns, and a
                        // query drained only on command arrival would sit unanswered until
                        // something else happened. So drain here too, on the way out.
                        drain_queries(&mut mixer, &query_rx);
                        break;
                    }
                    // Nobody left sending: stop rather than spin.
                    Err(TryRecvError::Disconnected) => return,
                }
                // Queries are non-urgent but must not starve behind a command burst.
                drain_queries(&mut mixer, &query_rx);
            }
            if quit {
                drain_queries(&mut mixer, &query_rx);
                let _ = resp_tx.send(CommandResponse::Ok);
                return;
            }

            // Backpressure from the devices, not from a clock: with a live output the callback drains
            // at the hardware rate, so producing only when a whole block fits keeps the transport
            // locked to real time. With no output there is no consumer, and only the sleep paces.
            if !mixer.outputs().has_room(BLOCK_SIZE) {
                sleep_until_next(started, pace);
                continue;
            }
            let ctx = mixer.make_ctx(sample_rate);
            mixer.process(&ctx);
            sleep_until_next(started, pace);
        }
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        // Closing the command channel is enough: the loop returns on a disconnected receiver, and
        // dropping the mixer on that thread stops every output.
        self.query_tx = None;
        self.cmd_tx = None;
        if let Some(worker) = self.producer.take() {
            let _ = worker.join();
        }
    }
}

fn drain_queries(mixer: &mut Mixer, rx: &Receiver<(Query, Sender<QueryResponse>)>) {
    while let Ok((query, answer)) = rx.try_recv() {
        let response = match query {
            Query::DeckSource(deck_id) => {
                let source = mixer
                    .deck(deck_id as usize)
                    .filter(|deck| deck.total_frames() > 0)
                    .map(Deck::source);
                QueryResponse::Source(source)
            }
            Query::ChannelCount => QueryResponse::Count(mixer.channel_count()),
            Query::Meters => QueryResponse::Meters(Meters {
                master_peak: mixer.master_peak(),
                cue_peak: mixer.cue_peak(),
                // `Outputs` has no aggregate today; summing here keeps the change to this file.
                overruns: mixer.outputs().iter().map(|output| output.overruns()).sum(),
            }),
        };
        // A dropped receiver means the caller gave up; not an error here.
        let _ = answer.send(response);
    }
}

/// Paces one iteration so the loop's period is `pace` rather than `pace` + a block's work.
fn sleep_until_next(started: Instant, pace: Duration) {
    match pace.checked_sub(started.elapsed()) {
        Some(remaining) => std::thread::sleep(remaining),
        // Fell behind: skip the sleep, not the block. If that keeps happening the rings fill and
        // `Output::overruns` starts counting dropped samples; better than backing the pipeline up.
        None => {}
    }
}

/// Applies one command, returning the answer when the caller expects one.
///
/// Transport commands go to the deck inside a channel; FX commands are the mixer's own business.
/// `Quit` is handled by the loop, so it never reaches here.
fn route(mixer: &mut Mixer, command: Command) -> Option<CommandResponse> {
    use Command::*;
    match command {
        Load { deck_id, source, analysis } => {
            let total_frames = source.total_frames();
            let Some(channel) = mixer.channel_mut(deck_id as usize) else {
                return Some(error(unknown_deck(deck_id, mixer.channel_count())));
            };
            // A fresh transport resets this deck's position and joins its old warm-up thread on drop;
            // the other channels keep playing untouched. Faders and FX deliberately survive, because
            // reloading a track should not reset the mixer a user is standing at.
            // `set_analysis` takes `&self` (the grid swaps in lock-free), so no `mut` needed.
            let deck = Deck::new(source);
            if let Some(analysis) = analysis {
                deck.set_analysis(analysis);
            }
            channel.replace_deck(deck);
            Some(CommandResponse::Loaded { deck_id, total_frames })
        }
        Play { deck_id } => transport(mixer, deck_id, |deck| deck.play()),
        Pause { deck_id } => transport(mixer, deck_id, |deck| deck.pause()),
        Jump { deck_id, target_frame } => {
            transport(mixer, deck_id, move |deck| deck.jump(target_frame))
        }
        BeatJump { deck_id, beats } => transport(mixer, deck_id, move |deck| deck.beatjump(beats)),
        SetRate { deck_id, rate } => transport(mixer, deck_id, move |deck| deck.set_ratio(rate)),
        Loop { deck_id, op } => {
            transport_result(mixer, deck_id, move |deck| deck.apply_loop(op))
        }
        SetProfile { deck_id, profile } => {
            let engine_profile = match profile.as_str() {
                "tape" => Some(timestretch::engine::EngineProfile::Tape),
                "keylock" => Some(timestretch::engine::EngineProfile::Keylock),
                "wide" | "widekeylock" => Some(timestretch::engine::EngineProfile::WideKeylock),
                _ => None,
            };
            match engine_profile {
                Some(engine_profile) => {
                    transport(mixer, deck_id, move |deck| deck.set_profile(engine_profile))
                }
                None => Some(error("unknown profile, use tape/keylock/wide".to_owned())),
            }
        }
        SetAnalysis { deck_id, analysis } => match mixer.deck_mut(deck_id as usize) {
            Some(deck) => {
                deck.set_analysis(analysis);
                Some(CommandResponse::Ok)
            }
            None => Some(error(unknown_deck(deck_id, mixer.channel_count()))),
        },
        GetState { deck_id } => match mixer.deck(deck_id as usize) {
            Some(deck) => Some(CommandResponse::State(state_of(deck_id, deck))),
            None => Some(error(unknown_deck(deck_id, mixer.channel_count()))),
        },
        GetAllStates => {
            // One pass over every channel: all frames in the answer come from the same block, so
            // differences between decks are free of sampling skew.
            let states = (0..mixer.channel_count())
                .filter_map(|index| mixer.deck(index).map(|deck| state_of(index as DeckId, deck)))
                .collect();
            Some(CommandResponse::States(states))
        }
        fx @ (AddFx { .. }
        | RemoveFx { .. }
        | SetFxEnabled { .. }
        | SetFxParam { .. }
        | FxTrigger { .. }
        | PadPress { .. }
        | PadRelease { .. }
        | ListFx { .. }) => Some(mixer.handle_fx_command(fx)),
        Quit => None,
    }
}

/// Runs `apply` on a loaded deck; an empty deck is answered with an error rather than ignored, so a
/// mistyped deck id is visible instead of silent.
fn transport(mixer: &mut Mixer, deck_id: DeckId, apply: impl FnOnce(&mut Deck)) -> Option<CommandResponse> {
    transport_result(mixer, deck_id, |deck| {
        apply(deck);
        Ok(())
    })
}

/// [`transport`] for commands that can refuse: the deck's message rides back as `Error` instead of
/// being swallowed — a loop without a grid or an `out` without an `in` must reach the caller.
fn transport_result(
    mixer: &mut Mixer,
    deck_id: DeckId,
    apply: impl FnOnce(&mut Deck) -> Result<(), String>,
) -> Option<CommandResponse> {
    let count = mixer.channel_count();
    let Some(channel) = mixer.channel_mut(deck_id as usize) else {
        return Some(error(unknown_deck(deck_id, count)));
    };
    if channel.deck().total_frames() == 0 {
        return Some(error(format!(
            "deck {deck_id} holds no track, use `load {deck_id} <path>`"
        )));
    }
    match apply(channel.deck_mut()) {
        Ok(()) => Some(CommandResponse::Ok),
        Err(message) => Some(error(message)),
    }
}

fn state_of(deck_id: DeckId, deck: &Deck) -> DeckState {
    DeckState {
        deck_id,
        current_frame: deck.current_frame(),
        playing: deck.is_playing(),
        total_frames: deck.total_frames(),
        bpm: deck.bpm(),
        key: deck.key().map(|key| key.traditional()),
        virtual_frame: deck.virtual_frame(),
        loop_range: deck
            .loop_range()
            .map(|range| (range.in_frame, range.out_frame)),
        loop_in_armed: deck.loop_in_armed(),
    }
}

fn unknown_deck(deck_id: DeckId, count: usize) -> String {
    format!(
        "unknown deck {deck_id}, valid ids are 0..={}",
        count.saturating_sub(1)
    )
}

fn error(message: String) -> CommandResponse {
    CommandResponse::Error(message)
}
