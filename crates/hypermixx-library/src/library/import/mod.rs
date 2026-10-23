//! Source adapters. Phase 1 ships the folder reader; the Rekordbox/VirtualDJ/Serato readers plug
//! into the same scanner contract in the next phase.

pub mod analysis;
pub mod anlz;
pub mod folder;
pub mod rekordbox;
pub mod serato;
pub mod util;
pub mod vdj;
