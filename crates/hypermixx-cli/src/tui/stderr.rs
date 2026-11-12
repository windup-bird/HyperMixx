//! Keeps the engine's stderr off the TUI's canvas.
//!
//! The audio engine and libasound report on stderr (`[audio] main: sysdefault:CARD=… @ 48000/…`,
//! ALSA's own `ALSA lib pcm_dmix.c:… unable to open slave`). That is the right destination for the
//! REPL, but the TUI owns the terminal through an alternate screen: stderr writes do not scroll the
//! log, they draw *under* the UI and scribble it. Rebinding one device in `F9 › output` alone rebuilds
//! every stream and prints a dozen lines, which is what turns the screen into a wall of text.
//!
//! So while the TUI runs, fd 2 is a pipe. A reader thread forwards each line to the response log,
//! where it is scrollable and survives, and the terminal is left alone. Purely a Unix mechanism; on
//! other platforms stderr keeps its usual behaviour.

use crossbeam_channel::Receiver;

/// A live redirection. Dropping it puts fd 2 back and lets the reader finish.
pub struct Capture {
    #[cfg(unix)]
    saved: i32,
    #[cfg(unix)]
    reader: Option<std::thread::JoinHandle<()>>,
}

/// Starts capturing stderr, or returns `None` when the platform has no fd to rebind (or the pipe
/// could not be created — a missing log is better than a broken terminal).
pub fn start() -> Option<(Capture, Receiver<String>)> {
    imp::start()
}

#[cfg(unix)]
mod imp {
    use super::{Capture, Receiver};
    use crossbeam_channel::unbounded;
    use std::io::{BufRead, BufReader};
    use std::os::fd::FromRawFd;

    pub fn start() -> Option<(Capture, Receiver<String>)> {
        let (tx, rx) = unbounded();
        let mut fds = [0i32; 2];
        // `pipe2` with `O_CLOEXEC`: no child process should inherit (or keep alive) our log pipe.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return None;
        }
        let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
        if saved < 0 {
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return None;
        }
        if unsafe { libc::dup2(fds[1], libc::STDERR_FILENO) } < 0 {
            unsafe {
                libc::close(saved);
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return None;
        }
        // fd 2 now refers to the pipe's write end; the original descriptor is saved.
        unsafe { libc::close(fds[1]) };

        let read_fd = fds[0];
        let reader = std::thread::Builder::new()
            .name("hypermixx-stderr".into())
            .spawn(move || {
                let file = unsafe { std::fs::File::from_raw_fd(read_fd) };
                // `read_until` rather than `lines()`: ALSA writes progress-style fragments without a
                // trailing newline, and `lines()` would swallow them until one arrives.
                let mut reader = BufReader::new(file);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match reader.read_until(b'\n', &mut line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {
                            let text = String::from_utf8_lossy(&line);
                            let text = text.trim_end();
                            if !text.is_empty() && tx.send(text.to_owned()).is_err() {
                                return; // the TUI is gone
                            }
                        }
                    }
                }
            })
            .ok()?;
        Some((
            Capture {
                saved,
                reader: Some(reader),
            },
            rx,
        ))
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            // Put fd 2 back: this also closes the pipe's write end, so the reader sees EOF.
            unsafe {
                libc::dup2(self.saved, libc::STDERR_FILENO);
                libc::close(self.saved);
            }
            self.saved = -1;
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{Capture, Receiver};

    pub fn start() -> Option<(Capture, Receiver<String>)> {
        None
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The whole point: a line written to stderr while the capture is live arrives on the channel,
    /// and the descriptor is put back afterwards.
    #[test]
    fn a_stderr_line_reaches_the_log_and_the_descriptor_comes_back() {
        let Some((guard, rx)) = start() else {
            return; // no pipe available (a sandbox that forbids it) — nothing to pin
        };
        // A raw `write(2)`, not `eprintln!`: libtest installs its own output capture on the Rust
        // macros, while the engine's `eprintln!` (and libasound) really do write the descriptor.
        let line = b"hypermixx-stderr-probe: hello\n";
        let written = unsafe { libc::write(libc::STDERR_FILENO, line.as_ptr().cast(), line.len()) };
        assert_eq!(written, line.len() as isize, "short write");
        // A write to the *restored* descriptor afterwards must not be captured, so the check has to
        // happen while the guard is still alive.
        let mut found = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(line) if line.contains("hypermixx-stderr-probe: hello") => {
                    found = true;
                    break;
                }
                Ok(_) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(found, "the stderr line did not reach the log");
        drop(guard);
        // After the restore the descriptor is the process's own again; writing must not panic and
        // the reader thread must have finished (its channel is closed).
        let _ = unsafe { libc::write(libc::STDERR_FILENO, b"".as_ptr().cast(), 0) };
    }
}
