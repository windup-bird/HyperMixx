//! [`Channel`]: one deck plus the fixed signal path that turns its audio into a mix contribution.
//!
//! The order is hard-coded — it *is* the design argument of this module:
//!
//! ```text
//! deck.pull_into(bus)                                          (per stream)
//!   → source gate    the stem mute/solo decision, exactly 0.0 or 1.0
//!   → flow_fx        per-stream inserts (the sweep that rides one flow)
//!   → flow_fader     per-stream level: a stem's own level, not the channel fader
//!   → Σ              the channel's sum
//!   → [cue tap]      always immediately **before** the channel fader
//!   → fader          the one channel fader; `[channel] fader = "pre_fx"` puts it here
//!   → chain          the channel's inserts (`[channel] chain = ["eq", "filter"]`)
//!   → [cue tap]      and here when `fader = "post_fx"`
//!   → fader
//!   → gain           channel trim, 0 dB .. +6 dB (the only stage that can boost)
//!   → master
//! ```
//!
//! There is **one** channel fader, and `[channel] fader` decides where it sits relative to the
//! channel's `chain`; the cue send always taps the signal just before it, so pulling a fader down
//! never silences the monitor (which is the whole point of cueing a track in).
//!
//! The shared crossfader's per-side gain is applied at whichever of the three positions
//! `[crossfader] place` names — always after the cue tap at that point, so a cue stays a preview of
//! the channel whatever the crossfader is doing.
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
use super::config::{ChannelConfig, CrossfaderConfig};
use crate::deck::Deck;
use crate::fx::dsp::trim_amp;
use crate::fx::sample::Fader;
use crate::fx::{FxChain, FxSlot, Param};
use crate::BLOCK_SIZE;

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

/// Where in the channel's path the crossfader's gain is applied. One physical control, but *where*
/// it acts is a design parameter: before the flow inserts, after the flow faders, or at the very end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossfaderPlace {
    /// Per stream, right after the source gate and before the flow inserts.
    Source,
    /// On the channel's sum, after the flow faders and before the deck inserts.
    Flow,
    /// On the channel's sum, after the deck fader — the historical position.
    #[default]
    Deck,
}

/// Where the cue send taps the channel, relative to the channel fader.
///
/// The tap is always a *point next to the fader* — never a free routing choice — because that is
/// what makes the two settings mean something. `Pre` (the default) is the historical behaviour:
/// the monitor hears the channel before the fader, so a track can be cued in with its fader down.
/// `Post` feeds the monitor the faded signal instead, so pulling the fader down silences the
/// headphones too — a deliberate choice for a scratch/throw workflow, and one the docs call out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CueTap {
    /// Immediately before the channel fader (the default, and what a cue means in a DJ mixer).
    #[default]
    Pre,
    /// Immediately after the channel fader, before the crossfader.
    Post,
}

/// How a crossfader position maps to per-side gain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossfaderCurve {
    /// Both sides at unity in the middle, each fading linearly to silence at its own end: the
    /// classic DJ crossfader. Sums to +6 dB for correlated material at centre, which is why it is a
    /// deliberate choice rather than a default nobody thinks about.
    #[default]
    Classic,
    /// `cos`/`sin` quarter-circles: equal perceived loudness through the travel, and unity power.
    ConstantPower,
    /// Both sides play together through almost all of the travel, dropping steeply at the ends — the
    /// curve a scratch cut wants. `cut_width` says how much of the travel the drop takes.
    Cut,
}

impl CrossfaderCurve {
    /// The gains of the two sides at pan `t ∈ 0..=1` (0 = hard left, 1 = hard right).
    ///
    /// One function for the crossfader and the cue/master blend, so the three curves mean the same
    /// thing in both places.
    pub fn gains(self, t: f32, cut_width: f32) -> (f32, f32) {
        let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.5 };
        match self {
            CrossfaderCurve::Classic => ((2.0 * (1.0 - t)).min(1.0), (2.0 * t).min(1.0)),
            CrossfaderCurve::ConstantPower => {
                let angle = t * std::f32::consts::FRAC_PI_2;
                (angle.cos(), angle.sin())
            }
            CrossfaderCurve::Cut => {
                let width = if cut_width.is_finite() {
                    cut_width.clamp(0.0, 0.5)
                } else {
                    0.0
                };
                if width <= 0.0 {
                    // A hard cut: the other side is gone at the very end, not before it.
                    (f32::from(t < 1.0), f32::from(t > 0.0))
                } else {
                    (((1.0 - t) / width).min(1.0), (t / width).min(1.0))
                }
            }
        }
    }
}

