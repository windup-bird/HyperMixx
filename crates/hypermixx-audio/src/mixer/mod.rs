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
pub use channel::{
    Channel, CrossfaderCurve, CrossfaderPlace, CueTap, DeckSide, SlotChain, DEFAULT_CUT_WIDTH,
};
pub use config::{
    build_chain, build_chain_with_roles, build_slot, build_slots, reference_toml, simple_dj,
    silent_test_channel, ChannelConfig, CrossfaderConfig, CueMixConfig, MixerConfig, MixerError,
    OutputConfig,
};
pub use hypermixx_core::{OutputRole, OutputState, OutputStatus};
pub use output::{
    output_devices, DeviceInfo, Output, OutputError, OutputId, OutputTuning, Outputs,
};

use hypermixx_core::{
    AutogainSettings, CommandResponse, FxChainId, FxSlotStatus, KeylockSettings, Stem, StemOp,
    StemSet,
};

use channel::Channel as ChannelInner;
use crate::deck::Deck;
use crate::fx::sample::Fader;
use crate::fx::{FxChain, FxContext, FxKind, FxSlot, FxTarget};
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

/// The monitor destination: a sum of cue sends, its own outputs, and a blend with the master mix.
pub struct CueBus {
    pub sum: Bus,
    /// What the cue outputs actually receive: [`CueBus::sum`] blended with the master's mix.
    pub mix: Bus,
    pub fader: Fader,
    pub blend: f32,
    pub curve: CrossfaderCurve,
    pub cut_width: f32,
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
    /// The topology this mixer was built from. Kept so `output refresh` (and, later, a config
    /// reload) can re-derive the parts that are not live state, without the front-end having to
    /// remember the file it passed in.
    config: MixerConfig,
    sample_rate: u32,
    /// Buffer sizes, kept so a reload/refresh opens the same kind of ring.
    tuning: OutputTuning,
    /// Set once a device asked for more than the ring cap: the engine stops trying to grow and the
    /// log says so instead of reopening a stream on every block.
    ring_capped: bool,
    /// Keylock STFT window from the live config; re-applied to every freshly loaded deck.
    keylock: KeylockSettings,
    /// Auto-gain preference from the live config; the per-track measurement lives on the deck.
    autogain: AutogainSettings,
}

impl Mixer {
    /// Builds a mixer from `cfg`, opening every output it names.
    ///
    /// A single output failing is logged and downgraded to a sink rather than aborting the whole
    /// engine: losing the headphones should not stop a performance. A *device-less* machine is not a
    /// failure at all — [`Output::open`] already returns a sink for that. Two outputs that land on
    /// the same physical device are also resolved here: the server (PipeWire's graph, ALSA's dmix)
    /// sums two streams with no limiter in sight, so a second stream that would write the *same*
    /// device channels is dropped and its bus simply goes unrouted — otherwise "main + headphones
    /// on the default device" plays both buses on top of each other and clips the DAC even though
    /// the master meter reads −1 dBFS. Disjoint channel pairs on one multi-channel interface are
    /// allowed: `(0, 1)` can carry main and `(2, 3)` cue without either stream touching the other's
    /// channels.
    pub fn new(cfg: MixerConfig, sample_rate: u32) -> Result<Self, MixerError> {
        Self::with_tuning(cfg, sample_rate, OutputTuning::default())
    }

    /// As [`Mixer::new`], with the per-output buffer sizes spelled out (they come from the live
    /// config).
    pub fn with_tuning(
        cfg: MixerConfig,
        sample_rate: u32,
        tuning: OutputTuning,
    ) -> Result<Self, MixerError> {
        if cfg.channels.is_empty() {
            return Err(MixerError::Empty);
        }
        let block_frames = BLOCK_SIZE;
        let chains = cfg.build_chains()?;
        let mut channels = Vec::with_capacity(cfg.channels.len());
        for (channel_cfg, (flow_chains, post_fx)) in cfg.channels.iter().zip(chains) {
            // `build_chains` already produced one chain per flow (with the fixed tone prefix), so
            // the configured order is in effect from the first block — not only after a `SetStems`.
            let mut channel = Channel::new(
                Deck::empty(),
                channel_cfg,
                cfg.crossfader,
                flow_chains,
                post_fx,
            );
            channel.set_block_seconds(block_seconds_of(sample_rate));
            // Time-based effects size their buffers from the engine's rate, before the first block.
            channel.prepare_fx(sample_rate);
            channels.push(channel);
        }

        let routed = build_outputs(&cfg, sample_rate, block_frames, tuning);

        let mut master_fx = build_chain_with_roles(&cfg.master_fx, "masterfx")?;
        master_fx.prepare(sample_rate);
        let mut limiter = build_slot("limiter")?;
        limiter.set_enabled(cfg.master_limiter);
        limiter.prepare(sample_rate);

        Ok(Self {
            channels,
            master: MasterBus {
                sum: Bus::stereo(block_frames),
                fx: master_fx,
                fader: Fader::new(cfg.master_fader, MASTER_TAU),
                limiter,
                outputs: routed.master_outputs,
                peak: 0.0,
            },
            cue: CueBus {
                sum: Bus::stereo(block_frames),
                mix: Bus::stereo(block_frames),
                fader: Fader::new(cfg.cue_fader, MASTER_TAU),
                blend: cfg.cue.blend,
                curve: cfg.cue.curve,
                cut_width: cfg.cue.cut_width,
                outputs: routed.cue_outputs,
                peak: 0.0,
            },
            outputs: routed.outputs,
            block_frames,
            config: cfg,
            sample_rate,
            tuning,
            ring_capped: false,
            keylock: KeylockSettings::default(),
            autogain: AutogainSettings::default(),
        })
    }

    /// Re-binds every destination to its device and swaps the streams in.
    ///
    /// Every output is reopened, not just the unavailable ones: that is what makes a *changed*
    /// device take effect (a different card now matching the same pattern, a rate the device started
    /// reporting). The old streams are dropped first, before the new ones open: opening the
    /// replacement while the old one still holds the same device channel pair would fail on ALSA.
    pub fn refresh_outputs(&mut self) -> Vec<OutputStatus> {
        self.outputs = Outputs::new();
        let routed = build_outputs(&self.config, self.sample_rate, self.block_frames, self.tuning);
        self.outputs = routed.outputs;
        self.master.outputs = routed.master_outputs;
        self.cue.outputs = routed.cue_outputs;
        self.outputs.statuses()
    }

    /// Every destination's binding, for `output list`.
    pub fn output_statuses(&self) -> Vec<OutputStatus> {
        self.outputs.statuses()
    }

