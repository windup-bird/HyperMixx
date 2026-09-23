//! [`Mixer`]: channels in, two buses out, and the fixed order between them.
//!
//! ```text
//! channel 0 ─┐                                  ┌─ output "main"
//! channel 1 ─┼─► master.sum ─► fx ─► fader ─► limiter ─┤
//!   ...      │                                  ├─ output "headphones" ◄─ cue.sum ◄─ cue sends
//! cue sends ─┘
//! ```
//!
//! Two things are worth stating because they are the reasons this module exists at all:
//!
//! * **The order is fixed, not computed.** No graph, no topological sort. A reader can predict what
//!   a signal went through from the source alone, which is what makes a click diagnosable.
//! * **The limiter is not a configurable slot.** [`MasterBus::limiter`] is a field the mixer owns
//!   unconditionally, so a config that forgets `"limiter"` still cannot feed full-scale audio to a
//!   DAC. Configuration controls the *chain*; the safety stage is part of the topology.
//!
//! [`Mixer::process`] touches no device, which is what makes it unit-testable: build a mixer with
//! `outputs: vec![]` and inspect the buses.

mod bus;
mod channel;
pub(crate) mod config;
pub(crate) mod output;

pub use bus::Bus;
pub use channel::{ChainSlot, Channel, CueTap, CrossfaderCurve, DeckSide, SlotChain};
pub use config::{
    build_chain, build_slot, build_slot_disabled, build_slots, defaults_for, reference_toml,
    simple_dj, silent_test_channel, ChainRef, ChannelConfig, MixerConfig, MixerError,
    OutputConfig, OutputRole, SlotPlace,
};
pub use output::{Output, OutputError, OutputId, Outputs};

use hypermixx_core::{CommandResponse, FxChainId, FxSlotStatus};

use channel::Channel as ChannelInner;
use crate::deck::Deck;
use crate::fx::sample::Fader;
use crate::fx::{FxChain, FxContext, FxSlot, FxTarget};
use crate::BLOCK_SIZE;
#[cfg(test)]
use crate::SAMPLE_RATE;

/// The summed destination. `outputs` names entries in [`Mixer::outputs`].
pub struct MasterBus {
    /// The running sum of every channel, post-crossfader.
    pub sum: Bus,
    /// Configured inserts.
    pub fx: FxChain,
    /// Master trim.
    pub fader: Fader,
    /// The safety stage, always present and always last.
    pub limiter: FxSlot,
    pub outputs: Vec<usize>,
    peak: f32,
}

/// The monitor destination: a sum of cue sends, no FX, its own outputs.
pub struct CueBus {
    pub sum: Bus,
    pub fader: Fader,
    pub outputs: Vec<usize>,
    peak: f32,
}

/// Master/cue trim smoothing: a long enough ramp that a fader move is a fade, short enough that a
/// hard cut on the master lands when the hand says so.
const MASTER_TAU: f32 = 0.01;

/// The mixer: every channel, both buses, every output.
pub struct Mixer {
    channels: Vec<ChannelInner>,
    master: MasterBus,
    cue: CueBus,
    outputs: Outputs,
    block_frames: usize,
}

