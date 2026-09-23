//! Sync: the two rates a deck plays at, and the phase controller stacked on top.
//!
//! A deck's engine rate is `playing_rate = tempo + nudgerate`:
//!
//! * **`tempo`** — the *stable* rate. The DJ's tempo fader, a one-shot `sync tempo` and a lock's
//!   group recompute all write here. It is what `sync unlock` deliberately keeps.
//! * **`nudgerate`** — the *temporary* rate. Phase tracking (`sync phase` / `sync phaselock`) and
//!   `nudge` both land here, so a correction never contaminates the tempo it is correcting. It
//!   returns to zero as soon as the error converges or the bend is released.
//!
//! Keeping them apart is what makes `sync unlock` "keep the speed" and what lets a nudge work
//! *while* a lock is running without fighting the group's BPM.
//!
//! ## Units
//!
//! Every phase comparison is converted to **seconds** before it reaches a controller:
//!
//! ```text
//! err_beats = wrap(leader_phase - own_phase)      // folded into [-0.5, 0.5]
//! err_sec   = err_beats * 60 / bpm                // seconds of playback to gain
//! pll_rate  = err_sec / t   |   pll.update(err_sec, dt)
//! ```
//!
//! `err_sec` (seconds) over `t` (seconds) is a plain ratio, which is the unit `nudgerate` needs —
//! so `kp`/`ki` and the ±5% clamp in [`Pll`] are dimensionally consistent instead of being a
//! number that happens to work at one BPM.

use hypermixx_core::{DeckId, PhaseMode};

/// Total ceiling on `nudgerate`: the phase controller's ±5% plus a single nudge's ±10%.
///
/// The controller is the backstop that stops a stuck correction from running away; the nudge is
/// the DJ's hand. Stacking them but bounding the sum keeps the engine inside a tempo change a
/// listener reads as "bent" rather than "broken".
pub const NUDGERATE_LIMIT: f64 = 0.15;
/// Ceiling on the phase controller alone — the ±5% convergence backstop.
pub const PLL_LIMIT: f64 = 0.05;
/// Ceiling on a single nudge bend.
pub const NUDGE_LIMIT: f64 = 0.10;
/// Engine tempo bounds, mirroring timestretch's `EngineConfig` clamp.
pub const MIN_RATE: f64 = 0.25;
pub const MAX_RATE: f64 = 4.0;
/// Seconds one processing block covers: the controller's `dt`.
pub const BLOCK_SECONDS: f64 = crate::BLOCK_SIZE as f64 / crate::SAMPLE_RATE as f64;

/// Wraps a phase difference (in beats) into `[-0.5, 0.5]`.
///
/// A raw `leader - follower` can be nearly a whole beat either way, and a controller fed `+0.9`
/// would sprint forward when the shortest correction is `−0.1`. Half a beat is a tie and stays put:
/// both directions close it in the same time.
pub fn wrap_phase(error: f64) -> f64 {
    let mut error = error;
    if error > 0.5 {
        error -= 1.0;
    } else if error < -0.5 {
        error += 1.0;
    }
    error
}

/// How a phase correction reaches alignment. The command-layer [`PhaseMode`] names the same three
/// choices; this is the runtime shape.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PhaseAlign {
    /// Land on the leader's phase in one flow switch. Handled at command time — it is a jump, not
    /// a controller, so it never sits here waiting to run.
    Instant,
    /// Close the gap at a fixed slope over `t_seconds`.
    Linear { t_seconds: f64 },
    /// PI controller.
    Pid(Pll),
}

impl PhaseAlign {
    /// Builds the runtime align for a command's mode. `t_seconds` only belongs to `Linear`.
    pub fn from_mode(mode: PhaseMode, t_seconds: Option<f64>) -> Result<Self, String> {
        match mode {
            PhaseMode::Instant => Ok(PhaseAlign::Instant),
            PhaseMode::Linear => {
                let t = t_seconds.unwrap_or(2.0);
                if !t.is_finite() || t <= 0.0 {
                    return Err(
                        "`sync phase linear` needs a positive duration, e.g. `sync phase linear 2.0`"
                            .into(),
                    );
                }
                Ok(PhaseAlign::Linear { t_seconds: t })
            }
            PhaseMode::Pid => Ok(PhaseAlign::Pid(Pll::new())),
        }
    }

