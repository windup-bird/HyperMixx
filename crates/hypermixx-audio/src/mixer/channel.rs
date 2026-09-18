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

use hypermixx_core::FxSlotStatus;

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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

/// One mixer input: a deck, two FX chains, four faders and a cue tap.
pub struct Channel {
    deck: Deck,
    flow_fx: FxChain,
    deck_fx: FxChain,
    /// The channel's own signal, rendered in place. Allocated once by [`Channel::new`].
    bus: Bus,
    /// The cue copy taken at [`Channel::cue_tap`], filled during [`Channel::process`].
    cue: Bus,
    flow_fader: Fader,
    /// Full range until a track exposes stems; separate from [`Channel::flow_fader`] so wiring stems
    /// later means a new source, not a re-plumbing.
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
    pub fn new(deck: Deck, cfg: &ChannelConfig, flow_fx: FxChain, deck_fx: FxChain) -> Self {
        let frames = BLOCK_SIZE;
        Self {
            deck,
            flow_fx,
            deck_fx,
            bus: Bus::stereo(frames),
            cue: Bus::stereo(frames),
            flow_fader: Fader::new(cfg.flow_fader, FADER_TAU),
            deck_fader: Fader::new(cfg.deck_fader, FADER_TAU),
            crossfader: Fader::new(cfg.crossfader, CROSSFADE_TAU),
            cue_send: Param::new(cfg.cue_send.clamp(0.0, 1.0), FADER_TAU),
            side: cfg.side,
            curve: cfg.crossfader_curve,
            cue_tap: cfg.cue_tap,
        }
    }

    pub fn deck(&self) -> &Deck {
        &self.deck
    }

    pub fn deck_mut(&mut self) -> &mut Deck {
        &mut self.deck
    }

