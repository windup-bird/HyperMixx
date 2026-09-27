//! Flow: one playback unit inside a deck, wrapping a [`PitchShiftEngine`].
//!
//! A flow carries **one engine over N streams** (1 for a plain track, up to
//! [`MAX_STREAMS`](crate::flow::pitchshift::MAX_STREAMS) for stems). There is therefore exactly one
//! transport, one clock and one loop mapping per flow, no matter how many streams it renders — see
//! [`PitchShiftEngine`] for why that is a correctness requirement and not a preference.

use crossbeam_channel::Sender;
use std::sync::Arc;

use timestretch::engine::EngineProfile;

use super::PitchShiftEngine;
use crate::deck::{LoopRange, LoopRangeCell, LoopSource};
use crate::mixer::Bus;
use crate::CHANNELS;
use hypermixx_core::Source;

/// Lifecycle of a flow. A deck always keeps exactly one `Active` flow; jumps spawn a
/// `Preparing` flow that becomes `Ready` on the warm-up thread and `Active` on the next block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowState {
    Preparing,
    Ready,
    Active,
    Retired,
}

/// A bounded (or open-ended) playback of one or more [`Source`]s, rendered as parallel streams.
pub struct Flow {
    pub id: u64,
    pub state: FlowState,
    pub start_frame: u64,
    /// `None` plays until the end of the source.
    pub end_frame: Option<u64>,
    /// The one engine every stream of this flow runs on. Its channel count is `2 × streams`.
    pitchshift: PitchShiftEngine,
    ready_tx: Option<Sender<u64>>,
    /// The loop range this flow reads through. Lives *on the flow*: a range edit is one atomic
    /// store (no flow change, and every stream sees it), and when a switch replaces the flow the
    /// range dies with it — the deck never clears a range in place.
    loop_cell: Arc<LoopRangeCell>,
}

impl Flow {
    /// A single-stream flow at unity tempo with the Tape profile.
    pub fn new(
        id: u64,
        source: Arc<dyn Source>,
        start_frame: u64,
        end_frame: Option<u64>,
        ready_tx: Sender<u64>,
    ) -> Self {
        Self::new_with(id, vec![source], start_frame, end_frame, ready_tx, 1.0, EngineProfile::Tape)
    }

    /// A flow born with a tempo, a profile and `sources.len()` streams.
    ///
    /// Every flow the deck spawns goes through here, so a rate the DJ set survives a jump instead
    /// of silently reverting to unity on the new flow.
    pub fn new_with(
        id: u64,
        sources: Vec<Arc<dyn Source>>,
        start_frame: u64,
        end_frame: Option<u64>,
        ready_tx: Sender<u64>,
        ratio: f32,
        profile: EngineProfile,
    ) -> Self {
        let loop_cell = Arc::new(LoopRangeCell::new());
        // Every stream reads through its own `LoopSource`, all driven by the one cell: the loop is
        // a deck-level decision, so it cannot be per-stream.
        let wrapped: Vec<Arc<dyn Source>> = sources
            .into_iter()
            .map(|source| LoopSource::new(source, Arc::clone(&loop_cell)) as Arc<dyn Source>)
            .collect();
        Self {
            id,
            state: FlowState::Preparing,
            start_frame,
            end_frame,
            pitchshift: PitchShiftEngine::with_profile(
                wrapped,
                ratio,
                profile,
                Arc::clone(&loop_cell),
            )
            .expect("timestretch engine config is statically valid"),
            ready_tx: Some(ready_tx),
            loop_cell,
        }
    }

    /// Streams this flow renders. `1` for a plain track, `4` for stems.
    pub fn stream_count(&self) -> usize {
        self.pitchshift.stream_count()
    }

    /// Warms the time-stretch engine up to `start_frame`, *including* the warm-start priming:
    /// when this returns the flow's stages are converged and its clock sits exactly on
    /// `start_frame`, so activation never plays priming silence. Idempotent, so the deck can
    /// safely re-apply it to its own copy when switching.
    ///
    /// **Must run after [`set_loop_range`](Self::set_loop_range).** The feed pre-fills the engine's
    /// ring *through* the mapping, so a range stored afterwards cannot retroactively fix what was
    /// already pushed: the first ~25 ms of audio would be the unmapped track. The deck gets this
    /// order for free (`make_flow` stamps the range, then the warm-up thread prepares), but a
    /// caller constructing a flow by hand has to respect it.
    pub fn prepare(&mut self) {
        self.pitchshift.prepare_jump(self.start_frame);
        self.pitchshift.drain_priming();
    }