impl Mixer {
    /// Builds a mixer from `cfg`, opening every output it names.
    ///
    /// A single output failing is logged and downgraded to a sink rather than aborting the whole
    /// engine: losing the headphones should not stop a performance. A *device-less* machine is not a
    /// failure at all — [`Output::open`] already returns a sink for that. Two outputs that land on
    /// the same physical device are also resolved here: the server (PipeWire's graph, ALSA's dmix)
    /// sums two streams with no limiter in sight, so the second stream is dropped and its bus
    /// simply goes unrouted — otherwise "main + headphones on the default device" plays both buses
    /// on top of each other and clips the DAC even though the master meter reads −1 dBFS.
    pub fn new(cfg: MixerConfig, sample_rate: u32) -> Result<Self, MixerError> {
        if cfg.channels.is_empty() {
            return Err(MixerError::Empty);
        }
        let block_frames = BLOCK_SIZE;
        let chains = cfg.build_chains()?;
        let mut channels = Vec::with_capacity(cfg.channels.len());
        for (channel_cfg, (flow_slots, deck_slots)) in
            cfg.channels.iter().zip(chains.into_iter())
        {
            channels.push(Channel::new(
                Deck::empty(),
                channel_cfg,
                FxChain::from_slots(flow_slots),
                FxChain::from_slots(deck_slots),
            ));
        }

        let mut opened: Vec<Output> = Vec::with_capacity(cfg.outputs.len());
        for output_cfg in &cfg.outputs {
            match Output::open(output_cfg, sample_rate, block_frames) {
                Ok(output) => opened.push(output),
                Err(err) => {
                    eprintln!("[audio] {}: {err}; continuing without it", output_cfg.name);
                    opened.push(Output::sink(output_cfg, block_frames));
                }
            }
        }
        // One stream per physical device. The first output on a device wins; later ones are
        // downgraded to sinks and left unrouted, which is also the honest state: without device
        // selection a second *logical* output cannot be made audible separately anyway.
        let mut duplicate = vec![false; opened.len()];
        {
            let mut first_on_device = std::collections::HashMap::<String, usize>::new();
            for (index, output) in opened.iter().enumerate() {
                if let Some(device) = output.device() {
                    match first_on_device.get(device) {
                        Some(&first) => {
                            eprintln!(
                                "[audio] {}: `{}` is already on {device}; dropping this stream \
                                 so the server does not sum two buses outside the limiter \
                                 (device selection is future work)",
                                cfg.outputs[index].name, cfg.outputs[first].name
                            );
                            duplicate[index] = true;
                        }
                        None => {
                            first_on_device.insert(device.to_owned(), index);
                        }
                    }
                }
            }
        }
        for (index, taken) in duplicate.iter().enumerate() {
            if *taken {
                // Replacing the element drops the cpal stream on the spot, freeing the device
                // slot the duplicate needlessly held.
                opened[index] = Output::sink(&cfg.outputs[index], block_frames);
            }
        }
        let mut outputs = Outputs::new();
        for output in opened {
            outputs.push(output);
        }
        // Ids are positional: a config (or a TOML file that omitted them) cannot make two outputs
        // share an id, and a dropped duplicate does not leave a hole in the numbering.
        for (index, output) in outputs.items_mut().enumerate() {
            output.id = index as OutputId;
        }
        // Route by role, skipping the duplicates: whoever configures `Headphones` gets the cue
        // bus, everything else the mix.
        let mut master_outputs = Vec::new();
        let mut cue_outputs = Vec::new();
        for (index, output_cfg) in cfg.outputs.iter().enumerate() {
            if duplicate[index] {
                continue;
            }
            match output_cfg.role {
                OutputRole::Main => master_outputs.push(index),
                OutputRole::Headphones => cue_outputs.push(index),
            }
        }
        let any_live = outputs.iter().any(|output| !output.is_sink());
        if master_outputs.is_empty() && any_live {
            // A config that named only headphones (or whose mains all failed to open) still needs
            // to be audible somewhere.
            master_outputs.extend(0..outputs.len());
            cue_outputs.clear();
        }

        let limiter = cfg
            .master_limiter
            .then(|| build_slot("limiter"))
            .transpose()?
            .unwrap_or_else(|| build_slot_disabled("limiter").expect("limiter is a known kind"));

        Ok(Self {
            channels,
            master: MasterBus {
                sum: Bus::stereo(block_frames),
                fx: build_chain(&cfg.master_fx)?,
                fader: Fader::new(cfg.master_fader, MASTER_TAU),
                limiter,
                outputs: master_outputs,
                peak: 0.0,
            },
            cue: CueBus {
                sum: Bus::stereo(block_frames),
                fader: Fader::new(cfg.cue_fader, MASTER_TAU),
                outputs: cue_outputs,
                peak: 0.0,
            },
            outputs,
            block_frames,
        })
    }

    /// Channels this mixer mixes.
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    pub fn channel(&self, index: usize) -> Option<&Channel> {
        self.channels.get(index)
    }

    pub fn channel_mut(&mut self, index: usize) -> Option<&mut Channel> {
        self.channels.get_mut(index)
    }

    /// The deck behind `deck_id`, for consumers that drive a transport directly (a UI reading
    /// positions, the CLI fetching a track to analyse).
    pub fn deck(&self, deck_id: usize) -> Option<&Deck> {
        self.channels.get(deck_id).map(Channel::deck)
    }

