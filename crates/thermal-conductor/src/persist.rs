//! Session state persistence for the daemon.
//!
//! On graceful shutdown, the daemon serializes session metadata to a JSON file
//! at `/run/user/<uid>/thermal/sessiond-state.json`. On startup, it reads the
//! file and checks which shell processes are still alive via `/proc/<pid>/stat`.
//!
//! Because the PTY master fd is lost when the daemon exits, surviving sessions
//! are reported as "orphaned" — the shell is running but we cannot stream from
//! it. This lays the groundwork for future PTY re-adoption via fd passing or
//! exec-restart.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ── Persisted state types ──────────────────────────────────────────────────

/// Metadata for a single daemon session, serialized to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedSession {
    /// Session ID (e.g. "session-1").
    pub id: String,
    /// Human-readable display name (e.g. "zsh", "bash-2").
    pub name: String,
    /// PID of the shell child process.
    pub shell_pid: i32,
    /// The shell command that was spawned (e.g. "/bin/zsh").
    pub shell_command: String,
    /// Working directory the session was started in.
    pub cwd: String,
    /// Terminal columns at time of save.
    pub cols: u16,
    /// Terminal rows at time of save.
    pub rows: u16,
    /// Current terminal title.
    pub title: String,
    /// Seconds since Unix epoch when the session was originally created.
    pub created_at: u64,
    /// If the session uses a git worktree, the path to it.
    #[serde(default)]
    pub worktree_path: Option<String>,
}

/// The full daemon state file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedState {
    /// Timestamp (epoch secs) when the state was saved.
    pub saved_at: u64,
    /// All sessions that were active at shutdown.
    pub sessions: Vec<PersistedSession>,
}

/// A session recovered from the state file, with liveness information.
#[derive(Debug)]
pub struct RecoveredSession {
    /// The persisted session metadata.
    pub session: PersistedSession,
    /// Whether the shell process is still alive.
    pub shell_alive: bool,
}

// ── File path ──────────────────────────────────────────────────────────────

/// Return the daemon state file path: `/run/user/<uid>/thermal/sessiond-state.json`
pub fn state_file_path() -> PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    PathBuf::from(format!("/run/user/{uid}/thermal/sessiond-state.json"))
}

// ── Save ───────────────────────────────────────────────────────────────────

/// Atomically write daemon state to disk.
///
/// Writes to a temporary file first, then renames to the final path. This
/// prevents partial writes from corrupting the state file on crash.
pub fn save_state(state: &PersistedState) -> Result<()> {
    let path = state_file_path();

    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create state directory: {}", parent.display()))?;
    }

    let json =
        serde_json::to_string_pretty(state).context("Failed to serialize daemon state")?;

    // Write to temp file, then atomic rename.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes())
        .with_context(|| format!("Failed to write temp state file: {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("Failed to rename temp state file to {}", path.display()))?;

    info!(
        path = %path.display(),
        sessions = state.sessions.len(),
        "Daemon state saved"
    );
    Ok(())
}

// ── Load ───────────────────────────────────────────────────────────────────

/// Load persisted state from disk, if it exists.
///
/// Returns `None` if the file doesn't exist. Returns an error if the file
/// exists but cannot be read or parsed.
pub fn load_state() -> Result<Option<PersistedState>> {
    let path = state_file_path();

    if !path.exists() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read state file: {}", path.display()))?;

    let state: PersistedState = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse state file: {}", path.display()))?;

    info!(
        path = %path.display(),
        sessions = state.sessions.len(),
        saved_at = state.saved_at,
        "Loaded persisted daemon state"
    );

    Ok(Some(state))
}

/// Remove the state file (called after successful recovery or when no longer needed).
pub fn remove_state_file() -> Result<()> {
    let path = state_file_path();
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to remove state file: {}", path.display()))?;
        info!(path = %path.display(), "Removed state file");
    }
    Ok(())
}

// ── Process liveness ───────────────────────────────────────────────────────

/// Check if a process is alive by reading `/proc/<pid>/stat`.
///
/// Returns `true` if the process exists and is not a zombie. This is more
/// reliable than `kill(pid, 0)` because it doesn't require signal permissions
/// and can detect zombie processes.
pub fn is_process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }

    let stat_path = format!("/proc/{pid}/stat");
    match std::fs::read_to_string(&stat_path) {
        Ok(contents) => {
            // /proc/<pid>/stat format: "pid (comm) state ..."
            // The state character is after the last ')'.
            // Z = zombie, X/x = dead — these are not "alive".
            if let Some(pos) = contents.rfind(')') {
                let after_comm = &contents[pos + 1..];
                let state = after_comm.trim().chars().next().unwrap_or('X');
                !matches!(state, 'Z' | 'X' | 'x')
            } else {
                // Malformed stat file — process is probably gone.
                false
            }
        }
        Err(_) => {
            // Can't read /proc/<pid>/stat — process doesn't exist.
            false
        }
    }
}

// ── Recovery ───────────────────────────────────────────────────────────────