    /// A stable label for state readouts and errors.
    pub fn label(&self) -> &'static str {
        match self {
            PhaseAlign::Instant => "instant",
            PhaseAlign::Linear { .. } => "linear",
            PhaseAlign::Pid(_) => "pid",
        }
    }
}

/// A PI controller over a phase error expressed in seconds.
///
/// `suppress` exists because a flow switch *moves* the playhead: the frame after a jump differs
/// from the one before it by the whole seek, and that is not a tempo error. Skipping exactly one
/// update keeps the controller from reacting to its own repositioning.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pll {
    /// Proportional gain, in `1/s`: how much of the remaining time error to gain back this second.
    kp: f64,
    /// Integral gain, in `1/s²`, for the steady error a proportional term alone leaves behind.
    ki: f64,
    /// Accumulated error. Clamped so a long-standing error cannot wind up into a lunge.
    integral: f64,
    last_output: f64,
    /// Skip the next update — the position moved for a reason other than tempo.
    suppress: bool,
}

impl Pll {
    /// The tuning the spec fixes: `kp` 1.0, `ki` 0.2, integral and output both limited to ±5%.
    pub fn new() -> Self {
        Self {
            kp: 1.0,
            ki: 0.2,
            integral: 0.0,
            last_output: 0.0,
            suppress: false,
        }
    }

    /// One controller step. `error` is the time still to be gained, in seconds; `dt` is the block.
    pub fn update(&mut self, error: f64, dt: f64) -> f64 {
        if self.suppress {
            self.suppress = false;
            return self.last_output;
        }
        self.integral += error * dt;
        self.integral = self.integral.clamp(-0.05, 0.05);
        let output = (self.kp * error + self.ki * self.integral).clamp(-PLL_LIMIT, PLL_LIMIT);
        self.last_output = output;
        output
    }

    /// Drop the next update: the playhead jumped (a flow switch), so its phase means nothing yet.
    pub fn suppress_next(&mut self) {
        self.suppress = true;
    }

    /// Forget the accumulated error — used when the correction is torn down.
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.last_output = 0.0;
        self.suppress = false;
    }
}

impl Default for Pll {
    fn default() -> Self {
        Self::new()
    }
}

/// A temporary rate bend: `nudge` writes here, and so does the phase controller's output.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Nudge {
    /// The rate asked for. Stays put until released; `applied` chases it.
    target: f64,
    /// The rate actually in force — smoothed, so a press is never a click.
    applied: f64,
    /// Blocks left before the bend releases itself; `None` means "hold until `Stop`".
    remaining_blocks: Option<u32>,
}

impl Nudge {
    /// Time constant of the bend's ramp: slow enough to be inaudible as a transient, short enough
    /// that a short timed nudge still reaches its target while it lasts.
    const TAU_SECONDS: f64 = 0.015;

    /// Starts bending by `delta` (clamped), optionally releasing itself after `seconds`.
    pub fn start(&mut self, delta: f64, seconds: Option<f64>) {
        self.target = delta.clamp(-NUDGE_LIMIT, NUDGE_LIMIT);
        self.remaining_blocks = seconds
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(|s| (s / BLOCK_SECONDS).ceil().max(1.0) as u32);
    }

    /// Releases the bend; [`rate`](Self::rate) then ramps to zero rather than snapping.
    pub fn stop(&mut self) {
        self.target = 0.0;
        self.remaining_blocks = None;
    }

    /// Advances the ramp one block and counts down a timed release.
    pub fn tick(&mut self) {
        if let Some(remaining) = self.remaining_blocks.as_mut() {
            *remaining = remaining.saturating_sub(1);
            if *remaining == 0 {
                self.remaining_blocks = None;
                self.target = 0.0;
            }
        }
        let alpha = 1.0 - (-BLOCK_SECONDS / Self::TAU_SECONDS).exp();
        self.applied += (self.target - self.applied) * alpha;
        // Snap the last few micro-frames so an idle deck reports exactly zero instead of `1e-9`.
        if self.target == 0.0 && self.applied.abs() < 1e-6 {
            self.applied = 0.0;
        }
    }