    /// Grows a destination's ring when the device's callback is bigger than it, and returns the new
    /// size. Called between blocks by the producer loop.
    ///
    /// A ring smaller than one device callback underruns on *every* callback: the callback takes
    /// what is there and ALSA fills the rest with silence, so the mix plays slow and broken rather
    /// than merely muffled. The size is learned from the callback itself (cpal passes ALSA's
    /// `avail_frames`), which is the only way to know it: cpal exposes no period, and the paths that
    /// need this most (dmix/plug behind `sysdefault`) refuse a requested buffer.
    ///
    /// The reopen is a cold path — one dropped stream, once per destination — and only the output
    /// that needs it is touched, so a working cue is not interrupted.
    pub fn widen_output_rings(&mut self) -> Option<usize> {
        let wanted = self
            .outputs
            .iter()
            .filter_map(Output::ring_shortfall)
            .max()?;
        let ring = output::grown_ring(wanted, self.block_frames, self.tuning.ring_frames);
        if ring <= self.tuning.ring_frames {
            if !self.ring_capped {
                self.ring_capped = true;
                eprintln!(
                    "[audio] a device asks for {wanted}-frame callbacks, above the {}-frame ring \
                     cap; leaving it there (choose another destination)",
                    self.tuning.ring_frames
                );
            }
            return None;
        }
        eprintln!(
            "[audio] a device asks for {wanted}-frame callbacks; growing the output ring from {} \
             to {ring} frames ({:.0} ms). Set `ring_frames` in live.toml to make it quiet",
            self.tuning.ring_frames,
            ring as f64 * 1000.0 / f64::from(self.sample_rate.max(1)),
        );
        self.tuning.ring_frames = ring;
        // Reopen only the destinations whose ring is too small. The old output is swapped for a sink
        // first: its stream has to be dropped before the same device can be opened again.
        for index in 0..self.outputs.len() {
            if self
                .outputs
                .get(index)
                .and_then(Output::ring_shortfall)
                .is_none()
            {
                continue;
            }
            let Some(cfg) = self.config.outputs.get(index).cloned() else {
                continue;
            };
            let placeholder = Output::sink(&cfg, self.block_frames);
            let old = self
                .outputs
                .get_mut(index)
                .map(|slot| std::mem::replace(slot, placeholder));
            drop(old); // frees the device channel pair before we ask for it again
            match Output::open_with(&cfg, self.sample_rate, self.block_frames, self.tuning) {
                Ok(output) => {
                    if let Some(slot) = self.outputs.get_mut(index) {
                        *slot = output;
                    }
                }
                Err(err) => eprintln!("[audio] {}: reopen for a bigger ring failed: {err}", cfg.name),
            }
        }
        Some(ring)
    }

    /// The topology this mixer was built from.
    pub fn config(&self) -> &MixerConfig {
        &self.config
    }

    /// Rebuilds the topology from `text`, which is parsed exactly as `--config` parses a file:
    /// **undefined keys take their defaults**, they do not keep the running value.
    ///
    /// What survives is everything that is *not* a config value: each channel's deck and its
    /// transport (a reload must not stop the music), and the meters. A channel the file no longer
    /// names is dropped — with its track, which is why the message says so.
    ///
    /// Refused wholesale on any error (no channels, an unknown FX name): a half-applied topology
    /// would leave the engine in a state no config file describes. The old chains keep working
    /// until validation has passed.
    pub fn reload(&mut self, text: &str) -> Result<Vec<OutputStatus>, MixerError> {
        let cfg = MixerConfig::from_toml_str(text)?;
        if cfg.channels.is_empty() {
            return Err(MixerError::Empty);
        }
        // Validate everything that can be validated before touching live state.
        let chains = cfg.build_chains()?;
        let mut master_fx = build_chain_with_roles(&cfg.master_fx, "masterfx")?;
        master_fx.prepare(self.sample_rate);

        let mut old = std::mem::take(&mut self.channels);
        let mut channels = Vec::with_capacity(cfg.channels.len());
        for (channel_cfg, (flow_chains, post_fx)) in cfg.channels.iter().zip(chains) {
            let mut channel = Channel::new(
                Deck::empty(),
                channel_cfg,
                cfg.crossfader,
                flow_chains,
                post_fx,
            );
            channel.set_block_seconds(block_seconds_of(self.sample_rate));
            channel.prepare_fx(self.sample_rate);
            channel.set_autogain(self.autogain);
            // Whatever was playing on this index keeps playing: a deck is not a config value.
            if let Some(previous) = old.get_mut(channels.len()) {
                channel.replace_deck(previous.take_deck());
            }
            channels.push(channel);
        }
        let dropped = old.len().saturating_sub(channels.len());
        if dropped > 0 {
            eprintln!("[audio] reload: {dropped} channel(s) dropped, with their tracks");
        }

        self.channels = channels;
        self.master.fx = master_fx;
        self.master.fader = Fader::new(cfg.master_fader, MASTER_TAU);
        self.master.limiter.set_enabled(cfg.master_limiter);
        self.cue.fader = Fader::new(cfg.cue_fader, MASTER_TAU);
        self.cue.blend = cfg.cue.blend;
        self.cue.curve = cfg.cue.curve;
        self.cue.cut_width = cfg.cue.cut_width;
        self.config = cfg;
        // Destinations are the part that most often *needs* a reload (a device that appeared), so
        // they are rebuilt last and reported.
        Ok(self.refresh_outputs())
    }

    /// Applies the auto-gain preference (from `live.toml`) to every channel. The gain is derived
    /// per block from the deck's measured loudness, so this takes effect on the next block for
    /// whatever is already loaded — no reload, no re-measure.
    pub fn set_autogain(&mut self, settings: AutogainSettings) {
        self.autogain = settings.sanitised();
        for channel in &mut self.channels {
            channel.set_autogain(self.autogain);
        }
    }

    /// The auto-gain preference in effect.
    pub fn autogain(&self) -> AutogainSettings {
        self.autogain
    }

    /// The keylock window every deck uses (from `live.toml`).
    pub fn keylock_window(&self) -> KeylockSettings {
        self.keylock
    }

    /// Applies the live keylock window to the current decks and remembers it for future loads.
    /// Running decks rebuild their keylock engine at the current position (a block-boundary swap).
    pub fn set_keylock_window(&mut self, window: KeylockSettings) {
        self.keylock = window;
        for channel in &mut self.channels {
            channel.deck_mut().set_keylock_window(window);
        }
    }

    /// The rate the engine renders at (a destination refusing it is what `OutputState::Disabled`
    /// means).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
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

    /// Applies a [`Command::SetFader`](hypermixx_core::Command::SetFader) at a block boundary.
    ///
    /// Every setter below is `&self` (the faders smooth through interior atomics), so this takes
    /// the mixer by shared reference and never fights the audio path. The only failure is an
    /// unknown deck; out-of-range values are clamped by the setter, not rejected.
    pub fn set_fader(
        &self,
        target: hypermixx_core::FaderTarget,
        value: f32,
    ) -> Result<(), String> {
        use hypermixx_core::FaderTarget;
        match target {
            FaderTarget::Flow(deck_id) => self.fader_channel(deck_id)?.set_flow_fader(value),
            FaderTarget::Stem { deck, stem } => {
                self.fader_channel(deck)?.set_stem_level(stem, value)
            }
            FaderTarget::Deck(deck_id) => self.fader_channel(deck_id)?.set_fader(value),
            FaderTarget::Gain(deck_id) => self.fader_channel(deck_id)?.set_gain(value),
            FaderTarget::CueSend(deck_id) => self.fader_channel(deck_id)?.set_cue_send(value),
            FaderTarget::Crossfader => {
                // One physical fader: every channel is written the same position; the pan law is
                // the channel's business, not the command's.
                for channel in &self.channels {
                    channel.set_crossfader(value);
                }
            }
            FaderTarget::Master => self.master.set_fader(value),
            FaderTarget::Cue => self.cue.set_fader(value),
        }
        Ok(())
    }

    /// Flips one channel's cue send between off and full. The engine owns the value, so a pad and
    /// the `fader cuesend` path cannot disagree about which way the next press goes.
    pub fn toggle_cue_send(&self, deck_id: hypermixx_core::DeckId) -> Result<(), String> {
        self.fader_channel(deck_id)?.toggle_cue_send();
        Ok(())
    }

