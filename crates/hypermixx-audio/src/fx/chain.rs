//! [`FxSlot`] and [`FxChain`]: the fixed-order list of effects one bus passes through.
//!
//! A slot owns its effect outright (`Box<dyn Fx>`, not `Arc`): a chain is built and run on the
//! producer thread, so there is nothing to share and no `ArcSwap` to reason about. Commands mutate
//! the chain between blocks, which is the same thread — hence no locks here at all.

use std::sync::atomic::{AtomicBool, Ordering};
use std::cell::Cell;

use super::{Fx, FxContext, FxError};
use crate::mixer::Bus;
use hypermixx_core::FxSlotStatus;

/// Addresses one slot: which chain, which index in it.
///
/// The protocol type lives in `core` so a front-end can name a slot without linking the engine.
pub type FxTarget = hypermixx_core::FxSlotRef;

/// One effect in a chain, plus its bypass and pad-latch state.
pub struct FxSlot {
    fx: Box<dyn Fx>,
    /// Stable identity for reporting (`"eq"`, `"filter"`, ...), independent of position.
    kind: &'static str,
    enabled: AtomicBool,
    /// Audio-thread memory of the previous block's engagement, to catch a bypass→engage edge and
    /// call [`Fx::reset`] exactly once.
    was_enabled: bool,
    /// Some(_) while a pad holds this slot engaged; the value is what to restore on release.
    ///
    /// `Cell`, not an atomic: only the producer thread ever presses or releases, and commands are
    /// drained on that same thread. Keeping it here (rather than in a map keyed by slot) means a
    /// removal cannot orphan a latch.
    pad_held: Cell<Option<bool>>,
}

impl FxSlot {
    pub fn new(fx: Box<dyn Fx>, kind: &'static str) -> Self {
        let enabled = true;
        Self {
            fx,
            kind,
            enabled: AtomicBool::new(enabled),
            was_enabled: enabled,
            pad_held: Cell::new(None),
        }
    }

    /// A slot that starts bypassed, e.g. a safety limiter wired in but armed off.
    pub fn disabled(fx: Box<dyn Fx>, kind: &'static str) -> Self {
        let mut slot = Self::new(fx, kind);
        slot.enabled.store(false, Ordering::Relaxed);
        slot.was_enabled = false;
        slot
    }

    pub fn fx(&self) -> &dyn Fx {
        &*self.fx
    }

    pub fn fx_mut(&mut self) -> &mut dyn Fx {
        &mut *self.fx
    }

    pub fn kind(&self) -> &'static str {
        self.kind
    }

    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, on: bool) {
        // Pad-aware: while a pad holds the slot, an explicit toggle cannot drop the sound (the finger
        // is down, it must be audible) but it *does* become the state to return to on release, so the
        // last deliberate intent wins. Without this, muting an effect mid-hold would silently revert.
        if let Some(was) = self.pad_held.get() {
            self.pad_held.set(Some(on));
            let _ = was;
            self.enabled.store(true, Ordering::Relaxed);
            return;
        }
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Bypasses or engages a slot, ignoring any pad hold (a config-time operation).
    pub fn set_enabled_ignoring_pad(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn toggle(&self) -> bool {
        let now = !self.enabled.load(Ordering::Relaxed);
        self.enabled.store(now, Ordering::Relaxed);
        now
    }

    pub fn set_param(&self, name: &str, value: f32) -> Result<(), FxError> {
        self.fx.set_param(name, value)
    }

    pub fn get_param(&self, name: &str) -> Option<f32> {
        self.fx.get_param(name)
    }

    /// The moment a pad fired. Needs `&mut` because an envelope effect restarts its own state.
    pub fn trigger(&mut self) {
        self.fx.on_trigger();
    }

    /// Holds the slot engaged for as long as the pad is down, remembering what to restore.
    pub fn press_pad(&self) {
        if self.pad_held.get().is_none() {
            self.pad_held.set(Some(
                self.enabled.swap(true, Ordering::Relaxed),
            ));
        }
    }

    pub fn release_pad(&self) {
        if let Some(was) = self.pad_held.take() {
            self.enabled.store(was, Ordering::Relaxed);
        }
    }

    pub fn pad_is_held(&self) -> bool {
        self.pad_held.get().is_some()
    }

    /// Runs the slot if engaged. The bypass edge is detected here so an effect that resumes from a
    /// stale filter memory never gets the chance to click.
    #[inline]
    pub fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        let on = self.is_enabled();
        if on && !self.was_enabled {
            self.fx.reset();
        }
        self.was_enabled = on;
        if on {
            self.fx.process(bus, ctx);
        }
    }

    pub fn into_fx(self) -> Box<dyn Fx> {
        self.fx
    }

    /// The protocol view of this slot at the given chain index.
    pub fn status(&self, index: usize) -> FxSlotStatus {
        FxSlotStatus {
            index,
            kind: self.kind.to_owned(),
            enabled: self.is_enabled(),
            params: super::param_snapshot(&*self.fx),
        }
    }
}

