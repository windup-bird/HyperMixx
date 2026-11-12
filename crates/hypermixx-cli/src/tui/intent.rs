//! Turning engine commands and MIDI selector presses into a small panel intent.
//!
//! The TUI owns the command channel but not the origin of a command: a line typed in the command
//! box, a mouse click and a MIDI binding all become the same `Command`. The command fan-out asks
//! this module what the newest one means for the panels, and the app applies it when `follow` is
//! on. Keeping the classification here (rather than in the engine) means the protocol stays free of
//! pad-mode/panel concepts.

use hypermixx_core::{Command, FaderTarget, FxChainId, FxSlotRef, LoopOp};
use hypermixx_midi::ModeEvent;

use crate::command::SlotBook;
use crate::tui::app::{BeatMode, PadMode};

/// Which panel/mode a command or MIDI mode selection should reveal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiIntent {
    /// Show the pad grid in `mode`.
    Pads { deck: usize, mode: PadMode },
    /// Show the beat controls in `mode`.
    Beat { deck: usize, mode: BeatMode },
    /// Show the EQ knobs.
    Eq { deck: usize },
    /// Show the FX panel without changing the selected slot.
    FxPanel { deck: usize },
    /// Show the FX panel with one flat slot selected.
    FxSlot { deck: usize, slot: usize },
    /// Show the FX panel and select the Nth non-eq/filter slot (1-based), for `deckfxN` units.
    FxOrdinal { deck: usize, ordinal: usize },
}

/// Classifies an engine command. `None` means the command is not interesting for panel following
/// (polling, loading, transport, master FX, ...).
pub fn from_command(command: &Command, slots: &SlotBook) -> Option<UiIntent> {
    match command {
        Command::HotCue { deck_id, .. } => Some(UiIntent::Pads {
            deck: *deck_id as usize,
            mode: PadMode::HotCue,
        }),
        Command::Loop { deck_id, op } => {
            let mode = match op {
                LoopOp::Beats(_) | LoopOp::SlipBeats(_) | LoopOp::SlipRoll(_) => PadMode::BeatLoop,
                // Every other loop verb is the manual-loop section's vocabulary.
                _ => {
                    return Some(UiIntent::Beat {
                        deck: *deck_id as usize,
                        mode: BeatMode::ManualLoop,
                    })
                }
            };
            Some(UiIntent::Pads {
                deck: *deck_id as usize,
                mode,
            })
        }
        Command::BeatJump { deck_id, .. } => Some(UiIntent::Pads {
            deck: *deck_id as usize,
            mode: PadMode::BeatJump,
        }),
        Command::BeatJumpStep { deck_id, .. } | Command::ScaleJumpBeats { deck_id, .. } => {
            Some(UiIntent::Beat {
                deck: *deck_id as usize,
                mode: BeatMode::ManualJump,
            })
        }
        Command::ScaleLoopBeats { deck_id, .. } => Some(UiIntent::Beat {
            deck: *deck_id as usize,
            mode: BeatMode::ManualLoop,
        }),
        Command::SetTempo { deck_id, .. }
        | Command::SetTempoFader { deck_id, .. }
        | Command::SetTempoRange { deck_id, .. }
        | Command::Nudge { deck_id, .. } => Some(UiIntent::Beat {
            deck: *deck_id as usize,
            mode: BeatMode::Tempo,
        }),
        Command::SetFader {
            // The trim is drawn as the first EQ knob; other faders do not move the panels.
            target: FaderTarget::Gain(deck),
            ..
        } => Some(UiIntent::Eq {
            deck: *deck as usize,
        }),
        Command::ToggleCueSend { deck_id } => Some(UiIntent::Eq {
            deck: *deck_id as usize,
        }),
        Command::SetFxEnabled { slot, .. }
        | Command::ToggleFx { slot }
        | Command::SetFxParam { slot, .. }
        | Command::SetFxParamIndex { slot, .. }
        | Command::SetFxFlows { slot, .. }
        | Command::SetFxKind { slot, .. }
        | Command::FxTrigger { slot }
        | Command::PadPress { slot }
        | Command::PadRelease { slot } => from_fx_slot(slot, slots),
        Command::AddFx { chain, .. } | Command::RemoveFx { chain, .. } => match chain {
            FxChainId::Deck(deck) => Some(UiIntent::FxPanel {
                deck: *deck as usize,
            }),
            FxChainId::Stem { deck, .. } => Some(UiIntent::FxPanel {
                deck: *deck as usize,
            }),
            FxChainId::Master => None,
        },
        _ => None,
    }
}

