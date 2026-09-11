//! Flow: one playback unit inside a deck, wrapping a [`PitchShiftEngine`].

use crossbeam_channel::Sender;
use std::sync::Arc;

use super::PitchShiftEngine;
use crate::source::Source;
use crate::CHANNELS;

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
}

impl Flow {
    pub fn new(
        id: u64,
        source: Arc<dyn Source>,
        start_frame: u64,
        end_frame: Option<u64>,
        ready_tx: Sender<u64>,
    ) -> Self {
        Self {
            id,
            state: FlowState::Preparing,
            start_frame,
            end_frame,
            pitchshift: PitchShiftEngine::new(source, 1.0),
            ready_tx: Some(ready_tx),
        }
    }

    /// Warms the time-stretch engine up to `start_frame`. Idempotent, so the deck can safely
    /// re-apply it to its own copy when switching.
    pub fn prepare(&mut self) {
        self.pitchshift.prepare_jump(self.start_frame);
    }

    /// Repositions the playhead without warming up again — the light path used by loops and by the
    /// deck's jump-latency compensation.
    pub fn reset_to(&mut self, target_frame: u64) {
        self.pitchshift.reset_to(target_frame);
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
        self.pitchshift = PitchShiftEngine::with_profile(source.clone(), ratio, profile)
            .unwrap_or_else(|_| PitchShiftEngine::new(source, ratio));
        self.pitchshift.prepare_jump(position);
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
        let budget = match self.end_frame {
            Some(end) => capacity.min(end.saturating_sub(self.current_frame()) as usize),
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

    /// Current playhead position, in frames.
    pub fn current_frame(&self) -> u64 {
        self.pitchshift.current_frame()
    }

    /// True once the playhead reached `end_frame`, or the end of the source.
    pub fn reached_end(&self) -> bool {
        let limit = self
            .end_frame
            .unwrap_or_else(|| self.pitchshift.total_frames());
        self.current_frame() >= limit
    }

    /// Marks the flow ready for activation and reports its id to the deck.
    pub fn mark_ready(&mut self) {
        self.state = FlowState::Ready;
        if let Some(tx) = self.ready_tx.take() {
            let _ = tx.send(self.id);
        }
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
    use crate::source::{DecodedAudio, PcmPool};
    use crossbeam_channel::{unbounded, Receiver};

    fn flow(n_frames: u64, start: u64, end: Option<u64>) -> (Flow, Receiver<u64>) {
        let source = Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n_frames as usize)
                .flat_map(|i| [i as f32, i as f32])
                .collect(),
            total_frames: n_frames,
            sample_rate: 48_000,
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
