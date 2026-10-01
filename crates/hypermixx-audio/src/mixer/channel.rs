//! [`Channel`]: one deck plus the fixed signal path that turns its audio into a mix contribution.
//!
//! The order is hard-coded — it *is* the design argument of this module:
//!
//! ```text
//! deck.pull_into(bus)
//!   → flow_fx        per-stream inserts (the sweep that rides one flow)
//!   → [cue tap]      PostFlowFx
//!   → flow_fader     per-stream level; equals the deck level while there is only one stream
//!   → [cue tap]      PostFlowFader
//!   → deck_fx        per-deck inserts (the tone controls a performance is built on)
//!   → [cue tap]      PostDeckFx
//!   → deck_fader     1.0 until a track exposes stems
//!   → [cue tap]      PostDeckFader
//!   → crossfader     per-side pan law
//!   → master
//! ```
//!
//! Two chains rather than one: a flow is a bounded playback (a cue point, an acapella, a future
//! stem) whose filter memory must reset when it is replaced, while a deck's tone controls persist
//! across jumps. Splitting at the fader that separates them keeps the two lifetimes from leaking
//! into each other.
//!
//! There is no routing graph. A channel owns its buses, its chains and its faders; the mixer owns
//! the channels.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use hypermixx_core::{FxSlotStatus, Stem, StemOp, StemStatus};

use super::bus::Bus;
use super::config::ChannelConfig;
use crate::deck::Deck;
use crate::fx::sample::Fader;
use crate::fx::{FxChain, FxSlot, Param};
use crate::BLOCK_SIZE;

/// Frames a channel's scratch bus holds. `Channel::process` re-sizes it to the block the mixer
/// actually asked for, so this is only the starting capacity.

/// Where a channel copies its signal into the cue bus.
///
/// The four points of the chain, in order. [`CueTap::PostDeckFader`] is the usual DJ choice ("what
/// I monitor is what would go out"); earlier taps are what you want when cueing while the channel
/// fader is down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CueTap {
    /// After the flow chain, before the flow fader: the raw voice, ignoring all level.
    PostFlowFx,
    /// After the flow fader, before the deck chain: level applies, tone does not.
    PostFlowFader,
    /// After the deck chain, before the deck fader: the full tone of a muted channel.
    PostDeckFx,
    /// After the deck fader, before the crossfader: exactly what the channel sends to master.
    #[default]
    PostDeckFader,
}

/// Which half of the crossfader a channel is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeckSide {
    /// Fades out as the crossfader moves right.
    #[default]
    Left,
    /// Fades out as the crossfader moves left.
    Right,
    /// Never attenuated — a channel outside the crossfade (a sampler, a future stem deck).
    Center,
}

/// How a crossfader position maps to per-side gain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossfaderCurve {
    /// Straight taper: `left = 1-t`, `right = t`. Sum of amplitudes stays at 1.
    Linear,
    /// `cos`/`sin` quarter-circles: equal perceived loudness through the travel, and unity power.
    #[default]
    EqualPower,
}

impl DeckSide {
    /// This side's gain for a crossfader position of `-1.0` (hard left) .. `1.0` (hard right).
    pub fn gain_at(self, position: f32, curve: CrossfaderCurve) -> f32 {
        if self == DeckSide::Center {
            return 1.0;
        }
        let t = (position.clamp(-1.0, 1.0) + 1.0) * 0.5;
        let (left, right) = match curve {
            CrossfaderCurve::Linear => (1.0 - t, t),
            CrossfaderCurve::EqualPower => {
                let angle = t * std::f32::consts::FRAC_PI_2;
                (angle.cos(), angle.sin())
            }
        };
        if self == DeckSide::Left {
            left
        } else {
            right
        }
    }
}

/// One mixer input: a deck of 1..=4 streams, an insert chain per stream plus a shared deck chain,
/// a fader per stream plus the channel faders, and a cue tap.
pub struct Channel {
    deck: Deck,
    /// One insert chain per **stream**: index 0 is the plain track, 0..4 are the stems. Built from
    /// [`Channel::flow_fx_names`] (the config's template) and grown when the deck starts rendering
    /// more streams — an [`FxChain`] is not `Clone` (its slots own `Box<dyn Fx>`), so growth rebuilds
    /// from names rather than copying a chain.
    flow_fx: Vec<FxChain>,
    /// Per-stream name lists, index = `Stem::index()`. Only a fallback: the mixer builds all
    /// `Stem::COUNT` chains up front, so growth never happens in practice. It is kept so a
    /// hand-built `Channel` (a test) that passes one chain still behaves.
    flow_fx_names: [Vec<String>; Stem::COUNT],
    /// The channel's shared inserts — the tone controls that survive a jump, and the only chain a
    /// track without stems ever has.
    deck_fx: FxChain,
    /// The channel's own signal, rendered in place. Allocated once by [`Channel::new`].
    bus: Bus,
    /// One scratch bus per stream, summed into [`Channel::bus`] through that stream's inserts and
    /// fader. This is what lets a stem have its own level and its own effects.
    stream_buses: Vec<Bus>,
    /// The cue copy taken at [`Channel::cue_tap`], filled during [`Channel::process`].
    cue: Bus,
    /// One level fader per stream, *derived* from the per-stem intent below (see
    /// [`Channel::refresh_stem_faders`]). With one stream this is the channel's flow fader.
    flow_fader: Vec<Fader>,
    /// The DJ's per-stem intent. Three independent facts — level, mute, solo — whose combination is
    /// computed in exactly one place, so they cannot disagree (a solo has to override a mute, and an
    /// unmute has to restore the level rather than unity).
    stem_level: [AtomicU32; Stem::COUNT],
    stem_mute: [AtomicBool; Stem::COUNT],
    /// The solo set as a bitmask of `1 << Stem::index()`; `0` = nothing soloed.
    stem_solo: AtomicU8,
    deck_fader: Fader,
    crossfader: Fader,
    /// Cue send level: **linear 0.0 (off) ..= 1.0 (full)**, not a bipolar fader. A send knob reads
    /// as a proportion of the channel, and running 1.0 through the bipolar law would make "full"
    /// mean +16 dB — which is exactly how a cue bus ends up 6× hot and a limiter fights the monitor
    /// path instead of the mix.
    cue_send: Param,
    side: DeckSide,
    curve: CrossfaderCurve,
    cue_tap: CueTap,
}

