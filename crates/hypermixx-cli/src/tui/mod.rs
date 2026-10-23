//! The `--tui` front-end: terminal setup, the 30Hz render/poll loop, and shutdown.
//!
//! The loop never blocks on the engine. Commands go out over the same unbounded channel the REPL
//! uses; responses are drained with `try_recv`; the one blocking call in the loop is the meters
//! query, which the producer answers between blocks. Drawing and input therefore cannot stall the
//! audio thread.

pub mod app;
pub mod browser;
pub mod completer;
pub mod guide;
pub mod input;
pub mod picker;
pub mod text;
pub mod view;
pub mod waveform_view;

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};
use hypermixx_audio::{AudioPipeline, Command};

use crate::command::Dispatcher;
use crate::preference;
use crate::response::LogLine;
use crate::session::Session;
use app::App;

/// ~60Hz. The image waveform transports can use the extra ticks; braille is still cheap at this
/// rate, and the engine block (5.8ms) is comfortably inside it.
const TICK: Duration = Duration::from_millis(16);
/// Ask for meters every sixth tick (~10Hz): enough for a readable meter, far from chatty.
const METERS_EVERY: u64 = 6;

/// Runs the TUI until the user quits. Restores the terminal and stops the engine on the way out.
///
/// `midi_port` / `midi_map` come from the startup flags; when a port is given the TUI connects it
/// itself (so the in-TUI pickers can replace it), otherwise it stays disconnected until F2.
#[allow(clippy::too_many_arguments)]
pub fn run(pipeline: &AudioPipeline, session: Session) {
    let Session {
        command_tx,
        backend,
        decks,
        notice_tx,
        notice_rx,
        topology,
        profile,
        live,
        live_source,
        midi_port,
        midi_map,
        library,
    } = session;
    let preference_path = preference_path(profile.as_deref());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let mut dispatcher = Dispatcher::new(command_tx.clone(), notice_tx).tap_events(event_tx);
    dispatcher.with_topology(topology);
    dispatcher.with_profile(profile);
    let ring_frames = live.audio.ring_frames;
    let sample_rate = live.audio.sample_rate;
    dispatcher.with_live(live, live_source);
    dispatcher.with_library(library.clone());
    // Prime every chain's slot list so `fx remove <name>` works from the first command.
    dispatcher.prime(decks, pipeline.response_rx());
    let mut app = App::new(decks, backend, notice_rx, event_rx, dispatcher.slots());
    app.library = library;
    // The header reports the configured ring and rate, not the compile-time defaults.
    app.ring_frames = ring_frames;
    app.sample_rate = sample_rate;
    match preference::load(&preference_path) {
        Ok((preference, notes)) => {
            app.waveform = preference.waveform;
            app.key_format = preference.display.key_format;
            for note in notes {
                app.push_log(LogLine::info(format!("[preference] {note}")));
            }
            app.sync_library(&preference.library);
        }
        Err(err) => {
            app.push_log(LogLine::error(format!("preference: {err}")));
            app.reload_browser();
        }
    }
    app.midi_map = (!midi_map.is_empty()).then_some(midi_map);
    if let Some(port) = midi_port {
        app.midi_port = Some(port);
        app.connect_midi(dispatcher.command_tx.clone(), &dispatcher.notices);
    }

    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(err) => {
            eprintln!("error: terminal: {err}");
            return;
        }
    };
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    // Detection needs the terminal's cell size, so it runs after `try_init` and before the first
    // draw. A failed guess falls back to braille, never to a broken screen.
    let (waveform_view, note) =
        crate::tui::waveform_view::WaveformView::detect(app.waveform, app.decks);
    app.waveform_view = waveform_view;
    app.push_log(LogLine::info(format!(
        "[waveform] renderer {}",
        app.waveform_view.backend().label()
    )));
    if let Some(note) = note {
        app.push_log(LogLine::info(format!("[waveform] {note}")));
    }
    let outcome = event_loop(&mut terminal, &mut app, pipeline, &dispatcher);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();

    // However the loop ended, stop the engine so the producer joins. Report a terminal failure on
    // the normal screen, after `restore`.
    let _ = dispatcher.command_tx.send(Command::Quit);
    drop(dispatcher);
    drop(command_tx);
    if let Err(err) = outcome {
        eprintln!("tui: {err}");
    }
}

/// Preferences live next to the profile that names them, so a profile folder carries its own
/// colours. No profile means the shipped default path.
fn preference_path(profile: Option<&str>) -> String {
    profile
        .and_then(|path| Path::new(path).parent())
        .map(|dir| dir.join("preference.toml"))
        .unwrap_or_else(|| std::path::PathBuf::from(preference::DEFAULT_PATH))
        .to_string_lossy()
        .into_owned()
}

/// A light exponential moving average: enough to keep the header from flickering on every frame,
/// while still showing a strip-rebuild spike for a second or so.
fn ema(previous: &mut f32, sample: f32) {
    if *previous <= 0.0 {
        *previous = sample;
    } else {
        *previous = *previous * 0.9 + sample * 0.1;
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    pipeline: &AudioPipeline,
    dispatcher: &Dispatcher,
) -> io::Result<()> {
    let mut next_tick = Instant::now() + TICK;
    let mut ticks: u64 = 0;
    while !app.quit {
        let now = Instant::now();
        if let Some(last) = app.last_draw_at {
            let interval_ms = now.duration_since(last).as_secs_f32() * 1000.0;
            ema(&mut app.ui_fps, 1000.0 / interval_ms.max(0.001));
        }
        app.last_draw_at = Some(now);
        let draw_start = Instant::now();
        terminal.draw(|frame| view::render(frame, app))?;
        ema(
            &mut app.ui_draw_ms,
            draw_start.elapsed().as_secs_f32() * 1000.0,
        );

        // Engine responses. State answers update the deck readouts silently; the rest is logged.
        while let Ok(response) = pipeline.response_rx().try_recv() {
            app.on_response(&response, pipeline);
        }
        app.drain();
        for action in app.drain_browser_actions() {
            input::apply_browser_action(app, action, dispatcher);
        }

        ticks = ticks.wrapping_add(1);
        // One outstanding state request at a time: a stalled engine must not build a backlog.
        // Polling every tick is what lets a 60Hz Kitty waveform move at 60Hz; the playhead is the
        // value it animates.
        if app.pending_states == 0 && dispatcher.command_tx.send(Command::GetAllStates).is_ok() {
            app.pending_states = 1;
        }
        if ticks.is_multiple_of(METERS_EVERY) {
            app.meters = pipeline.meters();
        }
        // Once a second; kept after the draw so a sysinfo read never sits in the input path.
        app.process.refresh();

        // Handle input until the next tick, then redraw. Keys stay responsive between frames.
        loop {
            let now = Instant::now();
            if now >= next_tick {
                break;
            }
            if event::poll(next_tick - now)? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        input::handle(app, key, dispatcher, pipeline);
                    }
                    Event::Mouse(mouse) => input::handle_mouse(app, mouse, dispatcher),
                    _ => {}
                }
            } else {
                break;
            }
        }
        next_tick += TICK;
        if next_tick <= Instant::now() {
            next_tick = Instant::now() + TICK;
        }
    }
    Ok(())
}
