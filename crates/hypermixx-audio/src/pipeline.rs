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
use crate::sync::SyncGroup;
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
                Self::producer_loop(mixer, SyncGroup::default(), cmd_rx, query_rx, resp_tx, sample_rate);
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
        mut sync: SyncGroup,
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
                        if let Some(response) = route(&mut mixer, &mut sync, command) {
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
            // Beat-sync samples positions *before* anything renders, so every follower compares
            // itself against its leader's position from this block rather than the last one.
            sync.prepare(&mut mixer);
            let ctx = mixer.make_ctx(sample_rate);
            mixer.process(&ctx);
            sleep_until_next(started, pace);
        }
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        // Quit rather than merely closing the channel. A caller may still hold a *clone* of the
        // command sender (the front-end's own copy, a test's handle), and waiting for that clone
        // to be dropped before the producer notices a disconnect would hang shutdown — a panic in
        // a test would then deadlock instead of reporting its failure.
        self.query_tx = None;
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(Command::Quit);
        }
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
fn route(
    mixer: &mut Mixer,
    sync: &mut SyncGroup,
    command: Command,
) -> Option<CommandResponse> {
    use Command::*;
    match command {
        Load { deck_id, source, analysis } => {
            let total_frames = source.total_frames();
            // The fader range is a controller setting, not a track property: read it before the
            // channel borrow and carry it over, so loading a track does not reset the DJ's range.
            let range = mixer
                .deck(deck_id as usize)
                .map(crate::deck::Deck::tempo_range);
            let Some(channel) = mixer.channel_mut(deck_id as usize) else {
                return Some(error(unknown_deck(deck_id, mixer.channel_count())));
            };
            // A fresh transport resets this deck's position and joins its old warm-up thread on drop;
            // the other channels keep playing untouched. Faders and FX deliberately survive, because
            // reloading a track should not reset the mixer a user is standing at.
            // `set_analysis` takes `&self` (the grid swaps in lock-free), so no `mut` needed.
            let mut deck = Deck::new(source);
            if let Some(range) = range {
                deck.set_tempo_range(range);
            }
            if let Some(analysis) = analysis {
                deck.set_analysis(analysis);
            }
            channel.replace_deck(deck);
            Some(CommandResponse::Loaded { deck_id, total_frames })
        }
        Play { deck_id } => transport(mixer, deck_id, |deck| deck.play()),
        Pause { deck_id } => transport(mixer, deck_id, |deck| deck.pause()),
        TogglePlay { deck_id } => transport(mixer, deck_id, |deck| deck.toggle_play()),
        Cue { deck_id, op } => transport(mixer, deck_id, move |deck| deck.apply_cue(op)),
        Jump { deck_id, target_frame } => {
            transport(mixer, deck_id, move |deck| deck.jump(target_frame))
        }
        BeatJump { deck_id, beats } => transport(mixer, deck_id, move |deck| deck.beatjump(beats)),
        SetTempo { deck_id, tempo } => answered(sync.set_tempo(mixer, deck_id, tempo)),
        SetTempoFader { deck_id, position } => {
            answered(sync.set_tempo_fader(mixer, deck_id, position))
        }
        SetTempoRange { deck_id, range } => answered(sync.set_tempo_range(mixer, deck_id, range)),
        Sync { deck_id, op } => answered(sync.handle_sync(mixer, deck_id, op)),
        Nudge { deck_id, op } => answered(sync.nudge(mixer, deck_id, op)),
        SetFader { target, value } => answered(mixer.set_fader(target, value)),
        Loop { deck_id, op } => {
            transport_result(mixer, deck_id, move |deck| deck.apply_loop(op))
        }
        SetKeylock { deck_id, mode } => {
            transport(mixer, deck_id, move |deck| deck.set_keylock_mode(mode))
        }
        // The pitch axis is a placeholder: the value is remembered and reported, nothing more.
        SetKey {
            deck_id,
            semitones,
        } => transport(mixer, deck_id, move |deck| deck.set_key_shift(semitones)),
        SetStems { deck_id, stems } => Some(match mixer.set_stems(deck_id, stems) {
            Ok(()) => CommandResponse::Ok,
            Err(err) => error(err),
        }),
        Stem { deck_id, op } => Some(match mixer.apply_stem(deck_id, op) {
            Ok(()) => CommandResponse::Ok,
            Err(err) => error(err),
        }),
        SetAnalysis { deck_id, analysis } => match mixer.deck_mut(deck_id as usize) {
            Some(deck) => {
                deck.set_analysis(analysis);
                Some(CommandResponse::Ok)
            }
            None => Some(error(unknown_deck(deck_id, mixer.channel_count()))),
        },
        GetState { deck_id } => match mixer.channel(deck_id as usize) {
            Some(channel) => Some(CommandResponse::State(state_of(deck_id, channel, sync))),
            None => Some(error(unknown_deck(deck_id, mixer.channel_count()))),
        },
        GetStemState { deck_id } => match mixer.channel(deck_id as usize) {
            Some(channel) => Some(CommandResponse::Stems(state_of(deck_id, channel, sync))),
            None => Some(error(unknown_deck(deck_id, mixer.channel_count()))),
        },
        GetAllStates => {
            // One pass over every channel: all frames in the answer come from the same block, so
            // differences between decks are free of sampling skew.
            let states = (0..mixer.channel_count())
                .filter_map(|index| {
                    mixer
                        .channel(index)
                        .map(|channel| state_of(index as DeckId, channel, sync))
                })
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

/// The snapshot a front-end reads. Takes the whole channel rather than just its deck because the
/// per-stem state (level, mute, solo) lives on the mixer side of the channel, while "are stems
/// live yet" is the deck's answer.
fn state_of(deck_id: DeckId, channel: &crate::mixer::Channel, sync: &SyncGroup) -> DeckState {
    let deck = channel.deck();
    DeckState {
        deck_id,
        current_frame: deck.current_frame(),
        playing: deck.is_playing(),
        total_frames: deck.total_frames(),
        bpm: deck.bpm(),
        bpm_at_frame: deck.bpm_at_frame() as f32,
        key: deck.key().map(|key| key.traditional()),
        virtual_frame: deck.virtual_frame(),
        loop_range: deck
            .loop_range()
            .map(|range| (range.in_frame, range.out_frame)),
        loop_in_armed: deck.loop_in_armed(),
        tempo: deck.tempo() as f32,
        tempo_fader: deck.tempo_fader() as f32,
        tempo_range: deck.tempo_range() as f32,
        nudgerate: deck.nudgerate() as f32,
        playing_rate: deck.playing_rate() as f32,
        lock: deck.lock(),
        align: deck.align_label().map(str::to_owned),
        nudge: deck.nudge_rate() as f32,
        // Reported as "who do I follow": a deck that leads nobody shows `None` even while it is
        // the group's leader, which the `sync_mode` / `group_bpm` fields already describe.
        sync_leader: sync.leader.filter(|&leader| leader != deck_id),
        sync_mode: sync.mode_label().to_owned(),
        group_bpm: sync.group_bpm as f32,
        cue_frame: deck.cue_point(),
        keylock: deck.keylock_mode(),
        key_shift: deck.key_shift(),
        stems: channel.stem_status(),
    }
}

/// Wraps a coordinator result into an answer: refusals must reach the caller as `Error`, not be
/// swallowed — a sync with no grid or a reversed lock is the user's mistake and they need to see it.
fn answered(result: Result<(), String>) -> Option<CommandResponse> {
    Some(match result {
        Ok(()) => CommandResponse::Ok,
        Err(message) => error(message),
    })
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