    /// Repositions the playhead and settles there — the path every flow switch goes through
    /// (jump, beatjump, loop exit).
    ///
    /// Delegates to `prepare_jump` (the timestretch engine always needs its seek protocol for
    /// correctness) and then **drains the warm-start priming here, discarded**: the switch's very
    /// first block is already converged audio. A breath between two flows would read as a gap in
    /// the mix, and every transition this deck makes is required to be seamless.
    pub fn reset_to(&mut self, target_frame: u64) {
        self.pitchshift.reset_to(target_frame);
        self.pitchshift.drain_priming();
    }

    /// Sets the tempo rate on the underlying engine.
    pub fn set_ratio(&mut self, ratio: f32) {
        self.pitchshift.set_ratio(ratio);
    }

    /// Rebuilds the engine with a different profile, preserving position, ratio and every stream.
    pub fn set_profile(&mut self, profile: timestretch::engine::EngineProfile) {
        let sources = self.pitchshift.sources_ref();
        let fallback = sources.clone();
        let ratio = self.pitchshift.ratio();
        let position = self.pitchshift.current_frame();
        self.pitchshift = PitchShiftEngine::with_profile(
            sources,
            ratio,
            profile,
            Arc::clone(&self.loop_cell),
        )
        .unwrap_or_else(|_| {
            PitchShiftEngine::new(fallback, ratio, Arc::clone(&self.loop_cell))
        });
        self.pitchshift.prepare_jump(position);
    }

    /// The loop range this flow reads through, or `None` (identity mapping).
    pub fn loop_range(&self) -> Option<LoopRange> {
        self.loop_cell.load()
    }

    /// Replaces the loop range in one store — the in-loop edit path: the feed's mapping re-reads
    /// it on the next block, no flow change and no re-warm. Every stream folds on the same store.
    pub fn set_loop_range(&self, range: Option<LoopRange>) {
        self.loop_cell.store(range);
    }

    /// The frames this flow still has to give, bounded by `capacity`.
    fn budget(&self, capacity: usize) -> usize {
        match self.end_frame {
            Some(end) => capacity.min(end.saturating_sub(self.virtual_frame()) as usize),
            None => capacity,
        }
    }

    /// Fills `output` with the next block from the time-stretch engine.
    /// Always fills the entire buffer (silence when inactive or past the end).
    /// Returns the number of frames written (= `output.len() / CHANNELS`).
    ///
    /// With more than one stream the result is their **sum**: right for the deck's own
    /// single-buffer view, wrong for a mixer. [`process_streams`](Self::process_streams) is what
    /// the channel uses, so each stem can take its own inserts and level.
    pub fn process_block(&mut self, output: &mut [f32]) -> usize {
        let capacity = output.len() / CHANNELS;
        if self.state != FlowState::Active {
            output[..capacity * CHANNELS].fill(0.0);
            return capacity;
        }
        self.render(output)
    }

    /// Fills one [`Bus`] per stream and returns the block's frame count.
    ///
    /// `outs.len()` must equal [`stream_count`](Self::stream_count). The tail of each bus beyond
    /// the flow's remaining budget is zeroed, so a bus reused across blocks cannot leak the
    /// previous block into a short one. Silence (inactive or past the end) clears every bus.
    pub fn process_streams(&mut self, outs: &mut [Bus]) -> usize {
        let frames = outs.iter().map(Bus::frames).min().unwrap_or(0);
        let budget = if self.state == FlowState::Active { self.budget(frames) } else { 0 };
        if budget == 0 {
            for bus in outs.iter_mut() {
                bus.clear();
            }
            return frames;
        }
        if budget < frames {
            for bus in outs.iter_mut() {
                bus.l[budget..frames].fill(0.0);
                bus.r[budget..frames].fill(0.0);
            }
        }
        self.pitchshift.process_streams(outs, budget);
        frames
    }

