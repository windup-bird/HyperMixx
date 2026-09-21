//! The `--tui` front-end: terminal setup, the 30Hz render/poll loop, and shutdown.
//!
//! The loop never blocks on the engine. Commands go out over the same unbounded channel the REPL
//! uses; responses are drained with `try_recv`; the one blocking call in the loop is the meters
//! query, which the producer answers between blocks. Drawing and input therefore cannot stall the
//! audio thread.

pub mod app;
pub mod completer;
pub mod input;
pub mod view;
pub mod waveform_view;

use std::io;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use crossterm::event::{self, Event, KeyEventKind};
use hypermixx_audio::{AudioPipeline, Command};
use hypermixx_core::Backend;

use crate::command::Dispatcher;
use crate::notices::NoticeTx;
use crate::response::LogLine;
use app::App;

/// ~30Hz. The engine's block is 5.8ms, so this is comfortably inside its command handling.
const TICK: Duration = Duration::from_millis(33);
/// Ask for meters every third tick (~10Hz): enough for a readable meter, far from chatty.
const METERS_EVERY: u64 = 3;

/// Runs the TUI until the user quits. Restores the terminal and stops the engine on the way out.
pub fn run(
    pipeline: &AudioPipeline,
    command_tx: Sender<Command>,
    backend: Backend,
    decks: usize,
    notice_tx: NoticeTx,
    notice_rx: Receiver<LogLine>,
) {
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let dispatcher = Dispatcher::new(command_tx.clone(), notice_tx).tap_events(event_tx);
    // Prime every chain's slot list so `fx remove <name>` works from the first command.
    dispatcher.prime(decks, pipeline.response_rx());
    let mut app = App::new(decks, backend, notice_rx, event_rx, dispatcher.slots());

    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(err) => {
            eprintln!("error: terminal: {err}");
            return;
        }
    };
    let outcome = event_loop(&mut terminal, &mut app, pipeline, &dispatcher);
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

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    pipeline: &AudioPipeline,
    dispatcher: &Dispatcher,
) -> io::Result<()> {
    let mut next_tick = Instant::now() + TICK;
    let mut ticks: u64 = 0;
    while !app.quit {
        let draw_start = Instant::now();
        terminal.draw(|frame| view::render(frame, app))?;
        app.ui_frame_ms = draw_start.elapsed().as_secs_f32() * 1000.0;

        // Engine responses. State answers update the deck readouts silently; the rest is logged.
        while let Ok(response) = pipeline.response_rx().try_recv() {
            app.on_response(&response, pipeline);
        }
        app.drain();

        // One outstanding state request at a time: a stalled engine must not build a backlog.
        if app.pending_states == 0 && dispatcher.command_tx.send(Command::GetAllStates).is_ok() {
            app.pending_states = 1;
            app.state_sent = Some(Instant::now());
        }
        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(METERS_EVERY) {
            app.meters = pipeline.meters();
        }

        // Handle input until the next tick, then redraw. Keys stay responsive between frames.
        loop {
            let now = Instant::now();
            if now >= next_tick {
                break;
            }
            if event::poll(next_tick - now)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Release {
                        input::handle(app, key, dispatcher, pipeline);
                    }
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
