//! `SyncGroup`: who leads, who follows, and the one shared BPM.
//!
//! This is **not** part of the mixer. The mixer's job is to turn decks into samples; beat-sync is
//! an editorial decision *about* the decks that happens to be driven from the same thread, so it
//! lives here — its own module, owned by the producer next to the mixer, reaching the decks through
//! the accessors the mixer already exposes. `Mixer::process` knows nothing about tempo or phase.
//!
//! ```text
//! producer ──► SyncGroup::prepare(&mut mixer)   snapshot + hand each deck its view
//! producer ──► Mixer::process(&ctx)             pure audio: decks → buses → outputs
//! ```
//!
//! The snapshot must happen **before** the mixer renders anything: a follower compares itself
//! against its leader's position from the *same* block, not last block's.
//!
//! Three things are rules rather than options:
//!
//! * **One direction only.** A pair is tracked or locked one way; the reverse is refused, so two
//!   decks can never end up following each other into a loop.
//! * **`tempo` and `nudgerate` never mix.** The group's BPM comes from a deck's *tempo* (its fader),
//!   never from `playing_rate`, so a nudge or a correction stays personal instead of feeding back
//!   into the shared speed.
//! * **`phase` includes `tempo`, `phaselock` includes `tempolock`.** Both locks are built the same
//!   way; `phaselock` just stops the leader from being driven by the group.

use hypermixx_core::{DeckId, NudgeOp, SyncOp};

use crate::deck::{LeaderSample, PhaseAlign, SyncCtx, MAX_RATE, MIN_RATE};
use crate::mixer::Mixer;

/// Why sync needs a grid the deck does not have.
const NO_GRID: &str = "sync needs a beat grid — `analyse` the track, or load it with a bpm";

/// What a sync group is doing to a pair of decks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncMode {
    /// No lock: `tempo` belongs to each deck alone.
    #[default]
    Free,
    /// Bidirectional shared `group_bpm`: either deck's fader moves both.
    Tempolock,
    /// One-way: only the follower derives its tempo from the group, which tracks the leader.
    Phaselock,
}

impl SyncMode {
    /// A stable label for state readouts and errors.
    pub fn label(&self) -> &'static str {
        match self {
            SyncMode::Free => "free",
            SyncMode::Tempolock => "tempolock",
            SyncMode::Phaselock => "phaselock",
        }
    }
}

/// The sync group's state: its leader, who is tracking it, the mode they are in, shared BPM, and
/// this block's positions.
#[derive(Debug, Default)]
pub struct SyncGroup {
    /// Who leads. Chosen by `sync set-leader`, otherwise fixed by the first sync command.
    pub leader: Option<DeckId>,
    /// Whether [`leader`](Self::leader) was chosen on purpose. An explicit leader never syncs to
    /// anything — you named it *because* the other deck follows it.
    explicit: bool,
    /// Who follows [`leader`](Self::leader), while anyone is tracking it at all.
    pub follower: Option<DeckId>,
    /// What the pair is doing.
    pub mode: SyncMode,
    /// The shared BPM. Under `tempolock` only a fader writes it; under `phaselock` every block
    /// takes it from the leader's tempo over its own grid.
    pub group_bpm: f64,
    /// This block's positions, one slot per channel, filled by [`SyncGroup::prepare`].
    samples: Vec<Option<LeaderSample>>,
}

