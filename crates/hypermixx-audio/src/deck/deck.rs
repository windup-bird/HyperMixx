//! Deck: one source, one active flow, non-blocking jumps — plus the loop state machine.
//!
//! Two clocks, deliberately split:
//!
//! - **`virtual_frame`** — the active flow's engine clock. Monotonic while playing, never folded
//!   by a loop. Jump compensation, loop bookkeeping and the LoopFlow lockstep all speak this
//!   position. A loop *exit* resumes where the looped flow's output stopped (the audible plays
//!   out its lap and flows past `out` on its own) — landing on the slipped clock instead is the
//!   separate `slip loop` feature.
//! - **`current_frame`** — `virtual` mapped through the flow's `loop_range`: what the listener
//!   hears, what the waveform shows, what a beatjump resolves from.
//!
//! A loop's range lives **on the flow**, not the deck: an in-loop edit is one atomic store (no
//! flow change), and any flow switch destroys the range with the flow it belonged to — the deck
//! never clears a range in place.
//!
//! The armed manual loop-in (the *LoopFlow*) is a second flow the deck keeps **outside** the
//! `flows` vector so switches can't reach it. It warms on the FlowShift thread, then runs in
//! lockstep with the active flow — driven block for block into a scratch buffer whose output is
//! discarded — so its engine stays live and its clock stays identical to the deck's. Pressing
//! `out` stores the range the lockstep driver already put there and promotes it in place: the
//! switch is a no-op on both the clock and the fed-but-unheard window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use hypermixx_core::{Key, LoopEditOp, LoopOp, LoopQuantum, Source, TrackAnalysis};
use timestretch::engine::EngineProfile;

use super::jump::{resolve, Seek};
use super::loop_::{
    beats_after, beat_shift, provisional_range, quantize_offset, quantize_to_beat, LoopRange,
};
use super::sync::{MAX_RATE, MIN_RATE, PhaseAlign, Playhead, SyncCtx};
use super::FlowShift;
use crate::flow::{Flow, FlowState};
use crate::fx::FxContext;
use crate::mixer::Bus;
use crate::CHANNELS;
use hypermixx_media::PcmPool;

/// Why a loop command was refused when the deck has no usable grid.
const LOOP_NO_GRID: &str = "loop needs a beat grid — `analyse` the track, or load it with a bpm";
/// Upper bound on catch-up blocks when an armed LoopFlow first arrives (its warm-up gap plus a
/// generous margin): 64 blocks ≈ 372 ms, an order of magnitude beyond any real warm-up.
const MAX_CATCHUP_BLOCKS: usize = 64;

/// An armed manual loop-in: the id of the LoopFlow warming for it, its quantized in point, and
/// the flow itself once announced ready.
struct LoopArm {
    id: u64,
    p_in: u64,
    flow: Option<Flow>,
    /// The profile the deck wanted when the arm was (re)built; applied on arrival if the deck's
    /// profile changed while the flow was still on the worker.
    profile: EngineProfile,
}

/// A single playback deck.
///
/// `flows` holds every flow the deck still cares about; v1 keeps exactly one active flow and
/// replaces it on a jump, but the shape is already right for a future crossfade (two active flows
/// plus a mixer).
pub struct Deck {
    pool: Arc<dyn Source>,
    flows: Vec<Flow>,
    active_index: usize,
    flowshift: FlowShift,
    next_flow_id: u64,
    /// Deck position when the pending jump was submitted (**virtual**, so the compensation is in
    /// the same clock the landing is expressed in), used to shift the landing point by the audio
    /// the deck played while the new flow warmed up.
    cued_from: u64,
    playing: AtomicBool,
    /// Analysis can arrive from the decode thread while the producer thread is sampling the deck,
    /// so it swaps in lock-free instead of riding on the deck mutex.
    analysis: ArcSwapOption<TrackAnalysis>,
    /// Armed manual loop-in waiting for its `out` press.
    loop_arm: Option<LoopArm>,
    /// LoopFlow ids abandoned mid-warm-up (re-armed, cancelled, jumped past). Reaped when they
    /// announce, so an abandoned engine cannot leak in FlowShift's warm map.
    loop_reap: Vec<u64>,
    /// Set by [`loop_exit`](Self::loop_exit): the *next* switch resumes where the looped flow's
    /// output stopped instead of applying `cued_from + played`. Cleared by the next
    /// [`jump`](Self::jump) (which supersedes that exit) and consumed by the switch itself.
    pending_exit_resume: bool,
    /// Out-point quantization for `loop out`, the LoopFlow's provisional range, and edits.
    loop_quantum: LoopQuantum,
    /// Transport every spawned flow inherits: a tempo (or profile) the DJ set must survive jumps.
    ///
    /// Split in two on purpose: `tempo` is what the fader, `sync tempo` and a lock write, and
    /// `nudgerate` is what phase tracking and `nudge` bend it by. See [`Playhead`].
    playhead: Playhead,
    /// The rate last handed to the engines, so an idle deck does not rewrite it every block.
    applied_rate: f64,
    /// The sync view for the block about to render, stashed by the mixer right before it drives
    /// this deck. `None` for a caller that drives a deck directly (the raw-transport tests), which
    /// then gets no group tempo and no phase correction — but still a ticking nudge.
    sync: Option<SyncCtx>,
    profile: EngineProfile,
}

impl Deck {
    /// Creates a deck over `pool`, cued at frame 0 and paused.
    pub fn new(pool: Arc<dyn Source>) -> Self {
        let flowshift = FlowShift::new();
        let mut first = Flow::new(0, Arc::clone(&pool), 0, None, flowshift.ready_sender());
        first.prepare();
        first.activate();
        Self {
            pool,
            flows: vec![first],
            active_index: 0,
            flowshift,
            next_flow_id: 1,
            cued_from: 0,
            playing: AtomicBool::new(false),
            analysis: ArcSwapOption::empty(),
            loop_arm: None,
            loop_reap: Vec::new(),
            pending_exit_resume: false,
            loop_quantum: LoopQuantum::default(),
            playhead: Playhead::new(),
            applied_rate: 1.0,
            sync: None,
            profile: EngineProfile::Tape,
        }
    }