    /// The rate this bend currently contributes.
    pub fn rate(&self) -> f64 {
        self.applied
    }

    /// True while a bend is still moving toward or away from zero.
    pub fn active(&self) -> bool {
        self.target != 0.0 || self.applied != 0.0
    }
}

/// One deck's position, sampled before the block renders so every follower in the block compares
/// itself against the *same* instant of the leader.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LeaderSample {
    pub deck_id: DeckId,
    /// The deck's beat phase where the listener hears it, `None` while it has no grid.
    pub phase: Option<f32>,
    /// The BPM in force at that position, `0.0` while it has no grid.
    pub bpm: f64,
    /// The deck's *stable* tempo. A lock derives the group's BPM from this rather than from
    /// `playing_rate`, so a nudge or a phase correction stays personal instead of leaking into the
    /// shared tempo and coming back as a feedback loop.
    pub tempo: f64,
}

impl LeaderSample {
    /// The BPM the leader is actually playing at — its tempo over its own grid.
    pub fn effective_bpm(&self) -> f64 {
        self.tempo * self.bpm
    }
}

/// What one deck needs to know about the group for one block. Owned and `Copy`: the coordinator
/// hands each deck its own view right before it renders, so no borrow reaches across the block.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SyncCtx {
    /// The shared BPM a locked deck derives its tempo from (`0.0` while free).
    pub group_bpm: f64,
    /// Who to track — `None` when this deck *is* the leader, when no leader is set, or when the
    /// leader has no grid to compare against.
    pub leader: Option<LeaderSample>,
}

/// The four knobs a deck's rate is made of.
#[derive(Clone, Debug, PartialEq)]
pub struct Playhead {
    /// The stable rate: the DJ's fader, `sync tempo`, and the group recompute all write here.
    pub tempo: f64,
    /// The temporary rate: phase correction plus nudge, recomputed every block.
    pub nudgerate: f64,
    /// The active phase correction, if any.
    pub align: Option<PhaseAlign>,
    /// Whether this deck derives its `tempo` from the group's shared BPM.
    pub lock: bool,
    nudge: Nudge,
}

impl Default for Playhead {
    fn default() -> Self {
        Self::new()
    }
}

impl Playhead {
    /// Unity tempo, no correction, no lock: exactly what an unloaded deck plays at.
    pub fn new() -> Self {
        Self {
            tempo: 1.0,
            nudgerate: 0.0,
            align: None,
            lock: false,
            nudge: Nudge::default(),
        }
    }

    /// The rate handed to the engine: stable tempo plus whatever is currently bending it.
    pub fn playing_rate(&self) -> f64 {
        self.tempo + self.nudgerate
    }

    /// The current bend (also counted inside [`nudgerate`](Self::nudgerate)).
    pub fn nudge_rate(&self) -> f64 {
        self.nudge.rate()
    }

    /// Starts a temporary bend; `seconds` releases it on its own, `None` holds it until
    /// [`stop_nudge`](Self::stop_nudge). Only `nudgerate` moves — the tempo stays put.
    pub fn start_nudge(&mut self, delta: f64, seconds: Option<f64>) {
        self.nudge.start(delta, seconds);
    }

    /// Releases a running bend; it ramps back to zero rather than snapping.
    pub fn stop_nudge(&mut self) {
        self.nudge.stop();
    }