impl Channel {
    /// Builds a channel over `deck`. The chains come from the config layer, which is what decides
    /// that a `simple_dj()` channel has an EQ and a filter.
    /// `flow_fx` is what the mixer built: one chain per stem, from the config's template and its
    /// per-stem overrides. A caller may pass fewer (a test passing one); the rest are then built
    /// from `cfg`'s names on first use.
    pub fn new(
        deck: Deck,
        cfg: &ChannelConfig,
        flow_fx: Vec<FxChain>,
        deck_fx: FxChain,
    ) -> Self {
        let frames = BLOCK_SIZE;
        let level = clamp_pos(cfg.flow_fader);
        if flow_fx.is_empty() {
            // Never happens from the mixer, but a `Channel` with no chain at all would index out of
            // bounds in `process`; an empty chain is the honest stand-in.
            debug_assert!(false, "a channel needs at least one stream chain");
        }
        let mut channel = Self {
            deck,
            flow_fx,
            flow_fx_names: std::array::from_fn(|i| {
                cfg.chain_names(Stem::from_index(i).expect("index < Stem::COUNT in range"))
                    .to_vec()
            }),
            deck_fx,
            bus: Bus::stereo(frames),
            stream_buses: vec![Bus::stereo(frames)],
            cue: Bus::stereo(frames),
            flow_fader: vec![Fader::new(level, FADER_TAU)],
            stem_level: std::array::from_fn(|_| AtomicU32::new(level.to_bits())),
            stem_mute: std::array::from_fn(|_| AtomicBool::new(false)),
            stem_solo: AtomicU8::new(0),
            deck_fader: Fader::new(cfg.deck_fader, FADER_TAU),
            crossfader: Fader::new(cfg.crossfader, CROSSFADE_TAU),
            cue_send: Param::new(cfg.cue_send.clamp(0.0, 1.0), FADER_TAU),
            side: cfg.side,
            curve: cfg.crossfader_curve,
            cue_tap: cfg.cue_tap,
        };
        // A channel can carry up to `Stem::COUNT` streams, and an `fx` command may address any of
        // them before the deck has grown — a stem chain is *configured*, not created, by being
        // named. The spare chains cost nothing while idle because `process` only drives the live
        // ones.
        channel.sync_streams(Stem::COUNT);
        channel
    }

    pub fn deck(&self) -> &Deck {
        &self.deck
    }

    pub fn deck_mut(&mut self) -> &mut Deck {
        &mut self.deck
    }

    /// Installs a fresh transport (`Command::Load`). The chains and faders deliberately survive: a
    /// loaded track should not reset the mixer's tone controls.
    ///
    /// The per-stem intent is *not* cleared here either, and does not need to be: a reload drops
    /// back to one stream, so no stem is audible until [`Channel::set_stems`] runs — and that resets
    /// the intent, which is the moment a stale mute could have mattered.
    pub fn replace_deck(&mut self, deck: Deck) {
        self.deck = deck;
    }

    /// Installs a separated track into this channel's deck.
    ///
    /// The swap is a source change at the current position, so it is seamless and whether the audio
    /// has stems yet is the *deck's* business ([`Channel::stem_status`] reports it). The intent is
    /// reset to "all four, unity, nothing soloed": installing a fresh stem set is a fresh start.
    pub fn set_stems(&mut self, stems: hypermixx_core::StemSet) {
        self.deck.set_sources(stems.stems.to_vec());
        self.reset_stem_intent();
    }

    pub fn side(&self) -> DeckSide {
        self.side
    }

    pub fn cue_tap(&self) -> CueTap {
        self.cue_tap
    }

    pub fn set_cue_tap(&mut self, tap: CueTap) {
        self.cue_tap = tap;
    }

    /// The first stream's insert chain — the only one a track without stems has.
    pub fn flow_fx(&self) -> &FxChain {
        &self.flow_fx[0]
    }

    /// One stream's insert chain, by stem.
    pub fn stem_fx(&self, stem: Stem) -> Option<&FxChain> {
        self.flow_fx.get(stem.index())
    }

    pub fn deck_fx(&self) -> &FxChain {
        &self.deck_fx
    }

    pub fn flow_fx_mut(&mut self) -> &mut FxChain {
        &mut self.flow_fx[0]
    }

    pub fn deck_fx_mut(&mut self) -> &mut FxChain {
        &mut self.deck_fx
    }

    pub fn set_crossfader_curve(&mut self, curve: CrossfaderCurve) {
        self.curve = curve;
    }

    /// Fader positions (`-1.0 ..= 1.0`), for a UI mirror.
    /// `(flow, deck, crossfader)` as bipolar positions, `(cue send)` as a linear level.
    ///
    /// The flow fader is stream 0's; per-stem levels live in [`Channel::stem_status`].
    pub fn levels(&self) -> (f32, f32, f32, f32) {
        (
            self.flow_fader[0].target(),
            self.deck_fader.target(),
            self.crossfader.target(),
            self.cue_send.target(),
        )
    }

    /// The per-stem state: whether the audio is separated yet, plus level, mute and solo.
    pub fn stem_status(&self) -> StemStatus {
        let mut status = StemStatus {
            // The audio has stems exactly when the active flow is rendering more than one stream.
            // `SetStems` is warmed on another thread, so this is the honest answer rather than the
            // intent.
            ready: self.deck.stream_count() > 1,
            solo: self.stem_solo.load(Ordering::Relaxed),
            ..Default::default()
        };
        for (i, level) in status.level.iter_mut().enumerate() {
            *level = f32::from_bits(self.stem_level[i].load(Ordering::Relaxed));
            status.mute[i] = self.stem_mute[i].load(Ordering::Relaxed);
        }
        status
    }

    /// Whether this channel's deck is currently rendering stems.
    pub fn has_stems(&self) -> bool {
        self.deck.stream_count() > 1
    }

