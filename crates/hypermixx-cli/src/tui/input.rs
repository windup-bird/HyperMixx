//! Keyboard mapping.
//!
//! The command box is always focused, so printable characters are text and every engine action goes
//! through a typed command. The only keys this layer owns are editing/navigation plus `Tab` to
//! cycle the command target (deck0 / deck1 / … / browser). `Ctrl+C` still quits outright.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use hypermixx_audio::AudioPipeline;
use hypermixx_core::{
    BrowserAction, Command, CueOp, FaderTarget, FxChainId, FxSlotRef, HotCueOp, LoopOp, NudgeOp,
    PhaseMode, SyncOp,
};

use crate::command::{self, Action, Dispatcher};
use crate::response::LogLine;
use crate::tui::app::{
    App, FilePurpose, Hit, Knob, LoopHit, Overlay, PadMode, Target, TransportHit,
};
use crate::tui::browser::Focus;
use crate::tui::picker::FilePicker;
use crate::tui::settings::SettingsScreen;

/// Extensions the `load` picker offers (the decoder's supported containers/audio).
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "mp1", "mp2", "wav", "aiff", "aif", "flac", "m4a", "mp4", "aac", "ogg", "oga",
];
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

    // `F9` opens the settings screen, which is where the controller port, the map file and the
    // mixer's outputs are chosen (it replaced the old `F2`/`F3` pickers).
    if key.code == KeyCode::F(9) {
        app.overlay = Some(Overlay::Settings(Box::new(open_settings(app))));
        return;
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
    let (x, y) = (mouse.column, mouse.row);
    // Controls come first: a click that lands on a pad or a fader belongs to that control, not to
    // whatever panel happens to be under it.
    if let Some(hit) = app
        .hits
        .iter()
        .rev()
        .find(|(rect, _)| x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom())
        .map(|(_, hit)| *hit)
    {
        apply_hit(app, hit, dispatcher);
        return;
    }
    let Some(db) = app.library.clone() else {
        return;
    };
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
            app.overlay = Some(Overlay::LoadTarget { x, y, selected: 0 });
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

/// Opens the settings screen: the profile's files, the controller, and the mixer outputs.
fn open_settings(app: &App) -> SettingsScreen {
    SettingsScreen::open(crate::tui::settings::Context {
        profile: app.profile_path.clone(),
        mixer: app.mixer_path.clone(),
        live: app.live_path.clone(),
        preference: Some(app.preference_path.clone()),
        controller_port: app.midi_port.clone(),
        controller_map: app.midi_map.clone(),
        controller: app.midi_port.clone(),
        engine_rate: app.sample_rate,
        // Probed before the terminal was taken over: ALSA/JACK print to stderr, which would scribble
        // over the alternate screen if the settings screen probed on open.
        devices: app.devices.clone(),
    })
}

/// Runs one click. Pads and faders are the mouse's whole vocabulary: every one of them is a command
/// the command line could also have typed.
fn apply_hit(app: &mut App, hit: Hit, dispatcher: &Dispatcher) {
    // Any control on a deck also makes that deck the command target, so a click and the next typed
    // command cannot disagree about which deck they mean.
    let deck = match hit {
        Hit::Pad { deck, .. }
        | Hit::PadMode { deck, .. }
        | Hit::BeatMode { deck, .. }
        | Hit::PadPanel { deck, .. }
        | Hit::EqPanel { deck, .. }
        | Hit::FxSelect { deck, .. }
        | Hit::FxToggle { deck, .. }
        | Hit::FxParam { deck, .. }
        | Hit::FxBeats { deck, .. }
        | Hit::TempoFader { deck, .. }
        | Hit::Bend { deck, .. }
        | Hit::LoopBeats { deck, .. }
        | Hit::JumpBeats { deck, .. }
        | Hit::Loop { deck, .. }
        | Hit::Jump { deck, .. }
        | Hit::Transport { deck, .. }
        | Hit::Knob { deck, .. }
        | Hit::Fader { deck, .. } => Some(deck),
        Hit::Crossfader { .. } | Hit::Settings => None,
    };
    if let Some(deck) = deck {
        app.target = Target::Deck(deck);
        app.focused = deck;
    }

    let state = deck.map(|deck| app.states[deck].clone());
    let send = |command: Command| {
        let _ = dispatcher.command_tx.send(command);
    };
    match hit {
        Hit::Settings => app.overlay = Some(Overlay::Settings(Box::new(open_settings(app)))),
        Hit::PadMode { deck, mode } => {
            if let Some(slot) = app.pad_mode.get_mut(deck) {
                *slot = mode;
            }
        }
        Hit::BeatMode { deck, mode } => {
            if let Some(slot) = app.beat_mode.get_mut(deck) {
                *slot = mode;
            }
        }
        Hit::PadPanel { deck, panel } => {
            if let Some(slot) = app.pad_panel.get_mut(deck) {
                *slot = panel;
            }
        }
        Hit::EqPanel { deck, panel } => {
            if let Some(slot) = app.eq_panel.get_mut(deck) {
                *slot = panel;
            }
        }
        Hit::FxSelect { deck, slot } => {
            if let Some(selected) = app.fx_slot.get_mut(deck) {
                *selected = slot;
            }
        }
        Hit::FxToggle { deck, slot } => send(Command::ToggleFx {
            slot: FxSlotRef {
                chain: FxChainId::Deck(deck as u8),
                index: slot,
            },
        }),
        Hit::FxParam {
            deck,
            slot,
            param,
            value,
        } => {
            let name = app
                .deck_fx
                .get(deck)
                .and_then(|slots| slots.iter().find(|status| status.index == slot))
                .and_then(|status| status.params.get(param))
                .map(|param| param.name.clone());
            if let Some(name) = name {
                send(Command::SetFxParam {
                    slot: FxSlotRef {
                        chain: FxChainId::Deck(deck as u8),
                        index: slot,
                    },
                    name,
                    value,
                });
            }
        }
        Hit::FxBeats { deck, slot, double } => {
            let param = app
                .deck_fx
                .get(deck)
                .and_then(|slots| slots.iter().find(|status| status.index == slot))
                .and_then(|status| status.params.iter().find(|param| param.beats != 0))
                .cloned();
            if let Some(param) = param {
                if let Some(value) = param.scaled_beat_value(double) {
                    send(Command::SetFxParam {
                        slot: FxSlotRef {
                            chain: FxChainId::Deck(deck as u8),
                            index: slot,
                        },
                        name: param.name,
                        value,
                    });
                }
            }
        }
        Hit::Pad { deck, index } => {
            let mode = app.pad_mode.get(deck).copied().unwrap_or_default();
            let deck_id = deck as u8;
            match mode {
                PadMode::HotCue => {
                    let set = state.as_ref().is_some_and(|state| {
                        state.hot_cues.get(index).is_some_and(Option::is_some)
                    });
                    let op = if set {
                        HotCueOp::Trigger(index as u8)
                    } else {
                        HotCueOp::Set(index as u8)
                    };
                    send(Command::HotCue { deck_id, op });
                }
                PadMode::BeatLoop => {
                    if let Some(beats) = mode.loop_beats(index) {
                        send(Command::Loop {
                            deck_id,
                            op: LoopOp::Beats(beats),
                        });
                    }
                }
                PadMode::BeatJump => {
                    if let Some(beats) = mode.jump_beats(index) {
                        send(Command::BeatJump { deck_id, beats });
                    }
                }
            }
        }
        Hit::TempoFader { deck, direction } => {
            let state = &app.states[deck];
            let position = (state.tempo_fader + f32::from(direction) * 0.01).clamp(-1.0, 1.0);
            send(Command::SetTempoFader {
                deck_id: deck as u8,
                position,
            });
        }
        Hit::Bend { deck, direction } => {
            let op = if direction == 0 {
                NudgeOp::Stop
            } else {
                NudgeOp::Start {
                    delta: f32::from(direction) * 0.04,
                    seconds: Some(0.25),
                }
            };
            send(Command::Nudge {
                deck_id: deck as u8,
                op,
            });
        }
        Hit::LoopBeats { deck, halve } => send(Command::ScaleLoopBeats {
            deck_id: deck as u8,
            halve,
        }),
        Hit::JumpBeats { deck, halve } => send(Command::ScaleJumpBeats {
            deck_id: deck as u8,
            halve,
        }),
        Hit::Loop { deck, op } => {
            let op = match op {
                LoopHit::In => LoopOp::In,
                LoopHit::Out => LoopOp::Out,
                LoopHit::Toggle => LoopOp::Toggle,
                LoopHit::Exit => LoopOp::Exit,
            };
            send(Command::Loop {
                deck_id: deck as u8,
                op,
            });
        }
        Hit::Jump { deck, direction } => send(Command::BeatJumpStep {
            deck_id: deck as u8,
            direction,
        }),
        Hit::Transport { deck, action } => {
            let deck_id = deck as u8;
            match action {
                TransportHit::Sync => send(Command::Sync {
                    deck_id,
                    op: SyncOp::Phase {
                        mode: PhaseMode::Pid,
                        t_seconds: None,
                    },
                }),
                TransportHit::Cue => send(Command::Cue {
                    deck_id,
                    op: CueOp::Hold,
                }),
                TransportHit::Play => send(Command::TogglePlay { deck_id }),
                TransportHit::Keylock => send(Command::ToggleKeylock { deck_id }),
            }
        }
        Hit::Knob { deck, knob, value } => {
            let value = value.clamp(-1.0, 1.0);
            if knob == Knob::Gain {
                send(Command::SetFader {
                    target: FaderTarget::Gain(deck as u8),
                    value,
                });
            } else if let Some(index) = app.knob_slot(deck, knob) {
                let name = match knob {
                    Knob::High => "high",
                    Knob::Mid => "mid",
                    Knob::Low => "low",
                    _ => "value",
                };
                send(Command::SetFxParam {
                    slot: FxSlotRef {
                        chain: FxChainId::Deck(deck as u8),
                        index,
                    },
                    name: name.to_owned(),
                    value,
                });
            }
        }
        Hit::Fader { deck, value } => send(Command::SetFader {
            target: FaderTarget::Deck(deck as u8),
            value: value.clamp(-1.0, 1.0),
        }),
        Hit::Crossfader { value } => send(Command::SetFader {
            target: FaderTarget::Crossfader,
            value: value.clamp(-1.0, 1.0),
        }),
    }
}

/// Reloads the mixer topology after the settings screen rewrote an output.
///
/// The fallback is the point: a session started without `--config`/`--profile` boots from
/// [`command::DEFAULT_TOPOLOGY`] (`main::default_config` reads exactly that file), but leaves
/// `app.mixer_path` unset. Without the fallback the screen would write the file and the running
/// engine would keep the old device bindings — silence that only a restart cures. The write path is
/// the same file, so remembering it here also keeps the two views in step.
fn reload_mixer(app: &mut App, dispatcher: &Dispatcher) {
    let path = app
        .mixer_path
        .clone()
        .unwrap_or_else(|| command::DEFAULT_TOPOLOGY.to_owned());
    app.mixer_path = Some(path.clone());
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let _ = dispatcher.command_tx.send(Command::MixerReload { text });
        }
        Err(err) => app.push_log(LogLine::error(format!("{path}: {err}"))),
    }
}

