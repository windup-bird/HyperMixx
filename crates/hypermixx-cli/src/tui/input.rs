//! Keyboard mapping.
//!
//! The command box is always focused, so printable characters are text and every engine action goes
//! through a typed command. The only keys this layer owns are editing/navigation plus `Tab` to
//! switch the focused deck. `Ctrl+C` still quits outright.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hypermixx_audio::AudioPipeline;

use crate::command::{self, Action, Dispatcher};
use crate::response::LogLine;
use crate::tui::app::App;

/// Handles one key press.
pub fn handle(app: &mut App, key: KeyEvent, dispatcher: &Dispatcher, pipeline: &AudioPipeline) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit = true;
        return;
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
    if words.next() != Some("zoom") {
        return false;
    }
    match words.next() {
        Some("in") => app.zoom_in(),
        Some("out") => app.zoom_out(),
        _ => app.fit(),
    }
    true
}

/// Pushes a multi-line block (help text) as individual log lines.
fn push_block(app: &mut App, text: String) {
    for line in text.lines() {
        app.push_log(LogLine::info(line));
    }
}