    /// The channel's per-stream level. With stems this writes **every** stem at once: one hardware
    /// fader still has to work on a stem deck, and "the channel's level" is the least surprising
    /// thing for it to mean. A single stem is addressed through
    /// [`set_stem_level`](Self::set_stem_level).
    pub fn set_flow_fader(&self, position: f32) {
        let level = clamp_pos(position);
        for slot in &self.stem_level {
            slot.store(level.to_bits(), Ordering::Relaxed);
        }
        self.refresh_stem_faders();
    }

    /// One stem's level (bipolar fader position).
    pub fn set_stem_level(&self, stem: Stem, position: f32) {
        self.stem_level[stem.index()].store(clamp_pos(position).to_bits(), Ordering::Relaxed);
        self.refresh_stem_faders();
    }

    /// Applies one per-stem command.
    ///
    /// `&self`: the intent lives in atomics and the faders smooth through interior mutability,
    /// exactly like every other mixer setter, so a controller thread never fights the audio path.
    pub fn apply_stem(&self, op: StemOp) -> Result<(), String> {
        match op {
            StemOp::Level { stem, position } => {
                self.stem_level[stem.index()]
                    .store(clamp_pos(position).to_bits(), Ordering::Relaxed);
            }
            StemOp::Mute { stem, on } => {
                self.stem_mute[stem.index()].store(on, Ordering::Relaxed);
            }
            StemOp::ToggleMute { stem } => {
                let slot = &self.stem_mute[stem.index()];
                let now = slot.load(Ordering::Relaxed);
                slot.store(!now, Ordering::Relaxed);
            }
            StemOp::ToggleSolo { stem } => {
                let bit = 1u8 << stem.index();
                let current = self.stem_solo.load(Ordering::Relaxed);
                self.stem_solo.store(current ^ bit, Ordering::Relaxed);
            }
            StemOp::Solo { stem, on } => {
                let bit = 1u8 << stem.index();
                let current = self.stem_solo.load(Ordering::Relaxed);
                self.stem_solo.store(
                    if on { current | bit } else { current & !bit },
                    Ordering::Relaxed,
                );
            }
            StemOp::Clear => {
                self.reset_stem_intent();
                return Ok(());
            }
            StemOp::Preset(preset) => {
                // A preset *is* a mute arrangement, so it also drops the solo set: leaving an old
                // solo in place would mask the arrangement the user just asked for.
                for (i, muted) in preset.mute().iter().enumerate() {
                    self.stem_mute[i].store(*muted, Ordering::Relaxed);
                }
                self.stem_solo.store(0, Ordering::Relaxed);
            }
        }
        self.refresh_stem_faders();
        Ok(())
    }

    /// All four stems audible at unity, nothing muted, nothing soloed.
    fn reset_stem_intent(&self) {
        for slot in &self.stem_level {
            slot.store(0.0f32.to_bits(), Ordering::Relaxed);
        }
        for slot in &self.stem_mute {
            slot.store(false, Ordering::Relaxed);
        }
        self.stem_solo.store(0, Ordering::Relaxed);
        self.refresh_stem_faders();
    }

    /// Recomputes every stream's fader from the intent.
    ///
    /// The one place level, mute and solo are combined, so they cannot disagree: a mute is `-1.0`
    /// — `bipolar_amp`'s *exact* silence, not a −80 dB leak — an unmute restores the level rather
    /// than unity, and a solo overrides the others' mutes.
    fn refresh_stem_faders(&self) {
        let solo = self.stem_solo.load(Ordering::Relaxed);
        for (i, fader) in self.flow_fader.iter().enumerate() {
            let audible = if solo != 0 {
                solo & (1 << i) != 0
            } else {
                !self.stem_mute[i].load(Ordering::Relaxed)
            };
            let position = if audible {
                f32::from_bits(self.stem_level[i].load(Ordering::Relaxed))
            } else {
                -1.0
            };
            fader.set(position);
        }
    }

    pub fn set_deck_fader(&self, position: f32) {
        self.deck_fader.set(clamp_pos(position));
    }

    /// Shared crossfader position: `-1.0` hard left, `0.0` both decks open, `1.0` hard right.
    pub fn set_crossfader(&self, position: f32) {
        self.crossfader.set(clamp_pos(position));
    }

    /// `0.0` (off) ..= `1.0` (full). Values outside that range are clamped, not wrapped.
    pub fn set_cue_send(&self, level: f32) {
        self.cue_send.set(if level.is_finite() { level.clamp(0.0, 1.0) } else { 0.0 });
    }

    /// The last block's cue copy. Only meaningful after [`Channel::process`].
    pub fn cue(&self) -> &Bus {
        &self.cue
    }