    /// A deck with nothing loaded, which is what a mixer built from a config needs: channels exist
    /// before any track does, and an empty deck contributes silence.
    pub fn empty() -> Self {
        Self::new(Arc::new(PcmPool::empty()))
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
    ///
    /// Any jump voids an armed loop-in: re-cueing is a different intent from waiting for `out`,
    /// and a stale in point would otherwise engage a loop over a position the DJ left.
    pub fn jump(&mut self, target_frame: u64) {
        self.disarm_loop();
        // A fresh jump supersedes any pending exit's landing rule: only `loop_exit` opts in, and
        // only until the switch it belongs to lands (or the next jump takes it over).
        self.pending_exit_resume = false;
        let target = target_frame.min(self.total_frames());
        self.cued_from = self.virtual_frame();
        let flow = self.make_flow(target, None);
        self.flowshift.submit_prepare(flow);
    }

    /// Publishes track analysis (beat grid). Safe to call while the deck is playing.
    pub fn set_analysis(&self, analysis: TrackAnalysis) {
        self.analysis.store(Some(Arc::new(analysis)));
    }

    /// The current analysis, if the loaded track has one.
    pub fn analysis(&self) -> Option<Arc<TrackAnalysis>> {
        self.analysis.load_full()
    }

    /// Detected musical key, or `None` without analysis.
    pub fn key(&self) -> Option<Key> {
        self.analysis().and_then(|a| a.key)
    }

    /// Reported BPM (from analysis), or grid-derived average. 0.0 without analysis.
    pub fn bpm(&self) -> f32 {
        self.analysis().map(|a| a.bpm()).unwrap_or(0.0)
    }

    /// Where a [`Seek`] would land from the current position, without jumping. `None` without a
    /// grid (except [`Seek::Frames`], which needs none). Resolves from what the listener hears
    /// (`current_frame`): a beatjump is a musical move relative to the audible position.
    pub fn seek_target_frame(&self, seek: Seek) -> Option<u64> {
        match seek {
            Seek::Frames(frame) => Some(frame),
            other => {
                let analysis = self.analysis()?;
                resolve(&other, &analysis.beatgrid, self.current_frame())
            }
        }
    }

    /// Where [`beatjump`](Self::beatjump) would land from the current position, without jumping.
    pub fn beat_target_frame(&self, beats: i64) -> Option<u64> {
        self.seek_target_frame(Seek::Beats(beats))
    }

    /// Non-blocking jump of whole beats, keeping the phase inside the current beat.
    ///
    /// Does nothing when the deck has no beat grid yet — a wrong grid is worse than no grid.
    pub fn beatjump(&mut self, beats: i64) {
        self.seek(Seek::Beats(beats));
    }

    /// Non-blocking seek from a musical [`Seek`]; no-op if it can't be resolved (empty grid).
    pub fn seek(&mut self, seek: Seek) {
        if let Some(target) = self.seek_target_frame(seek) {
            self.jump(target);
        }
    }

    /// The BPM in force at the position the listener hears, straight off the grid — `0.0` while
    /// the deck has no grid (an average over a track whose tempo changes would be a lie here).
    pub fn bpm_at_frame(&self) -> f64 {
        self.analysis()
            .map(|analysis| f64::from(analysis.beatgrid.bpm_at_frame(self.current_frame())))
            .unwrap_or(0.0)
    }

    /// Where inside its beat the deck sits, `0.0` on the beat — `None` while it has no grid.
    pub fn beat_phase(&self) -> Option<f32> {
        let analysis = self.analysis()?;
        if analysis.beatgrid.is_empty() {
            return None;
        }
        Some(analysis.beatgrid.phase(self.current_frame()))
    }

    /// The stable tempo: what the fader and `sync tempo` set, and what survives `sync unlock`.
    pub fn tempo(&self) -> f64 {
        self.playhead.tempo
    }

    /// The temporary rate stacked on `tempo` — phase correction plus nudge, `0.0` when idle.
    pub fn nudgerate(&self) -> f64 {
        self.playhead.nudgerate
    }

    /// `tempo + nudgerate`: the rate the engine is actually playing at.
    pub fn playing_rate(&self) -> f64 {
        self.playhead.playing_rate()
    }

    /// Whether this deck's tempo follows the group's shared BPM.
    pub fn lock(&self) -> bool {
        self.playhead.lock
    }

    /// The current nudge bend, a rate (`0.0` when idle).
    pub fn nudge_rate(&self) -> f64 {
        self.playhead.nudge_rate()
    }

    /// The running phase correction as a label (`"instant"`/`"linear"`/`"pid"`), or `None`.
    pub fn align_label(&self) -> Option<&'static str> {
        self.playhead.align_label()
    }

    /// Installs this block's sync view. The mixer calls this right before it renders the deck.
    pub fn set_sync(&mut self, sync: Option<SyncCtx>) {
        self.sync = sync;
    }

    /// Writes the stable tempo and pushes it to the engines. How a lock or the fader route a
    /// caller's intent *into* this is the mixer's decision, not the deck's.
    pub fn set_tempo(&mut self, rate: f64) {
        self.playhead.tempo = rate.clamp(MIN_RATE, MAX_RATE);
        self.push_rate();
    }

    /// Whether this deck derives its tempo from the group's shared BPM.
    pub fn set_lock(&mut self, lock: bool) {
        self.playhead.lock = lock;
        self.push_rate();
    }

    /// Installs (or drops) this deck's phase correction.
    pub fn set_align(&mut self, align: Option<PhaseAlign>) {
        self.playhead.align = align;
        self.push_rate();
    }

    /// Starts a temporary rate bend; `seconds` releases it on its own, `None` holds it until
    /// [`stop_nudge`](Self::stop_nudge). The tempo is untouched — only `nudgerate` moves.
    pub fn start_nudge(&mut self, delta: f64, seconds: Option<f64>) {
        self.playhead.start_nudge(delta, seconds);
        self.push_rate();
    }

    /// Releases a running bend; it ramps back to zero rather than snapping.
    pub fn stop_nudge(&mut self) {
        self.playhead.stop_nudge();
        self.push_rate();
    }

    /// Tears down the lock, the phase correction and any bend, keeping the tempo. `sync unlock`.
    pub fn clear_sync(&mut self) {
        self.playhead.unlock();
        self.push_rate();
    }

    /// Hand the engine the current rate, writing only when it actually moved. A controller that
    /// converges stops writing, so an idle deck leaves the timestretch engine alone.
    fn push_rate(&mut self) {
        let rate = self.playhead.playing_rate().clamp(MIN_RATE, MAX_RATE);
        if (rate - self.applied_rate).abs() < 1e-6 {
            return;
        }
        self.apply_rate(rate);
    }

    /// Writes `rate` to every flow this deck is driving — the active one and an armed LoopFlow
    /// that has already arrived — and remembers it as the last rate sent.
    fn apply_rate(&mut self, rate: f64) {
        self.applied_rate = rate;
        let ratio = rate as f32;
        if let Some(flow) = self.flows.get_mut(self.active_index) {
            flow.set_ratio(ratio);
        }
        if let Some(flow) = self
            .loop_arm
            .as_mut()
            .and_then(|arm| arm.flow.as_mut())
        {
            flow.set_ratio(ratio);
        }
    }

    /// Runs the sync math for the block about to render: the group recompute, the phase controller
    /// and the nudge all land in `playhead`, then whatever they settled on is pushed to the engine.
    fn apply_sync(&mut self) {
        let analysis = self.analysis(); // Arc bump — no allocation on this path
        let (own_bpm, own_phase) = match &analysis {
            Some(analysis) if !analysis.beatgrid.is_empty() => {
                let position = self.current_frame();
                (
                    f64::from(analysis.beatgrid.bpm_at_frame(position)),
                    Some(analysis.beatgrid.phase(position)),
                )
            }
            _ => (0.0, None),
        };
        self.playhead.update(self.sync.as_ref(), own_bpm, own_phase);
        self.push_rate();
    }

    /// Sets the tempo rate on the active flow's time-stretch engine — and remembers it, so the
    /// next spawned flow (or an armed LoopFlow warming right now) is built at the same rate.
    pub fn set_ratio(&mut self, rate: f32) {
        self.set_tempo(f64::from(rate));
    }

    /// Rebuilds the active flow's engine with a different time-stretch profile, remembering the
    /// choice for flows spawned later (an arm still warming gets it on arrival).
    pub fn set_profile(&mut self, profile: EngineProfile) {
        self.profile = profile;
        if let Some(flow) = self.flows.get_mut(self.active_index) {
            flow.set_profile(profile);
        }
        if let Some(arm) = self.loop_arm.as_mut() {
            arm.profile = profile;
            if let Some(flow) = arm.flow.as_mut() {
                flow.set_profile(profile);
            }
        }
    }