impl std::fmt::Debug for FxSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FxSlot")
            .field("kind", &self.kind)
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

/// The one-shot hook. `&mut self` is honest: an effect that restarts an envelope is mutating, and
/// routing a trigger through a `&self` handle would force every implementation into interior
/// mutability for no benefit.
impl FxSlot {
    pub fn trigger_via(&mut self) {
        self.fx.on_trigger();
    }
}

/// An ordered list of slots. Order is the processing order — no graph, no sorting.
#[derive(Default)]
pub struct FxChain {
    slots: Vec<FxSlot>,
}

impl FxChain {
    pub fn new() -> Self {
        Self::default()
    }

    /// A chain built from `(kind, instance)` pairs, in order.
    pub fn from_parts(parts: impl IntoIterator<Item = (&'static str, Box<dyn Fx>)>) -> Self {
        Self {
            slots: parts
                .into_iter()
                .map(|(kind, fx)| FxSlot::new(fx, kind))
                .collect(),
        }
    }

    /// Appends an effect and returns its index (the value a front-end must remember).
    pub fn push(&mut self, fx: Box<dyn Fx>) -> usize {
        self.push_named(fx, "fx")
    }

    pub fn push_named(&mut self, fx: Box<dyn Fx>, kind: &'static str) -> usize {
        self.slots.push(FxSlot::new(fx, kind));
        self.slots.len() - 1
    }

    pub fn push_named_disabled(&mut self, fx: Box<dyn Fx>, kind: &'static str) -> usize {
        self.slots.push(FxSlot::disabled(fx, kind));
        self.slots.len() - 1
    }

    /// A chain from pre-built slots, in order.
    pub fn from_slots(slots: impl IntoIterator<Item = FxSlot>) -> Self {
        Self {
            slots: slots.into_iter().collect(),
        }
    }

    /// Appends an already-built slot and returns its index.
    pub fn push_slot(&mut self, slot: FxSlot) -> usize {
        self.slots.push(slot);
        self.slots.len() - 1
    }

    /// Removes a slot; later indices shift down by one.
    pub fn remove(&mut self, index: usize) -> Option<FxSlot> {
        (index < self.slots.len()).then(|| self.slots.remove(index))
    }

    pub fn clear(&mut self) {
        self.slots.clear();
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn slot(&self, index: usize) -> Option<&FxSlot> {
        self.slots.get(index)
    }

    pub fn slot_mut(&mut self, index: usize) -> Option<&mut FxSlot> {
        self.slots.get_mut(index)
    }

    pub fn slots(&self) -> &[FxSlot] {
        &self.slots
    }

    /// True when any slot is engaged, i.e. the chain can actually change the signal. Lets the mixer
    /// skip a chain of bypassed effects entirely.
    pub fn any_active(&self) -> bool {
        self.slots.iter().any(FxSlot::is_enabled)
    }

    /// Runs every engaged slot in order.
    pub fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        for slot in &mut self.slots {
            slot.process(bus, ctx);
        }
    }

    /// Every slot as a protocol snapshot, indexed from zero.
    pub fn statuses(&self) -> Vec<FxSlotStatus> {
        self.statuses_at(0)
    }

    /// As [`FxChain::statuses`], but numbered from `offset` — how a channel labels its deck chain in
    /// the merged index space where the flow chain comes first.
    pub fn statuses_at(&self, offset: usize) -> Vec<FxSlotStatus> {
        self.slots
            .iter()
            .enumerate()
            .map(|(i, slot)| slot.status(offset + i))
            .collect()
    }

    /// Indexes of every slot of a given kind, for "the second EQ on the master" style lookups.
    pub fn indices_of(&self, kind: &str) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.kind() == kind)
            .map(|(i, _)| i)
            .collect()
    }
}

