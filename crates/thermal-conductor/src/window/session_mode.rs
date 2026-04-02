//! Session mode types and ConductorWindow session dispatch helpers.
//!
//! Defines BellMode, SessionMode, and the standalone session setup function.
//! Also contains ConductorWindow methods for writing/resizing the active
//! session and encoding key events.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::term::TermMode;
use smithay_client_toolkit::seat::keyboard::KeyEvent;

use crate::client::DaemonClient;
use crate::input;
use crate::pty::PtySession;
use crate::terminal::Terminal;

use super::ConductorWindow;

// ── Bell configuration ──────────────────────────────────────────────────────

/// How to handle BEL (0x07) from the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BellMode {
    /// Brief translucent screen flash.
    Visual,
    /// Bell is silently ignored.
    None,
}

impl BellMode {
    /// Read from `THERMAL_BELL` env var. Defaults to `Visual`.
    pub(super) fn from_env() -> Self {
        match std::env::var("THERMAL_BELL").as_deref() {
            Ok("none") => BellMode::None,
            _ => BellMode::Visual,
        }
    }
}

/// Duration of the visual bell flash overlay.
pub(super) const BELL_FLASH_DURATION: Duration = Duration::from_millis(200);

// ── Session mode ──────────────────────────────────────────────────────────────

/// How this window is connected to a terminal session.
///
/// In **client mode** the session daemon owns the PTY; we receive screen
/// updates over a Unix socket and forward input/resize there.
///
/// In **standalone mode** we own the PTY directly (legacy, no daemon).
pub(crate) enum SessionMode {
    /// Connected to the session daemon.
    Client {
        /// Daemon client for sending requests (input, resize, detach).
        client: DaemonClient,
        /// The session ID we are attached to.
        session_id: String,
    },
    /// Direct PTY ownership (no daemon running).
    Standalone { pty: PtySession },
}

// ── Standalone session setup ──────────────────────────────────────────────────

/// Set up a standalone PTY session (no daemon). This is the legacy code path.
///
/// Spawns a PTY, connects its output to the terminal byte processor, and
/// returns the session mode, event receiver, and child PID.
pub(super) fn setup_standalone_session(
    terminal: &mut Terminal,
    init_cols: usize,
    init_rows: usize,
    pty_dirty: Arc<AtomicBool>,
    wakeup_write: std::os::fd::OwnedFd,
    command: Option<Vec<String>>,
) -> (
    SessionMode,
    tokio::sync::mpsc::UnboundedReceiver<TermEvent>,
    i32,
) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    // Spawn the PTY at the correct initial size so the shell doesn't need a
    // SIGWINCH resize cycle (which creates spurious scrollback on launch).
    let mut pty = if let Some(ref cmd) = command {
        use std::collections::HashMap;
        let program = &cmd[0];
        let args: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
        let mut env = HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());
        let ws = nix::pty::Winsize {
            ws_row: init_rows as u16,
            ws_col: init_cols as u16,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        PtySession::spawn_command_sized(program, &args, None, env, Some(ws))
            .expect("Failed to spawn command PTY")
    } else {
        PtySession::spawn_sized(&shell, None, init_cols as u16, init_rows as u16)
            .expect("Failed to spawn PTY")
    };

    // Connect PTY output to the terminal byte processor.
    let pty_output_rx = pty.take_output();
    terminal.spawn_byte_processor(pty_output_rx, pty_dirty, wakeup_write);

    // Take the terminal event receiver.
    let term_event_rx = terminal.take_event_rx().expect("event_rx already taken");

    let child_pid = pty.child_pid().as_raw();

    let mode = SessionMode::Standalone { pty };
    (mode, term_event_rx, child_pid)
}

// ── ConductorWindow session dispatch methods ─────────────────────────────────

impl ConductorWindow {
    pub(super) fn current_term_mode(&self) -> TermMode {
        match &self.session_mode {
            SessionMode::Client { .. } => {
                TermMode::from_bits_retain(self.synced_term_mode.load(Ordering::Acquire))
            }
            SessionMode::Standalone { .. } => {
                let th = self.terminal.term_handle();
                let t = th.lock();
                *t.mode()
            }
        }
    }

    // ── Kitty keyboard protocol helpers ──────────────────────────────────

    /// Query the alacritty_terminal mode flags and return the active kitty
    /// keyboard protocol flags.  Returns `KittyFlags::NONE` when kitty
    /// keyboard mode is not active.
    pub(super) fn kitty_flags(&self) -> input::KittyFlags {
        let mode = self.current_term_mode();
        let mut flags: u8 = 0;
        if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
            flags |= 1;
        }
        if mode.contains(TermMode::REPORT_EVENT_TYPES) {
            flags |= 2;
        }
        if mode.contains(TermMode::REPORT_ALTERNATE_KEYS) {
            flags |= 4;
        }
        if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
            flags |= 8;
        }
        if mode.contains(TermMode::REPORT_ASSOCIATED_TEXT) {
            flags |= 16;
        }
        input::KittyFlags(flags)
    }

    /// Encode a key event, choosing kitty or legacy encoding based on the
    /// terminal's active mode.
    pub(super) fn encode_key_event(
        &self,
        event: &KeyEvent,
        event_type: input::KeyEventType,
    ) -> Option<Vec<u8>> {
        let flags = self.kitty_flags();
        if flags.contains(input::KittyFlags::DISAMBIGUATE) {
            input::encode_key_kitty(event, &self.modifiers, flags, event_type)
        } else {
            input::encode_key(event, &self.modifiers)
        }
    }

    // ── Session mode dispatch helpers ─────────────────────────────────────

    /// Write bytes to the active session (PTY or daemon).
    pub(super) fn write_session(&self, bytes: &[u8]) {
        match &self.session_mode {
            SessionMode::Standalone { pty } => {
                if let Err(e) = pty.write(bytes) {
                    tracing::warn!("Failed to write to PTY: {e}");
                }
            }
            SessionMode::Client { client, session_id } => {
                let data = bytes.to_vec();
                let id = session_id.clone();
                // Fire-and-forget async send — input is latency-sensitive so
                // we don't block the event loop waiting for a response.
                let client_tx = client.request_tx_clone();
                tokio::spawn(async move {
                    if let Err(e) = client_tx
                        .send(crate::protocol::Request::SendInput { id, data })
                        .await
                    {
                        tracing::warn!("Failed to send input to daemon: {e}");
                    }
                });
            }
        }
    }

    /// Resize the active session (PTY or daemon).
    pub(super) fn resize_session(&self, cols: u16, rows: u16) {
        match &self.session_mode {
            SessionMode::Standalone { pty } => {
                let _ = pty.resize(cols, rows);
            }
            SessionMode::Client { client, session_id } => {
                let id = session_id.clone();
                let client_tx = client.request_tx_clone();
                tokio::spawn(async move {
                    if let Err(e) = client_tx
                        .send(crate::protocol::Request::Resize { id, cols, rows })
                        .await
                    {
                        tracing::warn!("Failed to send resize to daemon: {e}");
                    }
                });
            }
        }
    }

    /// Check whether the session has exited.
    pub(super) fn session_has_exited(&self) -> bool {
        match &self.session_mode {
            SessionMode::Standalone { pty } => pty.has_exited(),
            // In client mode, the daemon reader task sets this flag when
            // it receives a `SessionExited` message.
            SessionMode::Client { .. } => self.daemon_exit_requested.load(Ordering::Acquire),
        }
    }
}