    /// Renders one block through the fixed chain, leaving the result in the channel's own bus.
    ///
    /// The chain is now **per stream**: each stream takes its own inserts and its own level, and the
    /// sum of those is what the shared deck chain then processes. With one stream this is the
    /// historical chain exactly (`flow_fx` then `flow_fader` on the channel's signal), which is why
    /// a track without stems is bit-identically unaffected.
    ///
    /// Returns a borrow of that bus so the mixer can sum it; the data is the channel's, so a
    /// consumer must read it before driving this channel again.
    pub fn process(&mut self, ctx: &crate::fx::FxContext) -> &mut Bus {
        // A source swap lands in `begin_block`, so this block's stream count is only known after it —
        // and the per-stream chains have to match it before anything renders.
        self.deck.begin_block();
        let streams = self.deck.stream_count().max(1);
        self.sync_streams(streams);

        // Move the scratch out so the deck, the chains and the cue copy can all be borrowed
        // mutably while the signal is in flight. No allocation: a swap of three `Vec`s.
        let mut bus = std::mem::take(&mut self.bus);
        bus.ensure_frames(ctx.block_frames);
        bus.clear();
        let mut stream_buses = std::mem::take(&mut self.stream_buses);

        // The cue send is an *advancing* smoother: one step per block. Stepping it once per stream
        // would shorten its time constant by the stream count, so it is taken here and passed in.
        let cue_gain = self.cue_send.next_block(ctx.block_frames, ctx.sample_rate);
        if self.cue_tap == CueTap::PostFlowFx {
            self.cue.clear();
        }

        let frames = ctx.block_frames;
        let live = streams.min(stream_buses.len());
        for stream in stream_buses[..live].iter_mut() {
            stream.ensure_frames(frames);
        }
        self.deck.render_streams(&mut stream_buses[..live]);

        for (stream, sb) in stream_buses[..live].iter_mut().enumerate() {
            if self.flow_fx[stream].any_active() {
                self.flow_fx[stream].process(sb, ctx);
            }
            if self.cue_tap == CueTap::PostFlowFx {
                self.cue.add_scaled(sb, cue_gain);
            }
            sb.scale(self.flow_fader[stream].next_amp(ctx));
            bus.add_from(sb);
        }
        if self.cue_tap == CueTap::PostFlowFader {
            self.take_cue_into(&bus, cue_gain);
        }

        if self.deck_fx.any_active() {
            self.deck_fx.process(&mut bus, ctx);
        }
        if self.cue_tap == CueTap::PostDeckFx {
            self.take_cue_into(&bus, cue_gain);
        }

        bus.scale(self.deck_fader.next_amp(ctx));
        if self.cue_tap == CueTap::PostDeckFader {
            self.take_cue_into(&bus, cue_gain);
        }

        // The crossfader is per-side and deliberately last: it is a routing decision, not tone, and
        // putting it before the FX would make a sweep change colour as the fader moved.
        let position = self.crossfader.next_position(ctx);
        let side_gain = self.side.gain_at(position, self.curve);
        bus.scale(side_gain);

        self.stream_buses = stream_buses;
        self.bus = bus;
        self.deck.end_block();
        &mut self.bus
    }

    /// Grows the per-stream scratch, chains and faders to `streams`.
    ///
    /// Chains are *rebuilt* from the config's names rather than copied — an [`FxSlot`] owns a
    /// `Box<dyn Fx>`, so an [`FxChain`] is not `Clone`. The names were resolved when the mixer was
    /// built, so a failure here is impossible.
    fn sync_streams(&mut self, streams: usize) {
        while self.flow_fx.len() < streams {
            let names = &self.flow_fx_names[self.flow_fx.len().min(Stem::COUNT - 1)];
            let slots = super::config::build_slots(names)
                .expect("flow_fx names were validated at mixer construction");
            self.flow_fx.push(FxChain::from_slots(slots));
            self.flow_fader.push(Fader::new(0.0, FADER_TAU));
            self.stream_buses.push(Bus::stereo(BLOCK_SIZE));
        }
        // Shrinking is not undone: the extra chains stay warm and unused if a reload drops back to
        // one stream, and are ready again when stems come back.
        self.refresh_stem_faders();
    }

    /// Copies `source` into the channel's cue bus at the current send level.
    fn take_cue_into(&mut self, source: &Bus, gain: f32) {
        self.cue.clear();
        if gain > 0.0 {
            self.cue.add_scaled(source, gain);
        }
    }

    /// One chain, by address.
    pub fn chain(&self, chain: SlotChain) -> Option<&FxChain> {
        match chain {
            SlotChain::Flow(stem) => self.flow_fx.get(stem.index()),
            SlotChain::Deck => Some(&self.deck_fx),
        }
    }

    pub fn chain_mut(&mut self, chain: SlotChain) -> Option<&mut FxChain> {
        match chain {
            SlotChain::Flow(stem) => self.flow_fx.get_mut(stem.index()),
            SlotChain::Deck => Some(&mut self.deck_fx),
        }
    }

    /// The slot at `index` within one chain.
    pub fn slot(&self, chain: SlotChain, index: usize) -> Option<&FxSlot> {
        self.chain(chain)?.slot(index)
    }

    pub fn slot_mut(&mut self, chain: SlotChain, index: usize) -> Option<&mut FxSlot> {
        self.chain_mut(chain)?.slot_mut(index)
    }

    /// Every slot in one chain.
    pub fn slot_statuses(&self, chain: SlotChain) -> Option<Vec<FxSlotStatus>> {
        Some(self.chain(chain)?.statuses())
    }

    /// Appends an effect to one chain, returning its index within *that* chain's own index space.
    ///
    /// There is no merged index space any more: with one chain per stream, "the deck's slot 2"
    /// would be ambiguous, so a caller always names the chain.
    pub fn add_slot(&mut self, chain: SlotChain, slot: FxSlot) -> Option<usize> {
        Some(self.chain_mut(chain)?.push_slot(slot))
    }

    /// Removes a slot by index from one chain, returning it so a caller can inspect what it dropped.
    pub fn remove_slot(&mut self, chain: SlotChain, index: usize) -> Option<FxSlot> {
        self.chain_mut(chain)?.remove(index)
    }

    /// The last rendered block, post-crossfader: exactly what this channel sends to the master.
    pub fn processed(&self) -> &Bus {
        &self.bus
    }

    /// Peak of the last rendered block, for a meter.
    pub fn peak(&self) -> f32 {
        self.bus.peak()
    }
}

/// A slot address inside one channel: which chain, and (implicitly, by the caller) where in it.
/// The chain is named outright — with one chain per stream there is no single index space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotChain {
    /// One stream's insert chain.
    Flow(Stem),
    /// The channel's shared deck chain.
    Deck,
}

/// Fader smoothing: short enough that a blend feels immediate, long enough that a stepped command
/// from a keyboard cannot click.
const FADER_TAU: f32 = 0.008;
/// The crossfade is the one move a listener watches for continuity, so it gets the longest ramp.
const CROSSFADE_TAU: f32 = 0.02;

