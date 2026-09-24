//! Keyboard mapping.
//!
//! The command box is always focused, so printable characters are text and every engine action goes
//! through a typed command. The only keys this layer owns are editing/navigation plus `Tab` to
//! switch the focused deck. `Ctrl+C` still quits outright.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hypermixx_audio::AudioPipeline;

use crate::command::{self, Action, Dispatcher};
use crate::response::LogLine;
use crate::tui::app::{App, FilePurpose, Overlay};
use crate::tui::picker::{FilePicker, PortList};

/// Extensions the `load` picker offers (the decoder's supported containers/audio).
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "mp1", "mp2", "wav", "aiff", "aif", "flac", "m4a", "mp4", "aac", "ogg", "oga",
];
/// The conventional map the MIDI pickers start from.
const DEFAULT_MAP: &str = "midi-map.toml";

/// Handles one key press.
pub fn handle(app: &mut App, key: KeyEvent, dispatcher: &Dispatcher, pipeline: &AudioPipeline) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }

    // A modal picker owns the keyboard while it is open.
    if app.overlay.is_some() {
        handle_overlay(app, key, dispatcher);
        return;
    }

    // Non-printable function keys open overlays the command box cannot express.
    match key.code {
        KeyCode::F(2) => {
            app.overlay = Some(Overlay::Ports {
                list: PortList::refresh(),
            });
            return;
        }
        KeyCode::F(3) => {
            app.overlay = Some(Overlay::Files {
                picker: FilePicker::new(Some(std::path::Path::new(DEFAULT_MAP)), &["toml"], "midi map"),
                purpose: FilePurpose::MidiMap,
            });
            return;
        }
        _ => {}
    }

    let popup = app.completion.is_some();
    match key.code {
        KeyCode::Enter => {
            // Enter applies the highlighted completion when it would change the word; when the
            // candidate already matches what was typed there is nothing to apply, so submit.
            if popup && app.completion_changes_token() {
                app.completion_accept();
            } else {
                submit(app, dispatcher, pipeline);
            }
        }
        KeyCode::Esc => {
            if popup {
                app.completion = None;
            } else {
                app.clear_command();
            }
        }
        KeyCode::Tab => {
            if popup {
                app.completion_accept();
            } else {
                cycle_deck(app, 1);
            }
        }
        KeyCode::BackTab => {
            if popup {
                app.completion_move(-1);
            } else {
                cycle_deck(app, -1);
            }
        }
        KeyCode::Up => {
            if popup {
                app.completion_move(-1);
            } else {
                app.history_prev();
            }
        }
        KeyCode::Down => {
            if popup {
                app.completion_move(1);
            } else {
                app.history_next();
            }
        }
        KeyCode::Left => app.move_cursor(-1),
        KeyCode::Right => app.move_cursor(1),
        KeyCode::Home => {
            app.cursor = 0;
            app.refresh_completion();
        }
        KeyCode::End => {
            app.cursor = app.command.len();
            app.refresh_completion();
        }
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete(),
        KeyCode::PageUp => app.log_scroll(5),
        KeyCode::PageDown => app.log_scroll(-5),
        KeyCode::Char(ch) if !ch.is_control() => app.insert_char(ch),
        _ => {}
    }
}

/// Moves the focus between decks; the command box prefix and the deck border follow it.
fn cycle_deck(app: &mut App, delta: isize) {
    let decks = app.decks as isize;
    if decks == 0 {
        return;
    }
    app.focused = (app.focused as isize + delta).rem_euclid(decks) as usize;
    // The implicit fx chain changed, so slot candidates must be rebuilt.
    app.refresh_completion();
}

/// Sends the buffer as a command line and logs the echo, exactly like the REPL prompt.
fn submit(app: &mut App, dispatcher: &Dispatcher, pipeline: &AudioPipeline) {
    let line = app.command_string();
    let line = line.trim().to_owned();
    app.clear_command();
    if line.is_empty() {
        return;
    }
    app.history.push(line.clone());
    app.push_log(LogLine::info(format!("> {line}")));
    if apply_ui_command(app, &line) {
        return;
    }
    let action = command::dispatch(
        &line,
        dispatcher,
        pipeline,
        app.backend,
        app.decks,
        app.focused as u8,
    );
    match action {
        Action::Continue => {}
        Action::Quit => app.quit = true,
        Action::Message(text) => push_block(app, text),
        Action::Failed(message) => app.push_log(LogLine::error(message)),
    }
}

/// UI-only commands the engine has no concept of. Handled here so they never reach the pipeline.
fn apply_ui_command(app: &mut App, line: &str) -> bool {
    let mut words = line.split_whitespace();
    if words.next() == Some("zoom") {
        match words.next() {
            Some("in") => app.zoom_in(),
            Some("out") => app.zoom_out(),
            _ => app.fit(),
        }
        return true;
    }
    // A bare `load` (no path) opens the file picker instead of failing on a missing argument.
    if let Some(deck) = bare_load_deck(line, app.decks, app.focused) {
        app.overlay = Some(Overlay::Files {
            picker: FilePicker::new(None, AUDIO_EXTENSIONS, "load track"),
            purpose: FilePurpose::Load { deck },
        });
        return true;
    }
    false
}

