//! The line-oriented front-end: read a line, dispatch it, print what comes back.
//!
//! A printer thread drains engine responses and worker notices while the main thread blocks on
//! stdin, so a slow analyst or a chatty engine never stalls typing.

use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::Receiver;
use hypermixx_audio::{AudioPipeline, Command, CommandResponse};
use hypermixx_library::{ScanProgress, ScanService};

use crate::command::{self, Action, Slots};
use crate::response::{self, Level, LogLine};
use crate::session::Session;

/// Runs one REPL session. Returns when the user quits or stdin hits EOF.
pub fn run(pipeline: &AudioPipeline, session: Session) {
    let Session {
        command_tx,
        backends,
        decks,
        notice_tx,
        notice_rx,
        topology,
        live,
        live_source,
        midi_control,
        midi_state,
        library,
        ..
    } = session;
    let sample_rate = live.audio.sample_rate;
    let response_rx = pipeline.response_rx().clone();
    let mut dispatcher = command::Dispatcher::new(command_tx.clone(), notice_tx);
    dispatcher.with_topology(topology);
    dispatcher.with_live(live, live_source);
    dispatcher.with_library(library);
    if let (Some(control), Some(state)) = (midi_control, midi_state) {
        dispatcher.set_midi(control, state);
    }
    // Prime every chain's slot list so `fx remove <name>` works from the first command.
    dispatcher.prime(decks, pipeline.response_rx());
    let print_slots = dispatcher.slots();
    let scan = dispatcher.scan();
    std::thread::scope(|scope| {
        // The printer owns its receiver clones so it can outlive this function's borrows. It also
        // polls scan progress: `browser refresh` is asynchronous, so its results arrive here.
        let printer = scope.spawn(move || {
            print_loop(response_rx, notice_rx, print_slots, sample_rate, scan)
        });

        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        // `read_line` returns None on EOF / Ctrl-D, which ends the session.
        while let Some(line) = read_line(&mut stdin) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match command::dispatch(line, &dispatcher, pipeline, backends, decks, Some(0)) {
                Action::Continue => {}
                Action::Quit => break,
                Action::Message(text) => println!("{text}"),
                Action::Failed(message) => eprintln!("error: {message}"),
            }
        }

        // However the loop ended, tell the engine to stop; its reply (and the channel closing)
        // is what lets the printer finish, and dropping the pipeline joins the producer.
        let _ = command_tx.send(Command::Quit);
        drop(command_tx);
        drop(dispatcher);
        let _ = printer.join();
    });
}

/// Prints responses and notices until the engine hangs up. Also keeps the slot book current so
/// `fx` name addressing works in the REPL, not just the TUI.
fn print_loop(
    response_rx: Receiver<CommandResponse>,
    notice_rx: Receiver<LogLine>,
    slots: Slots,
    sample_rate: u32,
    scan: Arc<Mutex<ScanService>>,
) {
    let mut notices_open = true;
    // The scan worker has no channel into the REPL's printer, so poll it on a slow tick. This is
    // the same pull-based API the TUI uses; nothing here blocks stdin.
    let tick = crossbeam_channel::tick(Duration::from_millis(100));
    loop {
        if notices_open {
            crossbeam_channel::select! {
                recv(response_rx) -> message => match message {
                    Ok(response) => {
                        slots.record(&response);
                        print_lines(&response::format_response(&response, sample_rate));
                    }
                    Err(_) => break,
                },
                recv(notice_rx) -> notice => match notice {
                    Ok(line) => print_line(&line),
                    // Workers are done; fall back to responses only.
                    Err(_) => notices_open = false,
                },
                recv(tick) -> _ => print_scan_progress(&scan),
            }
        } else {
            crossbeam_channel::select! {
                recv(response_rx) -> message => match message {
                    Ok(response) => {
                        slots.record(&response);
                        print_lines(&response::format_response(&response, sample_rate));
                    }
                    Err(_) => break,
                },
                recv(tick) -> _ => print_scan_progress(&scan),
            }
        }
    }
}

/// Turns the library's plain-data progress events into REPL lines. The TUI maps the same events
/// onto its browser and log instead.
fn print_scan_progress(scan: &Arc<Mutex<ScanService>>) {
    let Ok(service) = scan.lock() else {
        return;
    };
    while let Some(event) = service.poll_next() {
        let line = match event {
            ScanProgress::SourceRegistered { name, .. } => {
                LogLine::info(format!("[library] registered {name}"))
            }
            ScanProgress::SourcesRemoved { .. } => {
                LogLine::info("[library] device removed".to_owned())
            }
            ScanProgress::ListsReady { name, .. } => {
                LogLine::info(format!("[library] {name}: lists ready"))
            }
            ScanProgress::SourceDone {
                name, kind, stats, ..
            } => LogLine::info(format!(
                "[library] {name} [{kind}]: {}",
                stats.summary()
            )),
            ScanProgress::Failed { name, error, .. } => {
                LogLine::error(format!("[library] {name}: {error}"))
            }
        };
        print_line(&line);
    }
}

fn print_lines(lines: &[LogLine]) {
    for line in lines {
        print_line(line);
    }
}

fn print_line(line: &LogLine) {
    match line.level {
        Level::Error => eprintln!("{}", line.text),
        Level::Info => println!("{}", line.text),
    }
}

fn read_line(stdin: &mut impl BufRead) -> Option<String> {
    print!("hypermixx> ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match stdin.read_line(&mut line) {
        Ok(0) => {
            println!();
            None
        }
        Ok(_) => Some(line),
        Err(err) => {
            eprintln!("error: stdin ({err})");
            None
        }
    }
}