    /// Processes one block, applying any completed jump first.
    ///
    /// `output` is always fully written (silence when paused or past the end); the return value is
    /// the number of frames carrying actual audio.
    pub fn process_block(&mut self, output: &mut [f32]) -> usize {
        self.poll_ready_flows();
        // Sync runs first so the rate handed to the engine belongs to the same block as the
        // positions the leader was sampled at — a follower must never compare against last block.
        self.apply_sync();
        let capacity = output.len() / CHANNELS;
        if !self.is_playing() {
            output[..capacity * CHANNELS].fill(0.0);
            return 0;
        }
        let written = match self.flows.get_mut(self.active_index) {
            Some(flow) => flow.process_block(output),
            None => {
                output[..capacity * CHANNELS].fill(0.0);
                0
            }
        };
        // Lockstep last: the armed LoopFlow is brought to exactly the clock the active flow just
        // reached (catching up its warm-up gap, then one block per block). Its render lands in a
        // scratch buffer and is thrown away — the engine runs, the listener never hears it.
        self.drive_loop_flow();
        written
    }

    /// Renders one block into a [`Bus`], de-interleaving the transport's output as it goes.
    ///
    /// A thin wrapper on [`process_block`](Self::process_block) — the state machine, the jump
    /// compensation and the silence rules are unchanged, so a mixer and a ring buffer can never see
    /// two different decks. The tail of `bus` beyond the block length is zeroed, so a bus reused
    /// across blocks cannot leak the previous block into a short one.
    ///
    /// Returns the frames of actual audio (0 while paused or at the end), same contract as
    /// [`process_block`](Self::process_block).
    pub fn pull_into(&mut self, bus: &mut Bus, ctx: &FxContext) -> usize {
        let frames = ctx.block_frames.min(bus.frames());
        // Stack scratch for the interleaved block: no allocation on the audio path, and one
        // `BLOCK_SIZE` block is exactly what the transport produces.
        let mut block = [0.0f32; crate::BLOCK_SAMPLES];
        let want = frames.min(crate::BLOCK_SIZE) * CHANNELS;
        let written = self.process_block(&mut block[..want]);
        for (i, (left, right)) in bus.l[..frames].iter_mut().zip(&mut bus.r[..frames]).enumerate() {
            let base = i * CHANNELS;
            if base + 1 < want {
                *left = block[base];
                *right = block[base + 1];
            } else {
                // A bus longer than one transport block stays zero, so a reused bus cannot leak the
                // previous block into the tail of this one.
                *left = 0.0;
                *right = 0.0;
            }
        }
        written
    }

    /// What the listener hears, in frames: the virtual clock folded through the active flow's
    /// loop range (identity with no loop). The waveform, `state` and beat-seek all read this.
    pub fn current_frame(&self) -> u64 {
        self.flows
            .get(self.active_index)
            .map(Flow::actual_frame)
            .unwrap_or(0)
    }

    /// The slip clock: monotonic while playing, never folded by a loop. Jump compensation is
    /// measured in it and the loop state machine computes in it. A loop *exit* does **not** land
    /// here (or on `out`): it resumes exactly where the looped flow's output stopped — see
    /// [`loop_exit`](Self::loop_exit). Exiting onto this clock instead is the separate
    /// `slip loop` feature.
    pub fn virtual_frame(&self) -> u64 {
        self.flows
            .get(self.active_index)
            .map(Flow::virtual_frame)
            .unwrap_or(0)
    }

    /// The active loop as a whole range, or `None`. Read from the active flow, so it goes `None`
    /// by itself the moment a switch replaces that flow — the range dies with its flow.
    pub fn loop_range(&self) -> Option<LoopRange> {
        self.flows
            .get(self.active_index)
            .and_then(Flow::loop_range)
    }

    /// The armed manual in point (quantized), or `None`. Present while the LoopFlow warms *and*
    /// while it runs lockstep — arming is immediate, warmth is not.
    pub fn loop_in_armed(&self) -> Option<u64> {
        self.loop_arm.as_ref().map(|arm| arm.p_in)
    }

    /// The deck's out-point quantization granularity.
    pub fn loop_quantum(&self) -> LoopQuantum {
        self.loop_quantum
    }

    /// True once the active flow ran out of audio. A looped flow reports an unbounded virtual
    /// source, so a looping deck is never "at the end".
    pub fn is_at_end(&self) -> bool {
        self.flows
            .get(self.active_index)
            .is_none_or(Flow::reached_end)
    }

    // ------------------------------------------------------------------ loop state machine

    /// The whole loop family, one entry point for [`Command::Loop`](hypermixx_core::Command).
    pub fn apply_loop(&mut self, op: LoopOp) -> Result<(), String> {
        match op {
            LoopOp::In => self.loop_in(),
            LoopOp::Out => self.loop_out(),
            LoopOp::Cancel => {
                self.disarm_loop();
                Ok(())
            }
            LoopOp::Exit => self.loop_exit(),
            LoopOp::Beats(beats) => self.loop_beats(beats),
            LoopOp::Edit(edit) => self.loop_edit(edit),
            LoopOp::SetQuantum(quantum) => {
                self.loop_quantum = quantum;
                Ok(())
            }
        }
    }

    /// Arm a manual loop-in: quantize `in` to the beat, spawn the LoopFlow at the deck's virtual
    /// position and let it warm ahead in lockstep. Audible nothing — the loop starts at `out`.
    ///
    /// A running loop is exited first (循环中再按 in → 退出当前,开新) and any previous arm is
    /// dropped (in 后又按 in → 丢弃旧). `p_in` quantizes what is *heard*; the clock itself never
    /// moves — the quantization lands on the range, not on `virtual_pos`.
    fn loop_in(&mut self) -> Result<(), String> {
        let analysis = self.require_grid()?;
        let p_in = quantize_to_beat(&analysis.beatgrid, self.current_frame())
            .ok_or_else(|| LOOP_NO_GRID.to_owned())?
            .min(self.total_frames().saturating_sub(1));
        self.disarm_loop();
        if self.loop_range().is_some() {
            self.loop_exit()?;
        }
        let start = self.virtual_frame();
        let range = provisional_range(
            &analysis.beatgrid,
            p_in,
            start,
            self.loop_quantum,
            self.total_frames(),
        );
        let flow = self.make_flow(start, range);
        let id = flow.id;
        self.flowshift.submit_prepare_loop_flow(flow);
        self.loop_arm = Some(LoopArm {
            id,
            p_in,
            flow: None,
            profile: self.profile,
        });
        Ok(())
    }

    /// Set the quantized `out` and engage. The store writes exactly the provisional the lockstep
    /// driver last wrote (both are `quantize_offset(p_in, virtual_now, quantum)`), so nothing
    /// changes for the fed-but-unheard window: the warmed LoopFlow is promoted in place — no
    /// `cued_from`, no re-warm, no delay. If the LoopFlow is *still* warming (faster than its
    /// warm-up), fall back to the beat-loop path: a fresh ranged flow with jump compensation.
    fn loop_out(&mut self) -> Result<(), String> {
        let analysis = self.require_grid()?;
        let Some(arm) = self.loop_arm.take() else {
            return Err("no loop-in armed — press `loop in` first".into());
        };
        let out = quantize_offset(&analysis.beatgrid, arm.p_in, self.virtual_frame(), self.loop_quantum)
            .ok_or_else(|| LOOP_NO_GRID.to_owned())?
            .min(self.total_frames());
        if let Some(mut flow) = arm.flow {
            flow.set_loop_range(Some(LoopRange::new(arm.p_in, out)));
            flow.activate();
            for other in &mut self.flows {
                other.retire();
            }
            self.flows.push(flow);
            self.flows.retain(|f| f.state != FlowState::Retired);
            self.active_index = self.flows.len() - 1;
            Ok(())
        } else {
            self.loop_reap.push(arm.id);
            self.open_beat_loop(arm.p_in, out);
            Ok(())
        }
    }