    /// Installs a fresh transport (`Command::Load`). The chains and faders deliberately survive: a
    /// loaded track should not reset the mixer's tone controls.
    pub fn replace_deck(&mut self, deck: Deck) {
        self.deck = deck;
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

    pub fn flow_fx(&self) -> &FxChain {
        &self.flow_fx
    }

    pub fn deck_fx(&self) -> &FxChain {
        &self.deck_fx
    }

    pub fn flow_fx_mut(&mut self) -> &mut FxChain {
        &mut self.flow_fx
    }

    pub fn deck_fx_mut(&mut self) -> &mut FxChain {
        &mut self.deck_fx
    }

    pub fn set_crossfader_curve(&mut self, curve: CrossfaderCurve) {
        self.curve = curve;
    }

    /// Fader positions (`-1.0 ..= 1.0`), for a UI mirror.
    /// `(flow, deck, crossfader)` as bipolar positions, `(cue send)` as a linear level.
    pub fn levels(&self) -> (f32, f32, f32, f32) {
        (
            self.flow_fader.target(),
            self.deck_fader.target(),
            self.crossfader.target(),
            self.cue_send.target(),
        )
    }

    pub fn set_flow_fader(&self, position: f32) {
        self.flow_fader.set(clamp_pos(position));
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
    /// Returns a borrow of that bus so the mixer can sum it; the data is the channel's, so a
    /// consumer must read it before driving this channel again.
    pub fn process(&mut self, ctx: &crate::fx::FxContext) -> &mut Bus {
        // Move the scratch bus out so the deck, the chains and the cue copy can all be borrowed
        // mutably while the signal is in flight. No allocation: this is a swap of two `Vec`s.
        let mut bus = std::mem::take(&mut self.bus);
        bus.ensure_frames(ctx.block_frames);
        bus.clear();

        self.deck.pull_into(&mut bus, ctx);
        if self.flow_fx.any_active() {
            self.flow_fx.process(&mut bus, ctx);
        }
        if self.cue_tap == CueTap::PostFlowFx {
            self.take_cue_into(&bus, ctx);
        }

        bus.scale(self.flow_fader.next_amp(ctx));
        if self.cue_tap == CueTap::PostFlowFader {
            self.take_cue_into(&bus, ctx);
        }

        if self.deck_fx.any_active() {
            self.deck_fx.process(&mut bus, ctx);
        }
        if self.cue_tap == CueTap::PostDeckFx {
            self.take_cue_into(&bus, ctx);
        }

        bus.scale(self.deck_fader.next_amp(ctx));
        if self.cue_tap == CueTap::PostDeckFader {
            self.take_cue_into(&bus, ctx);
        }

        // The crossfader is per-side and deliberately last: it is a routing decision, not tone, and
        // putting it before the FX would make a sweep change colour as the fader moved.
        let position = self.crossfader.next_position(ctx);
        let side_gain = self.side.gain_at(position, self.curve);
        bus.scale(side_gain);

        self.bus = bus;
        &mut self.bus
    }

    /// Copies `source` into the channel's cue bus at the current send level.
    fn take_cue_into(&mut self, source: &Bus, ctx: &crate::fx::FxContext) {
        let gain = self.cue_send.next_block(ctx.block_frames, ctx.sample_rate);
        self.cue.clear();
        if gain > 0.0 {
            self.cue.add_scaled(source, gain);
        }
    }

    /// The slot at a deck-level index, where the flow chain is addressed first.
    pub fn slot(&self, index: usize) -> Option<&FxSlot> {
        let flow = self.flow_fx.len();
        if index < flow {
            self.flow_fx.slot(index)
        } else {
            self.deck_fx.slot(index - flow)
        }
    }

    pub fn slot_mut(&mut self, index: usize) -> Option<&mut FxSlot> {
        let flow = self.flow_fx.len();
        if index < flow {
            self.flow_fx.slot_mut(index)
        } else {
            self.deck_fx.slot_mut(index - flow)
        }
    }

    /// Which of the two chains `index` belongs to, and its local position there.
    pub fn locate_slot(&self, index: usize) -> Option<ChainSlot> {
        let flow = self.flow_fx.len();
        if index < flow {
            Some(ChainSlot { chain: SlotChain::Flow, local: index })
        } else {
            let local = index - flow;
            (local < self.deck_fx.len()).then_some(ChainSlot { chain: SlotChain::Deck, local })
        }
    }

    /// Every slot in the deck's merged index space.
    pub fn slot_statuses(&self) -> Vec<FxSlotStatus> {
        let mut out = self.flow_fx.statuses();
        out.extend(self.deck_fx.statuses_at(self.flow_fx.len()));
        out
    }

    /// Appends an effect to one of the channel's chains, returning the **merged** index (the one
    /// [`Channel::slot`] and the protocol's `FxSlotRef::index` speak).
    pub fn add_slot(&mut self, chain: SlotChain, slot: FxSlot) -> usize {
        match chain {
            SlotChain::Flow => self.flow_fx.push_slot(slot),
            SlotChain::Deck => {
                let flow = self.flow_fx.len();
                flow + self.deck_fx.push_slot(slot)
            }
        }
    }

    /// Removes a slot by merged index, returning it so a caller can inspect what it dropped.
    pub fn remove_slot(&mut self, index: usize) -> Option<FxSlot> {
        self.locate_slot(index).and_then(|at| match at.chain {
            SlotChain::Flow => self.flow_fx.remove(at.local),
            SlotChain::Deck => self.deck_fx.remove(at.local),
        })
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

/// A slot address inside one channel, split by chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainSlot {
    pub chain: SlotChain,
    pub local: usize,
}

/// Which of a channel's two chains a command means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotChain {
    Flow,
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
    use hypermixx_core::Source;
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

    fn channel_with(cfg: ChannelConfig, flow: FxChain, deck_chain: FxChain) -> Channel {
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
            FxChain::new(),
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
            FxChain::new(),
            FxChain::new(),
        );
        half.deck_mut().play();
        let mut unity = channel_with(unity(), FxChain::new(), FxChain::new());
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
            FxChain::new(),
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
            FxChain::new(),
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
                FxChain::new(),
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
                FxChain::new(),
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
            FxChain::new(),
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

    #[test]
    fn two_chains_are_addressed_through_one_index_space() {
        let mut channel = channel_with(
            unity(),
            FxChain::from_parts([("filter", Box::new(Filter::new()) as Box<dyn Fx>)]),
            FxChain::from_parts([
                ("gain", Box::new(Gain::new(1.0)) as Box<dyn Fx>),
                ("limiter", Box::new(Limiter::new()) as Box<dyn Fx>),
            ]),
        );
        assert_eq!(channel.slot_statuses().len(), 3);
        assert_eq!(channel.slot(0).map(FxSlot::kind), Some("filter"));
        assert_eq!(channel.slot(1).map(FxSlot::kind), Some("gain"));
        assert_eq!(channel.slot(2).map(FxSlot::kind), Some("limiter"));
        assert!(channel.slot(3).is_none());
        assert_eq!(
            channel.locate_slot(2),
            Some(ChainSlot { chain: SlotChain::Deck, local: 1 })
        );
        assert_eq!(channel.locate_slot(3), None);
        // Removal from the middle of the *flow* chain must renumber the deck slots.
        let dropped = channel.remove_slot(0).map(|slot| slot.kind().to_owned());
        assert_eq!(dropped.as_deref(), Some("filter"));
        assert_eq!(channel.slot(0).map(FxSlot::kind), Some("gain"));
        assert_eq!(channel.slot_statuses()[0].index, 0);
    }

    #[test]
    fn adding_a_slot_returns_its_merged_index() {
        let mut channel = channel_with(unity(), FxChain::new(), FxChain::new());
        assert_eq!(
            channel.add_slot(SlotChain::Flow, FxSlot::new(Box::new(Gain::new(1.0)), "gain")),
            0
        );
        assert_eq!(
            channel.add_slot(SlotChain::Deck, FxSlot::new(Box::new(Gain::new(1.0)), "gain")),
            1
        );
        assert_eq!(channel.slot_statuses().len(), 2);
    }

    #[test]
    fn a_bypassed_chain_is_not_entered_at_all() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut channel = channel_with(
            unity(),
            FxChain::from_parts([("counter", Box::new(Counter(hits.clone())) as Box<dyn Fx>)]),
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
            FxChain::from_parts([("counter", Box::new(Counter(hits.clone())) as Box<dyn Fx>)]),
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
        let mut channel = channel_with(unity(), FxChain::new(), FxChain::from_parts([("eq", Box::new(Eq::new()) as Box<dyn Fx>)]));
        channel.set_flow_fader(-0.25);
        channel.deck_fx().slot(0).unwrap().set_param("low", 0.5).unwrap();
        channel.replace_deck(Deck::new(ramp_source(1_000)));
        assert_eq!(channel.levels().0, -0.25, "a load must not reset the fader");
        assert_eq!(channel.slot_statuses()[0].params[0].1, 0.5, "...or the EQ");
        assert_eq!(channel.deck().total_frames(), 1_000);
    }

    #[test]
    fn analysis_reaches_the_deck_through_the_channel() {
        let mut channel = channel_with(unity(), FxChain::new(), FxChain::new());
        channel.deck_mut().set_analysis(crate::core::TrackAnalysis::from_grid(
            BeatGrid::from_constant_bpm(122.0, 0, 44_100 * 10, SAMPLE_RATE),
            122.0,
        ));
        assert!((channel.deck().bpm() - 122.0).abs() < 0.5);
    }

    #[test]
    fn a_bad_fader_value_cannot_break_the_law() {
        let mut channel = channel_with(unity(), FxChain::new(), FxChain::new());
        channel.set_flow_fader(f32::NAN);
        channel.set_crossfader(99.0);
        channel.set_deck_fader(-99.0);
        assert_eq!(channel.levels(), (0.0, -1.0, 1.0, 1.0));
        channel.deck_mut().play();
        channel.process(&ctx());
        assert!(channel.peak().is_finite());
    }
}