#[inline]
fn clamp_pos(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::sample::{Eq, Filter, Gain, Limiter};
    use crate::fx::{Fx, FxContext};
    use crate::{BeatGrid, CHANNELS, SAMPLE_RATE};
    use hypermixx_core::{Source, Stem, StemOp, StemPreset};
    use hypermixx_media::{DecodedAudio, PcmPool};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const BLOCK: usize = BLOCK_SIZE;

    fn ctx() -> FxContext {
        FxContext::gridless(SAMPLE_RATE, BLOCK)
    }

    /// A ramp whose sample value *is* its frame index, so a position check is also a level check.
    fn ramp_source(frames: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..frames as usize).flat_map(|i| [i as f32, -(i as f32)]).collect(),
            total_frames: frames,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    /// A bounded ±1 stereo tone, for tests that measure *attenuation*: the ramp's amplitude grows
    /// with its frame index, so "is it quiet" against it has no fixed answer.
    /// 100 Hz, i.e. inside the low shelf's stopband (f0 320 Hz), where a full counter-clockwise bass
    /// kill is decisive rather than transitional.
    fn tone_source(frames: u64) -> std::sync::Arc<dyn hypermixx_core::Source> {
        use std::f64::consts::PI;
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..frames as usize)
                .flat_map(|i| {
                    let s = (2.0 * PI * 100.0 * i as f64 / f64::from(SAMPLE_RATE)).sin() as f32;
                    [s, -s]
                })
                .collect(),
            total_frames: frames,
            sample_rate: SAMPLE_RATE,
            channels: crate::CHANNELS,
        }))
    }

    /// Long enough that no test in this module runs a deck to the end (which would make every
    /// "is it silent" assertion pass for the wrong reason).
    fn deck_at_zero() -> Deck {
        Deck::new(ramp_source((BLOCK * 2_000) as u64))
    }

    fn tone_deck() -> Deck {
        Deck::new(tone_source((BLOCK * 2_000) as u64))
    }

    /// A channel over the zero-ramp deck. `flow` is the per-stream chain list the mixer would have
    /// built; tests that pass one chain get three empty ones built from an empty config template.
    fn channel_with(cfg: ChannelConfig, flow: Vec<FxChain>, deck_chain: FxChain) -> Channel {
        Channel::new(deck_at_zero(), &cfg, flow, deck_chain)
    }

    fn unity() -> ChannelConfig {
        ChannelConfig {
            cue_send: 1.0,
            ..Default::default()
        }
    }

    #[test]
    fn unity_channel_passes_the_deck_straight_through() {
        // Two decks over the same source, driven identically: one through the channel, one straight.
        // (Driving the *channel's* deck first would compare block 1 against block 2 and pass only by
        // accident on a slowly-changing signal.)
        let mut reference = deck_at_zero();
        reference.play();
        let mut out = vec![0.0f32; BLOCK * crate::CHANNELS];
        reference.process_block(&mut out);
        // `Center` puts the crossfader out of the comparison: an equal-power law at centre is
        // −3 dB per side by definition (that is how two decks sum to unity power), so including it
        // here would measure the pan law rather than "the channel adds nothing".
        let mut channel = Channel::new(
            deck_at_zero(),
            &ChannelConfig { side: DeckSide::Center, cue_send: 1.0, ..Default::default() },
            vec![FxChain::new()],
            FxChain::new(),
        );
        channel.deck_mut().play();
        // A *tolerance* is honest here: the time-stretch engine's unity path is a resampling chain
        // whose coefficient product lands ~1e-17 off an exact copy. Pinning that to bit-exact would
        // be testing the library's rounding, not the mixer.
        let bus = channel.process(&ctx());
        assert_eq!(bus.frames(), BLOCK);
        let tolerance = 1e-4 * (BLOCK as f32);
        for i in 0..BLOCK {
            assert!(
                (bus.l[i] - out[i * crate::CHANNELS]).abs() < tolerance,
                "frame {i}: bus {} != deck {}",
                bus.l[i],
                out[i * crate::CHANNELS]
            );
            assert!(
                (bus.r[i] - out[i * crate::CHANNELS + 1]).abs() < tolerance,
                "frame {i}: right plane crossed over"
            );
        }
    }

    #[test]
    fn flow_fader_scales_the_block() {
        let mut half = channel_with(
            ChannelConfig { flow_fader: -0.5, cue_send: 1.0, ..Default::default() },
            vec![FxChain::new()],
            FxChain::new(),
        );
        half.deck_mut().play();
        let mut unity = channel_with(unity(), vec![FxChain::new()], FxChain::new());
        unity.deck_mut().play();
        let mut moved = 0.0f32;
        let mut full = 0.0f32;
        for _ in 0..3 {
            moved = half.process(&ctx()).peak();
            full = unity.process(&ctx()).peak();
        }
        assert!(moved < full * 0.9, "the fader did nothing: {moved} vs {full}");
    }

    #[test]
    fn a_closed_fader_is_silent() {
        let mut channel = Channel::new(
            tone_deck(),
            &ChannelConfig { flow_fader: -1.0, cue_send: 1.0, ..Default::default() },
            vec![FxChain::new()],
            FxChain::new(),
        );
        channel.deck_mut().play();
        for _ in 0..50 {
            channel.process(&ctx());
        }
        // `bipolar_amp(-1.0)` is defined as exactly zero, so a closed fader is digital silence —
        // the point of the kill clamp, and worth pinning because a −80 dB "floor" would leak here.
        assert!(channel.peak() < 1e-6, "a closed fader leaked {}", channel.peak());
    }

    #[test]
    fn crossfader_is_a_pan_law_not_a_volume() {
        // Hard left must mute the right-hand channel and leave the left one alone; at centre both
        // are open. That is what lets a crossfader isolate a deck without touching its fader.
        let left = DeckSide::Left.gain_at(-1.0, CrossfaderCurve::EqualPower);
        let right = DeckSide::Right.gain_at(-1.0, CrossfaderCurve::EqualPower);
        assert!(left > 0.99 && right < 1e-6, "hard left gave {left}/{right}");
        let (l, r) = (
            DeckSide::Left.gain_at(0.0, CrossfaderCurve::EqualPower),
            DeckSide::Right.gain_at(0.0, CrossfaderCurve::EqualPower),
        );
        assert!((l - r).abs() < 1e-6 && l > 0.7, "centre was not balanced: {l}/{r}");
        assert_eq!(DeckSide::Center.gain_at(1.0, CrossfaderCurve::Linear), 1.0);
        assert!(
            DeckSide::Left.gain_at(-1.0, CrossfaderCurve::Linear) > 0.99,
            "linear law must also hard-open at its own end"
        );
        // Monotonic across the travel for both sides.
        let mut prev_l = 2.0;
        let mut prev_r = -1.0;
        for i in -10..=10 {
            let p = i as f32 / 10.0;
            let l = DeckSide::Left.gain_at(p, CrossfaderCurve::EqualPower);
            let r = DeckSide::Right.gain_at(p, CrossfaderCurve::EqualPower);
            assert!(l <= prev_l + 1e-6, "left gain rose moving right");
            assert!(r >= prev_r - 1e-6, "right gain fell moving right");
            (prev_l, prev_r) = (l, r);
        }
    }

    #[test]
    fn a_crossfader_position_reaches_both_planes_of_a_stereo_signal() {
        let mut channel = channel_with(
            ChannelConfig {
                side: DeckSide::Right,
                crossfader: -1.0,
                cue_send: 1.0,
                ..Default::default()
            },
            vec![FxChain::new()],
            FxChain::new(),
        );
        channel.deck_mut().play();
        for _ in 0..20 {
            channel.process(&ctx());
        }
        assert!(channel.peak() < 1e-4, "a right channel at hard left still sounded");
        // The cue is taken *before* the crossfader, so a channel faded out of the mix still cues.
        assert!(channel.cue().peak() > 0.0, "cue must ignore the crossfader");
    }

    #[test]
    fn cue_taps_select_the_right_point_in_the_chain() {
        // A fully killed deck chain must be *missing from* the two pre-EQ taps and *applied to* the
        // two post-EQ ones. That difference is the whole reason four taps exist.
        //
        // Measured against a flat-EQ channel as the reference, so the assertion is about how much of
        // the signal survived the chain rather than about an absolute level.
        let reference = {
            let mut channel = Channel::new(
                tone_deck(),
                &ChannelConfig { cue_send: 1.0, cue_tap: CueTap::PostDeckFx, ..Default::default() },
                vec![FxChain::new()],
                FxChain::from_parts([("eq", Box::new(Eq::new()) as Box<dyn Fx>)]),
            );
            channel.deck_mut().play();
            for _ in 0..40 {
                channel.process(&ctx());
            }
            channel.cue().peak()
        };
        assert!(reference > 0.5, "the flat reference cue should carry the tone, got {reference}");

        for (tap, expects_cut) in [
            (CueTap::PostFlowFx, false),
            (CueTap::PostFlowFader, false),
            (CueTap::PostDeckFx, true),
            (CueTap::PostDeckFader, true),
        ] {
            let mut channel = Channel::new(
                tone_deck(),
                &ChannelConfig { cue_send: 1.0, cue_tap: tap, ..Default::default() },
                vec![FxChain::new()],
                FxChain::from_parts([("eq", Box::new(all_the_way_down()) as Box<dyn Fx>)]),
            );
            channel.deck_mut().play();
            for _ in 0..40 {
                channel.process(&ctx());
            }
            let peak = channel.cue().peak();
            let cut = peak < reference * 0.1;
            assert_eq!(
                cut, expects_cut,
                "tap {tap:?} cued {peak} against a flat reference of {reference}"
            );
        }
    }

    /// Every band pinned to the bottom of its travel: nothing should survive the chain.
    fn all_the_way_down() -> Eq {
        let eq = Eq::new();
        for band in Eq::PARAMS[..3].iter() {
            eq.set_param(band, -1.0).unwrap();
        }
        eq
    }

    #[test]
    fn cue_send_of_zero_leaves_a_silent_take() {
        let mut channel = Channel::new(
            tone_deck(),
            &ChannelConfig { cue_send: -1.0, ..Default::default() },
            vec![FxChain::new()],
            FxChain::new(),
        );
        channel.deck_mut().play();
        for _ in 0..20 {
            channel.process(&ctx());
        }
        assert!(channel.cue().is_silent(), "a zero cue send leaked {}", channel.cue().peak());
        // ...while the deck itself still reaches the master, i.e. the tap is what closed, not the path.
        assert!(channel.peak() > 0.5);
    }

    /// A chain is named outright. There is no merged index space any more: with one chain per
    /// stream, "the channel's slot 2" could mean four different effects.
    #[test]
    fn each_chain_has_its_own_index_space() {
        let mut channel = channel_with(
            unity(),
            vec![FxChain::from_parts([("filter", Box::new(Filter::new()) as Box<dyn Fx>)])],
            FxChain::from_parts([
                ("gain", Box::new(Gain::new(1.0)) as Box<dyn Fx>),
                ("limiter", Box::new(Limiter::new()) as Box<dyn Fx>),
            ]),
        );
        let stream = SlotChain::Flow(Stem::Drums);
        assert_eq!(channel.slot_statuses(stream).unwrap().len(), 1);
        assert_eq!(channel.slot_statuses(SlotChain::Deck).unwrap().len(), 2);
        assert_eq!(channel.slot(stream, 0).map(FxSlot::kind), Some("filter"));
        assert_eq!(channel.slot(SlotChain::Deck, 0).map(FxSlot::kind), Some("gain"));
        assert_eq!(channel.slot(SlotChain::Deck, 1).map(FxSlot::kind), Some("limiter"));
        assert!(channel.slot(SlotChain::Deck, 2).is_none());
        // Removing from the flow chain cannot renumber the deck chain.
        let dropped = channel
            .remove_slot(stream, 0)
            .map(|slot| slot.kind().to_owned());
        assert_eq!(dropped.as_deref(), Some("filter"));
        assert_eq!(channel.slot_statuses(stream).unwrap().len(), 0);
        assert_eq!(channel.slot(SlotChain::Deck, 0).map(FxSlot::kind), Some("gain"));
        // Every stem chain exists from construction (empty), so an `fx` command can address a stem
        // before the deck has ever been separated.
        assert_eq!(
            channel.slot_statuses(SlotChain::Flow(Stem::Vocals)).unwrap().len(),
            0
        );
    }

    #[test]
    fn adding_a_slot_returns_its_index_within_that_chain() {
        let mut channel = channel_with(unity(), vec![FxChain::new()], FxChain::new());
        let stream = SlotChain::Flow(Stem::Bass);
        assert_eq!(
            channel
                .add_slot(stream, FxSlot::new(Box::new(Gain::new(1.0)), "gain"))
                .unwrap(),
            0
        );
        // The deck chain counts from its own zero, not from the flow chain's length.
        assert_eq!(
            channel
                .add_slot(SlotChain::Deck, FxSlot::new(Box::new(Gain::new(1.0)), "gain"))
                .unwrap(),
            0
        );
        assert_eq!(channel.slot_statuses(stream).unwrap().len(), 1);
        assert_eq!(channel.slot_statuses(SlotChain::Deck).unwrap().len(), 1);
    }

    /// A constant DC source: the channel's output is then a plain sum of per-stream numbers, so a
    /// wrong gain, a crossed stream or a mute that is not silence shows up as an exact mismatch.
    fn dc_source(value: f32, frames: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..frames as usize).flat_map(|_| [value, -value]).collect(),
            total_frames: frames,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    /// A channel with nothing in the way of a plain sum: `Center` (no crossfader pan law), unity
    /// flow/deck faders, no cue send.
    fn stem_channel() -> Channel {
        Channel::new(
            deck_at_zero(),
            &ChannelConfig {
                side: DeckSide::Center,
                flow_fader: 0.0,
                deck_fader: 0.0,
                cue_send: 0.0,
                ..Default::default()
            },
            vec![FxChain::new()],
            FxChain::new(),
        )
    }

    const TAGS: [f32; 4] = [1.0, 1000.0, 2000.0, 3000.0];

    /// Installs four tagged stems and settles until the swap has landed (it is warmed on another
    /// thread), so the audio really has four streams.
    fn install_stems(channel: &mut Channel) {
        let sources = TAGS.map(|tag| dc_source(tag, 200_000) as Arc<dyn Source>);
        channel.set_stems(hypermixx_core::StemSet::new(sources));
        channel.deck_mut().play();
        assert!(!channel.has_stems(), "the swap must not land before it is warmed");
        for _ in 0..500 {
            channel.process(&ctx());
            if channel.has_stems() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("stems never installed");
    }

    /// Settles the fader smoothers, then reads the channel's left plane at frame 0.
    fn settled_left(channel: &mut Channel) -> f32 {
        for _ in 0..40 {
            channel.process(&ctx());
        }
        channel.process(&ctx()).l[0]
    }

    #[test]
    fn a_stem_deck_sums_its_four_streams_and_reports_them_ready() {
        let mut channel = stem_channel();
        install_stems(&mut channel);
        let status = channel.stem_status();
        assert!(status.ready, "four streams are rendering, so the stems are ready");
        assert_eq!(status.level, [0.0; 4]);
        assert!(!status.any_solo() && status.mute == [false; 4]);
        assert!((settled_left(&mut channel) - TAGS.iter().sum::<f32>()).abs() < 1e-2);
    }

    #[test]
    fn a_muted_stem_is_exactly_silent_and_an_unmute_restores_its_level() {
        let mut channel = stem_channel();
        install_stems(&mut channel);
        channel
            .apply_stem(StemOp::Level { stem: Stem::Bass, position: -0.5 })
            .unwrap();
        // Bass at -0.5 is a quarter of its amplitude (the squared lower half of the fader law).
        let bass = TAGS[1] * 0.25;
        let expected = TAGS[0] + bass + TAGS[2] + TAGS[3];
        assert!((settled_left(&mut channel) - expected).abs() < 1e-2);

        channel
            .apply_stem(StemOp::Mute { stem: Stem::Vocals, on: true })
            .unwrap();
        let muted = settled_left(&mut channel);
        assert!(
            (muted - (expected - TAGS[3])).abs() < 1e-2,
            "a muted stem must contribute nothing, got {muted}"
        );

        // Unmuting restores the *level* the DJ had set, it does not reset to unity — that is why
        // mute is kept as its own fact rather than baked into the level.
        channel
            .apply_stem(StemOp::Mute { stem: Stem::Vocals, on: false })
            .unwrap();
        assert!((settled_left(&mut channel) - expected).abs() < 1e-2);
    }

    #[test]
    fn solo_isolates_stems_and_wins_over_mute() {
        let mut channel = stem_channel();
        install_stems(&mut channel);
        channel
            .apply_stem(StemOp::Mute { stem: Stem::Drums, on: true })
            .unwrap();
        channel
            .apply_stem(StemOp::Solo { stem: Stem::Drums, on: true })
            .unwrap();
        // Solo overrides everybody's mute, including its own target's.
        assert!((settled_left(&mut channel) - TAGS[0]).abs() < 1e-2);

        // The solo set is a *set* — a second solo adds to it rather than replacing, which is what
        // makes "solo the drums and the vocals together" expressible with one button per stem.
        channel
            .apply_stem(StemOp::Solo { stem: Stem::Vocals, on: true })
            .unwrap();
        assert!((settled_left(&mut channel) - (TAGS[0] + TAGS[3])).abs() < 1e-2);

        // Leaving the set re-masks everything else.
        channel
            .apply_stem(StemOp::Solo { stem: Stem::Drums, on: false })
            .unwrap();
        assert!((settled_left(&mut channel) - TAGS[3]).abs() < 1e-2);

        channel.apply_stem(StemOp::Clear).unwrap();
        assert!((settled_left(&mut channel) - TAGS.iter().sum::<f32>()).abs() < 1e-2);
    }

    #[test]
    fn a_preset_is_a_mute_arrangement_that_keeps_the_djs_levels() {
        let mut channel = stem_channel();
        install_stems(&mut channel);
        channel
            .apply_stem(StemOp::Level { stem: Stem::Vocals, position: -0.5 })
            .unwrap();
        channel
            .apply_stem(StemOp::Preset(StemPreset::Instrumental))
            .unwrap();
        let instrumental = TAGS[0] + TAGS[1] + TAGS[2];
        assert!((settled_left(&mut channel) - instrumental).abs() < 1e-2);

        channel
            .apply_stem(StemOp::Preset(StemPreset::Acapella))
            .unwrap();
        // Acapella brings the vocals back at the level they were left at, not at unity.
        assert!((settled_left(&mut channel) - TAGS[3] * 0.25).abs() < 1e-2);
    }

    /// The channel end of `MixerConfig::build_chains`: the config's template reaches every stream
    /// and its `stem_fx` override reaches exactly one.
    ///
    /// The chains a caller *passes* are authoritative — that is how a test injects an arbitrary
    /// chain — and the config's names only fill the gaps, so a hand-built channel that passes one
    /// chain still ends up with `Stem::COUNT` of them. The mixer passes all four, so production
    /// never depends on the precedence.
    #[test]
    fn a_channel_names_a_chain_per_stem_from_the_config() {
        let cfg = ChannelConfig {
            flow_fx: vec!["gain".into()],
            stem_fx: [(Stem::Vocals, vec!["filter".into()])].into_iter().collect(),
            ..Default::default()
        };
        let passed = FxChain::from_parts([("eq", Box::new(Eq::new()) as Box<dyn Fx>)]);
        let mut channel = Channel::new(deck_at_zero(), &cfg, vec![passed], FxChain::new());
        channel.sync_streams(Stem::COUNT);
        let kinds = |stem| -> Vec<&'static str> {
            channel
                .stem_fx(stem)
                .expect("every stem has a chain")
                .slots()
                .iter()
                .map(|slot| slot.kind())
                .collect()
        };
        assert_eq!(kinds(Stem::Drums), ["eq"], "what the caller passed wins");
        assert_eq!(kinds(Stem::Bass), ["gain"], "the template");
        assert_eq!(kinds(Stem::Other), ["gain"], "the template");
        assert_eq!(kinds(Stem::Vocals), ["filter"], "the override");
        // Each is its own instance, not one shared chain: a filter's state must never be shared
        // between stems.
        let (bass, other) = (
            channel.stem_fx(Stem::Bass).unwrap(),
            channel.stem_fx(Stem::Other).unwrap(),
        );
        assert!(!std::ptr::eq(bass, other));
    }

    /// The point of a chain per stream: an insert on one stem must not touch the others.
    #[test]
    fn a_stem_chain_only_processes_its_own_stream() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut channel = stem_channel();
        install_stems(&mut channel);
        let vocals = SlotChain::Flow(Stem::Vocals);
        channel
            .add_slot(
                vocals,
                FxSlot::new(Box::new(Counter(hits.clone())), "counter"),
            )
            .unwrap();
        channel.process(&ctx());
        assert_eq!(hits.load(Ordering::Relaxed), 1, "the stem's chain ran once");

        // Bypassing it stops the work; the other streams never had it.
        channel
            .chain_mut(vocals)
            .unwrap()
            .slot_mut(0)
            .unwrap()
            .set_enabled(false);
        channel.process(&ctx());
        assert_eq!(hits.load(Ordering::Relaxed), 1, "a bypassed chain is not entered");

        // A fresh insert lands on the *other* stream's chain and leaves this one alone.
        let drums = SlotChain::Flow(Stem::Drums);
        assert_eq!(channel.slot_statuses(drums).unwrap().len(), 0);
        assert_eq!(channel.slot_statuses(vocals).unwrap().len(), 1);
    }

    #[test]
    fn a_bypassed_chain_is_not_entered_at_all() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut channel = channel_with(
            unity(),
            vec![FxChain::from_parts([("counter", Box::new(Counter(hits.clone())) as Box<dyn Fx>)])],
            FxChain::new(),
        );
        channel.deck_mut().play();
        channel.flow_fx().slot(0).unwrap().set_enabled(false);
        channel.process(&ctx());
        assert_eq!(hits.load(Ordering::Relaxed), 0, "the mixer's guard is what keeps FX cheap");
        channel.flow_fx().slot(0).unwrap().set_enabled(true);
        channel.process(&ctx());
        assert_eq!(hits.load(Ordering::Relaxed), 1);
    }

    struct Counter(Arc<AtomicUsize>);
    impl Fx for Counter {
        fn process(&mut self, _bus: &mut Bus, _ctx: &FxContext) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn each_chain_runs_exactly_once_per_block() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut channel = channel_with(
            unity(),
            vec![FxChain::from_parts([("counter", Box::new(Counter(hits.clone())) as Box<dyn Fx>)])],
            FxChain::new(),
        );
        channel.deck_mut().play();
        for _ in 0..7 {
            channel.process(&ctx());
        }
        assert_eq!(hits.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn loading_a_new_deck_keeps_the_mixer_controls() {
        let mut channel = channel_with(unity(), vec![FxChain::new()], FxChain::from_parts([("eq", Box::new(Eq::new()) as Box<dyn Fx>)]));
        channel.set_flow_fader(-0.25);
        channel.deck_fx().slot(0).unwrap().set_param("low", 0.5).unwrap();
        channel.replace_deck(Deck::new(ramp_source(1_000)));
        assert_eq!(channel.levels().0, -0.25, "a load must not reset the fader");
        assert_eq!(
            channel.slot_statuses(SlotChain::Deck).unwrap()[0].params[0].1,
            0.5,
            "...or the EQ"
        );
        assert_eq!(channel.deck().total_frames(), 1_000);
    }

    #[test]
    fn analysis_reaches_the_deck_through_the_channel() {
        let mut channel = channel_with(unity(), vec![FxChain::new()], FxChain::new());
        channel.deck_mut().set_analysis(crate::core::TrackAnalysis::from_grid(
            BeatGrid::from_constant_bpm(122.0, 0, 44_100 * 10, SAMPLE_RATE),
            122.0,
        ));
        assert!((channel.deck().bpm() - 122.0).abs() < 0.5);
    }

    #[test]
    fn a_bad_fader_value_cannot_break_the_law() {
        let mut channel = channel_with(unity(), vec![FxChain::new()], FxChain::new());
        channel.set_flow_fader(f32::NAN);
        channel.set_crossfader(99.0);
        channel.set_deck_fader(-99.0);
        assert_eq!(channel.levels(), (0.0, -1.0, 1.0, 1.0));
        channel.deck_mut().play();
        channel.process(&ctx());
        assert!(channel.peak().is_finite());
    }
}
