//! Deck: one source, one active flow, non-blocking jumps.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::TimeShift;
use crate::flow::{Flow, FlowState};
use crate::source::Source;
use crate::CHANNELS;

/// A single playback deck.
///
/// `flows` holds every flow the deck still cares about; v1 keeps exactly one active flow and
/// replaces it on a jump, but the shape is already right for a future crossfade (two active flows
/// plus a mixer).
pub struct Deck {
    pool: Arc<dyn Source>,
    flows: Vec<Flow>,
    active_index: usize,
    timeshift: TimeShift,
    next_flow_id: u64,
    playing: AtomicBool,
}

impl Deck {
    /// Creates a deck over `pool`, cued at frame 0 and paused.
    pub fn new(pool: Arc<dyn Source>) -> Self {
        let timeshift = TimeShift::new();
        let mut first = Flow::new(0, Arc::clone(&pool), 0, None, timeshift.ready_sender());
        first.prepare();
        first.activate();
        Self {
            pool,
            flows: vec![first],
            active_index: 0,
            timeshift,
            next_flow_id: 1,
            playing: AtomicBool::new(false),
        }
    }

    pub fn play(&self) {
        self.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Total frames of the loaded track.
    pub fn total_frames(&self) -> u64 {
        self.pool.total_frames()
    }

    /// Non-blocking jump: spawns a flow at `target_frame` and hands it to the warm-up thread.
    /// The switch happens on the next [`process_block`](Self::process_block), so the audible
    /// position may lag by at most the current block plus the warm-up time.
    pub fn jump(&mut self, target_frame: u64) {
        let target = target_frame.min(self.pool.total_frames());
        let id = self.next_flow_id;
        self.next_flow_id = id.wrapping_add(1);
        let flow = Flow::new(
            id,
            Arc::clone(&self.pool),
            target,
            None,
            self.timeshift.ready_sender(),
        );
        self.timeshift.submit_prepare(flow);
    }

    /// Processes one block, applying any completed jump first.
    ///
    /// `output` is always fully written (silence when paused or past the end); the return value is
    /// the number of frames carrying actual audio.
    pub fn process_block(&mut self, output: &mut [f32]) -> usize {
        self.poll_ready_flows();
        let capacity = output.len() / CHANNELS;
        if !self.is_playing() {
            output[..capacity * CHANNELS].fill(0.0);
            return 0;
        }
        match self.flows.get_mut(self.active_index) {
            Some(flow) => flow.process_block(output),
            None => {
                output[..capacity * CHANNELS].fill(0.0);
                0
            }
        }
    }

    /// Current playhead position, in frames.
    pub fn current_frame(&self) -> u64 {
        self.flows
            .get(self.active_index)
            .map(Flow::current_frame)
            .unwrap_or(0)
    }

    /// True once the active flow ran out of audio.
    pub fn is_at_end(&self) -> bool {
        self.flows
            .get(self.active_index)
            .is_none_or(Flow::reached_end)
    }

    fn poll_ready_flows(&mut self) {
        while let Some(id) = self.timeshift.poll_ready() {
            self.switch_to(id);
        }
    }

    fn switch_to(&mut self, flow_id: u64) {
        let Some(mut flow) = self.timeshift.take_ready_flow(flow_id) else {
            return;
        };
        for other in &mut self.flows {
            if other.id != flow_id {
                other.retire();
            }
        }
        flow.activate();
        self.flows.push(flow);
        self.flows.retain(|f| f.state != FlowState::Retired);
        self.active_index = self
            .flows
            .iter()
            .position(|f| f.id == flow_id)
            .unwrap_or(self.active_index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{DecodedAudio, PcmPool};

    fn pool(n_frames: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n_frames as usize)
                .flat_map(|i| [i as f32, i as f32])
                .collect(),
            total_frames: n_frames,
            sample_rate: 48_000,
            channels: CHANNELS,
        }))
    }

    /// Drives blocks until the deck reaches `target`; warm-up happens on another thread.
    fn settle_at(deck: &mut Deck, out: &mut [f32], target: u64) {
        for _ in 0..500 {
            if deck.current_frame() >= target {
                return;
            }
            deck.process_block(out);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!(
            "deck never reached frame {target}, stopped at {}",
            deck.current_frame()
        );
    }

    #[test]
    fn new_deck_is_cued_at_zero_and_paused() {
        let deck = Deck::new(pool(10_000));
        assert!(!deck.is_playing());
        assert_eq!(deck.current_frame(), 0);
        assert_eq!(deck.total_frames(), 10_000);
    }

    #[test]
    fn paused_deck_emits_silence_and_holds_position() {
        let mut deck = Deck::new(pool(10_000));
        let mut out = vec![1.0f32; 256 * CHANNELS];
        assert_eq!(deck.process_block(&mut out), 0);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(deck.current_frame(), 0);
    }

    #[test]
    fn playing_advances_the_playhead() {
        let mut deck = Deck::new(pool(10_000));
        deck.play();
        let mut out = vec![0.0f32; 256 * CHANNELS];
        for _ in 0..3 {
            assert_eq!(deck.process_block(&mut out), 256);
        }
        assert_eq!(deck.current_frame(), 768);
        assert_eq!(out[0], 512.0);
    }

    #[test]
    fn pause_then_play_resumes_from_position() {
        let mut deck = Deck::new(pool(10_000));
        deck.play();
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.process_block(&mut out);
        deck.pause();
        deck.process_block(&mut out);
        let held = deck.current_frame();
        deck.play();
        deck.process_block(&mut out);
        assert_eq!(deck.current_frame(), held + 256);
    }

    #[test]
    fn jump_switches_to_the_new_flow() {
        let mut deck = Deck::new(pool(10_000));
        deck.play();
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.process_block(&mut out);

        deck.jump(5000);
        settle_at(&mut deck, &mut out, 5000);
        assert!(
            deck.current_frame() >= 5000,
            "jump did not take effect: {}",
            deck.current_frame()
        );
        assert_eq!(deck.flows.len(), 1, "retired flows must be cleaned up");
        assert_eq!(deck.flows[deck.active_index].state, FlowState::Active);
    }

    #[test]
    fn jump_past_end_clamps_and_silences() {
        let mut deck = Deck::new(pool(100));
        deck.play();
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.jump(999_999);
        settle_at(&mut deck, &mut out, 100);
        assert!(deck.is_at_end());
        assert_eq!(deck.process_block(&mut out), 0);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn only_the_newest_of_rapid_jumps_is_applied() {
        let mut deck = Deck::new(pool(10_000));
        deck.play();
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.jump(1000);
        deck.jump(2000);
        deck.jump(3000);
        settle_at(&mut deck, &mut out, 3000);
        assert!(deck.current_frame() >= 3000);
        assert!(
            deck.current_frame() < 3000 + 2 * 256,
            "stale jump leaked: {}",
            deck.current_frame()
        );
    }

    #[test]
    fn deck_over_empty_source_is_silent() {
        let mut deck = Deck::new(Arc::new(PcmPool::empty()));
        deck.play();
        let mut out = vec![1.0f32; 256 * CHANNELS];
        assert_eq!(deck.process_block(&mut out), 0);
        assert!(out.iter().all(|s| *s == 0.0));
        assert!(deck.is_at_end());
    }
}
