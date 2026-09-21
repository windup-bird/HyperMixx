//! Progress lines from the CLI's own worker threads.
//!
//! Decoding and analysis run off the main thread and used to `eprintln!` directly, which would
//! corrupt a TUI's alternate screen. They now send through a channel: the REPL drains it and prints,
//! the TUI appends it to the same log as engine responses.

use crossbeam_channel::Sender;

use crate::response::LogLine;

/// Sender half handed to media/analyse workers. Clone it per worker.
pub type NoticeTx = Sender<LogLine>;

/// Sends a line if anyone is still listening; a closed channel during shutdown is not an error.
pub fn send(tx: &NoticeTx, line: LogLine) {
    let _ = tx.send(line);
}

/// Informational progress (e.g. `[analyse] deck0: running Stratum...`).
pub fn info(tx: &NoticeTx, text: impl Into<String>) {
    send(tx, LogLine::info(text));
}

/// A worker failure, rendered with the same `error: ` prefix as engine errors.
pub fn error(tx: &NoticeTx, text: impl Into<String>) {
    send(tx, LogLine::error(text));
}