    /// Beat loop. While a loop runs, this only re-times its `out` to `in + N` beats — in place,
    /// clock untouched (halve/double). Otherwise it opens a fresh N-beat loop at the quantized
    /// current beat: a flow born *with* the range, warmed, then switched in (不跟跑).
    fn loop_beats(&mut self, beats: u64) -> Result<(), String> {
        if beats == 0 {
            return Err("a loop needs at least 1 beat".into());
        }
        let analysis = self.require_grid()?;
        if let Some(range) = self.loop_range() {
            let out = beats_after(&analysis.beatgrid, range.in_frame, beats)
                .ok_or_else(|| LOOP_NO_GRID.to_owned())?
                .min(self.total_frames())
                .max(range.in_frame + 1);
            self.set_active_loop_range(LoopRange::new(range.in_frame, out));
            return Ok(());
        }
        self.disarm_loop();
        let p_in = quantize_to_beat(&analysis.beatgrid, self.current_frame())
            .ok_or_else(|| LOOP_NO_GRID.to_owned())?
            .min(self.total_frames().saturating_sub(1));
        let out = beats_after(&analysis.beatgrid, p_in, beats)
            .ok_or_else(|| LOOP_NO_GRID.to_owned())?
            .min(self.total_frames())
            .max(p_in + 1);
        self.open_beat_loop(p_in, out);
        Ok(())
    }

    /// Leave the loop with **an unbroken stream**: the switch is prepared eagerly (press →
    /// warm-up → switch, [`cued_from`] compensation untouched) but its landing is pinned to
    /// wherever the looped flow's output stops — the first frame after it would be the next
    /// audible either way. So the listener hears no splice at all: the current position plays out
    /// the rest of its lap and flows straight past `out`, continuing beyond the loop.
    ///
    /// The `switch_to` landing rule for this jump is selected by [`pending_exit_resume`], set
    /// here (and cleared by the next [`jump`](Self::jump), which would supersede the exit).
    ///
    /// Exiting onto the slipped `virtual_pos` is kept as the separate **slip loop** feature.
    /// Idempotent: exiting with nothing engaged just cancels an arm.
    fn loop_exit(&mut self) -> Result<(), String> {
        if self.loop_range().is_some() {
            // Warm around where we'll land: the audible position (mapped through the range).
            self.jump(self.current_frame());
            self.pending_exit_resume = true;
        } else {
            self.disarm_loop();
        }
        Ok(())
    }

    /// An in-loop edit: recompute the whole range and store it in one shot. The clock does not
    /// move — the mapping formula re-derives the position on the next block. No flow change.
    fn loop_edit(&mut self, edit: LoopEditOp) -> Result<(), String> {
        let analysis = self.require_grid()?;
        let Some(range) = self.loop_range() else {
            return Err("no loop to edit".into());
        };
        let grid = &analysis.beatgrid;
        let total = self.total_frames();
        let beat = grid.beat_width(grid.floor_beat(range.in_frame)).max(1);
        let quantum_frames = ((beat as f64) * self.loop_quantum.beats())
            .round()
            .max(1.0) as u64;
        let next = match edit {
            LoopEditOp::Length { beats } => {
                if !beats.is_finite() || beats <= 0.0 {
                    return Err("length must be a positive number of beats".into());
                }
                // Edits floor at one quantum (the one-beat rule belongs to the manual `out`
                // press): `loop edit len 0.125` at eighth quantization is a legal loop roll.
                let len = ((beats * beat as f64).round() as u64).max(quantum_frames);
                let out = range.in_frame.saturating_add(len).min(total);
                LoopRange::new(range.in_frame, out)
            }
            // The dedicated ÷2/×2 keys: scale the *current* length in place, clamped to the
            // dedicated domain 1/32..=64 beats (a 32nd is legal even when the quantum is a beat;
            // 64 beats stops a runaway double from swallowing the track — or running past it).
            LoopEditOp::Halve | LoopEditOp::Double => {
                let min_len = ((beat as f64) / 32.0).round().max(1.0) as u64;
                let max_len = beat.saturating_mul(64);
                let len = range.len();
                let next_len = if edit == LoopEditOp::Halve {
                    (len / 2).max(min_len)
                } else {
                    len.saturating_mul(2).min(max_len)
                };
                let out = range.in_frame.saturating_add(next_len).min(total);
                LoopRange::new(range.in_frame, out)
            }
            LoopEditOp::Move { beats } => {
                let shifted = beat_shift(grid, range.in_frame, beats)
                    .ok_or_else(|| LOOP_NO_GRID.to_owned())?;
                let wanted = shifted as i64 - range.in_frame as i64;
                // Length preserved, clamped onto the track: `in ≥ 0`, `out ≤ total`.
                let delta = wanted.clamp(
                    -(range.in_frame as i64),
                    total.saturating_sub(range.out_frame) as i64,
                );
                LoopRange::new(
                    (range.in_frame as i64 + delta) as u64,
                    (range.out_frame as i64 + delta) as u64,
                )
            }
            LoopEditOp::In { beats } => {
                let shifted = beat_shift(grid, range.in_frame, beats)
                    .ok_or_else(|| LOOP_NO_GRID.to_owned())?;
                // Keep at least a quantum of length: `in` can never reach `out`.
                let max_in = range.out_frame.saturating_sub(quantum_frames);
                LoopRange::new(shifted.min(max_in), range.out_frame)
            }
            LoopEditOp::Out { beats } => {
                let shifted = beat_shift(grid, range.out_frame, beats)
                    .ok_or_else(|| LOOP_NO_GRID.to_owned())?;
                let min_out = range
                    .in_frame
                    .saturating_add(quantum_frames)
                    .min(total)
                    .max(range.in_frame + 1);
                LoopRange::new(range.in_frame, shifted.clamp(min_out, total))
            }
        };
        if next.is_empty() {
            // Defensive: a degenerate range would disable the loop instead of editing it.
            return Err("that would collapse the loop — out must stay past in".into());
        }
        self.set_active_loop_range(next);
        Ok(())
    }

    /// The analysis a loop (or a sync command) needs: present *and* non-empty (quantization is
    /// meaningless without beats — same rule as `beatjump`).
    pub fn require_grid(&self) -> Result<Arc<TrackAnalysis>, String> {
        let analysis = self.analysis().ok_or_else(|| LOOP_NO_GRID.to_owned())?;
        if analysis.beatgrid.is_empty() {
            return Err(LOOP_NO_GRID.to_owned());
        }
        Ok(analysis)
    }

    /// Opens a loop `[p_in, out)` as a flow change: born with the range, started at the current
    /// virtual position, warmed on the worker, switched in with `cued_from` compensation (the
    /// same machinery as a beatjump).
    fn open_beat_loop(&mut self, p_in: u64, out: u64) {
        // A new loop targets its own range + compensation; it is not a loop exit's resume.
        self.pending_exit_resume = false;
        let start = self.virtual_frame();
        let flow = self.make_flow(start, Some(LoopRange::new(p_in, out)));
        self.cued_from = start;
        self.flowshift.submit_prepare(flow);
    }

    /// Stores a new range on the active flow — the in-loop edit primitive (one atomic store).
    fn set_active_loop_range(&self, range: LoopRange) {
        if let Some(flow) = self.flows.get(self.active_index) {
            flow.set_loop_range(Some(range));
        }
    }

    /// Drops the armed loop-in. An arm whose LoopFlow is still warming is reaped on announcement
    /// so its engine cannot leak in FlowShift's warm map; a warmed one the deck owns is freed
    /// with the `LoopArm` itself.
    fn disarm_loop(&mut self) {
        if let Some(arm) = self.loop_arm.take() {
            if arm.flow.is_none() {
                self.loop_reap.push(arm.id);
            }
        }
    }