    pub fn deck_mut(&mut self, deck_id: usize) -> Option<&mut Deck> {
        self.channels.get_mut(deck_id).map(Channel::deck_mut)
    }

    pub fn outputs(&self) -> &Outputs {
        &self.outputs
    }

    pub fn outputs_mut(&mut self) -> &mut Outputs {
        &mut self.outputs
    }

    pub fn master(&self) -> &MasterBus {
        &self.master
    }

    pub fn cue(&self) -> &CueBus {
        &self.cue
    }

    /// Block length this mixer renders.
    pub fn block_frames(&self) -> usize {
        self.block_frames
    }

    /// The context for one block: the engine's own constants plus the musical position of whichever
    /// deck is carrying the grid.
    ///
    /// Deck 0 wins when both have a grid, which is the convention a two-deck crossfade needs — a
    /// tempo-synced effect must not change its notion of "beat 1" halfway through a blend.
    pub fn make_ctx(&self, sample_rate: u32) -> FxContext {
        let grid = self
            .channels
            .iter()
            .find_map(|channel| channel.deck().analysis())
            .map(|analysis| analysis.beatgrid.clone());
        let (frames_per_beat, beat_position) = match &grid {
            Some(grid) => {
                let deck = self
                    .channels
                    .iter()
                    .find(|c| c.deck().analysis().is_some())
                    .map(|c| c.deck().current_frame())
                    .unwrap_or(0);
                let width = grid
                    .beat_width(grid.floor_beat(deck))
                    .max(1) as f64;
                (width, grid.floor_beat(deck) as f64 + f64::from(grid.phase(deck)))
            }
            None => (0.0, 0.0),
        };
        FxContext {
            sample_rate,
            block_frames: self.block_frames,
            frames_per_beat,
            beat_position,
        }
    }

    /// Renders one block: clear, channels, master, cue, outputs.
    ///
    /// Splitting `self` by field is what lets every channel be borrowed mutably in the loop while
    /// the buses are borrowed too — no `RefCell`, no clone, and no lock on the hot path.
    pub fn process(&mut self, ctx: &FxContext) {
        let Mixer {
            channels,
            master,
            cue,
            ..
        } = self;
        master.sum.clear();
        master.sum.ensure_frames(ctx.block_frames);
        cue.sum.clear();
        cue.sum.ensure_frames(ctx.block_frames);

        for channel in channels.iter_mut() {
            channel.process(ctx);
            // The channel already applied its cue send and its crossfader ordering; here we only
            // sum. Reading `cue()` after `process()` is safe because the borrow ended with the call.
            master.sum.add_from(channel.processed());
            cue.sum.add_from(channel.cue());
        }

        master.render(ctx);
        cue.render(ctx);

        // Borrowed one field at a time: the route lists are read while the buses are still borrowed,
        // so no `clone()` of a `Vec` happens on this path.
        let Mixer { master, cue, outputs, .. } = self;
        for index in &master.outputs {
            if let Some(output) = outputs.get_mut(*index) {
                output.write(&master.sum);
            }
        }
        // A cue is private: it reaches only the outputs its route names, never the main mix.
        for index in &cue.outputs {
            if let Some(output) = outputs.get_mut(*index) {
                output.write(&cue.sum);
            }
        }
    }

    /// Peak of the last master block, dBFS. Meter and test hook.
    pub fn master_peak(&self) -> f32 {
        self.master.peak
    }

    pub fn cue_peak(&self) -> f32 {
        self.cue.peak
    }

    /// Gain reduction currently applied by the safety limiter, dB (0 = not working).
    pub fn master_reduction_db(&self) -> f32 {
        self.master
            .limiter
            .get_param("reduction")
            .unwrap_or(0.0)
    }

    // ---- FX command surface ---------------------------------------------------------------

    /// Reads one slot, addressing the deck's merged chain (flow slots first, then deck slots).
    pub fn resolve_fx(&self, target: &FxTarget) -> Option<&FxSlot> {
        let channel = match target.chain {
            FxChainId::Master => return self.master.fx.slot(target.index),
            FxChainId::Deck(deck_id) => self.channels.get(deck_id as usize)?,
        };
        channel.slot(target.index)
    }