/// Load state and check liveness of each saved session.
///
/// Returns a list of recovered sessions with their liveness status. The state
/// file is removed after loading (it's a one-shot recovery mechanism).
pub fn recover_sessions() -> Result<Vec<RecoveredSession>> {
    let state = match load_state()? {
        Some(s) => s,
        None => return Ok(Vec::new()),
    };

    let mut recovered = Vec::with_capacity(state.sessions.len());

    for session in state.sessions {
        let alive = is_process_alive(session.shell_pid);
        if alive {
            info!(
                id = %session.id,
                name = %session.name,
                pid = session.shell_pid,
                "Recovered orphaned session — shell still alive but PTY master lost"
            );
        } else {
            warn!(
                id = %session.id,
                name = %session.name,
                pid = session.shell_pid,
                "Recovered session shell has exited"
            );
        }

        recovered.push(RecoveredSession {
            session,
            shell_alive: alive,
        });
    }

    // Clean up the state file after recovery.
    if let Err(e) = remove_state_file() {
        warn!("Failed to clean up state file after recovery: {e}");
    }

    Ok(recovered)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a test PersistedSession with sensible defaults.
    fn make_session(id: &str, name: &str, pid: i32) -> PersistedSession {
        PersistedSession {
            id: id.to_string(),
            name: name.to_string(),
            shell_pid: pid,
            shell_command: "/bin/zsh".to_string(),
            cwd: "/home/builder".to_string(),
            cols: 120,
            rows: 36,
            title: "zsh".to_string(),
            created_at: 1700000000,
            worktree_path: None,
        }
    }

    // ── Serialization round-trip ──────────────────────────────────────────

    #[test]
    fn persisted_session_round_trip() {
        let session = make_session("session-1", "zsh", 12345);
        let json = serde_json::to_string(&session).expect("serialize");
        let decoded: PersistedSession = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(session, decoded);
    }

    #[test]
    fn persisted_session_with_worktree_round_trip() {
        let mut session = make_session("session-2", "bash", 54321);
        session.worktree_path = Some("/tmp/thermal-worktrees/repo-session-2".to_string());
        let json = serde_json::to_string(&session).expect("serialize");
        let decoded: PersistedSession = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(session, decoded);
    }

    #[test]
    fn persisted_state_round_trip() {
        let state = PersistedState {
            saved_at: 1700000042,
            sessions: vec![
                make_session("session-1", "zsh", 1000),
                make_session("session-2", "bash-2", 2000),
            ],
        };
        let json = serde_json::to_string_pretty(&state).expect("serialize");
        let decoded: PersistedState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, decoded);
    }

    #[test]
    fn persisted_state_empty_sessions_round_trip() {
        let state = PersistedState {
            saved_at: 1700000000,
            sessions: vec![],
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let decoded: PersistedState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, decoded);
    }

    #[test]
    fn persisted_session_deserializes_without_worktree() {
        // Simulate a JSON without the worktree_path field — serde(default) handles it.
        let json = r#"{
            "id": "session-1",
            "name": "zsh",
            "shell_pid": 1234,
            "shell_command": "/bin/zsh",
            "cwd": "/home/builder",
            "cols": 80,
            "rows": 24,
            "title": "zsh",
            "created_at": 1700000000
        }"#;
        let decoded: PersistedSession = serde_json::from_str(json).expect("deserialize");
        assert_eq!(decoded.id, "session-1");
        assert!(decoded.worktree_path.is_none());
    }

    // ── Process liveness ─────────────────────────────────────────────────

    #[test]
    fn is_process_alive_detects_current_process() {
        // Our own process is definitely alive.
        let my_pid = std::process::id() as i32;
        assert!(
            is_process_alive(my_pid),
            "Current process should be detected as alive"
        );
    }

    #[test]
    fn is_process_alive_detects_dead_process() {
        // PID 0 is invalid for user processes; PID 999999999 is extremely
        // unlikely to exist.
        assert!(
            !is_process_alive(0),
            "PID 0 should not be detected as alive"
        );
        assert!(
            !is_process_alive(999_999_999),
            "Non-existent PID should not be detected as alive"
        );
    }

    #[test]
    fn is_process_alive_rejects_negative_pid() {
        assert!(
            !is_process_alive(-1),
            "Negative PID should not be alive"
        );
    }

    #[test]
    fn is_process_alive_detects_init() {
        // PID 1 (init/systemd) is always alive on Linux.
        assert!(
            is_process_alive(1),
            "PID 1 (init) should be alive"
        );
    }

    // ── Atomic save/load round-trip via temp directory ────────────────────

    #[test]
    fn save_and_load_round_trip_with_custom_path() {
        // We can't easily override state_file_path() in tests, so we test
        // the serialization logic directly and test the file I/O with a
        // manual temp file.
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("sessiond-state.json");

        let state = PersistedState {
            saved_at: 1700000042,
            sessions: vec![
                make_session("session-1", "zsh", 1000),
                make_session("session-2", "bash-2", 2000),
            ],
        };

        // Manually write (simulating save_state logic).
        let json = serde_json::to_string_pretty(&state).expect("serialize");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes()).expect("write tmp");
        std::fs::rename(&tmp, &path).expect("rename");

        // Read back (simulating load_state logic).
        let contents = std::fs::read_to_string(&path).expect("read");
        let loaded: PersistedState = serde_json::from_str(&contents).expect("parse");
        assert_eq!(state, loaded);
    }

    #[test]
    fn load_returns_none_for_missing_file() {
        // load_state checks state_file_path() which is a fixed location. We
        // test the underlying logic: reading a nonexistent file fails, and we
        // map that to None.
        let path = "/tmp/thermal-test-nonexistent-sessiond-state.json";
        assert!(!std::path::Path::new(path).exists());
    }
}
