//! Claude session matching, context warnings, continuation spawning,
//! and cross-pane prompt injection.

use alacritty_terminal::term::TermMode;
use thermal_core::claude_state::ClaudeSessionState;

use crate::inject;

use super::ConductorWindow;
use super::session_mode::SessionMode;

impl ConductorWindow {
    /// Inject the current terminal selection into other thermal-conductor windows.
    ///
    /// If a daemon is running, sends via the daemon client to all other sessions.
    /// Otherwise, writes to `/tmp/thermal-inject/` for file-based pickup.
    pub(super) fn inject_selection(&self) {
        let term_handle = self.terminal.term_handle();
        let term = term_handle.lock();
        let text = term.selection_to_string();
        drop(term);

        let Some(text) = text else {
            tracing::debug!("Inject: no selection");
            return;
        };
        if text.is_empty() {
            tracing::debug!("Inject: selection is empty");
            return;
        }

        // NOTE: A future enhancement could use the daemon client to send
        // input directly to other sessions via `send_input`. For now we use
        // the file-based approach which works universally — both with and
        // without the daemon running.

        // File-based approach: write to /tmp/thermal-inject/.
        match inject::write_inject_file(&self.inject_session_id, &text) {
            Ok(path) => {
                tracing::info!(
                    path = %path.display(),
                    text_len = text.len(),
                    "Injected selection to other windows"
                );
                inject::notify_injection("sent", text.len());
            }
            Err(e) => {
                tracing::warn!("Failed to write inject file: {e}");
            }
        }
    }

    /// Poll the inject watcher for incoming injections from other windows.
    ///
    /// Any received text is pasted into this window's PTY session, respecting
    /// bracketed paste mode.
    pub(super) fn poll_inject_watcher(&self) {
        let watcher = match &self.inject_watcher {
            Some(w) => w,
            None => return,
        };

        let payloads = watcher.poll();
        for text in payloads {
            if text.is_empty() {
                continue;
            }

            // Check if the terminal has bracketed paste mode enabled.
            let bracketed = self.current_term_mode().contains(TermMode::BRACKETED_PASTE);

            if bracketed {
                let mut payload = Vec::with_capacity(text.len() + 12);
                payload.extend_from_slice(b"\x1b[200~");
                payload.extend_from_slice(text.as_bytes());
                payload.extend_from_slice(b"\x1b[201~");
                self.write_session(&payload);
            } else {
                self.write_session(text.as_bytes());
            }

            tracing::info!(
                text_len = text.len(),
                bracketed,
                "Injected text from another window into session"
            );
            inject::notify_injection("received", text.len());
        }
    }

    // ── Context saturation monitoring ──────────────────────────────────────

    /// Update context warning state based on the current Claude session.
    ///
    /// Sets `context_warning_active` when context_percent >= 85% and
    /// `context_critical_active` when >= 95%. Resets flags when context
    /// drops below thresholds (e.g. after a new session starts).
    pub(super) fn update_context_warnings(&mut self) {
        let context_pct = self
            .claude_session
            .as_ref()
            .and_then(|s| s.context_percent)
            .unwrap_or(0.0);

        let was_warning = self.context_warning_active;
        let was_critical = self.context_critical_active;

        self.context_warning_active = context_pct >= 85.0;
        self.context_critical_active = context_pct >= 95.0;

        // Log transitions for observability.
        if self.context_warning_active && !was_warning {
            tracing::warn!(
                context_percent = context_pct,
                "Context window approaching limit (>= 85%)"
            );
        }
        if self.context_critical_active && !was_critical {
            tracing::warn!(
                context_percent = context_pct,
                "Context window saturated (>= 95%) — Ctrl+Shift+N to spawn continuation"
            );
        }
        if !self.context_warning_active && was_warning {
            tracing::info!("Context warning cleared (dropped below 85%)");
        }

        // Mark dirty if state changed so the overlay is rendered/cleared.
        if self.context_warning_active != was_warning
            || self.context_critical_active != was_critical
        {
            self.dirty = true;
        }
    }