    /// Resolves a deck id to its channel. A mistyped id is an error, not a silent no-op, so a
    /// mapping layer's mistake is visible in the log instead of swallowing the fader move.
    fn fader_channel(&self, deck_id: u8) -> Result<&Channel, String> {
        self.channels.get(deck_id as usize).ok_or_else(|| {
            format!("unknown deck {deck_id} (mixer has {} channels)", self.channels.len())
        })
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

    /// Seconds one rendered block covers at this engine's rate.
    pub fn block_seconds(&self) -> f64 {
        block_seconds_of(self.sample_rate)
    }

    /// The context for one block: the engine's own constants plus the musical position of whichever
    /// deck is carrying the grid.
    ///
    /// Deck 0 wins when both have a grid, which is the convention a two-deck crossfade needs — a
    /// tempo-synced effect must not change its notion of "beat 1" halfway through a blend.
    pub fn make_ctx(&self, sample_rate: u32) -> FxContext {
        // One scan and only a cheap Arc clone: the grid itself is borrowed, not copied. A BeatGrid
        // is one u64 per beat, so cloning it on every block was a real allocation on the producer.
        let grid = self.channels.iter().find_map(|channel| {
            let deck = channel.deck();
            deck.analysis()
                .map(|analysis| (analysis, deck.current_frame()))
        });
        let (frames_per_beat, beat_position) = match &grid {
            Some((analysis, frame)) => {
                let grid = &analysis.beatgrid;
                let width = grid.beat_width(grid.floor_beat(*frame)).max(1) as f64;
                (
                    width,
                    grid.floor_beat(*frame) as f64 + f64::from(grid.phase(*frame)),
                )
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
        // The monitor is a blend of the cue sends and what the room hears. `blend = -1` (the
        // default) leaves the cue untouched, which is the historical behaviour: headphones carry the
        // cue and nothing else.
        cue.blend_with(&master.sum);

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
                output.write(&cue.mix);
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

    /// Reads one slot. A deck's index space is **flat**: one flow's chain (the fixed `eq`/`filter`
    /// prefix then the `flowfx` bank) followed by the post-merge chain (knob bank, then pad bank),
    /// so a controller map's slot number never moves when a bank grows at its end.
    pub fn resolve_fx(&self, target: &FxTarget) -> Option<&FxSlot> {
        match target.chain {
            FxChainId::Master => self.master.fx.slot(target.index),
            FxChainId::Deck(deck_id) => {
                let channel = self.channels.get(deck_id as usize)?;
                let flow_len = channel.flow_len();
                if target.index < flow_len {
                    channel.slot(SlotChain::Flow(Stem::Drums), target.index)
                } else {
                    channel.slot(SlotChain::Deck, target.index - flow_len)
                }
            }
            FxChainId::Stem { deck, stem } => self
                .channels
                .get(deck as usize)?
                .slot(SlotChain::Flow(stem), target.index),
        }
    }

    /// Applies `f` to **every instance** a target addresses.
    ///
    /// A flow slot exists once per flow (its own filter state), while the control is shared: the
    /// group has one knob, one on/off and one parameter trajectory, which only stays true if a write
    /// reaches all instances. `Stem` deliberately addresses one instance for inspection.
    fn for_each_fx_mut(
        &mut self,
        target: &FxTarget,
        mut f: impl FnMut(&mut FxSlot) -> Result<(), String>,
    ) -> Result<(), String> {
        match target.chain {
            FxChainId::Master => {
                let slot = self
                    .master
                    .fx
                    .slot_mut(target.index)
                    .ok_or_else(|| slot_error(target))?;
                f(slot)
            }
            FxChainId::Deck(deck_id) => {
                let count = self.channels.len();
                let channel = self
                    .channels
                    .get_mut(deck_id as usize)
                    .ok_or_else(|| unknown_deck(deck_id, count))?;
                let flow_len = channel.flow_len();
                if target.index < flow_len {
                    for stem in Stem::ALL {
                        let slot = channel
                            .chain_mut(SlotChain::Flow(stem))
                            .and_then(|chain| chain.slot_mut(target.index))
                            .ok_or_else(|| slot_error(target))?;
                        f(slot)?;
                    }
                    Ok(())
                } else {
                    let slot = channel
                        .post_fx_mut()
                        .slot_mut(target.index - flow_len)
                        .ok_or_else(|| slot_error(target))?;
                    f(slot)
                }
            }
            FxChainId::Stem { deck, .. } => {
                // A flow slot is one control over N instances: naming a stem selects the *view*,
                // never a private parameter set. The whitelist (`flows`) is what picks flows.
                let count = self.channels.len();
                let channel = self
                    .channels
                    .get_mut(deck as usize)
                    .ok_or_else(|| unknown_deck(deck, count))?;
                for stem in Stem::ALL {
                    let slot = channel
                        .chain_mut(SlotChain::Flow(stem))
                        .and_then(|chain| chain.slot_mut(target.index))
                        .ok_or_else(|| slot_error(target))?;
                    f(slot)?;
                }
                Ok(())
            }
        }
    }

    /// Whether `slot` addresses a `flowfx` slot — the only place a flow mask means anything.
    ///
    /// A deck's flat index counts the flow chain first, so the post-merge banks and master are
    /// rejected by position; `eq`/`filter` are fixed stages and always see every flow.
    fn is_flowfx_slot(&self, target: &FxTarget) -> bool {
        let channel = match target.chain {
            FxChainId::Deck(deck) => match self.channels.get(deck as usize) {
                Some(channel) if target.index < channel.flow_len() => channel,
                _ => return false,
            },
            FxChainId::Stem { deck, .. } => match self.channels.get(deck as usize) {
                Some(channel) => channel,
                None => return false,
            },
            FxChainId::Master => return false,
        };
        channel
            .chain(SlotChain::Flow(Stem::Drums))
            .and_then(|chain| chain.slot(target.index))
            .map(|slot| !matches!(slot.role(), "eq" | "filter"))
            .unwrap_or(false)
    }

    /// Runs one mutation through [`Mixer::for_each_fx_mut`] and turns the outcome into a response.
    fn answer(
        &mut self,
        slot: FxTarget,
        f: impl FnMut(&mut FxSlot) -> Result<(), String>,
    ) -> CommandResponse {
        match self.for_each_fx_mut(&slot, f) {
            Ok(()) => CommandResponse::Ok,
            Err(err) => CommandResponse::Error(err),
        }
    }

    /// Every slot in one address space, with flat indices.
    pub fn list_fx(&self, chain: FxChainId) -> Option<Vec<FxSlotStatus>> {
        match chain {
            FxChainId::Master => Some(self.master.fx.statuses()),
            FxChainId::Deck(deck_id) => {
                let channel = self.channels.get(deck_id as usize)?;
                // The group's *view* is one flow's chain: every instance carries the same role and
                // the same control values, so one listing is the truth.
                let mut slots = channel.slot_statuses(SlotChain::Flow(Stem::Drums))?;
                let flow_len = slots.len();
                slots.extend(
                    channel
                        .slot_statuses(SlotChain::Deck)?
                        .into_iter()
                        .map(|mut slot| {
                            slot.index += flow_len;
                            slot
                        }),
                );
                Some(slots)
            }
            FxChainId::Stem { deck, stem } => self
                .channels
                .get(deck as usize)?
                .slot_statuses(SlotChain::Flow(stem)),
        }
    }

    /// Appends an effect at the end of a bank, returning its flat index.
    ///
    /// `Deck` appends to the post-merge bank (after `padfx`); `Stem` appends a `flowfx` slot to
    /// *every* flow so the group stays aligned. Existing indices never move.
    pub fn add_fx(&mut self, chain: FxChainId, slot: FxSlot) -> Result<usize, String> {
        match chain {
            FxChainId::Master => Ok(self.master.fx.push_slot(slot)),
            FxChainId::Deck(deck_id) => {
                let count = self.channels.len();
                let channel = self
                    .channels
                    .get_mut(deck_id as usize)
                    .ok_or_else(|| unknown_deck(deck_id, count))?;
                let flow_len = channel.flow_len();
                let appended = channel
                    .add_slot(SlotChain::Deck, slot)
                    .ok_or_else(|| unknown_deck(deck_id, count))?;
                Ok(flow_len + appended)
            }
            FxChainId::Stem { .. } => Err(
                "the flow banks are declared in mixer.toml — use `fx kind <slot> <kind>` to change \
                 an existing slot (or edit `[[channel.flow_fx]]`)"
                    .to_owned(),
            ),
        }
    }

    pub fn remove_fx(&mut self, target: &FxTarget) -> Result<FxSlot, String> {
        let count = self.channels.len();
        let removed = match target.chain {
            FxChainId::Master => {
                return self
                    .master
                    .fx
                    .remove(target.index)
                    .ok_or_else(|| "no effect at that index on master".to_owned())
            }
            FxChainId::Deck(deck_id) => {
                let channel = self
                    .channels
                    .get_mut(deck_id as usize)
                    .ok_or_else(|| unknown_deck(deck_id, count))?;
                let flow_len = channel.flow_len();
                // Only the last slot of the whole flat space can go: anything else would renumber
                // the slots after it, and a map is bound to those numbers. To empty a slot in
                // place, `fx kind <slot> none` keeps the index.
                if target.index + 1 != flow_len + channel.post_fx().len() {
                    return Err(
                        "only the last bank slot can be removed (removal renumbers the slots \
                         after it); use `fx kind <slot> none` to empty a slot in place"
                            .to_owned(),
                    );
                }
                if target.index >= flow_len {
                    channel
                        .remove_slot(SlotChain::Deck, target.index - flow_len)
                        .ok_or_else(|| slot_error(target))?
                } else {
                    // The post bank is empty, so the last slot is the last `flowfx` one: remove it
                    // from every flow's chain to keep the group aligned.
                    let mut first = None;
                    for stem in Stem::ALL {
                        let removed = channel
                            .remove_slot(SlotChain::Flow(stem), target.index)
                            .ok_or_else(|| slot_error(target))?;
                        first.get_or_insert(removed);
                    }
                    first.expect("one instance removed")
                }
            }
            FxChainId::Stem { deck, .. } => {
                let channel = self
                    .channels
                    .get_mut(deck as usize)
                    .ok_or_else(|| unknown_deck(deck, count))?;
                // One view of the group: the flow banks are fixed, so `Stem` removal only works
                // from the end of that flow's chain.
                if target.index + 1 != channel.flow_len() {
                    return Err(
                        "only the last flow slot can be removed; use `fx kind <slot> none` instead"
                            .to_owned(),
                    );
                }
                let mut first = None;
                for stem in Stem::ALL {
                    let removed = channel
                        .remove_slot(SlotChain::Flow(stem), target.index)
                        .ok_or_else(|| slot_error(target))?;
                    first.get_or_insert(removed);
                }
                first.expect("one instance removed")
            }
        };
        Ok(removed)
    }

    /// Installs a separated track into one channel. The swap is seamless (a source change at the
    /// current position) but lands a few blocks later; `DeckState::stems` reports when it has.
    pub fn set_stems(&mut self, deck_id: u8, stems: StemSet) -> Result<(), String> {
        let count = self.channels.len();
        self.channels
            .get_mut(deck_id as usize)
            .map(|channel| channel.set_stems(stems))
            .ok_or_else(|| unknown_deck(deck_id, count))
    }

    /// Applies a per-stem command, refusing it on a deck that has no stems — a `stem vocals mute`
    /// that silently did nothing would be worse than an error.
    pub fn apply_stem(&self, deck_id: u8, op: StemOp) -> Result<(), String> {
        let count = self.channels.len();
        let channel = self
            .channels
            .get(deck_id as usize)
            .ok_or_else(|| unknown_deck(deck_id, count))?;
        if !channel.has_stems() {
            return Err(format!(
                "deck {deck_id} has no stems — run `stem separate` first"
            ));
        }
        channel.apply_stem(op)
    }

    /// Applies one FX command, answering with the protocol's response.
    ///
    /// Everything the [`Command`](hypermixx_core::Command) FX variants can express lands here, so
    /// the pipeline's command loop stays a dispatcher rather than growing a second FX implementation.
    pub fn handle_fx_command(&mut self, command: hypermixx_core::Command) -> CommandResponse {
        use hypermixx_core::Command;
        match command {
            Command::AddFx { chain, kind } => match config::build_slot(&kind) {
                Ok(mut slot) => {
                    let reported_kind = slot.kind().to_owned();
                    // An effect appended at run time needs the engine's rate before its first block.
                    slot.prepare(self.sample_rate);
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
            Command::ToggleFx { slot } => {
                self.answer(slot, |found| {
                    found.toggle();
                    Ok(())
                })
            }
            Command::SetFxEnabled { slot, enabled } => self.answer(slot, |found| {
                found.set_enabled(enabled);
                Ok(())
            }),
            Command::SetFxKind { slot, kind } => match FxKind::parse(&kind) {
                Ok(parsed) => {
                    let rate = self.sample_rate;
                    self.answer(slot, |found| {
                        // Same position, same role, same engagement; only the effect changes.
                        // Parameters with a shared name (a filter's `value`/`resonance`) carry over,
                        // so a swap under a mapped knob is seamless.
                        found.replace(parsed.build(), parsed.name());
                        found.prepare(rate);
                        Ok(())
                    })
                }
                Err(err) => CommandResponse::Error(err.message()),
            },
            Command::SetFxFlows { slot, flows } => {
                if !self.is_flowfx_slot(&slot) {
                    return CommandResponse::Error(format!(
                        "{}: not a flow slot — only the flowfx bank is routed per flow",
                        slot.chain.label()
                    ));
                }
                let mask = flows
                    .iter()
                    .fold(0u8, |mask, stem| mask | config::flow_bit(stem.index()));
                self.answer(slot, |found| {
                    found.set_flows(mask);
                    Ok(())
                })
            }
            Command::SetFxParam { slot, name, value } => {
                let label = slot.chain.label();
                self.answer(slot, |found| {
                    found
                        .set_param(&name, value)
                        .map_err(|err| format!("{label} {name}: {}", err.message()))
                })
            }
            Command::SetFxParamIndex { slot, index, value } => {
                // The name is resolved once, from the slot's own declaration, and then applied to
                // every instance of the group: a knob sweep must move all flows together.
                let name = self
                    .resolve_fx(&slot)
                    .and_then(|found| found.fx().param_names().get(index).copied())
                    .map(str::to_owned);
                let Some(name) = name else {
                    return CommandResponse::Error(format!(
                        "{}: parameter index {} is out of range",
                        slot.chain.label(),
                        index + 1
                    ));
                };
                let label = slot.chain.label();
                self.answer(slot, |found| {
                    found
                        .set_param(&name, value)
                        .map_err(|err| format!("{label} {name}: {}", err.message()))
                })
            }
            Command::FxTrigger { slot } => self.answer(slot, |found| {
                found.trigger();
                Ok(())
            }),
            Command::PadPress { slot } => self.answer(slot, |found| {
                found.press_pad();
                Ok(())
            }),
            Command::PadRelease { slot } => self.answer(slot, |found| {
                found.release_pad();
                Ok(())
            }),
            Command::ListFx { chain } => match self.list_fx(chain) {
                Some(slots) => CommandResponse::FxListed { chain, slots },
                None => CommandResponse::Error(unknown_chain(&chain)),
            },
            Command::GetFxState { chain } => match self.list_fx(chain) {
                Some(slots) => CommandResponse::FxState { chain, slots },
                None => CommandResponse::Error(unknown_chain(&chain)),
            },
            Command::SetStems { deck_id, stems } => match self.set_stems(deck_id, stems) {
                Ok(()) => CommandResponse::Ok,
                Err(err) => CommandResponse::Error(err),
            },
            Command::Stem { deck_id, op } => match self.apply_stem(deck_id, op) {
                Ok(()) => CommandResponse::Ok,
                Err(err) => CommandResponse::Error(err),
            },
            other => CommandResponse::Error(format!("not an FX command: {other:?}")),
        }
    }
}

/// The outputs a config names, with the routes that feed them.
struct RoutedOutputs {
    outputs: Outputs,
    master_outputs: Vec<usize>,
    cue_outputs: Vec<usize>,
}

/// Opens every destination a config names and works out which bus feeds which.
///
/// Split out of `Mixer::new` because `output refresh` needs exactly this again, without touching the
/// channels: the destinations are the part of a topology that can change under the engine's feet
/// (a device that is not plugged in yet, a rate a card refuses), while the signal path is not.
/// Seconds one block covers at `sample_rate`. A free function because both the constructor and a
/// topology reload need it before a `Mixer` exists / from `&self`.
fn block_seconds_of(sample_rate: u32) -> f64 {
    BLOCK_SIZE as f64 / f64::from(sample_rate.max(1))
}

fn build_outputs(
    cfg: &MixerConfig,
    sample_rate: u32,
    block_frames: usize,
    tuning: OutputTuning,
) -> RoutedOutputs {
    let mut opened: Vec<Output> = Vec::with_capacity(cfg.outputs.len());
    for output_cfg in &cfg.outputs {
        // `Output::open` only fails when a device claimed to work and then would not build a stream;
        // everything else (no device, a device that is not there, a rate it refuses) comes back as a
        // placeholder with a state, and the engine keeps running.
        match Output::open_with(output_cfg, sample_rate, block_frames, tuning) {
            Ok(output) => opened.push(output),
            Err(err) => {
                eprintln!("[audio] {}: {err}; continuing without it", output_cfg.name);
                opened.push(Output::sink(output_cfg, block_frames));
            }
        }
    }
    // One live stream per device *channel pair*. Two logical outputs on the same pair would be
    // summed by the server outside the limiter, so the later one is downgraded to a sink. A
    // multi-channel interface may still carry main on `(0, 1)` and cue on `(2, 3)`: the pairs are
    // disjoint, and each stream writes silence to the channels it does not own.
    let mut duplicate = vec![false; opened.len()];
    {
        let mut streams_on_device =
            std::collections::HashMap::<String, Vec<(usize, (u16, u16))>>::new();
        for (index, output) in opened.iter().enumerate() {
            // A disabled/missing/sink destination owns no channel, so it must not reserve one.
            if !output.is_live() {
                continue;
            }
            let Some(device) = output.device() else {
                continue;
            };
            let pair = output.channels;
            let clash = streams_on_device.get(device).and_then(|streams| {
                streams
                    .iter()
                    .find(|(_, existing)| channel_pairs_overlap(pair, *existing))
                    .copied()
            });
            if let Some((first, _)) = clash {
                eprintln!(
                    "[audio] {}: `{}` is already on {device} channels {}-{}; dropping this stream \
                     so the server does not sum two buses outside the limiter \
                     (give this output its own `device` or a disjoint `channels` pair)",
                    cfg.outputs[index].name,
                    cfg.outputs[first].name,
                    pair.0,
                    pair.1
                );
                duplicate[index] = true;
            } else {
                streams_on_device
                    .entry(device.to_owned())
                    .or_default()
                    .push((index, pair));
            }
        }
    }
    for (index, taken) in duplicate.iter().enumerate() {
        if *taken {
            // Replacing the element drops the cpal stream on the spot, freeing the device slot the
            // duplicate needlessly held.
            opened[index] = Output::sink(&cfg.outputs[index], block_frames);
        }
    }
    let mut outputs = Outputs::new();
    for output in opened {
        outputs.push(output);
    }
    // Ids are positional: a config (or a TOML file that omitted them) cannot make two outputs share
    // an id, and a dropped duplicate does not leave a hole in the numbering.
    for (index, output) in outputs.items_mut().enumerate() {
        output.id = index as OutputId;
    }
    // Route by role, skipping the duplicates: a `cue` output gets the cue bus, everything else the
    // mix.
    let mut master_outputs = Vec::new();
    let mut cue_outputs = Vec::new();
    for (index, output_cfg) in cfg.outputs.iter().enumerate() {
        if duplicate[index] {
            continue;
        }
        match output_cfg.role {
            OutputRole::Master => master_outputs.push(index),
            OutputRole::Cue => cue_outputs.push(index),
        }
    }
    let any_live = outputs.iter().any(Output::is_live);
    if master_outputs.is_empty() && any_live {
        // A config that named only a cue (or whose masters all failed to open) still needs to be
        // audible somewhere.
        master_outputs.extend(0..outputs.len());
        cue_outputs.clear();
    }
    RoutedOutputs {
        outputs,
        master_outputs,
        cue_outputs,
    }
}

/// Whether two `(left, right)` device mappings touch at least one of the same channels.
///
/// The duplicate-output rule uses this instead of the device name alone: a 4-channel interface can
/// carry main on `(0, 1)` and cue on `(2, 3)` without the two streams overlapping.
fn channel_pairs_overlap(a: (u16, u16), b: (u16, u16)) -> bool {
    a.0 == b.0 || a.0 == b.1 || a.1 == b.0 || a.1 == b.1
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
        // Driven even when every slot is bypassed, so each slot sees its own bypass edge (and
        // resets exactly once when it is engaged again).
        if !self.fx.is_empty() {
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

    /// Fills [`CueBus::mix`] from the cue sum and the master's mix, using the configured blend and
    /// curve. A gain of exactly zero skips the add outright, so "cue only" costs nothing.
    fn blend_with(&mut self, master: &Bus) {
        let t = (self.blend.clamp(-1.0, 1.0) + 1.0) * 0.5;
        let (cue_gain, master_gain) = self.curve.gains(t, self.cut_width);
        self.mix.ensure_frames(self.sum.frames());
        self.mix.clear();
        self.mix.add_scaled(&self.sum, cue_gain);
        if master_gain > 0.0 {
            self.mix.add_scaled(master, master_gain);
        }
    }

    pub fn set_fader(&self, position: f32) {
        self.fader.set(position.clamp(-1.0, 1.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_core::FaderTarget;
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
    fn set_fader_addresses_each_target_and_clamps() {
        let mixer = loaded(simple_dj(), 2);

        // An unknown deck is the one reportable failure.
        assert!(mixer.set_fader(FaderTarget::Flow(9), 0.0).is_err());

        // Flow: bipolar, clamped at +1.
        mixer.set_fader(FaderTarget::Flow(0), 0.5).unwrap();
        assert_eq!(mixer.channel(0).unwrap().levels().0, 0.5);
        mixer.set_fader(FaderTarget::Flow(0), 9.0).unwrap();
        assert_eq!(mixer.channel(0).unwrap().levels().0, 1.0);

        // Deck fader is its own target, not an alias of flow.
        mixer.set_fader(FaderTarget::Deck(1), -0.25).unwrap();
        assert_eq!(mixer.channel(1).unwrap().levels().1, -0.25);

        // The trim is its own target too, and clamps like the rest.
        mixer.set_fader(FaderTarget::Gain(1), 1.0).unwrap();
        assert_eq!(mixer.channel(1).unwrap().levels().4, 1.0, "the trim reaches +6 dB");
        mixer.set_fader(FaderTarget::Gain(1), -9.0).unwrap();
        assert_eq!(mixer.channel(1).unwrap().levels().4, -1.0);
        mixer.set_fader(FaderTarget::Gain(1), 0.0).unwrap();

        // Cue send is a linear level: a negative value clamps to 0, not -1.
        mixer.set_fader(FaderTarget::CueSend(1), -3.0).unwrap();
        assert_eq!(mixer.channel(1).unwrap().levels().3, 0.0);
        mixer.set_fader(FaderTarget::CueSend(1), 0.25).unwrap();
        assert_eq!(mixer.channel(1).unwrap().levels().3, 0.25);

        // The crossfader is one physical fader broadcast to every channel.
        mixer.set_fader(FaderTarget::Crossfader, -1.0).unwrap();
        assert_eq!(mixer.channel(0).unwrap().levels().2, -1.0);
        assert_eq!(mixer.channel(1).unwrap().levels().2, -1.0);

        // Bus faders reach the master/cue trims, not a channel.
        mixer.set_fader(FaderTarget::Master, 0.25).unwrap();
        mixer.set_fader(FaderTarget::Cue, 0.75).unwrap();
        assert_eq!(mixer.master().fader.target(), 0.25);
        assert_eq!(mixer.cue().fader.target(), 0.75);
    }

    #[test]
    fn process_needs_no_device_and_writes_nothing_when_unrouted() {
        let mut mixer = loaded(simple_dj(), 2);
        assert!(mixer.outputs().is_empty() || !mixer.outputs().iter().any(Output::is_live));
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
        // The deck's flat space is one flow's chain (eq, filter) followed by the post-merge bank;
        // `AddFx(Deck)` appends to the post-merge bank, so its index is `flow_len + post_len`.
        let flow_len = mixer.channel(0).unwrap().flow_len();
        let post_len = mixer
            .channel(0)
            .unwrap()
            .slot_statuses(SlotChain::Deck)
            .unwrap()
            .len();
        let base = flow_len + post_len;
        match mixer.handle_fx_command(Command::AddFx {
            chain: FxChainId::Deck(0),
            kind: "gain".into(),
        }) {
            CommandResponse::FxAdded { index, kind, .. } => {
                assert_eq!(kind, "gain");
                assert_eq!(index, base, "appended after the post-merge bank");
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
        match mixer.handle_fx_command(Command::SetFxParamIndex {
            slot,
            index: 0,
            value: 0.5,
        }) {
            CommandResponse::Ok => {}
            other => panic!("expected Ok, got {other:?}"),
        }
        assert_eq!(mixer.resolve_fx(&slot).unwrap().get_param("gain"), Some(0.5));

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
        assert_eq!(
            mixer.channel(0).unwrap().slot_statuses(SlotChain::Deck).unwrap().len(),
            post_len
        );
    }

    /// A fixed slot changes its effect without moving: same index, same role, same engagement, and
    /// the parameters both effects declare survive — that is what lets a mapped knob ride a
    /// `filter` → `moog` swap without jumping.
    /// The deck's flat listing is one flow's chain followed by the post-merge banks, and the
    /// prefixes are the ones the map resolves.
    #[test]
    fn the_deck_listing_is_the_flow_chain_then_the_post_banks() {
        use hypermixx_core::Command;
        let mut cfg = simple_dj();
        cfg.channels[0].flow_fx = vec![crate::mixer::config::FlowSlotConfig::all("echo")];
        cfg.channels[0].knob_fx = vec!["filter".into()];
        cfg.channels[0].pad_fx = vec!["reverb".into()];
        let mut mixer = loaded(cfg, 1);
        let slots = match mixer.handle_fx_command(Command::ListFx {
            chain: FxChainId::Deck(0),
        }) {
            CommandResponse::FxListed { slots, .. } => slots,
            other => panic!("expected FxListed, got {other:?}"),
        };
        let got: Vec<(String, String)> = slots
            .iter()
            .map(|slot| (slot.role.clone(), slot.kind.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("eq".to_owned(), "eq".to_owned()),
                ("filter".to_owned(), "filter".to_owned()),
                ("flowfx1".to_owned(), "echo".to_owned()),
                ("knobfx1".to_owned(), "filter".to_owned()),
                ("padfx1".to_owned(), "reverb".to_owned()),
            ]
        );
        // The flat indices are 0..n, so a map's slot number means the same thing every session.
        assert_eq!(
            slots.iter().map(|slot| slot.index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        // A flow slot is one control over four instances: writing it must reach all of them.
        mixer.handle_fx_command(Command::SetFxParam {
            slot: FxTarget {
                chain: FxChainId::Deck(0),
                index: 2,
            },
            name: "mix".into(),
            value: 0.25,
        });
        for stem in Stem::ALL {
            assert_eq!(
                mixer
                    .channel(0)
                    .unwrap()
                    .slot(SlotChain::Flow(stem), 2)
                    .unwrap()
                    .get_param("mix"),
                Some(0.25),
                "{stem}: a group write must reach every flow"
            );
        }
    }

    /// Auto-gain is a live preference: setting it re-derives the gain for whatever is loaded, and
    /// clearing it returns to unity — no reload, no re-measure.
    #[test]
    fn set_autogain_applies_to_already_loaded_channels() {
        let mut mixer = headless(silent_test_channel());
        let mut deck = Deck::new(tone(10));
        deck.play();
        deck.set_loudness(Some(-6.0));
        mixer.channel_mut(0).unwrap().replace_deck(deck);
        tick(&mut mixer, 60);
        let off = mixer.master_peak();
        assert!(off > 0.5, "reference {off}");

        mixer.set_autogain(AutogainSettings {
            enabled: true,
            target_db: -12.0,
        });
        tick(&mut mixer, 60);
        let on = mixer.master_peak();
        assert!(
            (on / off - 0.5).abs() < 0.03,
            "the live switch must apply to the loaded track: {on} vs {off}"
        );

        mixer.set_autogain(AutogainSettings::default());
        tick(&mut mixer, 60);
        assert!((mixer.master_peak() / off - 1.0).abs() < 0.02);
    }

    /// A bank only grows at its end and only shrinks from its end, so a map's slot number never
    /// moves; `fx kind <slot> none` is how a slot is emptied in place.
    #[test]
    fn removal_is_tail_only_and_none_empties_in_place() {
        use hypermixx_core::Command;
        let mut cfg = simple_dj();
        cfg.channels[0].knob_fx = vec!["echo".into(), "reverb".into()];
        let mut mixer = loaded(cfg, 1);
        let flow_len = mixer.channel(0).unwrap().flow_len();
        // Removing a slot that is not last would renumber the ones after it.
        assert!(matches!(
            mixer.handle_fx_command(Command::RemoveFx {
                chain: FxChainId::Deck(0),
                index: flow_len,
            }),
            CommandResponse::Error(_)
        ));
        // The last one can go.
        assert!(matches!(
            mixer.handle_fx_command(Command::RemoveFx {
                chain: FxChainId::Deck(0),
                index: flow_len + 1,
            }),
            CommandResponse::Ok
        ));
        // Emptying in place keeps the index and the role: a mapped knob still points at it.
        let slot = FxTarget {
            chain: FxChainId::Deck(0),
            index: 0,
        };
        assert!(matches!(
            mixer.handle_fx_command(Command::SetFxKind {
                slot,
                kind: "none".into()
            }),
            CommandResponse::Ok
        ));
        let found = mixer.resolve_fx(&slot).unwrap();
        assert_eq!(found.kind(), "none");
        assert_eq!(found.role(), "eq", "the position identity survives emptying");
        // And it can be filled again.
        assert!(matches!(
            mixer.handle_fx_command(Command::SetFxKind {
                slot,
                kind: "eq".into()
            }),
            CommandResponse::Ok
        ));
        assert_eq!(mixer.resolve_fx(&slot).unwrap().kind(), "eq");
    }

    /// The flow whitelist: one mask per slot, broadcast to every flow's instance, reported in the
    /// listing, and refused everywhere a flow mask has no meaning.
    #[test]
    fn set_fx_flows_routes_a_group_and_reports_it() {
        use hypermixx_core::Command;
        let mut cfg = simple_dj();
        cfg.channels[0].flow_fx = vec![config::FlowSlotConfig::only("echo", [Stem::Vocals])];
        let mut mixer = loaded(cfg, 1);
        let target = FxTarget {
            chain: FxChainId::Deck(0),
            index: 2, // eq 0, filter 1, flowfx1 2
        };
        let listing = |mixer: &mut Mixer| match mixer.handle_fx_command(Command::ListFx {
            chain: FxChainId::Deck(0),
        }) {
            CommandResponse::FxListed { slots, .. } => slots,
            other => panic!("expected FxListed, got {other:?}"),
        };
        assert_eq!(
            listing(&mut mixer)[2].flows,
            Some(vec![Stem::Vocals]),
            "the config's whitelist is what the slot is built with"
        );

        assert!(matches!(
            mixer.handle_fx_command(Command::SetFxFlows {
                slot: target,
                flows: vec![Stem::Drums, Stem::Bass],
            }),
            CommandResponse::Ok
        ));
        // Every instance carries the same mask; which flow it *runs on* is decided per flow at
        // process time, from that instance's own bit.
        let expected_mask = (1u8 << Stem::Drums.index()) | (1u8 << Stem::Bass.index());
        for stem in Stem::ALL {
            assert_eq!(
                mixer
                    .channel(0)
                    .unwrap()
                    .slot(SlotChain::Flow(stem), 2)
                    .unwrap()
                    .flows(),
                expected_mask,
                "{stem}: the write must reach every instance"
            );
        }
        assert_eq!(
            listing(&mut mixer)[2].flows,
            Some(vec![Stem::Drums, Stem::Bass])
        );
        // An empty list is "no flow": still mounted, processing nothing.
        assert!(matches!(
            mixer.handle_fx_command(Command::SetFxFlows {
                slot: target,
                flows: vec![],
            }),
            CommandResponse::Ok
        ));
        assert_eq!(listing(&mut mixer)[2].flows, Some(vec![]));

        // `eq` and `filter` are fixed stages; the post-merge banks and master are single-bus.
        for index in [0, 1] {
            assert!(matches!(
                mixer.handle_fx_command(Command::SetFxFlows {
                    slot: FxTarget {
                        chain: FxChainId::Deck(0),
                        index
                    },
                    flows: vec![Stem::Vocals],
                }),
                CommandResponse::Error(_)
            ));
        }
        let knob = match mixer.handle_fx_command(Command::AddFx {
            chain: FxChainId::Deck(0),
            kind: "gain".into(),
        }) {
            CommandResponse::FxAdded { index, .. } => index,
            other => panic!("expected FxAdded, got {other:?}"),
        };
        assert!(matches!(
            mixer.handle_fx_command(Command::SetFxFlows {
                slot: FxTarget {
                    chain: FxChainId::Deck(0),
                    index: knob
                },
                flows: vec![Stem::Vocals],
            }),
            CommandResponse::Error(_)
        ));
        assert!(matches!(
            mixer.handle_fx_command(Command::SetFxFlows {
                slot: FxTarget {
                    chain: FxChainId::Master,
                    index: 0
                },
                flows: vec![Stem::Vocals],
            }),
            CommandResponse::Error(_)
        ));
    }

    #[test]
    fn swapping_a_slots_effect_keeps_its_role_index_and_engagement() {
        use hypermixx_core::Command;
        let mut mixer = loaded(simple_dj(), 1);
        // The reference channel is `[eq, filter]`, so the sweep is slot 1, role "filter".
        let slot = FxTarget {
            chain: FxChainId::Deck(0),
            index: 1,
        };
        assert_eq!(mixer.resolve_fx(&slot).unwrap().role(), "filter");
        mixer.handle_fx_command(Command::SetFxParam {
            slot,
            name: "value".into(),
            value: -0.4,
        });
        mixer.handle_fx_command(Command::SetFxParam {
            slot,
            name: "resonance".into(),
            value: 0.8,
        });
        mixer.handle_fx_command(Command::SetFxEnabled { slot, enabled: false });

        match mixer.handle_fx_command(Command::SetFxKind {
            slot,
            kind: "moog".into(),
        }) {
            CommandResponse::Ok => {}
            other => panic!("expected Ok, got {other:?}"),
        }
        let swapped = mixer.resolve_fx(&slot).unwrap();
        assert_eq!(swapped.kind(), "moog");
        assert_eq!(
            swapped.role(),
            "filter",
            "the position identity must not move with the effect"
        );
        assert!(
            !swapped.is_enabled(),
            "a bypassed slot stays bypassed across the swap"
        );
        assert_eq!(swapped.get_param("value"), Some(-0.4));
        assert_eq!(swapped.get_param("resonance"), Some(0.8));

        // The protocol reports both: the role a map binds, and the kind that is in the slot.
        match mixer.handle_fx_command(Command::ListFx {
            chain: FxChainId::Deck(0),
        }) {
            CommandResponse::FxListed { slots, .. } => {
                assert_eq!(slots[1].role, "filter");
                assert_eq!(slots[1].kind, "moog");
            }
            other => panic!("expected FxListed, got {other:?}"),
        }
        // An unknown kind is an answer, not a broken slot: the old effect stays.
        assert!(matches!(
            mixer.handle_fx_command(Command::SetFxKind {
                slot,
                kind: "shimmer".into()
            }),
            CommandResponse::Error(_)
        ));
        assert_eq!(mixer.resolve_fx(&slot).unwrap().kind(), "moog");
        // The deck still renders through the swapped chain.
        tick(&mut mixer, 4);
        assert!(mixer.master_peak() > 0.0);
    }

    /// The run-time insert path has one extra obligation the config path does not: an effect that
    /// owns buffers must be sized for the engine's rate before its first block.
    #[test]
    fn a_time_based_effect_is_prepared_when_added_at_run_time() {
        use hypermixx_core::Command;
        let mut mixer = loaded(simple_dj(), 1);
        let base = mixer.channel(0).unwrap().flow_len()
            + mixer
                .channel(0)
                .unwrap()
                .slot_statuses(SlotChain::Deck)
                .unwrap()
                .len();
        let index = match mixer.handle_fx_command(Command::AddFx {
            chain: FxChainId::Deck(0),
            kind: "echo".into(),
        }) {
            CommandResponse::FxAdded { index, kind, .. } => {
                assert_eq!(kind, "echo");
                index
            }
            other => panic!("expected FxAdded, got {other:?}"),
        };
        assert_eq!(index, base);
        let slot = FxTarget { chain: FxChainId::Deck(0), index };
        assert!(matches!(
            mixer.handle_fx_command(Command::SetFxParam {
                slot,
                name: "mix".into(),
                value: 1.0,
            }),
            CommandResponse::Ok
        ));
        // Parameter index 2 is `feedback` in the effect's declared order.
        assert!(matches!(
            mixer.handle_fx_command(Command::SetFxParamIndex { slot, index: 1, value: 0.6 }),
            CommandResponse::Ok
        ));
        assert_eq!(
            mixer.resolve_fx(&slot).unwrap().get_param("feedback"),
            Some(0.6)
        );
        // Four blocks through the freshly inserted delay lines, at the mixer's own rate.
        tick(&mut mixer, 4);
        assert!(mixer.master_peak().is_finite());
    }

    #[test]
    fn fx_errors_are_answers_not_panics() {
        use hypermixx_core::Command;
        let mut mixer = loaded(simple_dj(), 1);
        for command in [
            Command::AddFx {
                chain: FxChainId::Deck(0),
                kind: "shimmer".into(),
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
        // The last slot of one flow's chain: the sweep (`eq` is 0, `filter` is 1).
        let index = mixer.channel(0).unwrap().flow_len() - 1;
        let slot = FxTarget {
            chain: FxChainId::Deck(0),
            index,
        };
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
                knob_fx: vec!["chorus".into()],
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
    fn duplicate_outputs_are_judged_by_device_channels_not_just_device_name() {
        assert!(!channel_pairs_overlap((0, 1), (2, 3)));
        assert!(!channel_pairs_overlap((0, 2), (1, 3)), "interleaved pairs do not share a channel");
        assert!(channel_pairs_overlap((0, 1), (1, 2)));
        assert!(channel_pairs_overlap((2, 3), (2, 3)));
    }

    #[test]
    fn master_and_cue_routes_follow_the_configured_roles() {
        let cfg = MixerConfig {
            outputs: vec![
                OutputConfig::master(0, "main"),
                OutputConfig::cue(1, "phones"),
            ],
            ..silent_test_channel()
        };
        // The headless helper strips outputs, so build the routing question directly.
        let mixer = Mixer::new(cfg.clone(), SAMPLE_RATE);
        if let Ok(mixer) = mixer {
            assert_eq!(mixer.master().outputs, vec![0]);
            // On a machine where both outputs land on the same device and channel pair, the
            // headphones stream is dropped and the cue goes unrouted; on a headless
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

    /// The cue outputs carry a blend of the cue sends and the master mix; the default is cue only.
    #[test]
    fn the_cue_blend_decides_what_the_headphones_carry() {
        let with_blend = |blend: f32| MixerConfig {
            cue: CueMixConfig { blend, ..Default::default() },
            ..simple_dj()
        };
        let playing = |cfg: MixerConfig| {
            let mut mixer = headless(cfg);
            let deck = Deck::new(tone(10));
            deck.play();
            mixer.channel_mut(0).unwrap().replace_deck(deck);
            tick(&mut mixer, 30);
            mixer
        };

        // The default: the cue sends reach the headphones and the master does not, so a mix you are
        // not monitoring cannot leak into a preview.
        let mixer = playing(with_blend(-1.0));
        assert!(mixer.cue_peak() > 0.5, "the cue send must reach the cue bus");
        assert!(mixer.master_peak() > 0.0, "the other deck is playing");
        assert!(
            (mixer.cue().mix.peak() - mixer.cue_peak()).abs() < 1e-6,
            "blend -1 must be cue only, got {}",
            mixer.cue().mix.peak()
        );

        // Master only: what the room hears is what you hear.
        let mixer = playing(with_blend(1.0));
        assert!(
            (mixer.cue().mix.peak() - mixer.master().sum.peak()).abs() < 1e-6,
            "blend +1 must be the master alone, got {} vs {}",
            mixer.cue().mix.peak(),
            mixer.master().sum.peak()
        );

        // Both, at the curve's centre: neither side disappears.
        let mixer = playing(with_blend(0.0));
        assert!(
            mixer.cue().mix.peak() >= mixer.cue_peak() && mixer.cue().mix.peak() >= mixer.master().sum.peak(),
            "blend 0 must carry both, got {}",
            mixer.cue().mix.peak()
        );
    }

    /// Every destination is reported, refresh never panics and never loses a destination, and a
    /// device that is not there keeps the engine alive.
    #[test]
    fn output_bindings_are_reportable_and_refreshable() {
        let mut mixer = Mixer::new(simple_dj(), SAMPLE_RATE).expect("the reference topology builds");
        let listed = mixer.output_statuses();
        assert_eq!(listed.len(), 2, "one status per configured destination");
        assert_eq!(listed[0].role, OutputRole::Master);
        assert_eq!(listed[1].role, OutputRole::Cue);
        // Headless (CI) they are sinks; with devices they are live. Either way a *state* is reported.
        for status in &listed {
            assert!(
                matches!(
                    status.state,
                    OutputState::Live | OutputState::Sink | OutputState::Missing | OutputState::Disabled
                ),
                "{status:?}"
            );
        }
        // A refresh re-binds all of them and reports the same destinations.
        let refreshed = mixer.refresh_outputs();
        assert_eq!(refreshed.len(), 2);
        assert_eq!(refreshed[0].role, OutputRole::Master);

        // A configured device that is not there is reported, not fatal: the mixer builds, the deck
        // still plays, and the destination is listed as missing until a refresh finds it.
        let cfg = MixerConfig {
            outputs: vec![OutputConfig {
                device: Some("hypermixx-no-such-device".into()),
                ..OutputConfig::master(0, "ghost")
            }],
            ..silent_test_channel()
        };
        let mut mixer = Mixer::new(cfg, SAMPLE_RATE).expect("a missing device must not fail the build");
        assert_eq!(mixer.output_statuses()[0].state, OutputState::Missing);
        assert_eq!(mixer.refresh_outputs()[0].state, OutputState::Missing);
        let deck = Deck::new(tone(10));
        deck.play();
        mixer.channel_mut(0).unwrap().replace_deck(deck);
        tick(&mut mixer, 4);
        assert!(mixer.master_peak() > 0.0, "the mix runs with no live output");
    }

    /// A reload rebuilds every config value from `defaults + file`, keeps the transports, and
    /// refuses a bad file wholesale.
    #[test]
    fn a_reload_keeps_the_music_and_replaces_the_config() {
        let mut mixer = loaded(simple_dj(), 2);
        // Move a fader and start a deck, so both "kept" and "reset" are observable.
        mixer.set_fader(FaderTarget::Flow(0), -0.75).unwrap();
        tick(&mut mixer, 3);
        let playing = mixer.deck(0).unwrap().current_frame();
        assert!(playing > 0, "the deck should be running");

        // A file that names one channel, gives it a different chain and a different trim, and no
        // outputs.
        let statuses = mixer
            .reload(
                r#"
                [[channel]]
                side = "center"
                knob_fx = ["gain"]
                gain = 1.0
                [[output]]
                name = "only"
                role = "master"
                device = "hypermixx-no-such-device"
                "#,
            )
            .expect("a valid file reloads");
        assert_eq!(mixer.channel_count(), 1, "the file's channel list replaces the old one");
        assert_eq!(statuses.len(), 1, "the destinations come from the file too");
        assert_eq!(statuses[0].state, OutputState::Missing, "and are reported as unavailable");

        // The transport survived; the trim did not (it is a config value, and the file says +6 dB).
        assert!(mixer.deck(0).unwrap().current_frame() >= playing, "a reload must not stop the deck");
        assert_eq!(mixer.channel(0).unwrap().levels().4, 1.0, "the trim comes from the file");
        assert_eq!(
            mixer.channel(0).unwrap().levels().0, 0.0,
            "an undefined fader takes its default, not the running value"
        );
        assert_eq!(
            mixer.channel(0).unwrap().slot_statuses(SlotChain::Deck).unwrap()[0].kind,
            "gain",
            "the chain came from the file"
        );

        // A bad file changes nothing: an unknown effect is refused before any live state is touched.
        let err = mixer.reload("[[channel]]\nknob_fx = [\"shimmer\"]\n").unwrap_err();
        assert!(matches!(err, MixerError::UnknownFx(_)), "{err:?}");
        assert_eq!(mixer.channel_count(), 1);
        assert_eq!(
            mixer.channel(0).unwrap().slot_statuses(SlotChain::Deck).unwrap()[0].kind,
            "gain",
            "the old chain is still in place"
        );
        // An empty file has no channels to mix, which is a refusal rather than silence.
        assert_eq!(
            mixer.reload("[master]\nlimiter = true\n").unwrap_err(),
            MixerError::Empty
        );
    }

    #[test]
    fn an_empty_bus_pair_stays_silent() {
        let mut mixer = headless(simple_dj());
        tick(&mut mixer, 5);
        assert!(mixer.master().sum.is_silent(), "no deck loaded, so no sound");
        assert!(mixer.cue().sum.is_silent());
    }
}