/// The role decides whether an FX command belongs to the EQ panel (`eq`/`filter`) or the FX panel.
/// The slot book is primed at startup, so the index can be resolved without asking the engine.
fn from_fx_slot(slot: &FxSlotRef, slots: &SlotBook) -> Option<UiIntent> {
    let FxChainId::Deck(deck) = slot.chain else {
        return match slot.chain {
            FxChainId::Stem { deck, .. } => Some(UiIntent::FxPanel {
                deck: deck as usize,
            }),
            _ => None,
        };
    };
    let deck = deck as usize;
    match slots.role_at(&slot.chain.label(), slot.index).as_deref() {
        Some("eq" | "filter") => Some(UiIntent::Eq { deck }),
        _ => Some(UiIntent::FxSlot {
            deck,
            slot: slot.index,
        }),
    }
}

/// Classifies a MIDI selector press. The event is emitted even when the router's mode did not
/// change, which is what lets a hardware `pad mode` key bring the TUI back to that mode.
pub fn from_mode_event(event: &ModeEvent, decks: usize) -> Option<UiIntent> {
    let deck = event.deck? as usize;
    if deck >= decks {
        return None;
    }
    let unit = event.unit.to_ascii_lowercase();
    let mode = event.mode.to_ascii_lowercase();

    if let Some((_, rest)) = unit.rsplit_once("deckfx") {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let ordinal = digits.parse::<usize>().unwrap_or(1).max(1);
        return Some(UiIntent::FxOrdinal { deck, ordinal });
    }
    if unit.contains(".pad") || matches!(mode.as_str(), "hotcue" | "loop" | "beatjump") {
        return match mode.as_str() {
            "hotcue" => Some(UiIntent::Pads {
                deck,
                mode: PadMode::HotCue,
            }),
            "loop" => Some(UiIntent::Pads {
                deck,
                mode: PadMode::BeatLoop,
            }),
            "beatjump" => Some(UiIntent::Pads {
                deck,
                mode: PadMode::BeatJump,
            }),
            _ => None,
        };
    }
    if unit.contains("manualloop") {
        return Some(UiIntent::Beat {
            deck,
            mode: BeatMode::ManualLoop,
        });
    }
    if unit.contains("manualjump") {
        return Some(UiIntent::Beat {
            deck,
            mode: BeatMode::ManualJump,
        });
    }
    if unit.contains("tempofader") {
        return Some(UiIntent::Beat {
            deck,
            mode: BeatMode::Tempo,
        });
    }
    if unit.contains("mixer") {
        return Some(UiIntent::Eq { deck });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_core::{CommandResponse, FxParamStatus, FxSlotRef, FxSlotStatus, LoopOp};

    fn status(index: usize, role: &str) -> FxSlotStatus {
        FxSlotStatus {
            index,
            role: role.to_owned(),
            kind: role.to_owned(),
            enabled: true,
            flows: None,
            params: vec![FxParamStatus {
                name: "value".to_owned(),
                value: 0.0,
                range: Some((-1.0, 1.0)),
                beats: 0,
            }],
        }
    }

    fn book() -> SlotBook {
        let book = SlotBook::default();
        book.record(&CommandResponse::FxListed {
            chain: FxChainId::Deck(0),
            slots: vec![status(0, "eq"), status(1, "filter"), status(2, "knobfx1")],
        });
        book
    }

    #[test]
    fn pad_and_beat_commands_pick_their_panels() {
        let book = book();
        let command = |command| from_command(&command, &book);
        assert_eq!(
            command(Command::HotCue {
                deck_id: 0,
                op: hypermixx_core::HotCueOp::Trigger(0),
            }),
            Some(UiIntent::Pads {
                deck: 0,
                mode: PadMode::HotCue,
            })
        );
        assert_eq!(
            command(Command::Loop {
                deck_id: 0,
                op: LoopOp::Beats(hypermixx_core::BeatLen::beats(4)),
            }),
            Some(UiIntent::Pads {
                deck: 0,
                mode: PadMode::BeatLoop,
            })
        );
        assert_eq!(
            command(Command::Loop {
                deck_id: 0,
                op: LoopOp::In,
            }),
            Some(UiIntent::Beat {
                deck: 0,
                mode: BeatMode::ManualLoop,
            })
        );
        assert_eq!(
            command(Command::BeatJump {
                deck_id: 0,
                beats: 4,
            }),
            Some(UiIntent::Pads {
                deck: 0,
                mode: PadMode::BeatJump,
            })
        );
        assert_eq!(
            command(Command::BeatJumpStep {
                deck_id: 0,
                direction: 1,
            }),
            Some(UiIntent::Beat {
                deck: 0,
                mode: BeatMode::ManualJump,
            })
        );
        assert_eq!(
            command(Command::SetTempoFader {
                deck_id: 0,
                position: 0.1,
            }),
            Some(UiIntent::Beat {
                deck: 0,
                mode: BeatMode::Tempo,
            })
        );
    }

    #[test]
    fn fx_roles_send_eq_and_filter_to_the_eq_panel() {
        let book = book();
        let slot = |index| FxSlotRef {
            chain: FxChainId::Deck(0),
            index,
        };
        assert_eq!(
            from_command(
                &Command::SetFxParam {
                    slot: slot(0),
                    name: "low".to_owned(),
                    value: 0.5,
                },
                &book
            ),
            Some(UiIntent::Eq { deck: 0 })
        );
        assert_eq!(
            from_command(
                &Command::SetFxEnabled {
                    slot: slot(1),
                    enabled: false,
                },
                &book
            ),
            Some(UiIntent::Eq { deck: 0 })
        );
        assert_eq!(
            from_command(
                &Command::SetFxParamIndex {
                    slot: slot(2),
                    index: 0,
                    value: 0.5,
                },
                &book
            ),
            Some(UiIntent::FxSlot { deck: 0, slot: 2 })
        );
        assert_eq!(
            from_command(
                &Command::AddFx {
                    chain: FxChainId::Deck(0),
                    kind: "echo".to_owned(),
                },
                &book
            ),
            Some(UiIntent::FxPanel { deck: 0 })
        );
    }

    #[test]
    fn mode_events_follow_builtin_unit_names() {
        let event = |unit: &str, mode: &str| ModeEvent {
            unit: unit.to_owned(),
            deck: Some(0),
            mode: mode.to_owned(),
        };
        assert_eq!(
            from_mode_event(&event("deck1.pad", "loop"), 2),
            Some(UiIntent::Pads {
                deck: 0,
                mode: PadMode::BeatLoop,
            })
        );
        assert_eq!(
            from_mode_event(&event("deck1.deckfx2", "default"), 2),
            Some(UiIntent::FxOrdinal {
                deck: 0,
                ordinal: 2,
            })
        );
        assert_eq!(
            from_mode_event(&event("deck1.manualloop", "default"), 2),
            Some(UiIntent::Beat {
                deck: 0,
                mode: BeatMode::ManualLoop,
            })
        );
        assert_eq!(
            from_mode_event(&event("deck1.mixer", "default"), 2),
            Some(UiIntent::Eq { deck: 0 })
        );
        assert_eq!(
            from_mode_event(&event("deck1.transport", "default"), 2),
            None
        );
        // A unit on another deck (or an unknown deck) is ignored.
        let mut wrong = event("deck1.pad", "hotcue");
        wrong.deck = Some(3);
        assert_eq!(from_mode_event(&wrong, 2), None);
    }
}