impl std::fmt::Debug for FxChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FxChain")
            .field("slots", &self.slots)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::sample::Gain;
    use crate::fx::{param_snapshot, Fx};
    use crate::mixer::Bus;

    /// Records what the chain did to it, in order.
    struct Spy(std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>);
    impl Fx for Spy {
        fn process(&mut self, _bus: &mut Bus, _ctx: &FxContext) {
            self.0.lock().unwrap().push("process");
        }
        fn reset(&mut self) {
            self.0.lock().unwrap().push("reset");
        }
    }

    fn ctx() -> FxContext {
        FxContext::gridless(44_100, 4)
    }

    #[test]
    fn processing_order_is_the_insertion_order() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut chain = FxChain::from_parts([
            ("first", Box::new(Tag(log.clone(), "a")) as Box<dyn Fx>),
            ("second", Box::new(Tag(log.clone(), "b"))),
            ("third", Box::new(Tag(log.clone(), "c"))),
        ]);
        let mut bus = Bus::stereo(4);
        chain.process(&mut bus, &ctx());
        assert_eq!(*log.lock().unwrap(), vec!["a", "b", "c"]);
    }

    struct Tag(std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>, &'static str);
    impl Fx for Tag {
        fn process(&mut self, _bus: &mut Bus, _ctx: &FxContext) {
            self.0.lock().unwrap().push(self.1);
        }
    }

    #[test]
    fn a_bypassed_slot_is_skipped_and_reset_on_reentry() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut chain = FxChain::from_parts([("spy", Box::new(Spy(log.clone())) as Box<dyn Fx>)]);
        let mut bus = Bus::stereo(4);

        chain.slot(0).unwrap().set_enabled(false);
        chain.process(&mut bus, &ctx());
        assert!(log.lock().unwrap().is_empty(), "bypassed must not run");

        chain.slot(0).unwrap().set_enabled(true);
        chain.process(&mut bus, &ctx());
        assert_eq!(*log.lock().unwrap(), vec!["reset", "process"]);

        // Still engaged: no second reset.
        log.lock().unwrap().clear();
        chain.process(&mut bus, &ctx());
        assert_eq!(*log.lock().unwrap(), vec!["process"]);
    }

    #[test]
    fn pad_press_holds_then_restores() {
        let gain = Gain::new(1.0);
        let slot = FxSlot::new(Box::new(gain), "gain");
        assert!(slot.is_enabled());
        slot.press_pad();
        assert!(slot.pad_is_held());
        slot.set_enabled(false); // a UI toggle while held
        assert!(slot.is_enabled(), "a held pad stays audible however the toggle moves");
        slot.release_pad();
        assert_eq!(
            slot.is_enabled(),
            false,
            "release must restore the state the toggle last asked for"
        );
    }

    #[test]
    fn a_pad_press_without_a_toggle_gives_the_original_state_back() {
        let slot = FxSlot::new(Box::new(Gain::new(1.0)), "gain");
        slot.press_pad();
        slot.release_pad();
        assert!(slot.is_enabled(), "an untouched hold-release must be a no-op");
        // A double press is one hold: the second must not clobber the saved state.
        slot.press_pad();
        slot.press_pad();
        slot.release_pad();
        assert!(slot.is_enabled());
        // And releasing an unheld slot changes nothing either.
        slot.release_pad();
        assert!(slot.is_enabled());
    }

    #[test]
    fn pad_press_on_a_bypassed_slot_leaves_it_bypassed_after_release() {
        let slot = FxSlot::disabled(Box::new(Gain::new(1.0)), "gain");
        slot.press_pad();
        assert!(slot.is_enabled(), "held pads engage");
        slot.release_pad();
        assert!(!slot.is_enabled(), "and give the previous state back");
    }

    #[test]
    fn removal_shifts_indices_and_reports_by_kind() {
        let mut chain = FxChain::new();
        chain.push_named(Box::new(Gain::new(1.0)), "gain");
        chain.push_named(Box::new(Gain::new(2.0)), "gain");
        let second = chain.indices_of("gain")[1];
        assert_eq!(chain.remove(second).map(|s| s.kind().to_owned()), Some("gain".into()));
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.slot(0).unwrap().get_param("gain"), Some(1.0));
        assert!(chain.slot(1).is_none());
    }

    #[test]
    fn any_active_tracks_engagement() {
        let mut chain = FxChain::new();
        chain.push_named_disabled(Box::new(Gain::new(1.0)), "gain");
        assert!(!chain.any_active());
        chain.slot(0).unwrap().set_enabled(true);
        assert!(chain.any_active());
    }

    #[test]
    fn slot_snapshot_lists_params_in_declaration_order() {
        let slot = FxSlot::new(Box::new(Gain::new(0.5)), "gain");
        let snapshot = param_snapshot(slot.fx());
        assert_eq!(snapshot.len(), slot.fx().param_names().len());
        assert_eq!(snapshot[0].0, "gain");
        assert!((snapshot[0].1 - 0.5).abs() < 1e-6);
    }
}