impl SyncGroup {
    /// The label of the running mode, for state readouts.
    pub fn mode_label(&self) -> &'static str {
        self.mode.label()
    }

    /// The per-block step, run immediately before the mixer renders: snapshot every deck's
    /// position, take the group's BPM off the leader (one-way, so the leader's moves flow down and
    /// nothing flows back up), then hand each deck the view it needs for this block.
    pub fn prepare(&mut self, mixer: &mut Mixer) {
        let count = mixer.channel_count();
        self.samples.clear();
        for index in 0..count {
            let sample = mixer.deck(index).and_then(|deck| {
                deck.beat_phase().map(|phase| LeaderSample {
                    deck_id: index as DeckId,
                    phase: Some(phase),
                    bpm: deck.bpm_at_frame(),
                    tempo: deck.tempo(),
                })
            });
            self.samples.push(sample);
        }
        if self.mode == SyncMode::Phaselock {
            if let Some(leader) = self.leader_sample() {
                if leader.bpm > 0.0 {
                    self.group_bpm = leader.effective_bpm();
                }
            }
        }
        for index in 0..count {
            let ctx = self.ctx(index as DeckId);
            if let Some(deck) = mixer.deck_mut(index) {
                deck.set_sync(Some(ctx));
            }
        }
    }

    /// This block's view for `deck_id`: the shared BPM plus whoever it tracks.
    pub fn ctx(&self, deck_id: DeckId) -> SyncCtx {
        SyncCtx {
            group_bpm: self.group_bpm,
            leader: self.tracked_by(deck_id),
        }
    }

    /// The leader sample, when `deck_id` is not the leader.
    fn tracked_by(&self, deck_id: DeckId) -> Option<LeaderSample> {
        let leader = self.leader?;
        if leader == deck_id {
            return None;
        }
        // A leader with no grid has no phase to compare against; treating that as "tracking" would
        // silently hold the follower at zero error, which is worse than doing nothing.
        self.samples
            .get(leader as usize)
            .copied()
            .flatten()
            .filter(|sample| sample.phase.is_some() && sample.bpm > 0.0)
    }

    /// The leader's own sample, whatever the asking deck is.
    fn leader_sample(&self) -> Option<LeaderSample> {
        let leader = self.leader?;
        self.samples.get(leader as usize).copied().flatten()
    }

    /// Records `target` following `leader`, refusing a direction that would close a loop or
    /// contradict a lock that is already running.
    fn track(&mut self, leader: DeckId, target: DeckId) -> Result<(), String> {
        if leader == target {
            return Err(format!("deck{target} cannot sync to itself"));
        }
        if let Some(follower) = self.follower {
            let known = self.leader;
            if known == Some(target) && follower == leader {
                return Err(format!(
                    "deck{target} and deck{leader} would follow each other — deck{leader} already \
                     tracks deck{target}; `sync unlock` first"
                ));
            }
            if self.mode != SyncMode::Free && !(known == Some(leader) && follower == target) {
                return Err(format!(
                    "deck{l} → deck{f} is already locked — `sync unlock` first",
                    l = known.unwrap_or(target),
                    f = follower
                ));
            }
        }
        self.leader = Some(leader);
        self.explicit = false;
        self.follower = Some(target);
        Ok(())
    }

    /// The deck `target` follows: the recorded leader, else the only other deck there is.
    ///
    /// With more than two decks and no explicit leader the answer is genuinely ambiguous, and a
    /// guess that happens to be wrong puts both tracks at the wrong tempo — so it is refused and
    /// the user is told which command settles it.
    fn resolve_leader(&self, mixer: &Mixer, target: DeckId) -> Result<DeckId, String> {
        match self.leader {
            Some(leader) if leader != target => Ok(leader),
            Some(leader) if self.explicit => Err(format!(
                "deck{target} is the leader (deck{leader}) — sync another deck to it, or move the \
                 lead with `deck<id> sync set-leader`"
            )),
            _ => {
                let mut others = (0..mixer.channel_count() as DeckId).filter(|&id| id != target);
                match (others.next(), others.next()) {
                    (Some(only), None) => Ok(only),
                    _ => Err(format!(
                        "no leader for deck{target} — with this many decks pick one with \
                         `deck<id> sync set-leader`"
                    )),
                }
            }
        }
    }

    /// Both BPMs a tempo match needs, read at the positions the two decks are actually at.
    fn match_bpm(mixer: &Mixer, leader: DeckId, target: DeckId) -> Result<(f64, f64), String> {
        let own = mixer
            .deck(target as usize)
            .map(crate::deck::Deck::bpm_at_frame)
            .unwrap_or(0.0);
        let theirs = mixer
            .deck(leader as usize)
            .map(crate::deck::Deck::bpm_at_frame)
            .unwrap_or(0.0);
        if own <= 0.0 {
            return Err(NO_GRID.to_owned());
        }
        if theirs <= 0.0 {
            return Err(format!(
                "leader deck{leader} has no beat grid — `analyse` it first"
            ));
        }
        Ok((theirs, own))
    }

    /// Refuses to sync something that is not holding a track, with the message the transport uses.
    fn require_track(mixer: &Mixer, deck_id: DeckId) -> Result<(), String> {
        let count = mixer.channel_count();
        let Some(deck) = mixer.deck(deck_id as usize) else {
            return Err(crate::mixer::unknown_deck(deck_id, count));
        };
        if deck.total_frames() == 0 {
            return Err(format!(
                "deck {deck_id} holds no track, use `load {deck_id} <path>`"
            ));
        }
        Ok(())
    }

    /// Applies one `sync` command.
    pub fn handle_sync(
        &mut self,
        mixer: &mut Mixer,
        deck_id: DeckId,
        op: SyncOp,
    ) -> Result<(), String> {
        match op {
            SyncOp::SetLeader => self.set_leader(mixer, deck_id),
            SyncOp::Unlock => {
                Self::require_track(mixer, deck_id)?;
                self.unlock(mixer, deck_id);
                Ok(())
            }
            SyncOp::Tempo => {
                self.tempo_must_be_free()?;
                self.sync_tempo(mixer, deck_id)
            }
            SyncOp::Phase { mode, t_seconds } => {
                // Validate before mutating: a bogus duration must not half-apply a sync.
                let align = PhaseAlign::from_mode(mode, t_seconds)?;
                // Under a lock the group already owns the tempo, so a one-shot match would be
                // written and then overwritten by the very next block's recompute. Re-check the
                // direction and install the correction — nothing else.
                if self.mode == SyncMode::Free {
                    self.sync_tempo(mixer, deck_id)?;
                } else {
                    self.hold(mixer, deck_id)?;
                }
                self.apply_align(mixer, deck_id, align)
            }
            SyncOp::TempoLock => self.sync_tempolock(mixer, deck_id),
            SyncOp::PhaseLock { mode, t_seconds } => {
                let align = PhaseAlign::from_mode(mode, t_seconds)?;
                self.sync_phaselock(mixer, deck_id, align)
            }
        }
    }

    /// `deck<id> sync set-leader`: the target *is* the leader.
    fn set_leader(&mut self, mixer: &mut Mixer, deck_id: DeckId) -> Result<(), String> {
        Self::require_track(mixer, deck_id)?;
        if self.mode != SyncMode::Free {
            return Err(format!(
                "already locked (deck{} → deck{}) — `sync unlock` first",
                self.leader.unwrap_or(deck_id),
                self.follower.unwrap_or(deck_id)
            ));
        }
        self.leader = Some(deck_id);
        self.explicit = true;
        // Direction is being restated; keeping a stale follower would refuse the next command.
        self.follower = None;
        Ok(())
    }

    /// Refuses a one-shot tempo match on a pair the group already drives.
    ///
    /// The match would write `tempo`, and the next block recomputes `tempo` from `group_bpm` —
    /// so the command would answer `Ok` while changing nothing. A discrete command that does
    /// nothing has to say so; silently succeeding is worse than refusing. (The *fader* under a
    /// phaselocked follower is the opposite case: a continuous control must not raise an error
    /// mid-performance, so that one is ignored on purpose.)
    fn tempo_must_be_free(&self) -> Result<(), String> {
        if self.mode == SyncMode::Free {
            return Ok(());
        }
        Err(format!(
            "this pair is already locked ({}) — its tempo follows the group, so a one-shot match \
             has nothing to do; `sync unlock` first",
            self.mode.label()
        ))
    }

    /// Re-checks the direction without touching any tempo: the branch a locked pair takes, where
    /// changing the phase mode is meaningful but re-matching the tempo is not.
    fn hold(&mut self, mixer: &Mixer, deck_id: DeckId) -> Result<(), String> {
        Self::require_track(mixer, deck_id)?;
        let leader = self.resolve_leader(mixer, deck_id)?;
        self.track(leader, deck_id)
    }

    /// One-shot tempo match: nothing is locked afterwards, so a later fader move on the leader
    /// does not drag the other deck with it.
    fn sync_tempo(&mut self, mixer: &mut Mixer, deck_id: DeckId) -> Result<(), String> {
        Self::require_track(mixer, deck_id)?;
        let leader = self.resolve_leader(mixer, deck_id)?;
        let (leader_bpm, own_bpm) = Self::match_bpm(mixer, leader, deck_id)?;
        self.track(leader, deck_id)?;
        let rate = (leader_bpm / own_bpm).clamp(MIN_RATE, MAX_RATE);
        if let Some(deck) = mixer.deck_mut(deck_id as usize) {
            deck.set_tempo(rate);
        }
        Ok(())
    }

    /// Bidirectional shared tempo: both decks re-derive their rate from one `group_bpm` every
    /// block, so whichever fader moves, the other deck follows on the next block.
    fn sync_tempolock(&mut self, mixer: &mut Mixer, deck_id: DeckId) -> Result<(), String> {
        Self::require_track(mixer, deck_id)?;
        let leader = self.resolve_leader(mixer, deck_id)?;
        let (leader_bpm, own_bpm) = Self::match_bpm(mixer, leader, deck_id)?;
        self.track(leader, deck_id)?;
        let leader_tempo = mixer.deck(leader as usize).map(crate::deck::Deck::tempo).unwrap_or(1.0);
        self.mode = SyncMode::Tempolock;
        // Seed the group with the leader's current tempo: locking must not nudge anything.
        self.group_bpm = leader_tempo * leader_bpm;
        if let Some(deck) = mixer.deck_mut(leader as usize) {
            deck.set_lock(true);
        }
        if let Some(deck) = mixer.deck_mut(deck_id as usize) {
            deck.set_lock(true);
            deck.set_tempo(self.group_bpm / own_bpm);
        }
        Ok(())
    }

    /// One-way: only `deck_id` derives its tempo from the group, and the group tracks the leader.
    fn sync_phaselock(
        &mut self,
        mixer: &mut Mixer,
        deck_id: DeckId,
        align: PhaseAlign,
    ) -> Result<(), String> {
        Self::require_track(mixer, deck_id)?;
        let leader = self.resolve_leader(mixer, deck_id)?;
        let (leader_bpm, own_bpm) = Self::match_bpm(mixer, leader, deck_id)?;
        self.track(leader, deck_id)?;
        let leader_tempo = mixer.deck(leader as usize).map(crate::deck::Deck::tempo).unwrap_or(1.0);
        self.mode = SyncMode::Phaselock;
        self.group_bpm = leader_tempo * leader_bpm;
        // The leader is the source of truth, so it is *not* group-driven; only the follower is.
        if let Some(deck) = mixer.deck_mut(leader as usize) {
            deck.set_lock(false);
        }
        if let Some(deck) = mixer.deck_mut(deck_id as usize) {
            deck.set_lock(true);
            deck.set_tempo(self.group_bpm / own_bpm);
        }
        self.apply_align(mixer, deck_id, align)
    }

    /// Installs a phase correction — or, for `Instant`, performs it as the flow switch it is and
    /// leaves no controller behind to react to the jump it just caused.
    fn apply_align(
        &mut self,
        mixer: &mut Mixer,
        deck_id: DeckId,
        align: PhaseAlign,
    ) -> Result<(), String> {
        if matches!(align, PhaseAlign::Instant) {
            let target = self.instant_target(mixer, deck_id)?;
            if let Some(deck) = mixer.deck_mut(deck_id as usize) {
                deck.jump(target);
                deck.set_align(None);
            }
            return Ok(());
        }
        if let Some(deck) = mixer.deck_mut(deck_id as usize) {
            deck.set_align(Some(align));
        }
        Ok(())
    }

    /// Where an `Instant` correction should land: inside this deck's current beat, at the phase
    /// the leader is at. Using the follower's own beat keeps the move to half a beat at most and
    /// stays correct on a grid whose beats are not all the same length.
    fn instant_target(&self, mixer: &Mixer, deck_id: DeckId) -> Result<u64, String> {
        let leader = self.resolve_leader(mixer, deck_id)?;
        let leader_phase = mixer
            .deck(leader as usize)
            .and_then(crate::deck::Deck::beat_phase)
            .ok_or_else(|| {
                format!("leader deck{leader} has no beat grid — `analyse` it first")
            })?;
        let deck = mixer
            .deck(deck_id as usize)
            .ok_or_else(|| crate::mixer::unknown_deck(deck_id, mixer.channel_count()))?;
        let analysis = deck.require_grid()?;
        let grid = &analysis.beatgrid;
        let position = deck.current_frame();
        let floor = grid.floor_beat(position);
        let offset = (f64::from(leader_phase) * grid.beat_width(floor) as f64).round() as u64;
        Ok(grid.frame_at_beat(floor).saturating_add(offset))
    }

    /// Drops the lock, every phase correction and every bend on the pair, keeping all tempos.
    fn unlock(&mut self, mixer: &mut Mixer, deck_id: DeckId) {
        // The named deck plus whoever the group tied to it: a lock covers two decks, so unlocking
        // one must not leave the other still taking its tempo from a group that no longer exists.
        for member in [Some(deck_id), self.leader, self.follower]
            .into_iter()
            .flatten()
        {
            if let Some(deck) = mixer.deck_mut(member as usize) {
                deck.clear_sync();
            }
        }
        self.mode = SyncMode::Free;
        self.follower = None;
        self.group_bpm = 0.0;
        // The leader survives `sync unlock`: `set-leader` is a statement about who leads, not part
        // of the lock being dropped.
    }

    /// Applies the DJ's tempo fader through whatever sync mode is running.
    ///
    /// * `phaselock`, following deck — ignored, its rate belongs to the leader.
    /// * `tempolock` — writes the shared BPM instead of one deck, so the other follows next block.
    /// * anything else — the deck's own `tempo`.
    pub fn set_tempo(&mut self, mixer: &mut Mixer, deck_id: DeckId, rate: f32) -> Result<(), String> {
        Self::require_track(mixer, deck_id)?;
        if !rate.is_finite() || rate <= 0.0 {
            return Err(format!("tempo must be a positive ratio, got {rate}"));
        }
        if self.mode == SyncMode::Phaselock && self.follower == Some(deck_id) {
            return Ok(());
        }
        let rate = f64::from(rate);
        let own_bpm = mixer
            .deck(deck_id as usize)
            .map(crate::deck::Deck::bpm_at_frame)
            .unwrap_or(0.0);
        let locked = mixer.deck(deck_id as usize).is_some_and(crate::deck::Deck::lock);
        if self.mode == SyncMode::Tempolock && locked && own_bpm > 0.0 {
            self.group_bpm = rate * own_bpm;
        }
        if let Some(deck) = mixer.deck_mut(deck_id as usize) {
            deck.set_tempo(rate);
        }
        Ok(())
    }

    /// Applies one `nudge`: a bend of `nudgerate` only, so it works while a lock runs without
    /// touching the tempo the lock owns.
    pub fn nudge(&mut self, mixer: &mut Mixer, deck_id: DeckId, op: NudgeOp) -> Result<(), String> {
        Self::require_track(mixer, deck_id)?;
        match op {
            NudgeOp::Start { delta, seconds } => {
                if !delta.is_finite() {
                    return Err(format!("nudge must be a number, got {delta}"));
                }
                if let Some(seconds) = seconds {
                    if !seconds.is_finite() || seconds < 0.0 {
                        return Err(format!(
                            "nudge duration must be a positive number of seconds, got {seconds}"
                        ));
                    }
                }
                if let Some(deck) = mixer.deck_mut(deck_id as usize) {
                    deck.start_nudge(f64::from(delta), seconds);
                }
                Ok(())
            }
            NudgeOp::Stop => {
                if let Some(deck) = mixer.deck_mut(deck_id as usize) {
                    deck.stop_nudge();
                }
                Ok(())
            }
        }
    }
}
