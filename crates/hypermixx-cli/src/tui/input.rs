//! Keyboard mapping.
//!
//! The command box is always focused, so printable characters are text and every engine action goes
//! through a typed command. The only keys this layer owns are editing/navigation plus `Tab` to
//! cycle the command target (deck0 / deck1 / … / browser). `Ctrl+C` still quits outright.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use hypermixx_audio::AudioPipeline;
use hypermixx_core::BrowserAction;

use crate::command::{self, Action, Dispatcher};
use crate::response::LogLine;
use crate::tui::app::{App, FilePurpose, Overlay, Target};
use crate::tui::browser::Focus;
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
                picker: FilePicker::new(
                    Some(std::path::Path::new(DEFAULT_MAP)),
                    &["toml"],
                    "midi map",
                ),
                purpose: FilePurpose::MidiMap,
            });
            return;
        }
        _ => {}
    }

    // When the browser owns the target and the command line is empty, the arrows are browser
    // navigation rather than history/cursor movement.
    if app.target == Target::Browser && app.command.is_empty() && handle_browser_key(app, key) {
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
                app.cycle_target(1);
            }
        }
        KeyCode::BackTab => {
            if popup {
                app.completion_move(-1);
            } else {
                app.cycle_target(-1);
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

/// Mouse handling for the browser: single-click selects, double-click on a track opens the deck
/// chooser, double-click on a list opens it.
pub fn handle_mouse(app: &mut App, mouse: MouseEvent, dispatcher: &Dispatcher) {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }
    if let Some(Overlay::LoadTarget { x, y, .. }) = &app.overlay {
        let popup = crate::tui::view::load_target_rect(app.screen, *x, *y, app.decks);
        if mouse.column >= popup.x
            && mouse.column < popup.x + popup.width
            && mouse.row > popup.y
            && mouse.row < popup.y + popup.height.saturating_sub(1)
        {
            let deck = (mouse.row - popup.y - 1) as usize;
            if deck < app.decks {
                app.overlay = None;
                load_browser_track(app, dispatcher, deck);
            }
        } else {
            app.overlay = None;
        }
        return;
    }
    if app.overlay.is_some() {
        return;
    }
    let Some(db) = app.library.clone() else {
        return;
    };
    let (x, y) = (mouse.column, mouse.row);
    if let Some(row) = app.browser.hit_track(x, y) {
        app.browser.track_selected = row;
        app.browser.focus = Focus::Tracks;
        let now = Instant::now();
        let double = matches!(
            app.browser.last_track_click,
            Some((last, at)) if last == row && now.duration_since(at) < Duration::from_millis(400)
        );
        app.browser.last_track_click = Some((row, now));
        if double {
            app.overlay = Some(Overlay::LoadTarget {
                x,
                y,
                selected: app.default_deck(),
            });
        }
        return;
    }
    if let Some((column, row)) = app.browser.hit_column(x, y) {
        let is_last = column + 1 == app.browser.columns.len();
        let now = Instant::now();
        let double = matches!(
            app.browser.last_track_click,
            Some((last, at)) if last == usize::MAX - row && now.duration_since(at) < Duration::from_millis(400)
        );
        app.browser.last_track_click = Some((usize::MAX - row, now));
        if let Err(err) = app.browser.select(column, row, &db) {
            app.push_log(LogLine::error(format!("library: {err}")));
        }
        if is_last && double {
            if let Err(err) = app.browser.open(&db) {
                app.push_log(LogLine::error(format!("library: {err}")));
            } else if app.browser.focus() == Focus::Tracks {
                // Double-clicking the last list enters its tracks; a second double-click on a
                // track then opens the deck chooser.
            }
        }
    }
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
    let prefix = app.target.prefix();
    app.push_log(LogLine::info(if prefix.is_empty() {
        format!("> {line}")
    } else {
        format!("{prefix}> {line}")
    }));
    if apply_ui_command(app, &line, dispatcher) {
        return;
    }
    if app.target == Target::Browser {
        app.push_log(LogLine::error(
            "browser target only accepts `browser ...` commands",
        ));
        return;
    }
    let deck = app.default_deck();
    let action = command::dispatch(
        &line,
        dispatcher,
        pipeline,
        app.backend,
        app.decks,
        deck as u8,
    );
    match action {
        Action::Continue => {}
        Action::Quit => app.quit = true,
        Action::Message(text) => push_block(app, text),
        Action::Failed(message) => app.push_log(LogLine::error(message)),
    }
}

/// UI-only commands the engine has no concept of. Handled here so they never reach the pipeline.
fn apply_ui_command(app: &mut App, line: &str, dispatcher: &Dispatcher) -> bool {
    let mut words = line.split_whitespace();
    if line == "browser" || line.starts_with("browser ") {
        apply_browser_command(app, line, dispatcher);
        return true;
    }
    if words.next() == Some("zoom") {
        match words.next() {
            Some("in") => app.zoom_in(),
            Some("out") => app.zoom_out(),
            _ => app.fit(),
        }
        return true;
    }
    // A bare `load` (no path) opens the file picker instead of failing on a missing argument.
    // Under the browser target only `browser ...` is accepted, so this stays a deck-target action.
    if app.target != Target::Browser {
        if let Some(deck) = bare_load_deck(line, app.decks, app.default_deck()) {
            app.overlay = Some(Overlay::Files {
                picker: FilePicker::new(None, AUDIO_EXTENSIONS, "load track"),
                purpose: FilePurpose::Load { deck },
            });
            return true;
        }
    }
    false
}

/// Executes a browser action from MIDI. The arrow/Enter map is the same one the keyboard uses.
pub(crate) fn apply_browser_action(app: &mut App, action: BrowserAction, dispatcher: &Dispatcher) {
    let Some(db) = app.library.clone() else {
        app.push_log(LogLine::error("library: no database"));
        return;
    };
    match action {
        BrowserAction::Load { deck } => {
            load_browser_track(
                app,
                dispatcher,
                deck.map_or(app.default_deck(), usize::from),
            );
        }
        BrowserAction::Refresh => {
            let allowed = app.allowed_sources.clone();
            match db.sources() {
                Ok(sources) => {
                    for source in sources
                        .into_iter()
                        .filter(|source| allowed.contains(&source.id))
                    {
                        match db.scan_source(source.id, &mut |_| {}) {
                            Ok(stats) => app.push_log(LogLine::info(format!(
                                "[library] {}: {}",
                                source.name,
                                stats.summary()
                            ))),
                            Err(err) => app.push_log(LogLine::error(format!(
                                "[library] {}: {err}",
                                source.name
                            ))),
                        }
                    }
                }
                Err(err) => app.push_log(LogLine::error(format!("library: {err}"))),
            }
            app.reload_browser();
        }
        BrowserAction::Up => {
            if let Err(err) = app.browser.move_selection(-1, &db) {
                app.push_log(LogLine::error(format!("library: {err}")));
            }
        }
        BrowserAction::Down => {
            if let Err(err) = app.browser.move_selection(1, &db) {
                app.push_log(LogLine::error(format!("library: {err}")));
            }
        }
        BrowserAction::Open => {
            if app.browser.focus() == Focus::Tracks {
                open_load_popup(app, app.default_deck());
            } else if let Err(err) = app.browser.open(&db) {
                app.push_log(LogLine::error(format!("library: {err}")));
            }
        }
        BrowserAction::Exit => {
            if let Err(err) = app.browser.back(&db) {
                app.push_log(LogLine::error(format!("library: {err}")));
            }
        }
    }
}

/// The `browser ...` command family in the TUI. Navigation and the deck chooser are UI state, so
/// they never reach the engine dispatcher.
fn apply_browser_command(app: &mut App, line: &str, dispatcher: &Dispatcher) {
    let mut words = line.split_whitespace();
    let _ = words.next();
    let Some(subcommand) = words.next() else {
        app.push_log(LogLine::info(
            "browser: list | refresh | tracks <id> | show <id> | up | down | open | exit | load | analyse",
        ));
        return;
    };
    let Some(db) = app.library.clone() else {
        app.push_log(LogLine::error("library: no database"));
        return;
    };
    match subcommand {
        "up" => {
            if let Err(err) = app.browser.move_selection(-1, &db) {
                app.push_log(LogLine::error(format!("library: {err}")));
            }
        }
        "down" => {
            if let Err(err) = app.browser.move_selection(1, &db) {
                app.push_log(LogLine::error(format!("library: {err}")));
            }
        }
        "open" => {
            if app.browser.focus() == Focus::Tracks {
                open_load_popup(app, app.default_deck());
            } else if let Err(err) = app.browser.open(&db) {
                app.push_log(LogLine::error(format!("library: {err}")));
            }
        }
        "exit" => {
            if let Err(err) = app.browser.back(&db) {
                app.push_log(LogLine::error(format!("library: {err}")));
            }
        }
        "load" => {
            let deck = match words.next() {
                Some(raw) => match deck_word(raw, app.decks) {
                    Some(deck) => deck,
                    None => {
                        app.push_log(LogLine::error(format!(
                            "browser load: `{raw}` is not a deck (0..{})",
                            app.decks.saturating_sub(1)
                        )));
                        return;
                    }
                },
                None => app.default_deck(),
            };
            open_load_popup(app, deck);
        }
        "analyse" | "analyze" => analyse_browser_track(app, dispatcher),
        "refresh" => {
            let allowed = app.allowed_sources.clone();
            let mut count = 0;
            match db.sources() {
                Ok(sources) => {
                    for source in sources.into_iter().filter(|source| allowed.contains(&source.id)) {
                        match db.scan_source(source.id, &mut |_| {}) {
                            Ok(stats) => {
                                count += 1;
                                app.push_log(LogLine::info(format!(
                                    "[library] {}: {}",
                                    source.name,
                                    stats.summary()
                                )));
                            }
                            Err(err) => app.push_log(LogLine::error(format!(
                                "[library] {}: {err}",
                                source.name
                            ))),
                        }
                    }
                }
                Err(err) => app.push_log(LogLine::error(format!("library: {err}"))),
            }
            if count == 0 {
                app.push_log(LogLine::info("browser: no configured sources"));
            }
            app.reload_browser();
        }
        "list" => match db.sources() {
            Ok(sources) => {
                let allowed = &app.allowed_sources;
                let visible: Vec<_> = sources
                    .into_iter()
                    .filter(|source| allowed.contains(&source.id))
                    .collect();
                if visible.is_empty() {
                    app.push_log(LogLine::info("browser: no configured sources"));
                }
                for source in visible {
                    let lists = db.root_playlists(source.id).map(|l| l.len()).unwrap_or(0);
                    app.push_log(LogLine::info(format!(
                        "{} [{}] {} — {lists} root list(s)",
                        source.id,
                        source.kind.label(),
                        source.root.display()
                    )));
                }
            }
            Err(err) => app.push_log(LogLine::error(format!("browser list: {err}"))),
        },
        "tracks" => {
            let Some(raw) = words.next() else {
                app.push_log(LogLine::error("usage: browser tracks <list-id>"));
                return;
            };
            let Ok(id) = raw.parse::<i64>() else {
                app.push_log(LogLine::error("browser tracks: id must be a number"));
                return;
            };
            match db.tracks_in_playlist(id) {
                Ok(tracks) if tracks.is_empty() => {
                    app.push_log(LogLine::info(format!("browser: list {id} has no tracks")))
                }
                Ok(tracks) => {
                    for track in tracks {
                        app.push_log(LogLine::info(format!(
                            "[{}] {}",
                            track.location_id, track.title
                        )));
                    }
                }
                Err(err) => app.push_log(LogLine::error(format!("browser tracks: {err}"))),
            }
        }
        "show" => {
            let Some(raw) = words.next() else {
                app.push_log(LogLine::error("usage: browser show <location-id>"));
                return;
            };
            let Ok(id) = raw.parse::<i64>() else {
                app.push_log(LogLine::error("browser show: id must be a number"));
                return;
            };
            match db.track_load(id) {
                Ok(Some(track)) => {
                    let bpm = track
                        .bpm
                        .map(|bpm| format!("{bpm:.2} BPM"))
                        .unwrap_or_else(|| "BPM --".to_owned());
                    let key = track
                        .key
                        .map(|key| format!("{} ({})", key.traditional(), key.camelot()))
                        .unwrap_or_else(|| "key --".to_owned());
                    app.push_log(LogLine::info(format!(
                        "{} — {} — {bpm}, {key} — {}",
                        track.title,
                        track.artist.as_deref().unwrap_or("unknown artist"),
                        track.path.display()
                    )));
                }
                Ok(None) => app.push_log(LogLine::error(format!("browser show: no location {id}"))),
                Err(err) => app.push_log(LogLine::error(format!("browser show: {err}"))),
            }
        }
        other => app.push_log(LogLine::error(format!(
            "unknown browser command `{other}` (list | refresh | tracks | show | up | down | open | exit | load | analyse)"
        ))),
    }
}

/// Arrow/Enter/Esc while the browser target owns an empty command line. Returns true when the key
/// was consumed, so the command-line editor never sees it.
fn handle_browser_key(app: &mut App, key: KeyEvent) -> bool {
    let Some(db) = app.library.clone() else {
        return false;
    };
    let result = match key.code {
        KeyCode::Up => app.browser.move_selection(-1, &db),
        KeyCode::Down => app.browser.move_selection(1, &db),
        KeyCode::Left | KeyCode::Esc => app.browser.back(&db),
        KeyCode::Right => app.browser.open(&db),
        KeyCode::Enter => {
            if app.browser.focus() == Focus::Tracks {
                open_load_popup(app, app.default_deck());
                return true;
            }
            app.browser.open(&db)
        }
        _ => return false,
    };
    if let Err(err) = result {
        app.push_log(LogLine::error(format!("library: {err}")));
    }
    true
}

/// Opens the two-row deck chooser for the highlighted track. Enter on a track and `browser load`
/// both land here; the chooser itself is mouse-clickable.
fn open_load_popup(app: &mut App, deck: usize) {
    if app.browser.selected_track().is_none() {
        app.push_log(LogLine::info("library: no track selected"));
        return;
    }
    let (x, y) = app.browser.track_anchor();
    app.overlay = Some(Overlay::LoadTarget {
        x,
        y,
        selected: deck.min(app.decks.saturating_sub(1)),
    });
}

/// Loads the highlighted track into `deck`, preferring a cached analysis over the tag's BPM hint.
fn load_browser_track(app: &mut App, dispatcher: &Dispatcher, deck: usize) {
    let Some(track) = app.browser.selected_track() else {
        app.push_log(LogLine::info("library: no track selected"));
        return;
    };
    let path = track.path.display().to_string();
    let bpm = track.bpm;
    let title = track.title.clone();
    let track_id = track.track_id;
    let location_id = track.location_id;
    let sample_rate = dispatcher.live().audio.sample_rate;
    let decoder_start_ms = app
        .decoder_start_ms
        .unwrap_or_else(|| hypermixx_media::decoder_start_ms(&path).unwrap_or(0.0));
    let grid_offset_ms = app.grid_offset_ms;
    let (analysis, cues) = match app.library.as_ref() {
        Some(db) => (
            db.imported_analysis(location_id, sample_rate, decoder_start_ms, grid_offset_ms)
                .ok()
                .flatten()
                .or_else(|| db.load_analysis(track_id).ok().flatten()),
            db.loaded_cues(location_id, sample_rate, decoder_start_ms, grid_offset_ms)
                .ok(),
        ),
        None => (None, None),
    };
    app.push_log(LogLine::info(format!("library: loading {title}")));
    command::spawn_load_with(deck as u8, path, bpm, analysis, cues, dispatcher);
}

/// Runs the analyser on the highlighted track and stores the result in the library cache.
fn analyse_browser_track(app: &mut App, dispatcher: &Dispatcher) {
    let Some(track) = app.browser.selected_track() else {
        app.push_log(LogLine::info("library: no track selected"));
        return;
    };
    let location_id = track.location_id;
    let Some(db) = app.library.clone() else {
        app.push_log(LogLine::error("library: no database"));
        return;
    };
    match db.track_load(location_id) {
        Ok(Some(load)) => command::spawn_analyse_track(load, db, app.backend, dispatcher),
        Ok(None) => app.push_log(LogLine::error(format!(
            "library: no location {location_id}"
        ))),
        Err(err) => app.push_log(LogLine::error(format!("library: {err}"))),
    }
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
        Overlay::Files {
            mut picker,
            purpose,
        } => match key.code {
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
                            app.connect_midi(dispatcher.command_tx.clone(), &dispatcher.notices);
                        }
                    }
                }
                None => app.overlay = Some(Overlay::Files { picker, purpose }),
            },
            _ => app.overlay = Some(Overlay::Files { picker, purpose }),
        },
        Overlay::LoadTarget { x, y, mut selected } => match key.code {
            KeyCode::Esc => {}
            KeyCode::Up => {
                selected = selected.saturating_sub(1);
                app.overlay = Some(Overlay::LoadTarget { x, y, selected });
            }
            KeyCode::Down => {
                selected = (selected + 1).min(app.decks.saturating_sub(1));
                app.overlay = Some(Overlay::LoadTarget { x, y, selected });
            }
            KeyCode::Enter => load_browser_track(app, dispatcher, selected),
            _ => app.overlay = Some(Overlay::LoadTarget { x, y, selected }),
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
