//! The line-oriented front-end: read a line, dispatch it, print what comes back.
//!
//! A printer thread drains engine responses and worker notices while the main thread blocks on
//! stdin, so a slow analyst or a chatty engine never stalls typing.

use std::io::{self, BufRead, Write};

use crossbeam_channel::Receiver;
use hypermixx_audio::{AudioPipeline, Command, CommandResponse};

use crate::command::{self, Action, Slots};
use crate::response::{self, Level, LogLine};
use crate::session::Session;

/// Runs one REPL session. Returns when the user quits or stdin hits EOF.
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
        library,
        ..
    } = session;
    let sample_rate = live.audio.sample_rate;
    let response_rx = pipeline.response_rx().clone();
    let mut dispatcher = command::Dispatcher::new(command_tx.clone(), notice_tx);
    dispatcher.with_topology(topology);
    dispatcher.with_profile(profile);
    dispatcher.with_live(live, live_source);
    dispatcher.with_library(library);
    // Prime every chain's slot list so `fx remove <name>` works from the first command.
    dispatcher.prime(decks, pipeline.response_rx());
    let print_slots = dispatcher.slots();
    std::thread::scope(|scope| {
        // The printer owns its receiver clones so it can outlive this function's borrows.
        let printer =
            scope.spawn(move || print_loop(response_rx, notice_rx, print_slots, sample_rate));

        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        // `read_line` returns None on EOF / Ctrl-D, which ends the session.
        while let Some(line) = read_line(&mut stdin) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match command::dispatch(line, &dispatcher, pipeline, backend, decks, 0) {
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
) {
    let mut notices_open = true;
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
            }
        } else if let Ok(response) = response_rx.recv() {
            slots.record(&response);
            print_lines(&response::format_response(&response, sample_rate));
        } else {
            break;
        }
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
