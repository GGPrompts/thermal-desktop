//! tmux command wrapper — Phase 1 backend for thermal-conductor.
//!
//! Wraps tmux CLI commands via std::process::Command. Handles session creation,
//! pane management, content capture, and key dispatch.

use std::process::Command;

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("tmux command failed: {0}")]
    CommandFailed(String),
    #[error("tmux not found")]
    NotFound,
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("pane not found: {0}")]
    PaneNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(String),
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Run a tmux subcommand. Returns stdout on success, TmuxError on failure.
fn tmux(args: &[&str]) -> Result<String, TmuxError> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                TmuxError::NotFound
            } else {
                TmuxError::Io(e)
            }
        })?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let msg = if stderr.is_empty() { stdout } else { stderr };
        Err(TmuxError::CommandFailed(msg))
    }
}

/// Run a tmux subcommand, returning Ok(()) — stderr content becomes the error.
fn tmux_quiet(args: &[&str]) -> Result<(), TmuxError> {
    tmux(args).map(|_| ())
}

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct PaneStatus {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub command: String,
    pub active: bool,
}

// ── TmuxSession ───────────────────────────────────────────────────────────────

pub struct TmuxSession {
    pub session_name: String,
    pub pane_ids: Vec<String>,
}

impl TmuxSession {
    /// Create a new tmux session (or attach to existing), then populate pane list.
    pub fn new(session_name: &str) -> Result<Self, TmuxError> {
        // Check whether the session already exists.
        let exists = Command::new("tmux")
            .args(["has-session", "-t", session_name])
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    TmuxError::NotFound
                } else {
                    TmuxError::Io(e)
                }
            })?
            .status
            .success();

        if !exists {
            // Create a new detached session with one window.
            tmux_quiet(&["new-session", "-d", "-s", session_name])?;
        }

        let mut session = TmuxSession {
            session_name: session_name.to_owned(),
            pane_ids: Vec::new(),
        };

        // Populate pane list from the live session.
        session.pane_ids = session.fetch_pane_ids()?;

        Ok(session)
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Query tmux for the current pane IDs in this session's first window.
    fn fetch_pane_ids(&self) -> Result<Vec<String>, TmuxError> {
        let target = format!("{}:", self.session_name); // session first window
        let raw = tmux(&[
            "list-panes",
            "-t",
            &target,
            "-F",
            "#{pane_id}",
        ])?;

        let ids: Vec<String> = raw
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();

        Ok(ids)
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Split the current window and return the new pane ID.
    /// If `command` is supplied it is sent to the pane immediately after creation.
    pub fn create_pane(&mut self, command: Option<&str>) -> Result<String, TmuxError> {
        let target = format!("{}:", self.session_name);

        // Split horizontally; -P -F prints the new pane ID.
        let raw = tmux(&[
            "split-window",
            "-t",
            &target,
            "-P",
            "-F",
            "#{pane_id}",
        ])?;

        let pane_id = raw.trim().to_owned();
        if pane_id.is_empty() {
            return Err(TmuxError::Parse(
                "split-window returned empty pane id".into(),
            ));
        }

        // Optionally execute a command in the new pane.
        if let Some(cmd) = command {
            self.send_keys(&pane_id, cmd)?;
            tmux_quiet(&["send-keys", "-t", &pane_id, "Enter"])?;
        }

        self.pane_ids.push(pane_id.clone());
        Ok(pane_id)
    }

    /// Capture pane content with ANSI escape codes.
    /// `lines` sets how many lines of scrollback to include (negative start offset).
    pub fn capture_pane(&self, pane_id: &str, lines: Option<i32>) -> Result<String, TmuxError> {
        if !self.pane_ids.iter().any(|id| id == pane_id) {
            return Err(TmuxError::PaneNotFound(pane_id.to_owned()));
        }

        match lines {
            None => tmux(&["capture-pane", "-t", pane_id, "-p", "-e"]),
            Some(n) => {
                // -S expects a negative number for scrollback offset.
                // Ensure we always produce a negative value regardless of the
                // sign of the caller's argument.
                let start = format!("{}", -(n.abs() as i64));
                tmux(&["capture-pane", "-t", pane_id, "-p", "-e", "-S", &start])
            }
        }
    }

    /// Send a tmux key name to a pane (e.g. "Enter", "Up", "C-a").
    pub fn send_keys(&self, pane_id: &str, keys: &str) -> Result<(), TmuxError> {
        if !self.pane_ids.iter().any(|id| id == pane_id) {
            return Err(TmuxError::PaneNotFound(pane_id.to_owned()));
        }

        tmux_quiet(&["send-keys", "-t", pane_id, keys])
    }

    /// Send literal text to a pane (uses -l flag so text is not parsed as key names).
    pub fn send_keys_literal(&self, pane_id: &str, text: &str) -> Result<(), TmuxError> {
        if !self.pane_ids.iter().any(|id| id == pane_id) {
            return Err(TmuxError::PaneNotFound(pane_id.to_owned()));
        }

        tmux_quiet(&["send-keys", "-t", pane_id, "-l", text])
    }

    /// Resize a pane to the given character dimensions.
    pub fn resize_pane(&self, pane_id: &str, width: u32, height: u32) -> Result<(), TmuxError> {
        if !self.pane_ids.iter().any(|id| id == pane_id) {
            return Err(TmuxError::PaneNotFound(pane_id.to_owned()));
        }

        let w = width.to_string();
        let h = height.to_string();
        tmux_quiet(&["resize-pane", "-t", pane_id, "-x", &w, "-y", &h])
    }

    /// Return the character dimensions (width, height) of a pane.
    pub fn pane_size(&self, pane_id: &str) -> Result<(u32, u32), TmuxError> {
        if !self.pane_ids.iter().any(|id| id == pane_id) {
            return Err(TmuxError::PaneNotFound(pane_id.to_owned()));
        }

        let raw = tmux(&[
            "display-message",
            "-t",
            pane_id,
            "-p",
            "#{pane_width} #{pane_height}",
        ])?;

        let parts: Vec<&str> = raw.trim().split_whitespace().collect();
        if parts.len() != 2 {
            return Err(TmuxError::Parse(format!(
                "unexpected pane_size output: {:?}",
                raw
            )));
        }

        let width: u32 = parts[0]
            .parse()
            .map_err(|_| TmuxError::Parse(format!("bad width: {}", parts[0])))?;
        let height: u32 = parts[1]
            .parse()
            .map_err(|_| TmuxError::Parse(format!("bad height: {}", parts[1])))?;

        Ok((width, height))
    }

    /// List all panes in the session with their current status.
    pub fn list_panes(&self) -> Result<Vec<PaneStatus>, TmuxError> {
        let target = format!("{}:", self.session_name);
        let raw = tmux(&[
            "list-panes",
            "-t",
            &target,
            "-F",
            "#{pane_id} #{pane_width} #{pane_height} #{pane_current_command} #{pane_active}",
        ])?;

        let mut panes = Vec::new();
        for line in raw.lines().map(str::trim).filter(|s| !s.is_empty()) {
            // Format: %N width height command 0|1
            // We only split on whitespace with a max of 5 parts so a command
            // name with spaces is captured as the fourth field.
            let parts: Vec<&str> = line.splitn(5, ' ').collect();
            if parts.len() < 5 {
                return Err(TmuxError::Parse(format!(
                    "unexpected list-panes line: {:?}",
                    line
                )));
            }

            let width: u32 = parts[1]
                .parse()
                .map_err(|_| TmuxError::Parse(format!("bad width: {}", parts[1])))?;
            let height: u32 = parts[2]
                .parse()
                .map_err(|_| TmuxError::Parse(format!("bad height: {}", parts[2])))?;
            let active = parts[4].trim() == "1";

            panes.push(PaneStatus {
                id: parts[0].to_owned(),
                width,
                height,
                command: parts[3].to_owned(),
                active,
            });
        }

        Ok(panes)
    }

    /// Remove a pane. Updates the internal pane list.
    pub fn kill_pane(&mut self, pane_id: &str) -> Result<(), TmuxError> {
        if !self.pane_ids.iter().any(|id| id == pane_id) {
            return Err(TmuxError::PaneNotFound(pane_id.to_owned()));
        }

        tmux_quiet(&["kill-pane", "-t", pane_id])?;
        self.pane_ids.retain(|id| id != pane_id);
        Ok(())
    }

    /// Destroy the entire tmux session.
    pub fn kill_session(&self) -> Result<(), TmuxError> {
        tmux_quiet(&["kill-session", "-t", &self.session_name])
    }
}