/// Recognises `<deck> load` with no further words. Returns the deck to load into.
fn bare_load_deck(line: &str, decks: usize, focused: usize) -> Option<usize> {
    let mut words = line.split_whitespace();
    let first = words.next()?;
    let (deck, verb) = match deck_word(first, decks) {
        Some(deck) => (deck, words.next()),
        None => (focused, Some(first)),
    };
    if verb == Some("load") && words.next().is_none() {
        Some(deck)
    } else {
        None
    }
}

/// Parses a target-first deck word (`deck1`, `d1`, `1`).
fn deck_word(token: &str, decks: usize) -> Option<usize> {
    let digits = token
        .strip_prefix("deck")
        .or_else(|| token.strip_prefix('d'))
        .unwrap_or(token);
    let index: usize = digits.parse().ok()?;
    (index < decks).then_some(index)
}

/// Routes keys to the open picker. On a choice it acts and closes; Esc cancels.
fn handle_overlay(app: &mut App, key: KeyEvent, dispatcher: &Dispatcher) {
    let Some(overlay) = app.overlay.take() else {
        return;
    };
    match overlay {
        Overlay::Ports { mut list } => match key.code {
            KeyCode::Esc => {}
            KeyCode::Up => {
                list.move_selection(-1);
                app.overlay = Some(Overlay::Ports { list });
            }
            KeyCode::Down => {
                list.move_selection(1);
                app.overlay = Some(Overlay::Ports { list });
            }
            KeyCode::Enter => match list.selected_port() {
                Some(port) => {
                    app.midi_port = Some(port.name);
                    // A usable map already remembered (flag or earlier pick) means we can connect
                    // now; otherwise ask for one.
                    let map_ready = app
                        .midi_map
                        .as_deref()
                        .is_some_and(|path| std::path::Path::new(path).is_file());
                    if map_ready {
                        app.connect_midi(dispatcher.command_tx.clone(), &dispatcher.notices);
                    } else {
                        app.overlay = Some(Overlay::Files {
                            picker: FilePicker::new(
                                Some(std::path::Path::new(DEFAULT_MAP)),
                                &["toml"],
                                "midi map",
                            ),
                            purpose: FilePurpose::MidiMap,
                        });
                    }
                }
                None => app.overlay = Some(Overlay::Ports { list }),
            },
            _ => app.overlay = Some(Overlay::Ports { list }),
        },
        Overlay::Files { mut picker, purpose } => match key.code {
            KeyCode::Esc => {}
            KeyCode::Up => {
                picker.move_selection(-1);
                app.overlay = Some(Overlay::Files { picker, purpose });
            }
            KeyCode::Down => {
                picker.move_selection(1);
                app.overlay = Some(Overlay::Files { picker, purpose });
            }
            KeyCode::Backspace | KeyCode::Left => {
                picker.up();
                app.overlay = Some(Overlay::Files { picker, purpose });
            }
            KeyCode::Enter => match picker.activate() {
                Some(path) => {
                    let path = path.display().to_string();
                    match purpose {
                        FilePurpose::Load { deck } => {
                            command::spawn_load(deck as u8, path, None, dispatcher);
                        }
                        FilePurpose::MidiMap => {
                            app.midi_map = Some(path);
                            app.connect_midi(
                                dispatcher.command_tx.clone(),
                                &dispatcher.notices,
                            );
                        }
                    }
                }
                None => app.overlay = Some(Overlay::Files { picker, purpose }),
            },
            _ => app.overlay = Some(Overlay::Files { picker, purpose }),
        },
    }
}

/// Pushes a multi-line block (help text) as individual log lines.
fn push_block(app: &mut App, text: String) {
    for line in text.lines() {
        app.push_log(LogLine::info(line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_load_is_recognised_with_and_without_a_target() {
        assert_eq!(bare_load_deck("load", 2, 1), Some(1));
        assert_eq!(bare_load_deck("deck0 load", 2, 1), Some(0));
        assert_eq!(bare_load_deck("0 load", 2, 1), Some(0));
        assert_eq!(bare_load_deck("d1 load", 2, 0), Some(1));
        // A map-like trailing word is not a bare load.
        assert_eq!(bare_load_deck("load test.mp3", 2, 0), None);
        assert_eq!(bare_load_deck("deck0 load test.mp3", 2, 0), None);
        assert_eq!(bare_load_deck("play", 2, 0), None);
    }

    #[test]
    fn deck_words_parse_and_reject_out_of_range() {
        assert_eq!(deck_word("deck1", 2), Some(1));
        assert_eq!(deck_word("d1", 2), Some(1));
        assert_eq!(deck_word("1", 2), Some(1));
        assert_eq!(deck_word("2", 2), None);
        assert_eq!(deck_word("load", 2), None);
    }
}