impl DeckSide {
    /// This side's gain for a crossfader position of `-1.0` (hard left) .. `1.0` (hard right).
    pub fn gain_at(self, position: f32, curve: CrossfaderCurve, cut_width: f32) -> f32 {
        if self == DeckSide::Center {
            return 1.0;
        }
        let t = (position.clamp(-1.0, 1.0) + 1.0) * 0.5;
        let (left, right) = curve.gains(t, cut_width);
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
    /// One chain per **flow**: the fixed tone prefix (`eq`, `filter`) followed by that flow's
    /// `flowfx` slots. The mixer builds one per stem from the same config; a caller that passes
    /// fewer gets chains padded in, so `process` can index every flow it renders.
    ///
    /// The *controls* are shared: an `fx` write to a prefix slot is broadcast to every flow's
    /// instance by the mixer, so a sweep keeps one knob and one trajectory.
    flow_chains: Vec<FxChain>,
    /// The post-merge chain: the knob bank then the pad bank. One instance, on the flow sum.
    post_fx: FxChain,
    /// The channel's own signal, rendered in place. Allocated once by [`Channel::new`].
    bus: Bus,
    /// One scratch bus per stream, summed into [`Channel::bus`] through that stream's inserts and
    /// fader. This is what lets a stem have its own level and its own effects.
    stream_buses: Vec<Bus>,
    /// The cue copy, taken just before the channel fader during [`Channel::process`].
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
    /// One **gate** per stream, the ``sourcefader'': `1.0` (audible) or `0.0` (exactly silent), driven
    /// by the stem mute/solo intent. It sits *before* the stream's inserts so muting a stem also
    /// mutes its effects' tail, and a reverb on a muted stem cannot leak back in.
    source_gate: Vec<Param>,
    /// The channel fader — one fader, after the post-merge chain.
    fader: Fader,
    crossfader: Fader,
    /// The channel trim: a static setting between the deck fader and the sum, `[`trim_amp`]`'s law.
    /// Kept apart from the faders because a fader is a performance control (attenuate only) while a
    /// trim is headroom you set once — including up to +6 dB.
    gain: Param,
    /// Cue send level: **linear 0.0 (off) ..= 1.0 (full)**, not a bipolar fader. A send knob reads
    /// as a proportion of the channel, and running 1.0 through a boost-capable law would make "full"
    /// mean well above unity — which is exactly how a cue bus ends up hot and a limiter fights the
    /// monitor path instead of the mix.
    cue_send: Param,
    side: DeckSide,
    cross: CrossfaderConfig,
    /// Which side of the fader the cue send taps.
    cue_tap: CueTap,
}

impl Channel {
    /// Builds a channel over `deck`. The chains come from the config layer, which is what decides
    /// that a `simple_dj()` channel has an EQ and a filter.
    /// `flow_fx` is what the mixer built: one chain per stem, from the config's template and its
    /// per-stem overrides. A caller may pass fewer (a test passing one); the rest are padded with
    /// empty chains, so every live stream always has a chain to address.
    ///
    /// `cross` is the shared crossfader, copied in: one physical control, but each channel applies
    /// it according to its own `side`.
    pub fn new(
        deck: Deck,
        cfg: &ChannelConfig,
        cross: CrossfaderConfig,
        mut flow_chains: Vec<FxChain>,
        post_fx: FxChain,
    ) -> Self {
        let frames = BLOCK_SIZE;
        let level = clamp_pos(cfg.flow_fader);
        while flow_chains.len() < Stem::COUNT {
            flow_chains.push(FxChain::new());
        }
        let mut channel = Self {
            deck,
            flow_chains,
            post_fx,
            bus: Bus::stereo(frames),
            stream_buses: vec![Bus::stereo(frames)],
            cue: Bus::stereo(frames),
            source_gate: vec![Param::new(1.0, GATE_TAU)],
            flow_fader: vec![Fader::new(level, FADER_TAU)],
            stem_level: std::array::from_fn(|_| AtomicU32::new(level.to_bits())),
            stem_mute: std::array::from_fn(|_| AtomicBool::new(false)),
            stem_solo: AtomicU8::new(0),
            fader: Fader::new(cfg.deck_fader, FADER_TAU),
            crossfader: Fader::new(cross.start, CROSSFADE_TAU),
            gain: Param::new(clamp_pos(cfg.gain), FADER_TAU),
            cue_send: Param::new(cfg.cue_send.clamp(0.0, 1.0), FADER_TAU),
            side: cfg.side,
            cross,
            cue_tap: cfg.cue,
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

    /// Hands the deck the engine's block duration, so its sync controller works in seconds rather
    /// than in "blocks at the default rate".
    pub fn set_block_seconds(&mut self, seconds: f64) {
        self.deck.set_block_seconds(seconds);
    }

    /// Hands the transport out, leaving an empty one behind. Used by a topology reload, which
    /// builds fresh channels but must not stop the music: the deck is not a config value, so it
    /// survives the rebuild that every *config* value goes through.
    pub fn take_deck(&mut self) -> Deck {
        std::mem::replace(&mut self.deck, Deck::empty())
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

    /// Which side of the fader the cue send taps.
    pub fn cue_tap(&self) -> CueTap {
        self.cue_tap
    }

    /// Slots in one flow's chain (the fixed prefix plus the `flowfx` bank).
    ///
    /// The mixer's flat deck index space is `[flow slots][post slots]`, and this is where the
    /// boundary between the two is.
    pub fn flow_len(&self) -> usize {
        self.flow_chains.first().map(FxChain::len).unwrap_or(0)
    }

    /// The post-merge chain (knob bank, then pad bank).
    pub fn post_fx(&self) -> &FxChain {
        &self.post_fx
    }

    pub fn post_fx_mut(&mut self) -> &mut FxChain {
        &mut self.post_fx
    }

    /// Sizes every chain's time-based buffers at the engine's rate.
    ///
    /// Called once by the mixer at construction, and again after a topology reload — the two moments
    /// a chain can appear at a rate it was not built for.
    pub fn prepare_fx(&mut self, sample_rate: u32) {
        for chain in &mut self.flow_chains {
            chain.prepare(sample_rate);
        }
        self.post_fx.prepare(sample_rate);
    }

    /// Fader positions (`-1.0 ..= 1.0`), for a UI mirror.
    /// `(flow, deck, crossfader, cue send, gain)`: the first three bipolar positions, then the send
    /// level and the trim.
    ///
    /// The flow fader is stream 0's; per-stem levels live in [`Channel::stem_status`].
    pub fn levels(&self) -> (f32, f32, f32, f32, f32) {
        (
            self.flow_fader[0].target(),
            self.fader.target(),
            self.crossfader.target(),
            self.cue_send.target(),
            self.gain.target(),
        )
    }

    /// The per-stem state: whether the audio is separated yet (and how much of it), plus level,
    /// mute and solo.
    pub fn stem_status(&self) -> StemStatus {
        // Streams are not the same as separated audio: a growing set is installed *before* its first
        // block arrives, and until then the deck plays the mix through the streams. So ask the source
        // how much of itself exists — it is the only thing that knows.
        let separated = match self.deck.sources().first() {
            Some(source) if self.deck.stream_count() > 1 => {
                let total = source.total_frames().max(1);
                (source.available_frames() as f64 / total as f64).clamp(0.0, 1.0) as f32
            }
            _ => 0.0,
        };
        let mut status = StemStatus {
            ready: self.deck.stream_count() > 1 && separated > 0.0,
            separated,
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

    /// Recomputes every stream's gate and fader from the intent.
    ///
    /// The one place level, mute and solo are combined, so they cannot disagree. The two halves are
    /// deliberately separate stages now: **audibility** is the source gate (`1.0` / exactly `0.0`,
    /// `fader_amp(-1.0)`'s silence rather than a −80 dB leak) and **level** is the flow fader, so an
    /// unmute restores the level the DJ had set rather than unity, and a solo overrides the others'
    /// mutes without touching their levels.
    fn refresh_stem_faders(&self) {
        let solo = self.stem_solo.load(Ordering::Relaxed);
        for i in 0..self.flow_fader.len().min(Stem::COUNT) {
            let audible = if solo != 0 {
                solo & (1 << i) != 0
            } else {
                !self.stem_mute[i].load(Ordering::Relaxed)
            };
            self.source_gate[i].set(if audible { 1.0 } else { 0.0 });
            let level = f32::from_bits(self.stem_level[i].load(Ordering::Relaxed));
            self.flow_fader[i].set(level);
        }
    }

    /// The channel fader's position, `-1.0` (silence) ..= `1.0` (unity). Where it acts is
    /// [`FaderPlace`].
    pub fn set_fader(&self, position: f32) {
        self.fader.set(clamp_pos(position));
    }

    /// The channel trim, `-1.0` (silence) .. `1.0` (`+6 dB`). 0.0 is unity.
    pub fn set_gain(&self, position: f32) {
        self.gain.set(clamp_pos(position));
    }

    /// The trim's position, for a UI mirror.
    pub fn gain(&self) -> f32 {
        self.gain.target()
    }

    /// Shared crossfader position: `-1.0` hard left, `0.0` both decks open, `1.0` hard right.
    pub fn set_crossfader(&self, position: f32) {
        self.crossfader.set(clamp_pos(position));
    }

    /// `0.0` (off) ..= `1.0` (full). Values outside that range are clamped, not wrapped.
    pub fn set_cue_send(&self, level: f32) {
        self.cue_send.set(if level.is_finite() { level.clamp(0.0, 1.0) } else { 0.0 });
    }

    /// Flips the cue send: off becomes full, anything audible becomes off. Used by the latch pad
    /// action; reading the engine's own target keeps it consistent with the `fader cuesend` path.
    pub fn toggle_cue_send(&self) {
        self.set_cue_send(if self.cue_send.target() > 0.5 { 0.0 } else { 1.0 });
    }

    /// The last block's cue copy. Only meaningful after [`Channel::process`].
    pub fn cue(&self) -> &Bus {
        &self.cue
    }

    /// Renders one block through the fixed chain, leaving the result in the channel's own bus.
    ///
    /// ```text
    /// per flow: gate → [crossfader at `source`] → flow fader → trim → eq → filter → flowfx… ─┐
    ///                                                                                      Σ │
    /// per deck: [crossfader at `flow`] → knobfx… → padfx…                                   │
    ///           → [cue tap `pre`] → fader → [cue tap `post`] → [crossfader at `deck`]
    /// ```
    ///
    /// `eq` and the sweep are part of the flow's prefix, so the per-flow inserts run *after* the
    /// tone controls and *before* the merge — which is the whole point of the order. The trim sits
    /// right after the flow fader (the plan's `flowfader → autogain → gain → eq`), so the prefix and
    /// the inserts see the trimmed signal; autogain will slot in between them.
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

        // The cue send and the crossfader are *advancing* smoothers: one step per block. Stepping
        // either once per stream would shorten its time constant by the stream count, so both are
        // taken here and passed in.
        let cue_gain = self.cue_send.next_block(ctx.block_frames, ctx.sample_rate);
        let cross_position = self.crossfader.next_position(ctx);
        let side_gain = self.side.gain_at(cross_position, self.cross.curve, self.cross.cut_width);
        let cross_at_source = self.cross.place == CrossfaderPlace::Source;
        let cross_at_flow = self.cross.place == CrossfaderPlace::Flow;

        let frames = ctx.block_frames;
        let live = streams.min(stream_buses.len());
        for stream in stream_buses[..live].iter_mut() {
            stream.ensure_frames(frames);
        }
        self.deck.render_streams(&mut stream_buses[..live]);

        // The trim advances once per block, not once per flow: stepping it per stream would shorten
        // its time constant by the stream count. It is applied per flow, though — the plan puts it
        // right after the flow fader (`flowfader → autogain → gain → eq …`), so the tone prefix and
        // the per-flow inserts see the trimmed signal. Autogain will slot in between the two.
        let trim = self.gain.next_block(ctx.block_frames, ctx.sample_rate);
        let trim_gain = if trim != 0.0 { trim_amp(trim) } else { 1.0 };

        for (stream, sb) in stream_buses[..live].iter_mut().enumerate() {
            // The source gate is the stem mute/solo decision: exactly 1.0 or 0.0, smoothed so a mute
            // is a fade rather than a step. Before the flow chain, so a muted stem's reverb tail goes
            // with it.
            let gate = self.source_gate[stream].next_block(ctx.block_frames, ctx.sample_rate);
            if gate != 1.0 {
                sb.scale(gate);
            }
            if cross_at_source {
                sb.scale(side_gain);
            }
            sb.scale(self.flow_fader[stream].next_amp(ctx));
            if trim != 0.0 {
                sb.scale(trim_gain);
            }
            // The tone prefix and the per-flow inserts run after the flow fader and the trim, before
            // the sum.
            if self.flow_chains[stream].any_active() {
                self.flow_chains[stream].process(sb, ctx);
            }
            bus.add_from(sb);
        }
        // At the flow position the crossfader sits on the flow sum, after the tone controls.
        if cross_at_flow {
            bus.scale(side_gain);
        }

        if self.post_fx.any_active() {
            self.post_fx.process(&mut bus, ctx);
        }

        // The cue taps one side of the channel fader: `pre` (the default) is what makes cueing a
        // track in with its fader down possible; `post` hands the monitor the faded signal. Either
        // way the crossfader stays outside the tap.
        if self.cue_tap == CueTap::Pre {
            self.take_cue_into(&bus, cue_gain);
        }
        bus.scale(self.fader.next_amp(ctx));
        if self.cue_tap == CueTap::Post {
            self.take_cue_into(&bus, cue_gain);
        }
        // The historical position, and the default: last, so a crossfade is a routing decision taken
        // after the tone controls and the levels rather than a level change that recolours them.
        if self.cross.place == CrossfaderPlace::Deck {
            bus.scale(side_gain);
        }

        self.stream_buses = stream_buses;
        self.bus = bus;
        self.deck.end_block();
        &mut self.bus
    }

    /// Grows the per-stream scratch, gates and faders to `streams`.
    ///
    /// The chains are already sized at construction ([`Channel::new`] pads to [`Stem::COUNT`]), so
    /// only the scratch buses and the per-stream gates/faders can be shorter than the live stream
    /// count.
    fn sync_streams(&mut self, streams: usize) {
        while self.flow_fader.len() < streams {
            self.source_gate.push(Param::new(1.0, GATE_TAU));
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
            SlotChain::Flow(stem) => self.flow_chains.get(stem.index()),
            SlotChain::Deck => Some(&self.post_fx),
        }
    }

    pub fn chain_mut(&mut self, chain: SlotChain) -> Option<&mut FxChain> {
        match chain {
            SlotChain::Flow(stem) => self.flow_chains.get_mut(stem.index()),
            SlotChain::Deck => Some(&mut self.post_fx),
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
/// A mute is a *gate*, not a fade: short enough to feel instant, long enough not to step.
const GATE_TAU: f32 = 0.005;
/// Default `[crossfader] cut_width`: the drop takes the outer 5% of the travel at each end.
pub const DEFAULT_CUT_WIDTH: f32 = 0.05;

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
    /// The crossfader is the default one, which is only ever a gain of 1.0 on a `Center` channel.
    fn channel_with(cfg: ChannelConfig, flow: Vec<FxChain>, deck_chain: FxChain) -> Channel {
        channel_cross(cfg, CrossfaderConfig::default(), flow, deck_chain)
    }

    fn channel_cross(
        cfg: ChannelConfig,
        cross: CrossfaderConfig,
        flow: Vec<FxChain>,
        deck_chain: FxChain,
    ) -> Channel {
        Channel::new(deck_at_zero(), &cfg, cross, flow, deck_chain)
    }

    /// A crossfader config, for the tests that care where or how it acts.
    fn cross(place: CrossfaderPlace, curve: CrossfaderCurve, start: f32) -> CrossfaderConfig {
        CrossfaderConfig { place, curve, start, ..Default::default() }
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
            CrossfaderConfig::default(),
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
            CrossfaderConfig::default(),
            vec![FxChain::new()],
            FxChain::new(),
        );
        channel.deck_mut().play();
        for _ in 0..50 {
            channel.process(&ctx());
        }
        // `fader_amp(-1.0)` is defined as exactly zero, so a closed fader is digital silence —
        // the point of the kill clamp, and worth pinning because a −80 dB "floor" would leak here.
        assert!(channel.peak() < 1e-6, "a closed fader leaked {}", channel.peak());
    }

    #[test]
    fn crossfader_curves_are_pan_laws_not_volumes() {
        let w = DEFAULT_CUT_WIDTH;
        // Hard left must silence the right-hand channel and leave the left one alone, for every
        // curve: that is what lets a crossfader isolate a deck without touching its fader.
        for curve in [
            CrossfaderCurve::Classic,
            CrossfaderCurve::ConstantPower,
            CrossfaderCurve::Cut,
        ] {
            let left = DeckSide::Left.gain_at(-1.0, curve, w);
            let right = DeckSide::Right.gain_at(-1.0, curve, w);
            assert!(left > 0.99 && right < 1e-6, "{curve:?} hard left gave {left}/{right}");
        }
        // `Center` is outside the crossfade entirely, whatever the curve.
        for curve in [
            CrossfaderCurve::Classic,
            CrossfaderCurve::ConstantPower,
            CrossfaderCurve::Cut,
        ] {
            assert_eq!(DeckSide::Center.gain_at(1.0, curve, w), 1.0, "{curve:?}");
        }

        // The three curves, at their defining points. `Classic` is flat (both sides at unity) across
        // the middle and linear into each end; `ConstantPower` is the quarter-circle, so its centre
        // is −3 dB per side; `Cut` keeps both sides up until the very ends.
        let gains = |t: f32, curve| {
            let p = t * 2.0 - 1.0;
            (
                DeckSide::Left.gain_at(p, curve, w),
                DeckSide::Right.gain_at(p, curve, w),
            )
        };
        assert_eq!(gains(0.5, CrossfaderCurve::Classic), (1.0, 1.0), "classic centre");
        assert_eq!(gains(0.25, CrossfaderCurve::Classic), (1.0, 0.5), "classic quarter");
        let (l, r) = gains(0.5, CrossfaderCurve::ConstantPower);
        assert!((l - r).abs() < 1e-6 && (l - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6, "constant power centre {l}/{r}");
        assert_eq!(gains(0.5, CrossfaderCurve::Cut), (1.0, 1.0), "cut plays both in the middle");
        let (l, r) = gains(0.02, CrossfaderCurve::Cut);
        assert!((l - 1.0).abs() < 1e-4 && (r - 0.4).abs() < 1e-3, "cut drops steeply at the end: {l}/{r}");
        assert_eq!(gains(0.0, CrossfaderCurve::Cut), (1.0, 0.0), "cut is silent past the end");
        // `cut_width = 0` degenerates to a hard cut: both sides stay up to the exact end, where the
        // other one drops instantly.
        assert_eq!(CrossfaderCurve::Cut.gains(0.5, 0.0), (1.0, 1.0));
        assert_eq!(CrossfaderCurve::Cut.gains(1.0, 0.0), (0.0, 1.0));
        assert_eq!(CrossfaderCurve::Cut.gains(0.0, 0.0), (1.0, 0.0));

        // Monotonic across the travel for both sides, every curve: a fader must not wobble.
        for curve in [
            CrossfaderCurve::Classic,
            CrossfaderCurve::ConstantPower,
            CrossfaderCurve::Cut,
        ] {
            let mut prev_l = 2.0;
            let mut prev_r = -1.0;
            for i in -10..=10 {
                let p = i as f32 / 10.0;
                let l = DeckSide::Left.gain_at(p, curve, w);
                let r = DeckSide::Right.gain_at(p, curve, w);
                assert!(l <= prev_l + 1e-6, "{curve:?} left gain rose moving right at {p}");
                assert!(r >= prev_r - 1e-6, "{curve:?} right gain fell moving right at {p}");
                (prev_l, prev_r) = (l, r);
            }
        }
    }

    #[test]
    fn a_crossfader_position_reaches_both_planes_of_a_stereo_signal() {
        let mut channel = channel_cross(
            ChannelConfig { side: DeckSide::Right, cue_send: 1.0, ..Default::default() },
            cross(CrossfaderPlace::Deck, CrossfaderCurve::Classic, -1.0),
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

    /// A non-linear insert makes the trim's *position* observable: the plan puts it right after the
    /// flow fader, so a squaring insert sees the trimmed signal (output ∝ gain², not gain).
    #[test]
    fn the_trim_runs_before_the_flow_chain() {
        struct Square;
        impl Fx for Square {
            fn process(&mut self, bus: &mut Bus, _ctx: &FxContext) {
                for sample in bus.l.iter_mut().chain(bus.r.iter_mut()) {
                    *sample = *sample * *sample;
                }
            }
        }
        let measure = |gain: f32| -> f32 {
            let mut channel = Channel::new(
                tone_deck(),
                &ChannelConfig {
                    cue_send: 0.0,
                    gain,
                    ..Default::default()
                },
                CrossfaderConfig::default(),
                vec![FxChain::from_parts([("square", Box::new(Square) as Box<dyn Fx>)])],
                FxChain::new(),
            );
            channel.deck_mut().play();
            for _ in 0..40 {
                channel.process(&ctx());
            }
            channel.peak()
        };
        let unity = measure(0.0);
        assert!(unity > 0.5, "reference: {unity}");
        // The bottom half of the trim law: -0.5 is a quarter of the amplitude.
        let trimmed = measure(-0.5);
        let ratio = trimmed / unity;
        assert!(
            (ratio - 0.0625).abs() < 0.01,
            "trim must be inside the flow chain (gain² = 0.0625), got ratio {ratio}"
        );
    }

    /// The cue tap sits after the flow chains *and* after the post-merge banks: a pre-fader cue is
    /// the channel as processed, not the raw sum. This is what makes cueing a track in with its
    /// tone controls and effects in place possible.
    #[test]
    fn a_pre_fader_cue_hears_the_flow_chain_and_the_post_banks() {
        let silent = || {
            FxChain::from_parts([("gain", Box::new(Gain::new(0.0)) as Box<dyn Fx>)])
        };
        let measure = |flow: FxChain, post: FxChain| -> (f32, f32) {
            let mut channel = Channel::new(
                tone_deck(),
                &ChannelConfig {
                    cue_send: 1.0,
                    ..Default::default()
                },
                CrossfaderConfig::default(),
                vec![flow],
                post,
            );
            channel.deck_mut().play();
            for _ in 0..40 {
                channel.process(&ctx());
            }
            (channel.peak(), channel.cue().peak())
        };
        // Reference: both empty, the cue is the channel.
        let (main, cue) = measure(FxChain::new(), FxChain::new());
        assert!(main > 0.5 && cue > 0.5, "reference: {main} / {cue}");
        // A flow slot that mutes is inside the tap.
        let (main, cue) = measure(silent(), FxChain::new());
        assert!(main < 1e-6 && cue < 1e-6, "flow mute: {main} / {cue}");
        // And so is a post-merge slot.
        let (main, cue) = measure(FxChain::new(), silent());
        assert!(main < 1e-6 && cue < 1e-6, "post mute: {main} / {cue}");
    }

    #[test]
    fn cue_send_of_zero_leaves_a_silent_take() {
        let mut channel = Channel::new(
            tone_deck(),
            &ChannelConfig { cue_send: -1.0, ..Default::default() },
            CrossfaderConfig::default(),
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
                gain: 0.0,
                cue_send: 0.0,
                ..Default::default()
            },
            CrossfaderConfig::default(),
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

    /// Every stream has a chain to address: the chains a caller passes are kept verbatim, and any
    /// stem they do not cover is padded with an empty one. (`MixerConfig::build_chains` is what
    /// turns the config's template and `stem_fx` overrides into the four chains the mixer passes.)
    #[test]
    fn a_channel_addresses_a_chain_for_every_stem() {
        let cfg = ChannelConfig::default();
        let passed = FxChain::from_parts([("eq", Box::new(Eq::new()) as Box<dyn Fx>)]);
        let channel = Channel::new(deck_at_zero(), &cfg, CrossfaderConfig::default(), vec![passed], FxChain::new());
        let kinds = |stem| -> Vec<&'static str> {
            channel
                .chain(SlotChain::Flow(stem))
                .expect("every stem has a chain")
                .slots()
                .iter()
                .map(|slot| slot.kind())
                .collect()
        };
        // What the caller passed is kept verbatim; the stems it did not cover are padded with empty
        // chains (the mixer passes all of them, so nothing is lost in production).
        assert_eq!(kinds(Stem::Drums), ["eq"]);
        assert_eq!(kinds(Stem::Bass), [] as [&str; 0]);
        assert_eq!(kinds(Stem::Other), [] as [&str; 0]);
        assert_eq!(kinds(Stem::Vocals), [] as [&str; 0]);
        // Each is its own instance, not one shared chain: a filter's state must never be shared
        // between stems.
        let (bass, other) = (
            channel.chain(SlotChain::Flow(Stem::Bass)).unwrap(),
            channel.chain(SlotChain::Flow(Stem::Other)).unwrap(),
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
        channel
            .chain(SlotChain::Flow(Stem::Drums))
            .unwrap()
            .slot(0)
            .unwrap()
            .set_enabled(false);
        channel.process(&ctx());
        assert_eq!(hits.load(Ordering::Relaxed), 0, "the mixer's guard is what keeps FX cheap");
        channel
            .chain(SlotChain::Flow(Stem::Drums))
            .unwrap()
            .slot(0)
            .unwrap()
            .set_enabled(true);
        channel.process(&ctx());
        assert_eq!(hits.load(Ordering::Relaxed), 1);
    }

    struct Counter(Arc<AtomicUsize>);
    impl Fx for Counter {
        fn process(&mut self, _bus: &mut Bus, _ctx: &FxContext) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records the highest peak it is handed, so a test can see *where* a stage sits relative to it
    /// (the gate, the crossfader) rather than only what the channel ends up sounding like.
    struct PeakProbe(Arc<AtomicU32>);
    impl PeakProbe {
        fn new() -> (Self, Arc<AtomicU32>) {
            let cell = Arc::new(AtomicU32::new(0));
            (Self(Arc::clone(&cell)), cell)
        }
    }
    impl Fx for PeakProbe {
        fn process(&mut self, bus: &mut Bus, _ctx: &FxContext) {
            // Peaks are non-negative, so `f32` bit patterns compare like their values.
            self.0.fetch_max(bus.peak().to_bits(), Ordering::Relaxed);
        }
    }

    fn probed_peak(cell: &Arc<AtomicU32>) -> f32 {
        f32::from_bits(cell.load(Ordering::Relaxed))
    }

    /// The source gate sits **before** the per-stream inserts, so a muted stem hands silence to its
    /// chain — the insert sees nothing, rather than being the thing that closes.
    #[test]
    fn a_muted_stem_hands_silence_to_its_inserts() {
        let mut channel = stem_channel();
        install_stems(&mut channel);
        let (probe, seen) = PeakProbe::new();
        let vocals = SlotChain::Flow(Stem::Vocals);
        channel
            .add_slot(vocals, FxSlot::new(Box::new(probe), "probe"))
            .unwrap();
        // The vocals tag is 3000, and the flow fader is at unity, so the probe sees ~3000.
        settled_left(&mut channel);
        assert!(probed_peak(&seen) > 1000.0, "the insert must see the live stream");

        channel
            .apply_stem(StemOp::Mute { stem: Stem::Vocals, on: true })
            .unwrap();
        // Let the gate land first: a mute is a 5 ms fade, not a step, so the first block after the
        // command still carries the tail of the ramp.
        settled_left(&mut channel);
        seen.store(0, Ordering::Relaxed);
        settled_left(&mut channel);
        assert_eq!(probed_peak(&seen), 0.0, "a muted stem must feed its inserts silence");
    }

    /// The gain stage is the one level that boosts, and it stops at +6 dB; the faders above it stay
    /// attenuation-only, so opening one past unity changes nothing.
    #[test]
    fn the_trim_boosts_to_six_db_and_the_faders_do_not() {
        let settled_peak = |mut channel: Channel| -> f32 {
            channel.deck_mut().play();
            for _ in 0..40 {
                channel.process(&ctx());
            }
            channel.process(&ctx()).peak()
        };
        let unity_peak = settled_peak(channel_with(unity(), vec![FxChain::new()], FxChain::new()));

        // A fader at the top of its travel is unity: a fader that could boost could clip the mix.
        let opened = settled_peak(channel_with(
            ChannelConfig { flow_fader: 1.0, cue_send: 1.0, ..Default::default() },
            vec![FxChain::new()],
            FxChain::new(),
        ));
        assert!(
            (opened - unity_peak).abs() < unity_peak * 0.02,
            "a fader above unity must not boost: {opened} vs {unity_peak}"
        );

        // The trim does, up to +6 dB (\u00d7 2.0), and no further.
        let boosted = settled_peak(channel_with(
            ChannelConfig { gain: 1.0, cue_send: 1.0, ..Default::default() },
            vec![FxChain::new()],
            FxChain::new(),
        ));
        let expected = unity_peak * 10f32.powf(6.0 / 20.0);
        assert!(
            (boosted - expected).abs() < expected * 0.05,
            "the trim should be +6 dB: {boosted} vs {expected}"
        );

        // And a trim at the bottom is exactly silence.
        let killed = settled_peak(channel_with(
            ChannelConfig { gain: -1.0, cue_send: 1.0, ..Default::default() },
            vec![FxChain::new()],
            FxChain::new(),
        ));
        assert!(killed < 1e-6, "a closed trim leaked {killed}");
    }

    /// The crossfader acts at the place the config names, and the cue tap sits just before the
    /// fader: a crossfader downstream of that point is not in the monitor, one upstream is.
    #[test]
    fn the_crossfader_acts_at_its_place_and_the_cue_tap_ignores_it_when_it_is_downstream() {
        let run = |place: CrossfaderPlace| -> Channel {
            let mut channel = channel_cross(
                ChannelConfig {
                    side: DeckSide::Right,
                    cue_send: 1.0,
                    ..Default::default()
                },
                cross(place, CrossfaderCurve::Classic, -1.0),
                vec![FxChain::new()],
                FxChain::new(),
            );
            channel.deck_mut().play();
            for _ in 0..20 {
                channel.process(&ctx());
            }
            channel
        };
        for (place, cue_ignores) in [
            (CrossfaderPlace::Source, false),
            (CrossfaderPlace::Flow, false),
            (CrossfaderPlace::Deck, true),
        ] {
            let channel = run(place);
            assert!(
                channel.peak() < 1e-4,
                "{place:?}: a right channel at hard left still sounded"
            );
            assert_eq!(
                channel.cue().peak() > 0.0,
                cue_ignores,
                "{place:?}: the cue tap is just before the fader"
            );
        }
    }

    #[test]
    fn loading_a_new_deck_keeps_the_mixer_controls() {
        let mut channel = channel_with(unity(), vec![FxChain::new()], FxChain::from_parts([("eq", Box::new(Eq::new()) as Box<dyn Fx>)]));
        channel.set_flow_fader(-0.25);
        channel.post_fx_mut().slot(0).unwrap().set_param("low", 0.5).unwrap();
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
        channel.set_fader(-99.0);
        assert_eq!(channel.levels(), (0.0, -1.0, 1.0, 1.0, 0.0));
        channel.deck_mut().play();
        channel.process(&ctx());
        assert!(channel.peak().is_finite());
    }
    #[test]
    fn the_cue_send_latch_flips_between_off_and_full() {
        let channel = channel_with(
            ChannelConfig { cue_send: 1.0, ..Default::default() },
            vec![FxChain::new()],
            FxChain::new(),
        );
        assert!(channel.cue_send.target() > 0.5, "the channel starts audible in the cue");
        channel.toggle_cue_send();
        assert!(channel.cue_send.target() < 0.5, "the first press mutes it");
        channel.toggle_cue_send();
        assert!(channel.cue_send.target() > 0.5, "the next press restores it");
    }

}