    /// Spawn a continuation session in a new window.
    ///
    /// In client mode: asks the daemon to spawn a new session.
    /// In standalone mode: spawns a new `thermal-conductor window` process.
    pub(super) fn spawn_continuation(&self) {
        tracing::info!("Spawning continuation session (Ctrl+Shift+N)");

        // Read current CWD from the PTY child process so the continuation
        // session starts in the same directory.
        let current_cwd = std::fs::read_link(format!("/proc/{}/cwd", self.pty_child_pid))
            .ok()
            .map(|p| p.to_string_lossy().to_string());

        match &self.session_mode {
            SessionMode::Client { client, .. } => {
                let client_tx = client.request_tx_clone();
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
                tokio::spawn(async move {
                    // Spawn a new session on the daemon.
                    if let Err(e) = client_tx
                        .send(crate::protocol::Request::SpawnSession {
                            shell: Some(shell),
                            cwd: current_cwd,
                            worktree: false,
                            name: None,
                        })
                        .await
                    {
                        tracing::warn!("Failed to spawn continuation session on daemon: {e}");
                        return;
                    }
                    tracing::info!("Continuation session spawn request sent to daemon");

                    // Launch a new window process to attach to the new session.
                    match std::process::Command::new(
                        std::env::current_exe()
                            .unwrap_or_else(|_| std::path::PathBuf::from("thermal-conductor")),
                    )
                    .arg("window")
                    .spawn()
                    {
                        Ok(_) => {
                            tracing::info!(
                                "Launched new thermal-conductor window for continuation"
                            );
                        }
                        Err(e) => {
                            tracing::warn!("Failed to launch continuation window: {e}");
                        }
                    }
                });
            }
            SessionMode::Standalone { .. } => {
                // Spawn a new thermal-conductor window process directly.
                match std::process::Command::new(
                    std::env::current_exe()
                        .unwrap_or_else(|_| std::path::PathBuf::from("thermal-conductor")),
                )
                .arg("window")
                .spawn()
                {
                    Ok(_) => {
                        tracing::info!(
                            "Launched new thermal-conductor window (standalone continuation)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Failed to launch continuation window: {e}");
                    }
                }
            }
        }

        // Try to place the new window adjacent via hyprctl.
        if let Err(e) = std::process::Command::new("hyprctl")
            .args(["dispatch", "layoutmsg", "preselect", "r"])
            .spawn()
        {
            tracing::debug!("hyprctl preselect hint failed (non-fatal): {e}");
        }
    }
}

// ── Claude session matching ───────────────────────────────────────────────────

/// Read the working directory of a process via `/proc/<pid>/cwd`.
///
/// Returns `None` if the process doesn't exist or the symlink can't be read.
fn read_proc_cwd(pid: i32) -> Option<String> {
    let link = format!("/proc/{}/cwd", pid);
    std::fs::read_link(link)
        .ok()
        .and_then(|p| p.to_str().map(String::from))
}

/// Find a Claude session whose `working_dir` matches the PTY child's cwd.
///
/// Reads the PTY child's working directory from `/proc/<pid>/cwd` and compares
/// it against each session's `working_dir` field. Returns the first match, or
/// `None` if no session matches.
pub(super) fn find_matching_session(
    sessions: &[ClaudeSessionState],
    pty_child_pid: i32,
) -> Option<ClaudeSessionState> {
    if sessions.is_empty() {
        return None;
    }

    // Read the PTY child's current working directory.
    let pty_cwd = read_proc_cwd(pty_child_pid)?;

    // Try exact match first.
    for session in sessions {
        if let Some(ref working_dir) = session.working_dir
            && working_dir == &pty_cwd
        {
            return Some(session.clone());
        }
    }

    // Try prefix match: the PTY cwd may be a subdirectory of the session's
    // working_dir (e.g. PTY in /home/user/project/src, session in /home/user/project).
    for session in sessions {
        if let Some(ref working_dir) = session.working_dir
            && pty_cwd.starts_with(working_dir)
        {
            return Some(session.clone());
        }
    }

    None
}
