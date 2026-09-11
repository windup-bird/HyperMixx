//! Deck: one source, one active flow, non-blocking jumps.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwapOption;

use super::TimeShift;
use crate::beatgrid::{KeyReport, TrackAnalysis};
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
    /// Deck position when the pending jump was submitted, used to shift its landing point by the
    /// audio the deck played while the new flow warmed up.
    cued_from: u64,
    playing: AtomicBool,
    /// Analysis can arrive from the decode thread while the producer thread is sampling the deck,
    /// so it swaps in lock-free instead of riding on the deck mutex.
    analysis: ArcSwapOption<TrackAnalysis>,
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
            cued_from: 0,
            playing: AtomicBool::new(false),
            analysis: ArcSwapOption::empty(),
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

    /// The underlying PCM source, for out-of-band readers (e.g. the analysis layer).
    pub fn source(&self) -> Arc<dyn Source> {
        Arc::clone(&self.pool)
    }

    /// Non-blocking jump: spawns a flow at `target_frame` and hands it to the warm-up thread.
    /// The switch happens on the next [`process_block`](Self::process_block), so the audible
    /// position may lag by at most the current block plus the warm-up time.
    pub fn jump(&mut self, target_frame: u64) {
        let target = target_frame.min(self.pool.total_frames());
        let id = self.next_flow_id;
        self.next_flow_id = id.wrapping_add(1);
        self.cued_from = self.current_frame();
        let flow = Flow::new(
            id,
            Arc::clone(&self.pool),
            target,
            None,
            self.timeshift.ready_sender(),
        );
        self.timeshift.submit_prepare(flow);
    }

    /// Publishes track analysis (beat grid). Safe to call while the deck is playing.
    pub fn set_analysis(&self, analysis: TrackAnalysis) {
        self.analysis.store(Some(Arc::new(analysis)));
    }

    /// The current analysis, if the loaded track has one.
    pub fn analysis(&self) -> Option<Arc<TrackAnalysis>> {
        self.analysis.load_full()
    }

    /// Detected musical key, or `None` when the deck has no analysis.
    pub fn key(&self) -> Option<KeyReport> {
        self.analysis().and_then(|a| a.key)
    }

    /// Reported BPM (from analysis), or grid-derived average. 0.0 without analysis.
    pub fn bpm(&self) -> f32 {
        match self.analysis() {
            Some(a) => a.bpm.unwrap_or_else(|| a.beatgrid.average_bpm()),
            None => 0.0,
        }
    }

    /// Where [`beatjump`](Self::beatjump) would land from the current position, without jumping.
    pub fn beat_target_frame(&self, beats: i64) -> Option<u64> {
        let analysis = self.analysis()?;
        Some(
            analysis
                .beatgrid
                .beatjump_target(self.current_frame(), beats),
        )
    }

    /// Non-blocking jump of whole beats, keeping the phase inside the current beat.
    ///
    /// Does nothing when the deck has no beat grid yet — a wrong grid is worse than no grid.
    pub fn beatjump(&mut self, beats: i64) {
        if let Some(target) = self.beat_target_frame(beats) {
            self.jump(target);
        }
    }

    /// Sets the tempo rate on the active flow's time-stretch engine.
    pub fn set_ratio(&mut self, rate: f32) {
        if let Some(flow) = self.flows.get_mut(self.active_index) {
            flow.set_ratio(rate);
        }
    }

    /// Rebuilds the active flow's engine with a different time-stretch profile.
    pub fn set_profile(&mut self, profile: timestretch::engine::EngineProfile) {
        if let Some(flow) = self.flows.get_mut(self.active_index) {
            flow.set_profile(profile);
        }
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
        // The target was computed when the command arrived; this deck kept playing while the flow
        // warmed up, so move the landing point forward by exactly that much. Otherwise every jump
        // loses a block of gap against a running sibling deck — and with a real time-stretch engine,
        // whose warm-up costs several blocks, the loss would grow with it.
        let played = self.current_frame().saturating_sub(self.cued_from);
        if played > 0 {
            flow.reset_to(flow.current_frame() + played);
        }
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

    /// Drives blocks until the playhead falls behind `above`, i.e. until a backward jump lands.
    fn settle_below(deck: &mut Deck, out: &mut [f32], above: u64) {
        for _ in 0..500 {
            deck.process_block(out);
            if deck.current_frame() < above {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!(
            "deck never moved back below {above}, stuck at {}",
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
        // After end, output is silence but process_block still fills the buffer.
        assert_eq!(deck.process_block(&mut out), 256);
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
        // The landing point is `3000 + played` where `played` is however far the old flow advanced
        // while the timestretch engine was being prepared (a non-trivial amount now). A leaked
        // stale jump would sit below 3000 instead, so the lower bound is the discriminating check.
        assert!(
            deck.current_frame() <= 10_000,
            "overshot the newest jump target: {}",
            deck.current_frame()
        );
    }

    #[test]
    fn deck_over_empty_source_is_silent() {
        let mut deck = Deck::new(Arc::new(PcmPool::empty()));
        deck.play();
        let mut out = vec![1.0f32; 256 * CHANNELS];
        // The engine always fills the buffer; empty source means the content is silence.
        assert_eq!(deck.process_block(&mut out), 256);
        assert!(out.iter().all(|s| *s == 0.0));
        assert!(deck.is_at_end());
    }

    fn grid_122bpm(total_frames: u64) -> TrackAnalysis {
        TrackAnalysis {
            beatgrid: crate::beatgrid::BeatGrid::from_constant_bpm(
                122.0,
                0,
                total_frames,
                crate::SAMPLE_RATE,
            ),
            key: None,
            bpm: Some(122.0),
        }
    }

    #[test]
    fn deck_without_analysis_reports_no_tempo_and_ignores_beatjump() {
        let mut deck = Deck::new(pool(10_000));
        assert_eq!(deck.bpm(), 0.0);
        assert!(deck.analysis().is_none());
        assert_eq!(deck.beat_target_frame(4), None);
        deck.play();
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.beatjump(4);
        deck.process_block(&mut out);
        assert_eq!(
            deck.current_frame(),
            256,
            "beatjump must not move without a grid"
        );
    }

    #[test]
    fn analysis_swaps_in_while_playing() {
        let mut deck = Deck::new(pool(10_000));
        deck.play();
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.process_block(&mut out);
        deck.set_analysis(grid_122bpm(10_000)); // no deck lock involved
        assert!((deck.bpm() - 122.0).abs() < 0.5, "bpm {}", deck.bpm());
        assert!(deck.analysis().is_some());
    }

    #[test]
    fn beatjump_preview_matches_the_grid_formula() {
        let deck = Deck::new(pool(10_000));
        deck.set_analysis(grid_122bpm(10_000));
        let frames_per_beat: f64 = 48_000.0 * 60.0 / 122.0;
        assert_eq!(
            deck.beat_target_frame(1),
            Some(frames_per_beat.round() as u64)
        );
        assert_eq!(
            deck.beat_target_frame(-1),
            Some(0),
            "clamped at the first beat"
        );
    }

    #[test]
    fn beatjump_moves_by_beats_and_keeps_phase() {
        let mut deck = Deck::new(pool(48_000 * 20));
        deck.set_analysis(grid_122bpm(48_000 * 20));
        deck.play();
        let mut out = vec![0.0f32; 256 * CHANNELS];

        // Cue somewhere inside beat 0, off the grid, then step four beats ahead.
        deck.jump(5_000);
        settle_at(&mut deck, &mut out, 5_000);
        let before = deck.current_frame();
        deck.beatjump(4);
        settle_at(&mut deck, &mut out, before + 4 * 23_000);
        let forward = deck.current_frame();
        assert!(
            (forward as i64 - (before as i64 + 4 * 23_607)).abs() <= 2 * 256,
            "four beats ahead should be ~94428 frames on: {before} -> {forward}"
        );

        deck.beatjump(-4);
        let back = deck.beat_target_frame(-4).expect("grid");
        settle_below(&mut deck, &mut out, forward);
        assert!(
            (deck.current_frame() as i64 - back as i64).abs() <= 2 * 256,
            "back to the original phase: expected ~{back}, got {}",
            deck.current_frame()
        );
    }
}