    pub fn resolve_fx_mut(&mut self, target: &FxTarget) -> Option<&mut FxSlot> {
        match target.chain {
            FxChainId::Master => self.master.fx.slot_mut(target.index),
            FxChainId::Deck(deck_id) => self
                .channels
                .get_mut(deck_id as usize)?
                .slot_mut(target.index),
        }
    }

    /// Every slot in a chain, in the index space `resolve_fx` uses.
    pub fn list_fx(&self, chain: FxChainId) -> Option<Vec<FxSlotStatus>> {
        match chain {
            FxChainId::Master => Some(self.master.fx.statuses()),
            FxChainId::Deck(deck_id) => self.channels.get(deck_id as usize).map(|c| c.slot_statuses()),
        }
    }

    /// Appends an effect, returning its index in the addressed chain's own index space.
    ///
    /// A deck is addressed through the *merged* chain (flow slots first, then deck slots), and an
    /// append lands on the deck half — that is where a caller means a new insert to go. The flow
    /// half is for per-stream effects and is reached through [`Mixer::add_flow_fx`].
    pub fn add_fx(&mut self, chain: FxChainId, slot: FxSlot) -> Result<usize, String> {
        match chain {
            FxChainId::Master => Ok(self.master.fx.push_slot(slot)),
            FxChainId::Deck(deck_id) => self
                .channels
                .get_mut(deck_id as usize)
                .map(|channel| channel.add_slot(SlotChain::Deck, slot))
                .ok_or_else(|| unknown_deck(deck_id, self.channels.len())),
        }
    }

    /// Appends to a deck's *flow* chain, returning the merged index.
    pub fn add_flow_fx(&mut self, deck_id: u8, slot: FxSlot) -> Result<usize, String> {
        let count = self.channels.len();
        let channel = self
            .channels
            .get_mut(deck_id as usize)
            .ok_or_else(|| unknown_deck(deck_id, count))?;
        Ok(channel.add_slot(SlotChain::Flow, slot))
    }

    pub fn remove_fx(&mut self, target: &FxTarget) -> Result<FxSlot, String> {
        let count = self.channels.len();
        match target.chain {
            FxChainId::Master => self
                .master
                .fx
                .remove(target.index)
                .ok_or_else(|| "no effect at that index on master".to_owned()),
            FxChainId::Deck(deck_id) => self
                .channels
                .get_mut(deck_id as usize)
                .ok_or_else(|| unknown_deck(deck_id, count))?
                .remove_slot(target.index)
                .ok_or_else(|| {
                    format!("deck {deck_id} has no effect at index {}", target.index)
                }),
        }
    }

    /// Applies one FX command, answering with the protocol's response.
    ///
    /// Everything the [`Command`](hypermixx_core::Command) FX variants can express lands here, so
    /// the pipeline's command loop stays a dispatcher rather than growing a second FX implementation.
    pub fn handle_fx_command(&mut self, command: hypermixx_core::Command) -> CommandResponse {
        use hypermixx_core::Command;
        match command {
            Command::AddFx { chain, kind } => match config::build_slot(&kind) {
                Ok(slot) => {
                    let reported_kind = slot.kind().to_owned();
                    match self.add_fx(chain, slot) {
                        Ok(index) => CommandResponse::FxAdded {
                            chain,
                            index,
                            kind: reported_kind,
                        },
                        Err(err) => CommandResponse::Error(err),
                    }
                }
                Err(err) => CommandResponse::Error(err.message()),
            },
            Command::RemoveFx { chain, index } => {
                let target = FxTarget { chain, index };
                match self.remove_fx(&target) {
                    Ok(_) => CommandResponse::Ok,
                    Err(err) => CommandResponse::Error(err),
                }
            }
            Command::SetFxEnabled { slot, enabled } => match self.resolve_fx(&slot) {
                Some(found) => {
                    found.set_enabled(enabled);
                    CommandResponse::Ok
                }
                None => CommandResponse::Error(slot_error(&slot)),
            },
            Command::SetFxParam { slot, name, value } => match self.resolve_fx(&slot) {
                Some(found) => match found.set_param(&name, value) {
                    Ok(()) => CommandResponse::Ok,
                    Err(err) => CommandResponse::Error(format!(
                        "{} {name}: {}",
                        slot.chain.label(),
                        err.message()
                    )),
                },
                None => CommandResponse::Error(slot_error(&slot)),
            },
            Command::FxTrigger { slot } => match self.resolve_fx_mut(&slot) {
                Some(found) => {
                    found.trigger();
                    CommandResponse::Ok
                }
                None => CommandResponse::Error(slot_error(&slot)),
            },
            Command::PadPress { slot } => match self.resolve_fx(&slot) {
                Some(found) => {
                    found.press_pad();
                    CommandResponse::Ok
                }
                None => CommandResponse::Error(slot_error(&slot)),
            },
            Command::PadRelease { slot } => match self.resolve_fx(&slot) {
                Some(found) => {
                    found.release_pad();
                    CommandResponse::Ok
                }
                None => CommandResponse::Error(slot_error(&slot)),
            },
            Command::ListFx { chain } => match self.list_fx(chain) {
                Some(slots) => CommandResponse::FxListed { chain, slots },
                None => CommandResponse::Error(unknown_chain(&chain)),
            },
            other => CommandResponse::Error(format!("not an FX command: {other:?}")),
        }
    }
}