    /// Renders one block unconditionally into a caller-supplied interleaved buffer, whatever
    /// lifecycle state this flow is in. With N streams the buffer receives their sum.
    ///
    /// The state gate on [`process_block`](Self::process_block) stays the safety net for callers
    /// that must not sample an unprepared flow; this variant exists for callers that have already
    /// decided to drive the engine regardless (the lockstep driver uses
    /// [`render_discard`](Self::render_discard) instead, so it does not need a block-sized buffer
    /// for a flow whose stream count it does not know).
    pub fn render(&mut self, output: &mut [f32]) -> usize {
        let capacity = output.len() / CHANNELS;
        let budget = self.budget(capacity);
        if budget == 0 {
            output[..capacity * CHANNELS].fill(0.0);
            return capacity;
        }
        self.pitchshift.process_block(&mut output[..budget * CHANNELS]);
        output[budget * CHANNELS..capacity * CHANNELS].fill(0.0);
        capacity
    }

    /// [`render`](Self::render) without a caller-supplied buffer: renders `frames` and throws the
    /// audio away. The lockstep driver uses this so it never has to allocate a block sized for a
    /// flow whose stream count it does not know.
    pub fn render_discard(&mut self, frames: usize) -> usize {
        let budget = self.budget(frames);
        if budget == 0 {
            return frames;
        }
        self.pitchshift.render_discard(budget);
        frames
    }

    /// The virtual playhead: the engine's output clock, advanced monotonically and never folded
    /// by a loop. This is the slip clock — what a loop exit and the jump-latency compensation
    /// speak. Every stream shares it.
    pub fn virtual_frame(&self) -> u64 {
        self.pitchshift.current_frame()
    }

    /// What the listener hears: `virtual` folded through this flow's loop range (identity when
    /// no loop is set). For an un-looped flow this equals [`virtual_frame`](Self::virtual_frame).
    pub fn actual_frame(&self) -> u64 {
        self.loop_cell.map(self.virtual_frame())
    }

    /// True once the virtual clock reached `end_frame`, or the end of the source. A looped flow
    /// reports an unbounded source, so it never "ends" — its clock is meant to run forever.
    pub fn reached_end(&self) -> bool {
        let limit = self.end_frame.unwrap_or_else(|| self.pitchshift.total_frames());
        self.virtual_frame() >= limit
    }

    /// Marks the flow ready for activation and reports its id to the deck.
    pub fn mark_ready(&mut self) {
        self.state = FlowState::Ready;
        if let Some(tx) = self.ready_tx.take() {
            let _ = tx.send(self.id);
        }
    }

    /// The tempo rate this flow was built with.
    pub fn ratio(&self) -> f32 {
        self.pitchshift.ratio()
    }

    /// The time-stretch profile this flow runs.
    pub fn profile(&self) -> EngineProfile {
        self.pitchshift.profile()
    }

    pub fn activate(&mut self) {
        self.state = FlowState::Active;
    }

    pub fn retire(&mut self) {
        self.state = FlowState::Retired;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{unbounded, Receiver};
    use hypermixx_media::{DecodedAudio, PcmPool};

    fn source(n_frames: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n_frames as usize)
                .flat_map(|i| [i as f32, i as f32])
                .collect(),
            total_frames: n_frames,
            sample_rate: crate::SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    /// A source whose samples encode `(frame index, stream tag)`, so a channel mix-up is visible
    /// as a value mismatch rather than a level difference.
    fn tagged_source(n_frames: u64, tag: f32) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n_frames as usize)
                .flat_map(|i| [i as f32 + tag, -(i as f32) - tag])
                .collect(),
            total_frames: n_frames,
            sample_rate: crate::SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    fn flow(n_frames: u64, start: u64, end: Option<u64>) -> (Flow, Receiver<u64>) {
        let (tx, rx) = unbounded();
        (Flow::new(7, source(n_frames), start, end, tx), rx)
    }

