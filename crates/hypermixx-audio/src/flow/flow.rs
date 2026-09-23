//! Flow: one playback unit inside a deck, wrapping a [`PitchShiftEngine`].

use crossbeam_channel::Sender;
use std::sync::Arc;

use timestretch::engine::EngineProfile;

use super::PitchShiftEngine;
use crate::deck::{LoopRange, LoopRangeCell, LoopSource};
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

/// A bounded (or open-ended) playback of a [`Source`].
pub struct Flow {
    pub id: u64,
    pub state: FlowState,
    pub start_frame: u64,
    /// `None` plays until the end of the source.
    pub end_frame: Option<u64>,
    pitchshift: PitchShiftEngine,
    ready_tx: Option<Sender<u64>>,
    /// The loop range this flow reads through. Lives *on the flow*: a range edit is one atomic
    /// store (no flow change), and when a switch replaces the flow the range dies with it — the
    /// deck never clears a range in place.
    loop_cell: Arc<LoopRangeCell>,
}

impl Flow {
    pub fn new(
        id: u64,
        source: Arc<dyn Source>,
        start_frame: u64,
        end_frame: Option<u64>,
        ready_tx: Sender<u64>,
    ) -> Self {
        Self::new_with(id, source, start_frame, end_frame, ready_tx, 1.0, EngineProfile::Tape)
    }

    /// A flow born with a tempo and profile. Every flow the deck spawns goes through here, so a
    /// rate the DJ set survives a jump instead of silently reverting to unity on the new flow.
    pub fn new_with(
        id: u64,
        source: Arc<dyn Source>,
        start_frame: u64,
        end_frame: Option<u64>,
        ready_tx: Sender<u64>,
        ratio: f32,
        profile: EngineProfile,
    ) -> Self {
        let loop_cell = Arc::new(LoopRangeCell::new());
        let wrapped = LoopSource::new(source, Arc::clone(&loop_cell));
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

    /// Warms the time-stretch engine up to `start_frame`, *including* the warm-start priming:
    /// when this returns the flow's stages are converged and its clock sits exactly on
    /// `start_frame`, so activation never plays priming silence. Idempotent, so the deck can
    /// safely re-apply it to its own copy when switching.
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

    /// Rebuilds the engine with a different profile, preserving position and ratio.
    pub fn set_profile(&mut self, profile: timestretch::engine::EngineProfile) {
        let source = self.pitchshift.source_ref();
        let ratio = self.pitchshift.ratio();
        let position = self.pitchshift.current_frame();
        self.pitchshift =
            PitchShiftEngine::with_profile(source.clone(), ratio, profile, Arc::clone(&self.loop_cell))
                .unwrap_or_else(|_| {
                    PitchShiftEngine::new(source, ratio, Arc::clone(&self.loop_cell))
                });
        self.pitchshift.prepare_jump(position);
    }

    /// The loop range this flow reads through, or `None` (identity mapping).
    pub fn loop_range(&self) -> Option<LoopRange> {
        self.loop_cell.load()
    }

    /// Replaces the loop range in one store — the in-loop edit path: the feed's mapping re-reads
    /// it on the next block, no flow change and no re-warm.
    pub fn set_loop_range(&self, range: Option<LoopRange>) {
        self.loop_cell.store(range);
    }

    /// Fills `output` with the next block from the time-stretch engine.
    /// Always fills the entire buffer (silence when inactive or past the end).
    /// Returns the number of frames written (= `output.len() / CHANNELS`).
    pub fn process_block(&mut self, output: &mut [f32]) -> usize {
        let capacity = output.len() / CHANNELS;
        if self.state != FlowState::Active {
            output[..capacity * CHANNELS].fill(0.0);
            return capacity;
        }
        self.render(output)
    }

    /// Renders one block unconditionally, whatever lifecycle state this flow is in.
    ///
    /// The deck uses this for an armed LoopFlow: a warmed (`Ready`) flow that must keep running
    /// in lockstep with the active flow so its engine stays live and its clock stays aligned —
    /// the caller discards the samples. Never called on a `Preparing` flow (the engine has no
    /// seek protocol applied yet), so the state gate on [`process_block`](Self::process_block)
    /// remains the safety net for everything else.
    pub fn render(&mut self, output: &mut [f32]) -> usize {
        let capacity = output.len() / CHANNELS;
        let budget = match self.end_frame {
            Some(end) => capacity.min(end.saturating_sub(self.virtual_frame()) as usize),
            None => capacity,
        };
        if budget == 0 {
            output[..capacity * CHANNELS].fill(0.0);
            return capacity;
        }
        self.pitchshift
            .process_block(&mut output[..budget * CHANNELS]);
        output[budget * CHANNELS..capacity * CHANNELS].fill(0.0);
        capacity
    }

    /// The virtual playhead: the engine's output clock, advanced monotonically and never folded
    /// by a loop. This is the slip clock — what a loop exit and the jump-latency compensation
    /// speak.
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
        let limit = self
            .end_frame
            .unwrap_or_else(|| self.pitchshift.total_frames());
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

    fn flow(n_frames: u64, start: u64, end: Option<u64>) -> (Flow, Receiver<u64>) {
        let source = Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n_frames as usize)
                .flat_map(|i| [i as f32, i as f32])
                .collect(),
            total_frames: n_frames,
            sample_rate: crate::SAMPLE_RATE,
            channels: CHANNELS,
        }));
        let (tx, rx) = unbounded();
        (Flow::new(7, source, start, end, tx), rx)
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
}