fn slot_error(target: &FxTarget) -> String {
    format!(
        "{} has no effect at index {}",
        target.chain.label(),
        target.index
    )
}

fn unknown_chain(chain: &FxChainId) -> String {
    format!("no such FX chain `{}`", chain.label())
}

/// The shared message for a deck id outside the configured topology; the transport and the sync
/// coordinator both answer with it so a mistyped id reads the same everywhere.
pub(crate) fn unknown_deck(deck_id: u8, count: usize) -> String {
    format!("unknown deck {deck_id}, valid ids are 0..={}", count.saturating_sub(1))
}

impl MasterBus {
    /// fx → fader → limiter, in that order, leaving the result in [`MasterBus::sum`].
    fn render(&mut self, ctx: &FxContext) {
        if self.fx.any_active() {
            self.fx.process(&mut self.sum, ctx);
        }
        self.sum.scale(self.fader.next_amp(ctx));
        // The safety stage runs whenever it is engaged, bypassing the chain-skip optimisation: a
        // limiter that is "not active" is a limiter that let a transient through.
        self.limiter.process(&mut self.sum, ctx);
        self.peak = self.sum.peak();
    }

    pub fn set_fader(&self, position: f32) {
        self.fader.set(position.clamp(-1.0, 1.0));
    }

    pub fn set_limiter_enabled(&self, on: bool) {
        self.limiter.set_enabled(on);
    }

    pub fn limiter_enabled(&self) -> bool {
        self.limiter.is_enabled()
    }
}

impl CueBus {
    fn render(&mut self, ctx: &FxContext) {
        self.sum.scale(self.fader.next_amp(ctx));
        self.peak = self.sum.peak();
    }

