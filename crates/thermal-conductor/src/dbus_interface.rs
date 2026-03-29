//! D-Bus interface for the session daemon: `org.thermal.Conductor`.
//!
//! Provides session lifecycle management (spawn, kill, list) over the session
//! bus so that thermal-bar, thermal-hud, and other components can discover and
//! interact with the daemon without a direct Unix socket connection.
//!
//! High-frequency screen updates still flow over the Unix socket (see
//! `protocol.rs`); D-Bus is used only for infrequent management calls and
//! signals as described in `docs/design/session-protocol.md` section 7.

use std::sync::Arc;

use zbus::interface;
use zbus::object_server::SignalEmitter;

use crate::daemon::Daemon;
use crate::protocol;

// ── D-Bus service constants ─────────────────────────────────────────────────

/// Well-known bus name for the conductor daemon.
pub const BUS_NAME: &str = "org.thermal.Conductor";

/// Object path where the interface is served.
pub const OBJECT_PATH: &str = "/org/thermal/conductor";

// ── Interface implementation ────────────────────────────────────────────────

/// D-Bus object that delegates to the shared `Daemon` state.
pub struct ConductorInterface {
    daemon: Arc<Daemon>,
}

impl ConductorInterface {
    pub fn new(daemon: Arc<Daemon>) -> Self {
        Self { daemon }
    }
}

#[interface(name = "org.thermal.Conductor")]
impl ConductorInterface {
    // ── Session lifecycle ────────────────────────────────────────────────

    /// Spawn a new session.
    ///
    /// * `shell` — shell binary path, or `""` to use `$SHELL`.
    /// * `cwd`   — initial working directory, or `""` for `$HOME`.
    /// * `name`  — desired human-readable name, or `""` for auto-assignment.
    ///
    /// Returns the new session ID.
    async fn spawn(
        &self,
        shell: &str,
        cwd: &str,
        name: &str,
        #[zbus(signal_context)] ctx: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<String> {
        let shell_opt = if shell.is_empty() {
            None
        } else {
            Some(shell.to_string())
        };
        let cwd_opt = if cwd.is_empty() {
            None
        } else {
            Some(cwd.to_string())
        };
        let name_opt = if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        };

        match self.daemon.spawn_session(shell_opt, cwd_opt, false, name_opt) {
            Ok((session_id, display_name)) => {
                // Emit SessionSpawned signal (best-effort).
                let _ = Self::session_spawned(&ctx, &session_id, &display_name).await;
                Ok(session_id)
            }
            Err(e) => Err(zbus::fdo::Error::Failed(format!(
                "Failed to spawn session: {e}"
            ))),
        }
    }

    /// Kill a session (sends SIGHUP to the PTY child).
    async fn kill(
        &self,
        session_id: &str,
        #[zbus(signal_context)] ctx: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let req = protocol::Request::KillSession {
            id: session_id.to_string(),
        };
        match self.daemon.handle_request(&req) {
            protocol::Response::Ok => {
                // Emit SessionExited signal with exit_code -1 (killed).
                let _ = Self::session_exited(&ctx, session_id, -1).await;
                Ok(())
            }
            protocol::Response::Error { message } => Err(zbus::fdo::Error::Failed(message)),
            _ => Ok(()),
        }
    }

    /// List all live sessions as a JSON string.
    ///
    /// Returns a JSON array of session objects (matches the `SessionInfo`
    /// struct from the wire protocol).
    fn list(&self) -> String {
        let sessions = self.daemon.list_sessions();
        serde_json::to_string(&sessions).unwrap_or_else(|_| "[]".to_string())
    }

    /// Get the Unix socket path that frontends should connect to for
    /// streaming screen updates for a given session.
    fn get_socket_path(&self, _session_id: &str) -> String {
        protocol::socket_path().to_string_lossy().into_owned()
    }

    // ── Properties ──────────────────────────────────────────────────────

    /// All active session IDs (backward-compatible with ConductorProxy.panes).
    #[zbus(property)]
    fn panes(&self) -> Vec<String> {
        self.daemon
            .list_sessions()
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    // ── Signals ─────────────────────────────────────────────────────────

    /// Emitted when a session is spawned.
    #[zbus(signal)]
    async fn session_spawned(
        ctx: &SignalEmitter<'_>,
        session_id: &str,
        name: &str,
    ) -> zbus::Result<()>;

    /// Emitted when a session exits (child process ended).
    #[zbus(signal)]
    async fn session_exited(
        ctx: &SignalEmitter<'_>,
        session_id: &str,
        exit_code: i32,
    ) -> zbus::Result<()>;
}