    #[test]
    fn inactive_flow_outputs_silence() {
        let (mut f, _) = flow(1000, 0, None);
        let mut out = vec![1.0f32; 4 * CHANNELS];
        assert_eq!(f.process_block(&mut out), 4);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn mark_ready_notifies_and_activate_starts_sampling() {
        let (mut f, rx) = flow(1000, 100, None);
        f.prepare();
        f.mark_ready();
        assert_eq!(rx.try_recv(), Ok(7));
        assert_eq!(f.state, FlowState::Ready);
        f.activate();
        let mut out = vec![0.0f32; 4 * CHANNELS];
        assert_eq!(f.process_block(&mut out), 4);
        assert_eq!(out[0], 100.0);
    }

    #[test]
    fn end_frame_stops_playback() {
        let (mut f, _) = flow(1000, 0, Some(6));
        f.prepare();
        f.activate();
        let mut out = vec![0.0f32; 8 * CHANNELS];
        assert_eq!(f.process_block(&mut out), 8);
        assert!(f.reached_end());
        // After end, output is silence but process_block still fills the buffer.
        assert_eq!(f.process_block(&mut out), 8);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn reaches_end_of_source() {
        let (mut f, _) = flow(10, 0, None);
        f.prepare();
        f.activate();
        let mut out = vec![0.0f32; 32 * CHANNELS];
        f.process_block(&mut out);
        assert!(f.reached_end());
    }
    /// The interleave / de-interleave round trip is lossless: four streams through one 8-channel
    /// engine must match four independent stereo engines sample for sample at the same rate.
    ///
    /// This is the invariant the whole design rests on. With keylock engaged the two *would* differ
    /// — each engine would pick its own SOLA splice landing — and that difference is precisely why
    /// the single engine is the correct one; at unity there is no correction to diverge on, so the
    /// audio path has to be identical.
    #[test]
    fn one_eight_channel_engine_matches_four_stereo_engines() {
        let tags = [0.0f32, 1000.0, 2000.0, 3000.0];
        let frames = crate::BLOCK_SIZE;

        // One engine carrying all four streams.
        let (tx, _rx) = unbounded();
        let mut joint = Flow::new_with(
            1,
            tags.iter().map(|tag| tagged_source(50_000, *tag)).collect(),
            0,
            None,
            tx,
            1.0,
            EngineProfile::Tape,
        );
        joint.prepare();
        joint.mark_ready();
        joint.activate();
        let mut joint_buses: Vec<Bus> = (0..4).map(|_| Bus::stereo(frames)).collect();
        joint.process_streams(&mut joint_buses);

        // One engine per stream.
        let mut singles = Vec::new();
        for tag in tags {
            let (tx, _rx) = unbounded();
            let mut single = Flow::new_with(
                1,
                vec![tagged_source(50_000, tag)],
                0,
                None,
                tx,
                1.0,
                EngineProfile::Tape,
            );
            single.prepare();
            single.mark_ready();
            single.activate();
            singles.push(single);
        }

        for (stream, single) in singles.iter_mut().enumerate() {
            let mut bus = Bus::stereo(frames);
            single.process_streams(std::slice::from_mut(&mut bus));
            for i in 0..frames {
                assert_eq!(
                    joint_buses[stream].l[i], bus.l[i],
                    "stream {stream} frame {i} left: joint != separate"
                );
                assert_eq!(
                    joint_buses[stream].r[i], bus.r[i],
                    "stream {stream} frame {i} right: joint != separate"
                );
            }
        }
    }

    /// Four streams through one engine: each bus must carry *its own* source, sample for sample,
    /// and there must be one clock for all of them.
    ///
    /// Runs at `BLOCK_SIZE` because that is the engine's contract: `feed` tops the ring up to a
    /// `BLOCK_SIZE` callback's demand, so rendering a shorter block is not sample-aligned. That is
    /// pre-existing and identical on the single-stream path (`Deck::process_block` shows it too) —
    /// the mixer always asks for `block_frames` = `BLOCK_SIZE`.
    #[test]
    fn four_streams_render_their_own_source_into_their_own_bus() {
        let (tx, _rx) = unbounded();
        let tags = [0.0f32, 1000.0, 2000.0, 3000.0];
        let sources: Vec<Arc<dyn Source>> = tags
            .iter()
            .map(|tag| tagged_source(10_000, *tag))
            .collect();
        let mut flow = Flow::new_with(1, sources, 0, None, tx, 1.0, EngineProfile::Tape);
        assert_eq!(flow.stream_count(), 4);
        flow.prepare();
        flow.mark_ready();
        flow.activate();

        let frames = crate::BLOCK_SIZE;
        let mut buses: Vec<Bus> = (0..4).map(|_| Bus::stereo(frames)).collect();
        assert_eq!(flow.process_streams(&mut buses), frames);

        for (stream, bus) in buses.iter().enumerate() {
            let tag = tags[stream];
            for i in 0..frames {
                // The unity path is a resampling chain whose coefficient product lands ~1e-17 off
                // an exact copy, so a tolerance is honest here; a stream mix-up is off by 1000.
                let want = i as f32 + tag;
                assert!(
                    (bus.l[i] - want).abs() < 1e-3,
                    "stream {stream} left frame {i}: {} != {want}",
                    bus.l[i]
                );
                assert!(
                    (bus.r[i] + want).abs() < 1e-3,
                    "stream {stream} right frame {i}: {} != {}",
                    bus.r[i],
                    -want
                );
            }
        }
        // One clock for every stream.
        assert_eq!(flow.virtual_frame(), frames as u64);
    }

    /// The mixed view (`process_block`) of a multi-stream flow is the sum of the streams, which is
    /// what the deck's compatibility entry points promise.
    #[test]
    fn process_block_sums_the_streams() {
        let (tx, _rx) = unbounded();
        let tags = [0.0f32, 1000.0, 2000.0, 3000.0];
        let sources: Vec<Arc<dyn Source>> =
            tags.iter().map(|tag| tagged_source(10_000, *tag)).collect();
        let mut flow = Flow::new_with(1, sources, 0, None, tx, 1.0, EngineProfile::Tape);
        flow.prepare();
        flow.mark_ready();
        flow.activate();

        let frames = crate::BLOCK_SIZE;
        let mut out = vec![0.0f32; frames * CHANNELS];
        flow.process_block(&mut out);
        let expected_tag: f32 = tags.iter().sum();
        for i in 0..frames {
            let want = i as f32 * 4.0 + expected_tag;
            assert!(
                (out[i * CHANNELS] - want).abs() < 1e-2,
                "frame {i}: {} != {want}",
                out[i * CHANNELS]
            );
        }
    }

    /// A loop is a deck-level decision: one store must fold every stream, on the same frames.
    ///
    /// This is the property four separate engines could not provide — each would pick its own SOLA
    /// splice landing and the streams would drift apart — so it is asserted directly: at every
    /// frame the buses must be reading the *same* source frame, straight through the wrap.
    #[test]
    fn one_loop_store_folds_every_stream() {
        let (tx, _rx) = unbounded();
        let tags = [0.0f32, 1000.0, 2000.0, 3000.0];
        let sources: Vec<Arc<dyn Source>> = tags
            .iter()
            .map(|tag| tagged_source(100_000, *tag))
            .collect();
        let mut flow = Flow::new_with(1, sources, 0, None, tx, 1.0, EngineProfile::Tape);
        // The range has to be in place *before* the ring is pre-filled: `prepare` feeds through the
        // mapping. That is also the order the deck uses — `make_flow` stamps the range and the
        // warm-up thread prepares afterwards.
        flow.set_loop_range(Some(LoopRange::new(10, 74)));
        flow.prepare();
        flow.mark_ready();
        flow.activate();

        let frames = crate::BLOCK_SIZE;
        let mut buses: Vec<Bus> = (0..4).map(|_| Bus::stereo(frames)).collect();
        // One block spans the wrap twice: 0..74 plays through, then [10, 74) repeats every 64.
        flow.process_streams(&mut buses);
        assert!((buses[0].l[73] - 73.0).abs() < 1e-3, "last frame before the wrap");
        assert!((buses[0].l[74] - 10.0).abs() < 1e-3, "first frame after the wrap");
        assert!((buses[0].l[137] - 73.0).abs() < 1e-3, "second lap's last frame");
        assert!((buses[0].l[138] - 10.0).abs() < 1e-3, "second lap's first frame");

        for i in 0..frames {
            let base = buses[0].l[i];
            for (stream, bus) in buses.iter().enumerate().skip(1) {
                assert!(
                    (bus.l[i] - tags[stream] - base).abs() < 1e-2,
                    "stream {stream} frame {i} is on source frame {} while stream 0 is on {base}",
                    bus.l[i] - tags[stream]
                );
            }
        }
        assert_eq!(flow.virtual_frame(), frames as u64);
        // The audible clock is folded; the slip clock is not.
        assert_eq!(flow.actual_frame(), 10 + (frames as u64 - 10) % 64);
    }
}