    /// Brings the armed LoopFlow's clock to the deck's `virtual_frame`, rendering into a scratch
    /// block whose output is discarded.
    ///
    /// Before each chunk the provisional range is stored — `quantize_offset(p_in, next, quantum)`,
    /// "the out point if `loop out` were pressed at the position this chunk ends on". That is the
    /// same formula `loop_out` uses, so the press writes the value already there: promotion is a
    /// no-op store and the ring's pending frames are mapped exactly as they will need to be.
    fn drive_loop_flow(&mut self) {
        if self.loop_arm.is_none() {
            return;
        }
        let target = self.virtual_frame();
        let analysis = self.analysis(); // Arc bump — no allocation on this path
        let quantum = self.loop_quantum;
        let total = self.total_frames();
        let Some(arm) = self.loop_arm.as_mut() else {
            return;
        };
        let Some(flow) = arm.flow.as_mut() else {
            return; // still warming; it catches up as soon as it arrives
        };
        let p_in = arm.p_in;
        let max_catchup = (MAX_CATCHUP_BLOCKS * crate::BLOCK_SIZE) as u64;
        let store_provisional = |flow: &Flow, at: u64| {
            if let Some(analysis) = &analysis {
                flow.set_loop_range(provisional_range(
                    &analysis.beatgrid,
                    p_in,
                    at,
                    quantum,
                    total,
                ));
            }
        };
        let mut scratch = [0.0f32; crate::BLOCK_SAMPLES];
        let mut driven = 0usize;
        // Clock discontinuity: a flow switch re-anchored the deck behind this arm's back — a loop
        // exit resumes at `out`, which sits *behind* where the slipped clock had lapped (or ahead
        // by up to a lap on a first-pass exit). Block-by-block grinding can't cross that gap, so
        // re-anchor the arm to the deck's clock with one reset; its discarded renders then drain
        // the priming that reset re-warms, and the parity the promotion depends on is exact.
        let clock = flow.virtual_frame();
        if clock > target || target - clock > max_catchup {
            store_provisional(flow, target);
            flow.reset_to(target);
        }
        while flow.virtual_frame() < target && driven < MAX_CATCHUP_BLOCKS {
            let next = (flow.virtual_frame() + crate::BLOCK_SIZE as u64).min(target);
            let chunk = (next - flow.virtual_frame()) as usize;
            store_provisional(flow, next);
            flow.render(&mut scratch[..chunk * CHANNELS]);
            driven += 1;
        }
    }

    /// Builds a flow at `start_frame`, inheriting this deck's transport (tempo and profile), and
    /// stamps its loop range before anything warms.
    fn make_flow(&mut self, start_frame: u64, range: Option<LoopRange>) -> Flow {
        let id = self.next_flow_id;
        self.next_flow_id = id.wrapping_add(1);
        let flow = Flow::new_with(
            id,
            Arc::clone(&self.pool),
            start_frame,
            None,
            self.flowshift.ready_sender(),
            self.playhead.playing_rate() as f32,
            self.profile,
        );
        flow.set_loop_range(range);
        flow
    }

    fn poll_ready_flows(&mut self) {
        while let Some(id) = self.flowshift.poll_ready() {
            if self.loop_arm.as_ref().is_some_and(|arm| arm.id == id) {
                let Some(flow) = self.flowshift.take_ready_flow(id) else {
                    continue;
                };
                let rate = self.playhead.playing_rate() as f32;
                let Some(arm) = self.loop_arm.as_mut() else {
                    continue; // disarmed between announcement and poll: fall through to reaping
                };
                arm.flow = Some(flow);
                let flow = arm.flow.as_mut().expect("just stored");
                // Land on the deck's transport; profile only if the deck changed it mid-warm
                // (that rebuild re-primes, which the catch-up drive then runs through).
                flow.set_ratio(rate);
                if flow.profile() != arm.profile {
                    let profile = arm.profile;
                    flow.set_profile(profile);
                }
            } else if let Some(pos) = self.loop_reap.iter().position(|&wid| wid == id) {
                self.loop_reap.remove(pos);
                let _ = self.flowshift.take_ready_flow(id);
            } else {
                self.switch_to(id);
            }
        }
    }

