//! [`Empty`]: a slot that is mounted, addressable and processes nothing.
//!
//! Fixed slot banks mean a slot's *position* has to survive a change of mind. `fx select <slot> none`
//! empties the slot in place — the index and role stay, so a controller map keeps pointing at it —
//! which is what "remove this effect" means in a mixer whose banks are declared in the config.

use super::super::{Fx, FxContext};
use crate::mixer::Bus;

/// The empty effect.
#[derive(Clone, Copy, Debug, Default)]
pub struct Empty;

impl Fx for Empty {
    fn process(&mut self, _bus: &mut Bus, _ctx: &FxContext) {}
}
