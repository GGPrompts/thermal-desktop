//! Session daemon: owns PTY sessions independently of any frontend window.
//!
//! The daemon listens on a Unix socket and accepts client connections.
//! Each session consists of a `PtySession` + `Terminal` (alacritty_terminal::Term).
//! Frontends connect, attach to sessions, receive screen updates, and send input.
//!
//! Socket path: `/run/user/<uid>/thermal/conductor.sock`

mod client_handler;
mod entry;
mod helpers;
mod session;
#[cfg(test)]
mod tests;

pub(crate) use entry::run_daemon;
// run_daemon_on is pub in entry.rs and used by tests via super::entry::run_daemon_on
#[cfg(test)]
pub(crate) use entry::run_daemon_on;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::SystemTime;

use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::protocol::{Response, SessionOutputMode};
use crate::pty::PtySession;
use crate::semantic_state::SemanticEventBus;
use crate::terminal::Terminal;

// ── Session ──────────────────────────────────────────────────────────────────

/// A daemon-owned PTY session.
#[allow(dead_code)]
struct Session {
    id: String,
    /// Human-readable display name (e.g. "zsh", "bash-2", "session-1").
    name: String,
    terminal: Terminal,
    pty: PtySession,
    /// The shell command that was spawned.
    shell_command: String,
    /// Working directory the session was started in.
    cwd: String,
    /// If this session uses a git worktree, the path to that worktree.
    /// Used for cleanup when the session is killed or exits.
    worktree_path: Option<String>,
    /// Broadcast channel for sending responses to all attached clients.
    update_tx: broadcast::Sender<Response>,
    /// Monotonically increasing sequence number for screen updates.
    seq: Arc<AtomicU64>,
    /// Set to true by the byte processor when new PTY output has been processed.
    pty_dirty: Arc<std::sync::atomic::AtomicBool>,
    /// Current terminal title.
    title: Arc<Mutex<String>>,
    /// Number of attached frontend clients.
    attached_count: Arc<AtomicU64>,
    created_at: SystemTime,
    /// How to interpret session output (ANSI vs structured JSON).
    output_mode: SessionOutputMode,
}

// ── Daemon state ─────────────────────────────────────────────────────────────

/// The session daemon, managing all sessions and client connections.
pub(crate) struct Daemon {
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<Session>>>>>,
    next_id: AtomicU64,
    /// Canonical semantic event bus — the single source of truth for session state.
    pub(crate) event_bus: Arc<SemanticEventBus>,
}

impl Daemon {
    pub(crate) fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            event_bus: Arc::new(SemanticEventBus::new(256)),
        }
    }
}