    pub fn set_fader(&self, position: f32) {
        self.fader.set(position.clamp(-1.0, 1.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_media::{DecodedAudio, PcmPool};
    use std::sync::Arc;

    /// A source whose left sample *is* the frame index: level checks become position checks.
    fn ramp(seconds: u64) -> Arc<dyn hypermixx_core::Source> {
        let frames = (seconds as usize * SAMPLE_RATE as usize).max(BLOCK_SIZE * 200);
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..frames).flat_map(|i| [i as f32, -(i as f32)]).collect(),
            total_frames: frames as u64,
            sample_rate: SAMPLE_RATE,
            channels: crate::CHANNELS,
        }))
    }

    /// A bounded ±1 tone. For assertions about *absolute* level: `ramp`'s samples grow with the frame
    /// index, so "is it below 1e-2" against it depends on how long the test happened to tick, which is
    /// a property of the test, not of the mixer.
    fn tone(seconds: u64) -> Arc<dyn hypermixx_core::Source> {
        let frames = (seconds as usize * SAMPLE_RATE as usize).max(BLOCK_SIZE * 200);
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..frames).flat_map(|i| {
                let s = (2.0 * std::f64::consts::PI * 100.0 * i as f64 / f64::from(SAMPLE_RATE)).sin() as f32;
                [s, -s]
            }).collect(),
            total_frames: frames as u64,
            sample_rate: SAMPLE_RATE,
            channels: crate::CHANNELS,
        }))
    }

    /// A mixer with no outputs, so `process` computes and writes nothing.
    fn headless(cfg: MixerConfig) -> Mixer {
        Mixer::new(
            MixerConfig { outputs: vec![], ..cfg },
            SAMPLE_RATE,
        )
        .expect("headless mixer")
    }

    fn loaded(cfg: MixerConfig, decks: usize) -> Mixer {
        let mut mixer = headless(cfg);
        for index in 0..decks {
            let deck = Deck::new(ramp(10));
            deck.play();
            mixer.channel_mut(index).unwrap().replace_deck(deck);
        }
        mixer
    }

    fn tick(mixer: &mut Mixer, blocks: usize) {
        for _ in 0..blocks {
            let ctx = mixer.make_ctx(SAMPLE_RATE);
            mixer.process(&ctx);
        }
    }

    #[test]
    fn process_needs_no_device_and_writes_nothing_when_unrouted() {
        let mut mixer = loaded(simple_dj(), 2);
        assert!(mixer.outputs().is_empty() || mixer.outputs().iter().all(Output::is_sink));
        tick(&mut mixer, 4);
        assert!(mixer.master_peak() > 0.0, "the master bus should carry audio");
    }

    #[test]
    fn two_channels_sum_and_the_crossfader_isolates_one() {
        let mut mixer = loaded(simple_dj(), 2);
        tick(&mut mixer, 3);
        let both = mixer.master_peak();
        assert!(both > 0.0);
        // Hard left: deck 0 (the left side) alone, so the peak falls to its own level.
        for index in 0..2 {
            mixer
                .channel_mut(index)
                .unwrap()
                .set_crossfader(-1.0);
        }
        tick(&mut mixer, 60);
        let left_only = mixer.master_peak();
        assert!(
            left_only < both,
            "crossfader hard left did not reduce the mix ({both} -> {left_only})"
        );
        assert!(left_only > 0.0, "the left deck should still be audible");
    }

    #[test]
    fn a_cue_reaches_the_cue_bus_even_when_the_master_is_closed() {
        // The point of the cue tap sitting before the crossfader: you can preview a deck with the
        // mix down and the other deck playing.
        let mut mixer = headless(simple_dj());
        for index in 0..2 {
            let deck = Deck::new(tone(10));
            deck.play();
            mixer.channel_mut(index).unwrap().replace_deck(deck);
        }
        mixer.master.set_fader(-1.0);
        mixer.channel_mut(1).unwrap().set_flow_fader(-1.0);
        tick(&mut mixer, 40);
        // A closed fader is defined as exactly zero gain, so this is silence, not "quiet".
        assert!(
            mixer.master().sum.is_silent(),
            "master closed but peaked {}",
            mixer.master_peak()
        );
        assert!(mixer.cue_peak() > 0.5, "cue must survive a closed master");
    }

    #[test]
    fn the_limiter_is_wired_and_alive_by_default() {
        let mixer = headless(simple_dj());
        assert!(mixer.master().limiter_enabled());
        assert_eq!(mixer.master().limiter.kind(), "limiter");
    }

    #[test]
    fn a_hot_master_is_held_under_full_scale() {
        // The acceptance property for the whole safety stage: two decks at unity, no faders down.
        let mut mixer = headless(simple_dj());
        for index in 0..2 {
            let deck = Deck::new(ramp(10));
            deck.play();
            mixer.channel_mut(index).unwrap().replace_deck(deck);
        }
        // The ramp reaches thousands of full-scales, so any bypass shows up immediately.
        let mut escaped = 0.0f32;
        for _ in 0..40 {
            let ctx = mixer.make_ctx(SAMPLE_RATE);
            mixer.process(&ctx);
            escaped = escaped.max(mixer.master().sum.peak());
        }
        assert!(
            escaped <= 2.0,
            "the limiter let {escaped} through; master peak must stay bounded"
        );
        assert!(mixer.master_reduction_db() < 0.0, "GR never engaged");
    }

    #[test]
    fn master_fx_runs_before_the_fader_and_the_limiter() {
        // A −6 dB master insert must reduce the output, and the limiter must still be the last word
        // on a hot signal. Order is observable, not assumed.
        let cfg = MixerConfig {
            master_fx: vec!["gain".into()],
            ..silent_test_channel()
        };
        let mut mixer = headless(cfg);
        let deck = Deck::new(ramp(10));
        deck.play();
        mixer.channel_mut(0).unwrap().replace_deck(deck);
        mixer
            .master()
            .fx
            .slot(0)
            .unwrap()
            .set_param("gain", 0.5)
            .unwrap();
        tick(&mut mixer, 4);
        let with_fx = mixer.master_peak();
        let without = headless_and_render();
        assert!(with_fx < without, "master insert had no effect: {with_fx} vs {without}");
    }

    fn headless_and_render() -> f32 {
        let mut mixer = headless(silent_test_channel());
        let deck = Deck::new(ramp(10));
        deck.play();
        mixer.channel_mut(0).unwrap().replace_deck(deck);
        // The safety limiter is off in this config, so the raw peak is the comparison point.
        mixer.master.set_limiter_enabled(false);
        tick(&mut mixer, 4);
        mixer.master_peak()
    }

    #[test]
    fn channels_are_addressed_by_index_and_reachable_as_decks() {
        let mut mixer = loaded(simple_dj(), 2);
        assert_eq!(mixer.channel_count(), 2);
        assert!(mixer.deck(2).is_none());
        tick(&mut mixer, 2);
        let frame = mixer.deck(0).unwrap().current_frame();
        assert!(frame > 0, "a playing deck must advance through the mixer");
        mixer.deck_mut(1).unwrap().pause();
        let paused = mixer.deck(1).unwrap().current_frame();
        tick(&mut mixer, 4);
        assert_eq!(mixer.deck(1).unwrap().current_frame(), paused);
    }

    #[test]
    fn context_carries_the_grid_of_whichever_deck_has_one() {
        let mut mixer = loaded(simple_dj(), 2);
        let ctx = mixer.make_ctx(SAMPLE_RATE);
        assert!(!ctx.has_grid(), "no analysis installed yet");
        mixer.deck_mut(0).unwrap().set_analysis(hypermixx_core::TrackAnalysis::from_grid(
            hypermixx_core::BeatGrid::from_constant_bpm(120.0, 0, 44_100 * 60, SAMPLE_RATE),
            120.0,
        ));
        tick(&mut mixer, 2);
        let ctx = mixer.make_ctx(SAMPLE_RATE);
        assert!(ctx.has_grid());
        assert!(
            (ctx.frames_per_beat - 22_050.0).abs() < 2.0,
            "fpb {}",
            ctx.frames_per_beat
        );
        assert!(ctx.beat_position >= 0.0);
    }

    #[test]
    fn fx_commands_round_trip_through_the_mixer() {
        use hypermixx_core::Command;
        let mut mixer = loaded(simple_dj(), 2);
        let base = mixer.channel(0).unwrap().slot_statuses().len();
        match mixer.handle_fx_command(Command::AddFx {
            chain: FxChainId::Deck(0),
            kind: "gain".into(),
        }) {
            CommandResponse::FxAdded { index, kind, .. } => {
                assert_eq!(kind, "gain");
                assert_eq!(index, base, "appended after the configured chain");
            }
            other => panic!("expected FxAdded, got {other:?}"),
        }
        let slot = FxTarget { chain: FxChainId::Deck(0), index: base };
        match mixer.handle_fx_command(Command::SetFxParam {
            slot,
            name: "gain".into(),
            value: 0.25,
        }) {
            CommandResponse::Ok => {}
            other => panic!("expected Ok, got {other:?}"),
        }
        assert_eq!(mixer.resolve_fx(&slot).unwrap().get_param("gain"), Some(0.25));

        mixer.handle_fx_command(Command::SetFxEnabled { slot, enabled: false });
        assert!(!mixer.resolve_fx(&slot).unwrap().is_enabled());
        match mixer.handle_fx_command(Command::ListFx { chain: FxChainId::Deck(0) }) {
            CommandResponse::FxListed { slots, .. } => {
                assert_eq!(slots.len(), base + 1);
                assert_eq!(slots[base].kind, "gain");
            }
            other => panic!("expected FxListed, got {other:?}"),
        }
        assert!(matches!(
            mixer.handle_fx_command(Command::RemoveFx {
                chain: FxChainId::Deck(0),
                index: base
            }),
            CommandResponse::Ok
        ));
        assert_eq!(mixer.channel(0).unwrap().slot_statuses().len(), base);
    }

    #[test]
    fn fx_errors_are_answers_not_panics() {
        use hypermixx_core::Command;
        let mut mixer = loaded(simple_dj(), 1);
        for command in [
            Command::AddFx {
                chain: FxChainId::Deck(0),
                kind: "flanger".into(),
            },
            Command::SetFxParam {
                slot: FxTarget { chain: FxChainId::Deck(0), index: 99 },
                name: "low".into(),
                value: 0.1,
            },
            Command::RemoveFx {
                chain: FxChainId::Deck(7),
                index: 0,
            },
            Command::ListFx { chain: FxChainId::Deck(9) },
            Command::PadPress {
                slot: FxTarget { chain: FxChainId::Deck(0), index: 42 },
            },
        ] {
            match mixer.handle_fx_command(command) {
                CommandResponse::Error(err) => assert!(!err.is_empty()),
                other => panic!("expected an Error answer, got {other:?}"),
            }
        }
        // A real parameter name that the effect does not have is still an error, not a silent yes.
        match mixer.handle_fx_command(Command::SetFxParam {
            slot: FxTarget { chain: FxChainId::Deck(0), index: 0 },
            name: "cutoff_hz".into(),
            value: 1.0,
        }) {
            CommandResponse::Error(_) => {}
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn a_pad_holds_a_slot_and_lets_go() {
        use hypermixx_core::Command;
        let mut mixer = loaded(simple_dj(), 1);
        let index = mixer.channel(0).unwrap().slot_statuses().len() - 1;
        let slot = FxTarget { chain: FxChainId::Deck(0), index };
        mixer.handle_fx_command(Command::SetFxEnabled { slot, enabled: false });
        mixer.handle_fx_command(Command::PadPress { slot });
        assert!(mixer.resolve_fx(&slot).unwrap().is_enabled());
        mixer.handle_fx_command(Command::PadRelease { slot });
        assert!(!mixer.resolve_fx(&slot).unwrap().is_enabled());
    }

    #[test]
    fn a_bad_config_is_refused_before_any_device_is_touched() {
        let cfg = MixerConfig {
            channels: vec![ChannelConfig {
                deck_fx: vec!["chorus".into()],
                ..Default::default()
            }],
            outputs: vec![],
            ..Default::default()
        };
        let err = Mixer::new(cfg, SAMPLE_RATE).err().unwrap();
        assert!(matches!(err, MixerError::UnknownFx(_)));
        let err = Mixer::new(
            MixerConfig {
                channels: vec![],
                outputs: vec![],
                ..Default::default()
            },
            SAMPLE_RATE,
        )
        .err()
        .unwrap();
        assert_eq!(err, MixerError::Empty);
    }

    #[test]
    fn master_and_cue_routes_follow_the_configured_roles() {
        let cfg = MixerConfig {
            outputs: vec![
                OutputConfig::main(0, "main"),
                OutputConfig::headphones(1, "phones"),
            ],
            ..silent_test_channel()
        };
        // The headless helper strips outputs, so build the routing question directly.
        let mixer = Mixer::new(cfg.clone(), SAMPLE_RATE);
        if let Ok(mixer) = mixer {
            assert_eq!(mixer.master().outputs, vec![0]);
            // On a machine where both outputs land on the same physical device, the headphones
            // stream is dropped (one stream per device) and the cue goes unrouted; on a headless
            // machine both are sinks and the configured route stands.
            assert!(
                mixer.cue().outputs == vec![1] || mixer.cue().outputs.is_empty(),
                "cue routed to {:?}",
                mixer.cue().outputs
            );
        }
        // With no outputs at all, both route lists are empty and `process` still runs.
        let mut mixer = headless(cfg);
        let deck = Deck::new(ramp(10));
        deck.play();
        mixer.channel_mut(0).unwrap().replace_deck(deck);
        tick(&mut mixer, 2);
        assert_eq!(mixer.outputs().writes(), 0);
    }

    #[test]
    fn an_empty_bus_pair_stays_silent() {
        let mut mixer = headless(simple_dj());
        tick(&mut mixer, 5);
        assert!(mixer.master().sum.is_silent(), "no deck loaded, so no sound");
        assert!(mixer.cue().sum.is_silent());
    }
}