    /// The label of the running phase correction, for readouts.
    pub fn align_label(&self) -> Option<&'static str> {
        self.align.as_ref().map(PhaseAlign::label)
    }

    /// Tears down everything temporary — the lock, the phase correction, any bend — and leaves the
    /// tempo exactly where it is. That is the whole contract of `sync unlock`.
    pub fn unlock(&mut self) {
        self.align = None;
        self.lock = false;
        self.nudge.stop();
        self.nudgerate = 0.0;
    }

    /// The playhead just moved for a reason other than tempo; skip the controller's next read so
    /// the seek is not mistaken for a phase error.
    pub fn suppress(&mut self) {
        if let Some(PhaseAlign::Pid(pll)) = self.align.as_mut() {
            pll.suppress_next();
        }
    }

    /// Runs one block's sync math: recompute `tempo` from the group if locked, compute the phase
    /// correction, roll the nudge, and fold both into `nudgerate`.
    ///
    /// `own_bpm` is the BPM in force at this deck's position (`0.0` without a grid) and `own_phase`
    /// is where inside its beat it sits; both come from the deck so this stays a pure calculation.
    pub fn update(&mut self, sync: Option<&SyncCtx>, own_bpm: f64, own_phase: Option<f32>) {
        // 1. Group tempo. A locked deck's rate is the group's BPM over its own track BPM, so two
        //    tracks of different tempo land on one audible speed. Free decks keep their own tempo.
        if self.lock && own_bpm > 0.0 {
            if let Some(group_bpm) = sync.map(|ctx| ctx.group_bpm) {
                if group_bpm > 0.0 {
                    self.tempo = (group_bpm / own_bpm).clamp(MIN_RATE, MAX_RATE);
                }
            }
        }

        // 2. Phase correction — the controller half of `nudgerate`. Without both grids there is
        //    nothing to compare, which is why every sync command refuses a deck with no grid.
        let mut correction = 0.0;
        if own_bpm > 0.0 {
            let tracked = sync.and_then(|ctx| ctx.leader);
            if let (Some(leader), Some(align), Some(phase)) = (tracked, self.align.as_mut(), own_phase)
            {
                if let Some(leader_phase) = leader.phase {
                    let err_beats = wrap_phase(f64::from(leader_phase) - f64::from(phase));
                    let err_sec = err_beats * 60.0 / own_bpm;
                    correction = match align {
                        // Instant never rests here: it is a jump performed at command time.
                        PhaseAlign::Instant => 0.0,
                        PhaseAlign::Linear { t_seconds } => {
                            let t = (*t_seconds).max(BLOCK_SECONDS);
                            (err_sec / t).clamp(-PLL_LIMIT, PLL_LIMIT)
                        }
                        PhaseAlign::Pid(pll) => pll.update(err_sec, BLOCK_SECONDS),
                    };
                }
            }
        }

        // 3. The bend, then both halves stacked and bounded.
        self.nudge.tick();
        let nudge_rate = self.nudge.rate();
        self.nudgerate = (correction + nudge_rate).clamp(-NUDGERATE_LIMIT, NUDGERATE_LIMIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_folds_a_full_beat_into_the_short_direction() {
        assert_eq!(wrap_phase(0.0), 0.0);
        assert_eq!(wrap_phase(0.25), 0.25);
        // Leader a hair past the follower, read a whole beat later: the short correction is −0.1.
        assert!((wrap_phase(0.9) - -0.1).abs() < 1e-12);
        // Leader a hair behind, read as 0.9 the other way.
        assert!((wrap_phase(-0.9) - 0.1).abs() < 1e-12);
        for probe in [-0.99, -0.5, -0.49, -0.01, 0.0, 0.01, 0.49, 0.5, 0.99] {
            let wrapped = wrap_phase(probe);
            assert!(
                (-0.5..=0.5).contains(&wrapped),
                "wrap({probe}) = {wrapped} escaped [-0.5, 0.5]"
            );
        }
    }

    #[test]
    fn pll_closes_a_standing_error_and_parks_at_zero() {
        let mut pll = Pll::new();
        let dt = BLOCK_SECONDS;
        let mut error = 0.1;
        let mut peak = 0.0f64;
        for _ in 0..8_000 {
            let output = pll.update(error, dt);
            peak = peak.max(output.abs());
            assert!(output.abs() <= PLL_LIMIT, "output escaped the clamp: {output}");
            error -= output * dt; // the controller gains `output * dt` seconds per block
        }
        assert!(
            error.abs() < 1e-4,
            "a 100ms error must be gone after ~46s of control, left {error}"
        );
        let settled = pll.update(0.0, dt);
        assert!(
            settled.abs() < 1e-6,
            "a converged controller must go silent, still outputting {settled}"
        );
        assert!(peak > 0.0, "the controller must actually have done something");
    }

    #[test]
    fn pll_anti_windup_bounds_the_integral() {
        let mut pll = Pll::new();
        let dt = BLOCK_SECONDS;
        // A permanently saturated error. Without the integral clamp this accumulates ~58 seconds
        // of error and the next recovery would lunge at kp * 58 ≈ 11.6 — fifty times the ceiling.
        for _ in 0..10_000 {
            let output = pll.update(1.0, dt);
            assert!(output.abs() <= PLL_LIMIT, "saturation must not escape: {output}");
        }
        // With the error gone, all that is left is the bounded residue: ki * 0.05 = 0.01.
        let residue = pll.update(0.0, dt);
        assert!(
            residue > 0.0 && residue <= 0.01 + 1e-9,
            "windup must stay at the clamp, not at ki * 58: {residue}"
        );
        // …and it unwinds as soon as the error runs the other way.
        let recovery = pll.update(-1.0, dt);
        assert!(
            (-PLL_LIMIT..=0.0).contains(&recovery),
            "a wound-up controller must recover downward: {recovery}"
        );
    }

    #[test]
    fn suppress_skips_exactly_one_update() {
        let mut pll = Pll::new();
        // Small enough errors that neither saturates the ±5% clamp, so the difference is visible.
        let steady = pll.update(0.01, BLOCK_SECONDS);
        assert_ne!(steady, 0.0);
        pll.suppress_next();
        let after_jump = pll.update(0.03, BLOCK_SECONDS);
        assert_eq!(
            after_jump, steady,
            "a flow switch must not read as a phase error"
        );
        // …and only once: the block after the switch is real again.
        let real = pll.update(0.03, BLOCK_SECONDS);
        assert!(
            real > steady,
            "the next update must react to the actual error: {real} vs {steady}"
        );
    }

    #[test]
    fn nudge_ramps_to_its_target_and_back() {
        let mut nudge = Nudge::default();
        assert_eq!(nudge.rate(), 0.0);
        nudge.start(0.04, None);
        for _ in 0..60 {
            nudge.tick();
        }
        let held = nudge.rate();
        assert!((held - 0.04).abs() < 1e-3, "should settle on 4%: {held}");
        nudge.stop();
        for _ in 0..120 {
            nudge.tick();
        }
        assert_eq!(nudge.rate(), 0.0, "an idle bend reports exactly zero");
        assert!(!nudge.active());
    }

    #[test]
    fn a_timed_nudge_releases_itself() {
        let mut nudge = Nudge::default();
        nudge.start(0.1, Some(0.05)); // ~2.4 blocks of hold
        for _ in 0..200 {
            nudge.tick();
        }
        assert_eq!(nudge.rate(), 0.0, "a timed bend must not hold on");
    }

    #[test]
    fn a_nudge_is_clamped() {
        let mut nudge = Nudge::default();
        nudge.start(9.0, None);
        for _ in 0..60 {
            nudge.tick();
        }
        assert!((nudge.rate() - NUDGE_LIMIT).abs() < 1e-3);
    }

    #[test]
    fn a_locked_deck_takes_its_tempo_from_the_group() {
        let mut playhead = Playhead::new();
        playhead.lock = true;
        let ctx = SyncCtx {
            group_bpm: 128.0,
            ..SyncCtx::default()
        };
        // Own grid says 122 BPM: matching a 128 BPM group means running a bit fast.
        playhead.update(Some(&ctx), 122.0, Some(0.0));
        assert!((playhead.tempo - 128.0 / 122.0).abs() < 1e-9);
        assert_eq!(playhead.nudgerate, 0.0, "no correction, no nudge");
        assert!((playhead.playing_rate() - 128.0 / 122.0).abs() < 1e-9);

        // A free deck's tempo is its own: the group must not touch it.
        let mut free = Playhead { tempo: 0.9, ..Playhead::new() };
        free.update(Some(&ctx), 122.0, Some(0.0));
        assert_eq!(free.tempo, 0.9);
    }

    #[test]
    fn unlock_keeps_the_tempo_and_drops_everything_else() {
        let mut playhead = Playhead {
            tempo: 1.13,
            nudgerate: 0.0,
            align: Some(PhaseAlign::Pid(Pll::new())),
            lock: true,
            nudge: Nudge::default(),
        };
        playhead.nudge.start(0.05, None);
        playhead.unlock();

        assert_eq!(playhead.tempo, 1.13, "unlock keeps the speed");
        assert_eq!(playhead.align, None);
        assert!(!playhead.lock);
        assert_eq!(playhead.nudgerate, 0.0);
        assert_eq!(playhead.playing_rate(), 1.13);
    }

    #[test]
    fn align_modes_build_and_label() {
        assert_eq!(
            PhaseAlign::from_mode(PhaseMode::Instant, None).unwrap(),
            PhaseAlign::Instant
        );
        assert_eq!(
            PhaseAlign::from_mode(PhaseMode::Linear, Some(2.5)).unwrap(),
            PhaseAlign::Linear { t_seconds: 2.5 }
        );
        assert_eq!(
            PhaseAlign::from_mode(PhaseMode::Linear, None).unwrap(),
            PhaseAlign::Linear { t_seconds: 2.0 },
            "linear defaults to two seconds"
        );
        assert!(PhaseAlign::from_mode(PhaseMode::Linear, Some(0.0)).is_err());
        assert!(PhaseAlign::from_mode(PhaseMode::Linear, Some(-1.0)).is_err());
        assert!(matches!(
            PhaseAlign::from_mode(PhaseMode::Pid, None).unwrap(),
            PhaseAlign::Pid(_)
        ));
        assert_eq!(PhaseAlign::from_mode(PhaseMode::Pid, None).unwrap().label(), "pid");
    }

    #[test]
    fn phase_pid_closes_the_gap_and_goes_silent() {
        let mut playhead = Playhead::new();
        playhead.align = Some(PhaseAlign::Pid(Pll::new()));
        let leader = LeaderSample {
            deck_id: 0,
            phase: Some(0.0),
            bpm: 122.0,
            tempo: 1.0,
        };
        let ctx = SyncCtx {
            group_bpm: 0.0,
            leader: Some(leader),
        };
        // A quarter beat out of step (0.75 reads as −0.25 once wrapped), closed the way the engine
        // closes it: the correction integrates into the position, which shrinks the error.
        let mut phase = 0.75_f32;
        let mut final_distance = 1.0_f64;
        for _ in 0..8_000 {
            playhead.update(Some(&ctx), 122.0, Some(phase));
            assert!(
                playhead.nudgerate.abs() <= PLL_LIMIT + 1e-12,
                "the controller must stay inside ±5%: {}",
                playhead.nudgerate
            );
            let gained = playhead.nudgerate * 122.0 * BLOCK_SECONDS / 60.0;
            phase = (f64::from(phase) + gained).rem_euclid(1.0) as f32;
            final_distance = wrap_phase(-f64::from(phase)).abs();
        }
        assert!(
            final_distance < 1e-3,
            "the phase gap must close, still {final_distance} beats"
        );
        assert!(
            playhead.nudgerate.abs() < 1e-3,
            "a converged correction must park, still {}",
            playhead.nudgerate
        );
        assert_eq!(
            playhead.tempo, 1.0,
            "phase correction must never touch the tempo it corrects with"
        );
    }

    #[test]
    fn linear_correction_closes_the_gap_at_a_bounded_slope() {
        let mut playhead = Playhead::new();
        playhead.align = Some(PhaseAlign::Linear { t_seconds: 2.0 });
        let ctx = SyncCtx {
            group_bpm: 0.0,
            leader: Some(LeaderSample {
                deck_id: 0,
                phase: Some(0.0),
                bpm: 122.0,
                tempo: 1.0,
            }),
        };
        let mut phase = 0.75_f32;
        let mut final_distance = 1.0_f64;
        for _ in 0..4_000 {
            playhead.update(Some(&ctx), 122.0, Some(phase));
            assert!(playhead.nudgerate.abs() <= PLL_LIMIT + 1e-12);
            let gained = playhead.nudgerate * 122.0 * BLOCK_SECONDS / 60.0;
            phase = (f64::from(phase) + gained).rem_euclid(1.0) as f32;
            final_distance = wrap_phase(-f64::from(phase)).abs();
        }
        assert!(final_distance < 1e-3, "still {final_distance} beats");
    }
}