    fn switch_to(&mut self, flow_id: u64) {
        let Some(mut flow) = self.flowshift.take_ready_flow(flow_id) else {
            return;
        };
        // The target was computed when the command arrived; this deck kept playing while the flow
        // warmed up, so move the landing point forward by exactly that much — in the *virtual*
        // clock, the same clock the compensation is measured in. Otherwise every jump loses a block
        // of gap against a running sibling deck — and with a real time-stretch engine, whose warm-up
        // costs several blocks, the loss would grow with it.
        //
        // A loop exit is the exception: its landing is *not* `target + played` but exactly where
        // the looped flow's output stopped, so the stream is unbroken across the flow change — the
        // audible finishes its lap and continues past `out` without a seam.
        let landing = if std::mem::take(&mut self.pending_exit_resume) {
            self.flows
                .get(self.active_index)
                .map(Flow::actual_frame)
                .unwrap_or_else(|| flow.virtual_frame())
        } else {
            let played = self.virtual_frame().saturating_sub(self.cued_from);
            flow.virtual_frame() + played
        };
        if landing != flow.virtual_frame() {
            // `reset_to` re-warms *and* settles (priming drained) — the first block after the
            // switch is already converged audio, so no transition breathes a warm-up gap.
            flow.reset_to(landing);
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
        // The new flow was built at the rate it was spawned with; the playhead may have moved
        // since (a controller runs while it warms), so land it on today's rate rather than
        // replaying a stale one for a block.
        self.apply_rate(self.playhead.playing_rate().clamp(MIN_RATE, MAX_RATE));
        // The playhead moved because of the switch, not because of tempo — tell the controller not
        // to read the jump as a phase error.
        self.playhead.suppress();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_core::BeatGrid;
    use hypermixx_media::{DecodedAudio, PcmPool};

    fn pool(n_frames: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n_frames as usize)
                .flat_map(|i| [i as f32, i as f32])
                .collect(),
            total_frames: n_frames,
            sample_rate: crate::SAMPLE_RATE,
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
    fn an_empty_deck_is_silent_and_holds_no_audio() {
        let mut deck = Deck::empty();
        deck.play();
        let mut out = vec![1.0f32; 256 * CHANNELS];
        assert_eq!(deck.process_block(&mut out), 256);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(deck.total_frames(), 0);
        assert!(deck.is_at_end());
    }

    /// `pull_into` is a wrapper, not a second transport: it must agree with `process_block` exactly.
    #[test]
    fn pull_into_matches_process_block_sample_for_sample() {
        let mut via_bus = Deck::new(pool(10_000));
        let mut via_block = Deck::new(pool(10_000));
        via_bus.play();
        via_block.play();
        let ctx = crate::fx::FxContext::gridless(crate::SAMPLE_RATE, 256);
        let mut bus = Bus::stereo(256);
        let mut out = vec![0.0f32; 256 * CHANNELS];
        for block in 0..5 {
            assert_eq!(via_bus.pull_into(&mut bus, &ctx), 256);
            assert_eq!(via_block.process_block(&mut out), 256);
            for i in 0..256 {
                // The ramp source encodes the frame index in the sample value, so this also checks
                // that the de-interleave picked the right channel.
                assert_eq!(bus.l[i], out[i * CHANNELS], "block {block} frame {i} left");
                assert_eq!(bus.r[i], out[i * CHANNELS + 1], "block {block} frame {i} right");
            }
            assert_eq!(via_bus.current_frame(), via_block.current_frame());
        }
    }

    #[test]
    fn a_paused_deck_pulls_silence_into_a_bus_that_had_signal() {
        let mut deck = Deck::new(pool(10_000));
        deck.play();
        let ctx = crate::fx::FxContext::gridless(crate::SAMPLE_RATE, 256);
        let mut bus = Bus::stereo(256);
        deck.pull_into(&mut bus, &ctx);
        assert!(bus.peak() > 0.0);
        deck.pause();
        deck.pull_into(&mut bus, &ctx);
        assert!(bus.is_silent(), "a paused deck must zero the whole bus");
    }

    #[test]
    fn pull_into_zeroes_the_tail_of_an_oversized_bus() {
        // A bus longer than one transport block must not keep the previous block in it, or a mixer
        // configured for a bigger block would hear an echo of the last one.
        let mut deck = Deck::new(pool(10_000));
        deck.play();
        let ctx = crate::fx::FxContext::gridless(crate::SAMPLE_RATE, 256);
        let mut bus = Bus::stereo(1_024);
        deck.pull_into(&mut bus, &ctx);
        assert!(bus.l[300..].iter().all(|s| *s == 0.0), "tail was not cleared");
        assert!(bus.l[1] > 0.0, "the head must still carry audio");
    }

    #[test]
    fn pull_into_respects_a_short_bus() {
        let mut deck = Deck::new(pool(10_000));
        deck.play();
        let ctx = crate::fx::FxContext::gridless(crate::SAMPLE_RATE, 64);
        let mut bus = Bus::stereo(64);
        // The transport is asked for 64 frames, so it advances 64: same clock, smaller block.
        assert_eq!(deck.pull_into(&mut bus, &ctx), 64);
        assert_eq!(bus.frames(), 64);
        assert!(bus.peak() > 0.0);
        assert_eq!(deck.current_frame(), 64);
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
            beatgrid: BeatGrid::from_constant_bpm(122.0, 0, total_frames, crate::SAMPLE_RATE),
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
        let frames_per_beat: f64 = crate::SAMPLE_RATE as f64 * 60.0 / 122.0;
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
        let mut deck = Deck::new(pool(44_100 * 20));
        deck.set_analysis(grid_122bpm(48_000 * 20));
        deck.play();
        let mut out = vec![0.0f32; 256 * CHANNELS];

        // Cue somewhere inside beat 0, off the grid, then step four beats ahead.
        deck.jump(5_000);
        settle_at(&mut deck, &mut out, 5_000);
        let before = deck.current_frame();
        deck.beatjump(4);
        settle_at(&mut deck, &mut out, before + 4 * 21_000);
        let forward = deck.current_frame();
        assert!(
            (forward as i64 - (before as i64 + 4 * 21_689)).abs() <= 2 * 256,
            "four beats ahead should be ~86754 frames on: {before} -> {forward}"
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

    // ---------------------------------------------------------------------------- loop tests

    /// Frames per beat at 122 BPM on the engine rate.
    fn fpb() -> u64 {
        let g = BeatGrid::from_constant_bpm(122.0, 0, 0, crate::SAMPLE_RATE);
        g.beat_width(0)
    }

    fn loop_deck(total: u64) -> Deck {
        let deck = Deck::new(pool(total));
        deck.set_analysis(grid_122bpm(total));
        deck.play();
        deck
    }

    /// Drives until the loop engages (a warm-up hop happened), then a few more blocks.
    fn settle_loop(deck: &mut Deck, out: &mut [f32]) {
        for _ in 0..500 {
            deck.process_block(out);
            if deck.loop_range().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the loop never engaged");
    }

    #[test]
    fn beat_loop_folds_the_playhead_but_never_the_slip_clock() {
        let total = crate::SAMPLE_RATE as u64 * 60;
        let mut deck = loop_deck(total);
        let mut out = vec![0.0f32; 256 * CHANNELS];

        deck.apply_loop(LoopOp::Beats(4)).expect("beat loop");
        settle_loop(&mut deck, &mut out);
        let range = deck.loop_range().expect("engaged");
        let grid = deck.analysis().expect("grid").beatgrid.clone();
        let b = grid.floor_beat(range.in_frame);
        assert_eq!(
            range.out_frame - range.in_frame,
            grid.frame_at_beat(b + 4) - grid.frame_at_beat(b),
            "four beats on the grid (per-beat rounding, not a nominal fpb)"
        );
        assert_eq!(deck.virtual_frame(), deck.current_frame(), "no wrap yet");

        // Play past `out`: the audible position folds back, the slip clock keeps climbing.
        let out_frame = range.out_frame;
        for _ in 0..((6 * fpb()) / 256) as usize {
            deck.process_block(&mut out);
        }
        assert!(
            deck.virtual_frame() > out_frame,
            "slip clock must lap past `out`: virtual {} (out {out_frame})",
            deck.virtual_frame()
        );
        assert!(
            deck.current_frame() < out_frame && deck.current_frame() >= range.in_frame,
            "audible position must sit inside the loop: {}",
            deck.current_frame()
        );
        assert!(!deck.is_at_end(), "a looping deck is never at the end");
    }

    #[test]
    fn loop_audio_follows_the_mapped_position_across_the_wrap() {
        // The ramp encodes its frame index in the sample value, and Tape at unity is passthrough:
        // the first sample of every block *is* the deck's mapped position. That makes "the engine
        // reads through the mapping" checkable sample-exactly at the wrap itself.
        let total = crate::SAMPLE_RATE as u64 * 60;
        let mut deck = loop_deck(total);
        let mut out = vec![0.0f32; 256 * CHANNELS];

        deck.apply_loop(LoopOp::Beats(2)).expect("beat loop");
        settle_loop(&mut deck, &mut out);
        let range = deck.loop_range().expect("engaged");

        // Drive across at least one wrap; every block's first sample must equal the position the
        // deck reports (which is inside [in, out) after the fold, unlike the engine's own clock).
        let mut crossed = false;
        for _ in 0..((3 * fpb()) / 256) as usize {
            let before = deck.current_frame();
            deck.process_block(&mut out);
            assert_eq!(
                out[0],
                before as f32,
                "block audio must be the mapped position {before}"
            );
            if before + 256 > range.out_frame && deck.virtual_frame() > range.out_frame {
                crossed = true;
            }
        }
        assert!(crossed, "the loop should have wrapped at least once");
    }

    #[test]
    fn manual_loop_promotes_the_lockstep_flow_without_moving_the_clock() {
        let total = crate::SAMPLE_RATE as u64 * 60;
        let mut deck = loop_deck(total);
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.process_block(&mut out);

        deck.apply_loop(LoopOp::In).expect("arm");
        let armed = deck.loop_in_armed().expect("in is armed immediately");
        assert!(deck.loop_range().is_none(), "arming alone must not engage");
        // Run blocks until the LoopFlow finishes warming and arrives (the test lives in this
        // module, so the private arm is observable), then it is driven in lockstep.
        for _ in 0..500 {
            deck.process_block(&mut out);
            if deck.loop_arm.as_ref().is_some_and(|arm| arm.flow.is_some()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            deck.loop_arm.as_ref().is_some_and(|arm| arm.flow.is_some()),
            "the LoopFlow never finished warming"
        );
        let before = deck.virtual_frame();
        deck.apply_loop(LoopOp::Out).expect("engage");
        let range = deck.loop_range().expect("engaged");
        assert_eq!(range.in_frame, armed, "the armed in point is the loop's in");
        assert!(range.out_frame > range.in_frame);
        // Promotion: no jump, no compensation — the slip clock is untouched by the switch.
        assert_eq!(deck.virtual_frame(), before, "the promotion must not move the clock");
        // And the audio continues from exactly the mapped position (sample-exact on the ramp).
        let mapped = range.map(before);
        deck.process_block(&mut out);
        assert_eq!(out[0], mapped as f32, "first audible frame after promotion");
    }

    #[test]
    fn loop_exit_resumes_seamlessly_where_the_loop_was_playing() {
        let total = crate::SAMPLE_RATE as u64 * 60;
        let mut deck = loop_deck(total);
        let mut out = vec![0.0f32; 256 * CHANNELS];

        deck.apply_loop(LoopOp::Beats(4)).expect("beat loop");
        settle_loop(&mut deck, &mut out);
        let range = deck.loop_range().expect("engaged");
        // Run three laps: the slip clock climbs far past `out` while the audible wraps.
        for _ in 0..((4 * fpb()) / 256 * 3 + 8) as usize {
            deck.process_block(&mut out);
        }
        assert!(
            deck.virtual_frame() > range.in_frame + 2 * 4 * fpb(),
            "should have lapped at least twice: {}",
            deck.virtual_frame()
        );

        let pre = deck.current_frame();
        assert!(pre >= range.in_frame && pre < range.out_frame);
        deck.apply_loop(LoopOp::Exit).expect("exit");
        for _ in 0..500 {
            deck.process_block(&mut out);
            if deck.loop_range().is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(deck.loop_range().is_none(), "the range died with its flow");
        assert_eq!(
            deck.current_frame(),
            deck.virtual_frame(),
            "no mapping after exit"
        );

        // The exit does NOT jump — not to `out`, not to the slipped clock. The audible resumes
        // *inside* the loop, only the exit flow's warm-up further along (wrapped into loop space),
        // and plays the rest of the lap out from there.
        let landed = deck.current_frame();
        assert!(
            landed >= range.in_frame && landed < range.out_frame,
            "exit must resume inside the loop: landed {landed}, loop {}..{}",
            range.in_frame,
            range.out_frame
        );
        let len = (range.out_frame - range.in_frame) as i64;
        let advanced = (landed as i64 - pre as i64).rem_euclid(len);
        assert!(
            advanced <= 4096,
            "only the exit flow's warm-up may pass between press and switch: {advanced} frames"
        );

        // Sample-exact seam: the ramp makes the next block's first sample *be* the reported
        // position — the stream does not splice anywhere across the flow change.
        let before = deck.current_frame();
        deck.process_block(&mut out);
        assert_eq!(
            out[0], before as f32,
            "audio must continue exactly at the reported position"
        );

        // …and with no loop left wrapping it, that stream plays the lap out and flows past `out`.
        for _ in 0..((range.out_frame - before) / 256 + 8) as usize {
            if deck.current_frame() > range.out_frame {
                break;
            }
            deck.process_block(&mut out);
        }
        assert!(
            deck.current_frame() > range.out_frame,
            "the lap must play out and continue past the loop's end: {}",
            deck.current_frame()
        );
    }

    #[test]
    fn in_loop_edits_retime_the_range_and_leave_the_clock_alone() {
        let total = crate::SAMPLE_RATE as u64 * 60;
        let mut deck = loop_deck(total);
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.apply_loop(LoopOp::Beats(4)).expect("beat loop");
        settle_loop(&mut deck, &mut out);
        let original = deck.loop_range().expect("engaged");
        let clock = deck.virtual_frame();
        let grid = deck.analysis().expect("grid").beatgrid.clone();
        let b = grid.floor_beat(original.in_frame);
        // A distance of `n` beats *from the in point*, exactly as the grid lays it out.
        let beats_from_in = |n: u64| grid.frame_at_beat(b + n) - grid.frame_at_beat(b);

        // halve/double through the beat-loop verb (decision 5): out moves, in and clock don't.
        deck.apply_loop(LoopOp::Beats(2)).expect("halve");
        let halved = deck.loop_range().expect("still looping");
        assert_eq!(halved.in_frame, original.in_frame, "`in` must not move");
        assert_eq!(halved.out_frame - halved.in_frame, beats_from_in(2));
        assert_eq!(deck.virtual_frame(), clock, "an edit never moves the clock");

        deck.apply_loop(LoopOp::Beats(8)).expect("double");
        let doubled = deck.loop_range().expect("still looping");
        assert_eq!(doubled.out_frame - doubled.in_frame, beats_from_in(8));

        // A length edit floors at one quantum (default: one beat) and works in fractional beats.
        let beat = grid.beat_width(b);
        deck.apply_loop(LoopOp::Edit(LoopEditOp::Length { beats: 0.1 }))
            .expect("floor");
        let floored = deck.loop_range().unwrap();
        assert_eq!(
            floored.out_frame - floored.in_frame,
            beat,
            "0.1 beats floors at the quantum (1 beat)"
        );
        deck.apply_loop(LoopOp::Edit(LoopEditOp::Length { beats: 3.5 }))
            .expect("fractional length");
        let length = deck.loop_range().unwrap();
        assert_eq!(
            length.out_frame - length.in_frame,
            (3.5 * beat as f64).round() as u64
        );

        // Shift the whole loop: length preserved, clock untouched, moved by exactly the grid's
        // two-beat distance from the (on-beat) in point.
        let before = deck.loop_range().unwrap();
        deck.apply_loop(LoopOp::Edit(LoopEditOp::Move { beats: 2 }))
            .expect("move");
        let moved = deck.loop_range().unwrap();
        assert_eq!(moved.out_frame - moved.in_frame, before.out_frame - before.in_frame);
        assert_eq!(moved.in_frame, before.in_frame + beats_from_in(2));
        assert_eq!(deck.virtual_frame(), clock, "still no clock movement");

        // Moving `out` / `in` individually pins the other end; the distance moved is the grid's
        // two-beat span up to the per-beat rounding of phase-preserving shift (±2 frames).
        deck.apply_loop(LoopOp::Edit(LoopEditOp::Out { beats: -2 }))
            .expect("out back");
        let shrunk = deck.loop_range().unwrap();
        assert_eq!(shrunk.in_frame, moved.in_frame, "`in` stays pinned");
        let out_delta = moved.out_frame - shrunk.out_frame;
        assert!(
            (out_delta as i64 - beats_from_in(2) as i64).abs() <= 2,
            "out should move back two beats, moved {out_delta}"
        );
        deck.apply_loop(LoopOp::Edit(LoopEditOp::In { beats: -2 }))
            .expect("in back");
        let grown = deck.loop_range().unwrap();
        assert_eq!(grown.out_frame, shrunk.out_frame, "`out` stays pinned");
        let in_delta = moved.in_frame - grown.in_frame;
        assert!(
            (in_delta as i64 - beats_from_in(2) as i64).abs() <= 2,
            "in should move back two beats, moved {in_delta}"
        );
    }

    #[test]
    fn halve_and_double_are_dedicated_keys_clamped_to_a_32nd_through_64_beats() {
        let total = crate::SAMPLE_RATE as u64 * 60;
        let mut deck = loop_deck(total);
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.apply_loop(LoopOp::Beats(4)).expect("beat loop");
        settle_loop(&mut deck, &mut out);
        let engaged = deck.loop_range().expect("engaged");
        let clock = deck.virtual_frame();
        let grid = deck.analysis().expect("grid").beatgrid.clone();
        let beat = grid.beat_width(grid.floor_beat(engaged.in_frame)).max(1);
        let min_len = ((beat as f64) / 32.0).round().max(1.0) as u64;
        let max_len = beat.saturating_mul(64);
        let len = |deck: &Deck| deck.loop_range().expect("still looping").len();

        // One double is exactly ×2 — scaling the *current* length, not a grid figure.
        let base = engaged.len();
        deck.apply_loop(LoopOp::Edit(LoopEditOp::Double)).expect("double");
        assert_eq!(len(&deck), base * 2, "double must scale the current length");
        assert_eq!(
            deck.loop_range().unwrap().in_frame,
            engaged.in_frame,
            "`in` stays pinned"
        );
        assert_eq!(deck.virtual_frame(), clock, "an edit never moves the clock");

        // Keep doubling: the length saturates exactly at 64 beats and goes no further.
        let mut guard = 0;
        while len(&deck) < max_len {
            deck.apply_loop(LoopOp::Edit(LoopEditOp::Double)).expect("double");
            guard += 1;
            assert!(guard <= 8, "doubles must reach the 64-beat ceiling");
            assert!(len(&deck) <= max_len, "…and never exceed it");
        }
        assert_eq!(len(&deck), max_len, "the ceiling is exactly 64 beats");
        deck.apply_loop(LoopOp::Edit(LoopEditOp::Double)).expect("double");
        assert_eq!(len(&deck), max_len, "already at the ceiling");

        // Halving walks all the way down to exactly 1/32 beat — below the beat quantum — and
        // stops there instead of collapsing the range.
        let mut guard = 0;
        while len(&deck) > min_len {
            deck.apply_loop(LoopOp::Edit(LoopEditOp::Halve)).expect("halve");
            guard += 1;
            assert!(guard <= 16, "halves must reach the 1/32-beat floor");
            assert!(len(&deck) >= min_len, "…and never fall under it");
        }
        assert_eq!(len(&deck), min_len, "the floor is exactly 1/32 beat");
        deck.apply_loop(LoopOp::Edit(LoopEditOp::Halve)).expect("halve");
        assert_eq!(len(&deck), min_len, "already at the floor");
        assert_eq!(
            deck.loop_range().unwrap().in_frame,
            engaged.in_frame,
            "`in` still pinned after both directions"
        );
        assert_eq!(deck.virtual_frame(), clock, "clock untouched throughout");

        // A 1/32-beat loop is a legal, *playable* loop: drive a few blocks (several laps of it —
        // the feed splits multi-wrap reads) and it is still looping, still inside its range.
        for _ in 0..4 {
            deck.process_block(&mut out);
            let now = deck.current_frame();
            let range = deck.loop_range().expect("still looping");
            assert!(now >= range.in_frame && now < range.out_frame, "in range: {now}");
        }

        // …and with no loop running, both keys are refusals rather than no-ops.
        deck.apply_loop(LoopOp::Exit).expect("exit");
        for _ in 0..500 {
            if deck.loop_range().is_none() {
                break;
            }
            deck.process_block(&mut out);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(deck.loop_range().is_none(), "the exit landed");
        assert!(
            deck.apply_loop(LoopOp::Edit(LoopEditOp::Halve)).is_err(),
            "halve without a loop"
        );
        assert!(
            deck.apply_loop(LoopOp::Edit(LoopEditOp::Double)).is_err(),
            "double without a loop"
        );
    }

    #[test]
    fn loop_boundary_cases_match_the_spec_table() {
        let total = crate::SAMPLE_RATE as u64 * 60;
        let mut deck = loop_deck(total);
        let mut out = vec![0.0f32; 256 * CHANNELS];

        // `out` without an armed `in` is an error, not a silent no-op.
        assert!(deck.apply_loop(LoopOp::Out).is_err(), "unarmed out");
        // `out ≤ in` (an early press) falls back to the one-beat minimum.
        deck.apply_loop(LoopOp::In).expect("arm");
        // Force a degenerate quantization: quantize the out against a position *before* `in` by
        // rewinding the in point past the current position — an `In` edit can't do that (no loop
        // yet), so instead verify the rule directly through `quantize_offset`.
        let grid = deck.analysis().expect("grid").beatgrid.clone();
        let in_frame = grid.frame_at_beat(10);
        assert_eq!(
            quantize_offset(&grid, in_frame, in_frame - 5, LoopQuantum::Beat),
            Some(in_frame + grid.beat_width(10)),
            "out ≤ in → minimum one beat"
        );

        // Pressing `in` again replaces the arm instead of stacking one.
        deck.apply_loop(LoopOp::In).expect("re-arm");
        assert!(deck.loop_in_armed().is_some());
        // The abandoned LoopFlow announces eventually and must be reaped, not switched in:
        for _ in 0..16 {
            deck.process_block(&mut out);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(deck.loop_range().is_none(), "a reap must not engage a loop");
        assert!(deck.loop_in_armed().is_some(), "the new arm survives");

        // Cancel drops the arm entirely.
        deck.apply_loop(LoopOp::Cancel).expect("cancel");
        assert_eq!(deck.loop_in_armed(), None);
        for _ in 0..8 {
            deck.process_block(&mut out);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(deck.loop_range().is_none());

        // A zero-beat loop is refused.
        assert!(deck.apply_loop(LoopOp::Beats(0)).is_err(), "0-beat loop");
        // Editing with no loop is refused.
        assert!(
            deck
                .apply_loop(LoopOp::Edit(LoopEditOp::Move { beats: 1 }))
                .is_err(),
            "edit without a loop"
        );
        // Loop commands needing a grid refuse without one; the idempotent ones (cancel/exit) and
        // the grid-independent one (quantum) stay usable.
        let mut gridless = Deck::new(pool(total));
        gridless.play();
        for op in [
            LoopOp::In,
            LoopOp::Out,
            LoopOp::Exit,
            LoopOp::Beats(4),
            LoopOp::Cancel,
            LoopOp::SetQuantum(LoopQuantum::Half),
        ] {
            match op {
                LoopOp::Cancel | LoopOp::Exit | LoopOp::SetQuantum(_) => {
                    assert!(gridless.apply_loop(op).is_ok(), "{op:?} needs no grid")
                }
                other => assert!(gridless.apply_loop(other).is_err(), "{other:?} without a grid"),
            }
        }
        assert_eq!(deck.loop_in_armed(), None);
    }

    #[test]
    fn rearming_after_looping_exits_the_running_loop_first() {
        let total = crate::SAMPLE_RATE as u64 * 60;
        let mut deck = loop_deck(total);
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.apply_loop(LoopOp::Beats(4)).expect("beat loop");
        settle_loop(&mut deck, &mut out);

        // 循环中再按 in → 退出当前,开新: the arm appears at once, the running loop goes away
        // on the exit flow's switch.
        deck.apply_loop(LoopOp::In).expect("re-arm over a loop");
        assert!(deck.loop_in_armed().is_some(), "armed immediately");
        for _ in 0..40 {
            deck.process_block(&mut out);
            if deck.loop_range().is_none() && deck.loop_in_armed().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(deck.loop_range().is_none(), "the old loop must exit");
        assert!(deck.loop_in_armed().is_some(), "the new arm must survive");
    }

    #[test]
    fn a_jump_voids_an_armed_loop_in() {
        let total = crate::SAMPLE_RATE as u64 * 60;
        let mut deck = loop_deck(total);
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.apply_loop(LoopOp::In).expect("arm");
        assert!(deck.loop_in_armed().is_some());
        deck.beatjump(8);
        assert_eq!(deck.loop_in_armed(), None, "jumping re-cues: the arm is gone");
        for _ in 0..16 {
            deck.process_block(&mut out);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(deck.loop_range().is_none(), "the abandoned LoopFlow must be reaped");
    }

    #[test]
    fn a_loop_near_the_end_clamps_out_to_the_track() {
        // 10 beats total; ask for a loop of 4 from the last beat.
        let total = (fpb() * 10) + 100;
        let mut deck = loop_deck(total);
        let mut out = vec![0.0f32; 256 * CHANNELS];
        deck.jump(fpb() * 8);
        settle_at(&mut deck, &mut out, fpb() * 8);
        deck.apply_loop(LoopOp::Beats(4)).expect("beat loop");
        settle_loop(&mut deck, &mut out);
        let range = deck.loop_range().expect("engaged");
        assert!(range.out_frame <= total, "out clamped to the track end");
        assert!(range.out_frame > range.in_frame, "still a legal range");
    }
}
