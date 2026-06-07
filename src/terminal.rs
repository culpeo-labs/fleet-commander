//! Embedded terminal emulator.
//!
//! Wraps a PTY child process and a `vt100::Parser` so the TUI can render
//! a fully interactive terminal panel. Used for:
//!
//!   * Interactive auth flows (`copilot login`)
//!   * In-container shell sessions (future)

#![allow(dead_code)]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use tokio::sync::mpsc;

/// Handle to a running embedded terminal session.
pub struct EmbeddedTerminal {
    /// PTY writer — send keystrokes here.
    writer: Box<dyn Write + Send>,
    /// PTY master — needed for resize.
    master: Box<dyn portable_pty::MasterPty>,
    /// Shared terminal state (updated by the reader task).
    state: Arc<Mutex<TerminalState>>,
}

struct TerminalState {
    parser: vt100::Parser,
    exited: bool,
    exit_code: Option<u32>,
}

impl EmbeddedTerminal {
    /// Spawn a new terminal running `command` with the given arguments.
    ///
    /// `notify_tx` is used to wake the TUI event loop whenever new output
    /// arrives so it can redraw.
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        rows: u16,
        cols: u16,
        notify_tx: mpsc::UnboundedSender<()>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;

        let state = Arc::new(Mutex::new(TerminalState {
            parser: vt100::Parser::new(rows, cols, 200),
            exited: false,
            exit_code: None,
        }));

        // Reader task — runs on a blocking thread since PTY reads are sync.
        let read_state = state.clone();
        let read_notify = notify_tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut st = read_state.lock().unwrap();
                        st.parser.process(&buf[..n]);
                        drop(st);
                        let _ = read_notify.send(());
                    }
                }
            }
        });

        // Waiter task — reaps the child process.
        let wait_state = state.clone();
        let wait_notify = notify_tx;
        std::thread::spawn(move || {
            let mut child = child;
            let status = child.wait();
            let mut st = wait_state.lock().unwrap();
            st.exited = true;
            st.exit_code = status.ok().map(|s| s.exit_code());
            drop(st);
            let _ = wait_notify.send(());
        });

        Ok(Self {
            writer,
            master: pair.master,
            state,
        })
    }

    /// Write raw bytes to the PTY (keyboard input).
    pub fn write(&mut self, data: &[u8]) {
        let _ = self.writer.write_all(data);
        let _ = self.writer.flush();
    }

    /// Resize the terminal.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        let mut st = self.state.lock().unwrap();
        st.parser.screen_mut().set_size(rows, cols);
    }

    /// Take a snapshot of the current terminal screen.
    pub fn screen(&self) -> vt100::Screen {
        let st = self.state.lock().unwrap();
        st.parser.screen().clone()
    }

    /// Whether the child process has exited.
    pub fn is_finished(&self) -> bool {
        self.state.lock().unwrap().exited
    }

    /// Exit code of the child process (if finished).
    pub fn exit_code(&self) -> Option<u32> {
        self.state.lock().unwrap().exit_code
    }
}