/// Points the session at a different topology file: remember it, then reload from its text (the
/// settings screen's `mixer` row). The typed equivalent is `mixer reload <path>`.
fn switch_mixer(app: &mut App, dispatcher: &Dispatcher, path: &str) {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            app.mixer_path = Some(path.to_owned());
            dispatcher.with_topology(Some(path.to_owned()));
            let _ = dispatcher.command_tx.send(Command::MixerReload { text });
            app.push_log(LogLine::info(format!("mixer <- {path}")));
        }
        Err(err) => app.push_log(LogLine::error(format!("{path}: {err}"))),
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
    // A deck verb needs a deck target: while the browser owns the command line there is none, so
    // `play` is refused instead of quietly landing on whichever deck was focused last. `Tab` picks a
    // deck, and a line that names one (`deck1 play`) needs no target at all.
    let deck = match app.target {
        Target::Deck(index) => Some(index as u8),
        Target::Browser => None,
    };
    let action = command::dispatch(
        &line,
        dispatcher,
        pipeline,
        app.backends,
        app.decks,
        deck,
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
    if let Some(deck) = bare_load_deck(line, app.decks, app.default_deck()) {
        app.overlay = Some(Overlay::Files {
            picker: FilePicker::new(None, AUDIO_EXTENSIONS, "load track"),
            purpose: FilePurpose::Load { deck },
        });
        return true;
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
                open_load_popup(app, None);
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
                open_load_popup(app, None);
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
            open_load_popup(app, Some(deck));
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
                open_load_popup(app, None);
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
///
/// `preferred` only comes from an explicit `browser load <deck>`: every other way in starts on the
/// first deck, because the chooser is about picking one, not about confirming the current focus.
fn open_load_popup(app: &mut App, preferred: Option<usize>) {
    if app.browser.selected_track().is_none() {
        app.push_log(LogLine::info("library: no track selected"));
        return;
    }
    let (x, y) = app.browser.track_anchor();
    app.overlay = Some(Overlay::LoadTarget {
        x,
        y,
        selected: preferred.unwrap_or(0).min(app.decks.saturating_sub(1)),
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
        Ok(Some(load)) => command::spawn_analyse_track(load, db, app.backends, dispatcher),
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

/// Parses a target-first deck word (`deck1`, `d1`, `1`). Numbers are 1-based, like every label in
/// the interface; the index returned is the engine's 0-based one.
fn deck_word(token: &str, decks: usize) -> Option<usize> {
    let digits = token
        .strip_prefix("deck")
        .or_else(|| token.strip_prefix('d'))
        .unwrap_or(token);
    let number: usize = digits.parse().ok()?;
    let index = number.checked_sub(1)?;
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
                        app.connect_midi(
                            dispatcher.command_tx.clone(),
                            &dispatcher.notices,
                            dispatcher,
                        );
                    } else {
                        app.overlay = Some(Overlay::Files {
                            picker: FilePicker::new(
                                Some(std::path::Path::new(crate::profile::default_map_path())),
                                &["toml"],
                                "controller map",
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
                            app.connect_midi(
                                dispatcher.command_tx.clone(),
                                &dispatcher.notices,
                                dispatcher,
                            );
                        }
                    }
                }
                None => app.overlay = Some(Overlay::Files { picker, purpose }),
            },
            _ => app.overlay = Some(Overlay::Files { picker, purpose }),
        },
        Overlay::Settings(mut screen) => {
            let outcome = screen.handle_key(key.code);
            // A newly chosen topology first, then any device binding the screen just wrote into it:
            // the second step has to read the file the first one selected.
            if let Some(path) = outcome.mixer.clone() {
                switch_mixer(app, dispatcher, &path);
            }
            if outcome.mixer_saved {
                reload_mixer(app, dispatcher);
            }
            if let Some((port, map)) = outcome.controller {
                // The screen picked a controller port and/or a map: connect it now, exactly as the
                // old F2/F3 shortcuts did, then keep the screen open.
                app.midi_port = Some(port);
                app.midi_map = Some(map);
                app.connect_midi(
                    dispatcher.command_tx.clone(),
                    &dispatcher.notices,
                    dispatcher,
                );
            }
            if let Some(path) = outcome.live {
                // Re-read the file exactly as `live reload <path>` would; the engine keeps running.
                app.live_path = Some(path.clone());
                match command::reload_live(dispatcher, Some(&path)) {
                    Action::Message(text) => push_block(app, text),
                    Action::Failed(message) => app.push_log(LogLine::error(message)),
                    Action::Continue | Action::Quit => {}
                }
            }
            if let Some(path) = outcome.preference {
                app.preference_path = path.clone();
                match crate::preference::load(&path) {
                    Ok((preference, notes)) => {
                        app.theme = preference.theme;
                        for note in notes {
                            app.push_log(LogLine::error(note));
                        }
                    }
                    Err(err) => app.push_log(LogLine::error(err)),
                }
            }
            if !outcome.close {
                app.overlay = Some(Overlay::Settings(screen));
            }
        }
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

    /// An app wired to a command channel, so a click can be checked by the command it produced.
    fn wired() -> (App, Dispatcher, crossbeam_channel::Receiver<Command>) {
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (notice_tx, _notice_rx) = crossbeam_channel::unbounded();
        let (_app_notice_tx, app_notice_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        let app = App::new(
            2,
            hypermixx_library::analyser::Backends::default(),
            app_notice_rx,
            event_rx,
            Default::default(),
            Default::default(),
        );
        let dispatcher = Dispatcher::new(command_tx, notice_tx);
        (app, dispatcher, command_rx)
    }

    /// `Command` carries a trait object, so it has no `PartialEq`; its `Debug` form names every
    /// field, which is enough to assert on what a click sent.
    fn sent(rx: &crossbeam_channel::Receiver<Command>) -> String {
        format!("{:?}", rx.try_recv().expect("a command was sent"))
    }

    #[test]
    fn clicking_an_empty_hot_cue_pad_sets_it_and_a_set_one_triggers_it() {
        let (mut app, dispatcher, rx) = wired();
        app.pad_mode[0] = crate::tui::app::PadMode::HotCue;
        apply_hit(&mut app, Hit::Pad { deck: 0, index: 3 }, &dispatcher);
        let command = sent(&rx);
        assert!(command.contains("HotCue"), "{command}");
        assert!(command.contains("Set(3)"), "{command}");

        app.states[0].hot_cues[3] = Some(hypermixx_core::HotCue { slot: 3, frame: 10 });
        apply_hit(&mut app, Hit::Pad { deck: 0, index: 3 }, &dispatcher);
        let command = sent(&rx);
        assert!(command.contains("Trigger(3)"), "{command}");
    }

    #[test]
    fn beat_loop_and_beat_jump_pads_send_the_beat_length_they_show() {
        let (mut app, dispatcher, rx) = wired();
        app.pad_mode[0] = crate::tui::app::PadMode::BeatLoop;
        apply_hit(&mut app, Hit::Pad { deck: 0, index: 0 }, &dispatcher);
        let command = sent(&rx);
        assert!(command.contains("Beats"), "{command}");

        app.pad_mode[0] = crate::tui::app::PadMode::BeatJump;
        apply_hit(&mut app, Hit::Pad { deck: 0, index: 0 }, &dispatcher);
        let command = sent(&rx);
        assert!(command.contains("BeatJump"), "{command}");
        assert!(command.contains("-16"), "{command}");
    }

    #[test]
    fn a_knob_click_writes_the_engine_parameter_and_a_click_on_a_deck_makes_it_the_target() {
        let (mut app, dispatcher, rx) = wired();
        app.deck_fx[1] = vec![
            hypermixx_core::FxSlotStatus {
                index: 0,
                role: "eq".to_owned(),
                kind: "eq".to_owned(),
                enabled: true,
                flows: None,
                params: vec![
                    hypermixx_core::FxParamStatus {
                        name: "low".to_owned(),
                        value: 0.0,
                        range: Some((-1.0, 1.0)),
                        beats: 0,
                    },
                    hypermixx_core::FxParamStatus {
                        name: "mid".to_owned(),
                        value: 0.0,
                        range: Some((-1.0, 1.0)),
                        beats: 0,
                    },
                    hypermixx_core::FxParamStatus {
                        name: "high".to_owned(),
                        value: 0.0,
                        range: Some((-1.0, 1.0)),
                        beats: 0,
                    },
                ],
            },
            hypermixx_core::FxSlotStatus {
                index: 1,
                role: "filter".to_owned(),
                kind: "filter".to_owned(),
                enabled: true,
                flows: None,
                params: vec![hypermixx_core::FxParamStatus {
                    name: "value".to_owned(),
                    value: 0.0,
                    range: Some((-1.0, 1.0)),
                    beats: 0,
                }],
            },
        ];
        apply_hit(
            &mut app,
            Hit::Knob {
                deck: 1,
                knob: Knob::Filter,
                value: 0.5,
            },
            &dispatcher,
        );
        assert_eq!(app.target, Target::Deck(1));
        let command = sent(&rx);
        assert!(command.contains("SetFxParam"), "{command}");
        assert!(command.contains("Deck(1)"), "{command}");
        assert!(command.contains("index: 1"), "{command}");
        assert!(command.contains("\"value\""), "{command}");
        assert!(command.contains("0.5"), "{command}");

        // The trim knob has no slot: it is a fader, not an effect.
        apply_hit(
            &mut app,
            Hit::Knob {
                deck: 1,
                knob: Knob::Gain,
                value: -0.5,
            },
            &dispatcher,
        );
        let command = sent(&rx);
        assert!(command.contains("SetFader"), "{command}");
        assert!(command.contains("Gain(1)"), "{command}");

        // A deck with no `eq` slot gets nothing at all rather than an error.
        apply_hit(
            &mut app,
            Hit::Knob {
                deck: 0,
                knob: Knob::Low,
                value: 0.2,
            },
            &dispatcher,
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_crossfader_and_the_shown_fader_are_absolute_positions() {
        let (mut app, dispatcher, rx) = wired();
        apply_hit(&mut app, Hit::Crossfader { value: -0.75 }, &dispatcher);
        let command = sent(&rx);
        assert!(command.contains("Crossfader"), "{command}");
        assert!(command.contains("-0.75"), "{command}");

        apply_hit(
            &mut app,
            Hit::Fader {
                deck: 1,
                value: 0.25,
            },
            &dispatcher,
        );
        let command = sent(&rx);
        assert!(command.contains("Deck(1)"), "{command}");
        assert!(command.contains("0.25"), "{command}");
    }

    #[test]
    fn pad_and_beat_modes_are_front_end_state_and_send_nothing() {
        let (mut app, dispatcher, rx) = wired();
        apply_hit(
            &mut app,
            Hit::PadMode {
                deck: 0,
                mode: crate::tui::app::PadMode::BeatJump,
            },
            &dispatcher,
        );
        apply_hit(
            &mut app,
            Hit::BeatMode {
                deck: 0,
                mode: crate::tui::app::BeatMode::ManualLoop,
            },
            &dispatcher,
        );
        assert_eq!(app.pad_mode[0], crate::tui::app::PadMode::BeatJump);
        assert_eq!(app.beat_mode[0], crate::tui::app::BeatMode::ManualLoop);
        assert!(rx.try_recv().is_err());
    }

    /// A knob finds its slot by role, so swapping the effect in it does not move the control.
    #[test]
    fn a_swapped_effect_stays_under_the_same_knob() {
        let (mut app, _dispatcher, _rx) = wired();
        app.deck_fx[0] = vec![
            hypermixx_core::FxSlotStatus {
                index: 0,
                role: "eq".to_owned(),
                kind: "eq".to_owned(),
                enabled: true,
                flows: None,
                params: vec![hypermixx_core::FxParamStatus {
                    name: "low".to_owned(),
                    value: 0.0,
                    range: Some((-1.0, 1.0)),
                    beats: 0,
                }],
            },
            hypermixx_core::FxSlotStatus {
                index: 1,
                role: "filter".to_owned(),
                kind: "moog".to_owned(),
                enabled: true,
                flows: None,
                params: vec![
                    hypermixx_core::FxParamStatus {
                        name: "value".to_owned(),
                        value: -0.4,
                        range: Some((-1.0, 1.0)),
                        beats: 0,
                    },
                    hypermixx_core::FxParamStatus {
                        name: "resonance".to_owned(),
                        value: 0.3,
                        range: Some((0.0, 1.0)),
                        beats: 0,
                    },
                ],
            },
        ];
        assert_eq!(app.knob_slot(0, Knob::Filter), Some(1));
        assert_eq!(app.knob_value(0, Knob::Filter), Some(-0.4));
    }

    #[test]
    fn the_transport_buttons_send_toggle_cue_and_keylock() {
        let (mut app, dispatcher, rx) = wired();
        apply_hit(
            &mut app,
            Hit::Transport {
                deck: 0,
                action: TransportHit::Play,
            },
            &dispatcher,
        );
        assert!(sent(&rx).contains("TogglePlay"));
        apply_hit(
            &mut app,
            Hit::Transport {
                deck: 0,
                action: TransportHit::Cue,
            },
            &dispatcher,
        );
        let command = sent(&rx);
        assert!(command.contains("Cue"), "{command}");
        assert!(command.contains("Hold"), "{command}");
        apply_hit(
            &mut app,
            Hit::Transport {
                deck: 0,
                action: TransportHit::Keylock,
            },
            &dispatcher,
        );
        assert!(sent(&rx).contains("ToggleKeylock"));
    }

    /// The browser owns the arrow keys *and* the command line: a bare `load` is still the UI's own
    /// picker affordance, but a bare deck verb is refused (see `command.rs`'s `Target::Unset`).
    #[test]
    fn a_bare_load_under_the_browser_still_opens_the_picker() {
        let (mut app, dispatcher, _rx) = wired();
        app.target = Target::Browser;
        assert!(apply_ui_command(&mut app, "load", &dispatcher));
        let expected = app.default_deck();
        match app.overlay {
            Some(Overlay::Files {
                purpose: FilePurpose::Load { deck },
                ..
            }) => assert_eq!(deck, expected),
            _ => panic!("a bare `load` should open the track picker"),
        }
    }

    #[test]
    fn a_bare_load_is_recognised_with_and_without_a_target() {
        // Deck words are 1-based: `deck1` is the engine's deck 0.
        assert_eq!(bare_load_deck("load", 2, 1), Some(1));
        assert_eq!(bare_load_deck("deck1 load", 2, 1), Some(0));
        assert_eq!(bare_load_deck("1 load", 2, 1), Some(0));
        assert_eq!(bare_load_deck("d2 load", 2, 0), Some(1));
        // A map-like trailing word is not a bare load.
        assert_eq!(bare_load_deck("load test.mp3", 2, 0), None);
        assert_eq!(bare_load_deck("deck1 load test.mp3", 2, 0), None);
        assert_eq!(bare_load_deck("play", 2, 0), None);
    }

    #[test]
    fn deck_words_parse_and_reject_out_of_range() {
        assert_eq!(deck_word("deck1", 2), Some(0));
        assert_eq!(deck_word("d1", 2), Some(0));
        assert_eq!(deck_word("1", 2), Some(0));
        assert_eq!(deck_word("deck2", 2), Some(1));
        assert_eq!(deck_word("3", 2), None, "past the last deck");
        assert_eq!(deck_word("0", 2), None, "decks are 1-based");
        assert_eq!(deck_word("load", 2), None);
    }
}
