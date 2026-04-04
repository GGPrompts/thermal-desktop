//! `impl Daemon` methods: spawn_session, list_sessions, handle_request,
//! collect_persisted_state, and internal helpers (subscribe, get_session_state,
//! create_worktree, remove_worktree).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::kitty::{
    SidecarEntry, now_epoch, sidecar_locked_update,
    sidecar_remove as sidecar_remove_entry,
};
use crate::persist::{PersistedSession, PersistedState};
use crate::protocol::{
    self, CursorData, DirtyCellData, Request, Response, SessionInfo,
    SessionOutputMode,
};
use crate::pty::PtySession;
use crate::terminal::Terminal;
use thermal_terminal::state_inference::{AgentType, InferenceConfig};

use super::helpers::{assign_unique_name, cell_to_data, generate_name_from_shell, snapshot_cells};
use super::{Daemon, Session};

impl Daemon {
    /// Spawn a new PTY session and register it.
    ///
    /// When `worktree` is true, a git worktree is created from the cwd's repo
    /// and the PTY session runs in the worktree directory instead. If the cwd
    /// is not a git repo, the worktree request is silently ignored.
    ///
    /// If `name` is `Some`, it is used as the display name (with dedup
    /// numbering against existing sessions). If `None`, a name is derived
    /// from the shell basename (e.g. "zsh", "bash") or falls back to
    /// "session-N".
    // TODO: [code-review] decompose into pty_setup, event_relay, update_broadcaster, sidecar_write helpers
    pub(crate) fn spawn_session(
        &self,
        shell: Option<String>,
        cwd: Option<String>,
        worktree: bool,
        name: Option<String>,
    ) -> Result<(String, String)> {
        let shell_path =
            shell.unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));
        let cwd_path = cwd.unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()));

        let id_num = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("session-{id_num}");

        // Optionally create a git worktree for this session.
        let (effective_cwd, worktree_path) = if worktree {
            match Self::create_worktree(&cwd_path, &id) {
                Ok(wt_path) => {
                    info!(session = %id, worktree = %wt_path, "Created git worktree");
                    (wt_path.clone(), Some(wt_path))
                }
                Err(e) => {
                    warn!(session = %id, error = %e, "Failed to create worktree, using original cwd");
                    (cwd_path.clone(), None)
                }
            }
        } else {
            (cwd_path.clone(), None)
        };

        let mut terminal = Terminal::with_size(120, 36);
        // Split multi-word commands into program + args. Single-word commands
        // (bare shell paths like "zsh" or "claude") use spawn_sized which
        // creates a login shell. Multi-word commands use spawn_command_sized
        // to exec with proper argv splitting.
        let parts: Vec<&str> = shell_path.split_whitespace().collect();
        let mut pty = if parts.len() > 1 {
            let mut env = std::collections::HashMap::new();
            env.insert("TERM".to_string(), "xterm-256color".to_string());
            env.insert("COLORTERM".to_string(), "truecolor".to_string());
            let ws = nix::pty::Winsize {
                ws_row: 36,
                ws_col: 120,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            PtySession::spawn_command_sized(parts[0], &parts, Some(&effective_cwd), env, Some(ws))
                .with_context(|| format!("Failed to spawn PTY with command: {shell_path}"))?
        } else {
            PtySession::spawn_sized(&shell_path, Some(&effective_cwd), 120, 36)
                .with_context(|| format!("Failed to spawn PTY with shell: {shell_path}"))?
        };
        let pty_output_rx = pty.take_output();

        // Attach agent state inference to the terminal byte processor.
        // Infers agent type from the shell command; state files are written
        // to /tmp/{claude-code,codex,copilot}-state/ for the ClaudeStatePoller.
        let child_pid = pty.child_pid().as_raw() as u32;
        {
            let agent_type = AgentType::from_command(&shell_path);
            terminal.attach_state_inference(InferenceConfig {
                session_id: id.clone(),
                child_pid,
                agent_type,
                working_dir: Some(effective_cwd.clone()),
            });
        }

        // Attach state change notification channel for semantic event bridging.
        // The std::sync::mpsc sender is used from the byte processor thread;
        // a relay task drains it into the SemanticEventBus via tokio.
        let (change_tx, change_rx) = std::sync::mpsc::channel();
        if let Some(si) = terminal.state_inference() {
            let mut si_guard = si.lock();
            si_guard.set_change_tx(change_tx);
            // Eagerly check /proc/<PID>/cmdline for --output-format json.
            si_guard.check_proc_cmdline_for_json_mode();
        }

        // Notification relay is spawned after session_arc is created (below)
        // so we can update output_mode on StructuredJsonDetected.

        // Attach per-session JSONL event log for structured diagnostics.
        {
            match thermal_terminal::EventLog::for_session(
                &id,
                thermal_terminal::event_log::DEFAULT_MAX_ENTRIES,
            ) {
                Ok(mut event_log) => {
                    event_log.log(&thermal_terminal::SessionEvent::Spawn {
                        command: shell_path.clone(),
                        cwd: effective_cwd.clone(),
                    });
                    if let Some(si) = terminal.state_inference() {
                        si.lock().set_event_log(event_log);
                    }
                }
                Err(e) => {
                    warn!(session = %id, error = %e, "Failed to create session event log");
                }
            }
        }

        // Shared dirty flag for the byte processor.
        let pty_dirty = Arc::new(AtomicBool::new(false));

        // Wakeup pipe for the byte processor to signal the update loop.
        let (wakeup_read, wakeup_write) =
            nix::unistd::pipe().context("Failed to create wakeup pipe")?;

        // Set read end to non-blocking.
        {
            use nix::fcntl::{FcntlArg, OFlag, fcntl};
            use std::os::fd::AsRawFd;
            let flags = fcntl(wakeup_read.as_raw_fd(), FcntlArg::F_GETFL).unwrap_or(0);
            let _ = fcntl(
                wakeup_read.as_raw_fd(),
                FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
            );
        }

        // Spawn the byte processor (feeds PTY output into alacritty Term).
        terminal.spawn_byte_processor(pty_output_rx, Arc::clone(&pty_dirty), wakeup_write);

        // Broadcast channel for screen updates (capacity 64 — slow clients drop).
        let (update_tx, _) = broadcast::channel::<Response>(64);

        let seq = Arc::new(AtomicU64::new(0));
        let title = Arc::new(Mutex::new(String::from("thermal-conductor")));
        let attached_count = Arc::new(AtomicU64::new(0));

        // Derive a unique display name for this session.
        let display_name = {
            let base = match name {
                Some(ref n) if !n.is_empty() => n.clone(),
                _ => generate_name_from_shell(&shell_path, id_num),
            };
            let sessions = self.sessions.lock();
            let existing_names: Vec<String> =
                sessions.values().map(|s| s.lock().name.clone()).collect();
            assign_unique_name(&base, &existing_names)
        };

        // Clone values for sidecar and semantic events before they're moved into Session.
        let sidecar_cwd = cwd_path.clone();
        let semantic_cwd = cwd_path.clone();
        let sidecar_worktree = worktree_path.clone();

        let session = Session {
            id: id.clone(),
            name: display_name.clone(),
            terminal,
            pty,
            shell_command: shell_path.clone(),
            cwd: cwd_path,
            worktree_path,
            update_tx: update_tx.clone(),
            seq: Arc::clone(&seq),
            pty_dirty: Arc::clone(&pty_dirty),
            title: Arc::clone(&title),
            attached_count: Arc::clone(&attached_count),
            created_at: SystemTime::now(),
            output_mode: SessionOutputMode::default(),
        };

        let session_arc = Arc::new(Mutex::new(session));
        self.sessions
            .lock()
            .insert(id.clone(), Arc::clone(&session_arc));

        // Spawn the notification relay task.
        // Drains StateChangeNotifications from the byte processor thread into
        // the SemanticEventBus, and updates session output_mode on detection.
        {
            let event_bus = Arc::clone(&self.event_bus);
            let session_id = id.clone();
            let session_ref_for_relay = Arc::clone(&session_arc);
            tokio::spawn(async move {
                loop {
                    match change_rx.try_recv() {
                        Ok(notif) => {
                            if matches!(
                                notif,
                                thermal_terminal::state_inference::StateChangeNotification::StructuredJsonDetected
                            ) {
                                session_ref_for_relay.lock().output_mode =
                                    SessionOutputMode::StructuredJson;
                            }
                            event_bus.process_notification(&session_id, notif);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            break;
                        }
                    }
                }
            });
        }

        // Spawn a task to handle terminal events (PtyWrite, title changes, etc.)
        {
            let session_ref = Arc::clone(&session_arc);
            let update_tx = update_tx.clone();
            let title_ref = Arc::clone(&title);
            let session_id = id.clone();
            let event_bus = Arc::clone(&self.event_bus);

            tokio::spawn(async move {
                let mut event_rx = {
                    let mut s = session_ref.lock();
                    match s.terminal.take_event_rx() {
                        Some(rx) => rx,
                        None => return,
                    }
                };

                while let Some(event) = event_rx.recv().await {
                    match event {
                        alacritty_terminal::event::Event::PtyWrite(text) => {
                            let s = session_ref.lock();
                            if let Err(e) = s.pty.write(text.as_bytes()) {
                                warn!("Failed to relay PtyWrite to PTY: {e}");
                            }
                        }
                        alacritty_terminal::event::Event::Title(new_title) => {
                            *title_ref.lock() = new_title.clone();
                            let _ = update_tx.send(Response::TitleChanged {
                                id: session_id.clone(),
                                title: new_title.clone(),
                            });
                            // Emit semantic title change event.
                            event_bus.title_changed(&session_id, new_title);
                        }
                        _ => {}
                    }
                }
            });
        }

        // Spawn a task that watches for PTY dirty flag and broadcasts screen updates.
        {
            let session_ref = Arc::clone(&session_arc);
            let pty_dirty_ref = Arc::clone(&pty_dirty);
            let seq_ref = Arc::clone(&seq);
            let update_tx = update_tx.clone();
            let session_id = id.clone();
            let event_bus = Arc::clone(&self.event_bus);
            let sessions_map = Arc::clone(&self.sessions);

            tokio::spawn(async move {
                let wakeup_fd = {
                    use std::os::fd::AsRawFd;
                    wakeup_read.as_raw_fd()
                };
                // Keep the OwnedFd alive for the duration of the task.
                let _wakeup_owner = wakeup_read;
                let mut last_mode: Option<u32> = None;

                loop {
                    // Wait a bit before checking dirty flag.
                    tokio::time::sleep(std::time::Duration::from_millis(8)).await;

                    // Drain wakeup pipe. Use nix::unistd::read with the raw
                    // fd directly — avoids the from_raw_fd + forget pattern
                    // which can close the fd if cancelled at an await point.
                    {
                        let mut buf = [0u8; 64];
                        let _ = nix::unistd::read(wakeup_fd, &mut buf);
                    }

                    // A clean PTY exit does not necessarily produce one last
                    // damaged frame, so check for child exit before bailing
                    // out on an idle dirty flag.
                    {
                        let session = session_ref.lock();
                        if session.pty.has_exited() {
                            let exit_reason = session.pty.exit_reason();
                            let (exit_code, reason_str) = match &exit_reason {
                                Some(thermal_terminal::ExitReason::PtyEof { exit_code }) => {
                                    (*exit_code, exit_reason.as_ref().unwrap().to_string())
                                }
                                Some(thermal_terminal::ExitReason::Signal(sig)) => {
                                    (None, format!("killed by signal {sig}"))
                                }
                                Some(reason) => (None, reason.to_string()),
                                None => (None, String::new()),
                            };
                            // Clean up event log file (mirrors kill path).
                            if let Some(si) = session.terminal.state_inference() {
                                let mut guard = si.lock();
                                if let Some(log) = guard.event_log_mut() {
                                    log.log(&thermal_terminal::SessionEvent::PtyEof {
                                        reason: reason_str.clone(),
                                    });
                                    thermal_terminal::EventLog::remove(log.path());
                                }
                            }
                            // Clean up worktree if present (mirrors kill path).
                            let worktree_path = session.worktree_path.clone();
                            drop(session);

                            let _ = update_tx.send(Response::SessionExited {
                                id: session_id.clone(),
                                exit_code,
                                reason: reason_str.clone(),
                            });
                            // Emit semantic session exit + removal events.
                            event_bus.session_exited(&session_id, exit_code, reason_str);
                            event_bus.session_removed(&session_id);

                            // Remove from sessions HashMap (mirrors kill path).
                            sessions_map.lock().remove(&session_id);

                            // Clean up sidecar entry (fire-and-forget).
                            let sidecar_id = session_id.clone();
                            tokio::spawn(async move {
                                if let Err(e) = sidecar_remove_entry(&sidecar_id).await {
                                    warn!("Failed to remove sidecar entry on exit: {e}");
                                }
                            });

                            // Clean up worktree if present.
                            if let Some(wt_path) = worktree_path {
                                Daemon::remove_worktree(&wt_path);
                            }

                            break;
                        }
                    }

                    if !pty_dirty_ref.swap(false, Ordering::AcqRel) {
                        continue;
                    }

                    // Check if anyone is listening.
                    if update_tx.receiver_count() == 0 {
                        continue;
                    }

                    // Build dirty cell list from the terminal.
                    let session = session_ref.lock();
                    let term_handle = session.terminal.term_handle();
                    let mut term = term_handle.lock();

                    use alacritty_terminal::grid::Dimensions;
                    use alacritty_terminal::term::TermDamage;

                    let screen_lines = term.screen_lines();
                    let cols = term.columns();

                    let dirty_cells: Vec<DirtyCellData>;
                    let full_redraw;
                    let cursor: CursorData;
                    let mode: u32;

                    match term.damage() {
                        TermDamage::Full => {
                            full_redraw = true;
                            dirty_cells = Vec::new();
                            // Extract cursor from renderable content for the
                            // full snapshot path below.
                            let content = term.renderable_content();
                            cursor = CursorData {
                                col: content.cursor.point.column.0 as u16,
                                row: content.cursor.point.line.0.max(0) as u16,
                                visible: content.cursor.shape
                                    != alacritty_terminal::vte::ansi::CursorShape::Hidden,
                            };
                            mode = content.mode.bits();
                        }
                        TermDamage::Partial(iter) => {
                            full_redraw = false;
                            let damaged_rows: std::collections::HashSet<usize> =
                                iter.filter(|b| b.is_damaged()).map(|b| b.line).collect();

                            if damaged_rows.is_empty() {
                                let content = term.renderable_content();
                                let current_mode = content.mode.bits();
                                if last_mode == Some(current_mode) {
                                    term.reset_damage();
                                    drop(term);
                                    drop(session);
                                    continue;
                                }

                                cursor = CursorData {
                                    col: content.cursor.point.column.0 as u16,
                                    row: content.cursor.point.line.0.max(0) as u16,
                                    visible: content.cursor.shape
                                        != alacritty_terminal::vte::ansi::CursorShape::Hidden,
                                };
                                mode = current_mode;
                                dirty_cells = Vec::new();
                                term.reset_damage();
                                drop(term);
                                drop(session);
                                let s = seq_ref.fetch_add(1, Ordering::Relaxed);
                                let _ = update_tx.send(Response::ScreenUpdate {
                                    id: session_id.clone(),
                                    seq: s,
                                    dirty_cells,
                                    cursor,
                                    mode,
                                });
                                last_mode = Some(mode);
                                continue;
                            }

                            // Single renderable_content() call for both dirty
                            // cells and cursor — avoids inconsistent state from
                            // calling it twice.
                            let content = term.renderable_content();
                            cursor = CursorData {
                                col: content.cursor.point.column.0 as u16,
                                row: content.cursor.point.line.0.max(0) as u16,
                                visible: content.cursor.shape
                                    != alacritty_terminal::vte::ansi::CursorShape::Hidden,
                            };
                            mode = content.mode.bits();
                            dirty_cells = content
                                .display_iter
                                .filter_map(|indexed| {
                                    let point = indexed.point;
                                    let cell = indexed.cell;
                                    let viewport_line =
                                        point.line.0 + content.display_offset as i32;
                                    let row = usize::try_from(viewport_line).ok()?;
                                    if row >= screen_lines {
                                        return None;
                                    }
                                    if !damaged_rows.contains(&row) {
                                        return None;
                                    }
                                    Some(DirtyCellData {
                                        col: point.column.0 as u16,
                                        row: row as u16,
                                        cell: cell_to_data(cell),
                                    })
                                })
                                .collect();
                        }
                    }

                    term.reset_damage();
                    drop(term);
                    drop(session);

                    if full_redraw {
                        // For a full redraw, send a SessionState instead (clients handle both).
                        let session = session_ref.lock();
                        let cells = snapshot_cells(&session.terminal, screen_lines, cols);
                        let title = session.title.lock().clone();
                        let output_mode = session.output_mode;
                        drop(session);

                        let _ = update_tx.send(Response::SessionState {
                            id: session_id.clone(),
                            cols: cols as u16,
                            rows: screen_lines as u16,
                            cells,
                            cursor,
                            mode,
                            title,
                            output_mode,
                        });
                        last_mode = Some(mode);
                    } else {
                        let s = seq_ref.fetch_add(1, Ordering::Relaxed);
                        let _ = update_tx.send(Response::ScreenUpdate {
                            id: session_id.clone(),
                            seq: s,
                            dirty_cells,
                            cursor,
                            mode,
                        });
                        last_mode = Some(mode);
                    }
                }

                info!(session = %session_id, "Update broadcaster exiting");
            });
        }

        // Write session metadata to the sidecar file so TUI and HUD can
        // discover daemon sessions alongside kitty sessions.
        {
            let sidecar_id = id.clone();
            let sidecar_name = display_name.clone();
            tokio::spawn(async move {
                if let Err(e) = sidecar_locked_update(move |data| {
                    data.sessions.retain(|e| e.session_id != sidecar_id);
                    data.sessions.push(SidecarEntry {
                        session_id: sidecar_id,
                        worktree_path: sidecar_worktree,
                        profile_name: None,
                        original_cwd: sidecar_cwd,
                        spawn_time: now_epoch(),
                        display_name: Some(sidecar_name),
                    });
                })
                .await
                {
                    warn!("Failed to update sidecar on spawn: {e}");
                }
            });
        }

        // Emit semantic SessionSpawned event.
        self.event_bus.session_spawned(
            &id,
            Some(display_name.clone()),
            Some(semantic_cwd),
            Some(child_pid),
        );

        info!(session = %id, name = %display_name, "Session spawned");
        Ok((id, display_name))
    }

    /// Create a git worktree for a session.
    ///
    /// Detects the repo name from the cwd's git toplevel, then creates a
    /// worktree at `/tmp/thermal-worktrees/{repo_name}-{session_id}`.
    /// Returns the worktree path on success, or an error if the cwd is not
    /// a git repo or the worktree command fails.
    fn create_worktree(cwd: &str, session_id: &str) -> Result<String> {
        // Check if cwd is inside a git repo.
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(cwd)
            .output()
            .context("Failed to run git rev-parse")?;

        if !output.status.success() {
            anyhow::bail!("Not a git repository: {cwd}");
        }

        let repo_root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let repo_name = std::path::Path::new(&repo_root)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());

        let worktree_dir = format!("/tmp/thermal-worktrees/{repo_name}-{session_id}");

        // Ensure the parent directory exists.
        std::fs::create_dir_all("/tmp/thermal-worktrees")
            .context("Failed to create /tmp/thermal-worktrees")?;

        // Create the worktree from the current HEAD.
        let wt_output = std::process::Command::new("git")
            .args(["worktree", "add", &worktree_dir, "HEAD"])
            .current_dir(&repo_root)
            .output()
            .context("Failed to run git worktree add")?;

        if !wt_output.status.success() {
            let stderr = String::from_utf8_lossy(&wt_output.stderr);
            anyhow::bail!("git worktree add failed: {stderr}");
        }

        Ok(worktree_dir)
    }

    /// Remove a git worktree, cleaning up the directory.
    fn remove_worktree(worktree_path: &str) {
        // Use `git worktree remove --force` to clean up even if there are
        // uncommitted changes (the session is being killed anyway).
        let result = std::process::Command::new("git")
            .args(["worktree", "remove", "--force", worktree_path])
            .output();

        match result {
            Ok(output) if output.status.success() => {
                info!(path = %worktree_path, "Removed git worktree");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(path = %worktree_path, error = %stderr, "Failed to remove git worktree");
                // Fall back to removing the directory directly.
                if let Err(e) = std::fs::remove_dir_all(worktree_path) {
                    warn!(path = %worktree_path, error = %e, "Failed to remove worktree directory");
                }
            }
            Err(e) => {
                warn!(path = %worktree_path, error = %e, "Failed to run git worktree remove");
                let _ = std::fs::remove_dir_all(worktree_path);
            }
        }
    }

    /// Get a list of all sessions.
    pub(crate) fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock();
        sessions
            .values()
            .map(|s| {
                let session = s.lock();
                let term_handle = session.terminal.term_handle();
                let term = term_handle.lock();
                use alacritty_terminal::grid::Dimensions;
                let start_secs = session
                    .created_at
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                SessionInfo {
                    id: session.id.clone(),
                    name: Some(session.name.clone()),
                    shell_command: session.shell_command.clone(),
                    cwd: session.cwd.clone(),
                    shell_pid: session.pty.child_pid().as_raw(),
                    cols: term.columns() as u16,
                    rows: term.screen_lines() as u16,
                    title: session.title.lock().clone(),
                    start_time: start_secs,
                    connected_client_count: session.attached_count.load(Ordering::Relaxed) as usize,
                    is_alive: !session.pty.has_exited(),
                    worktree_path: session.worktree_path.clone(),
                    output_mode: session.output_mode,
                }
            })
            .collect()
    }

    /// Build a full grid snapshot for a session.
    pub(super) fn get_session_state(&self, id: &str) -> Option<Response> {
        let sessions = self.sessions.lock();
        let session_arc = sessions.get(id)?;
        let session = session_arc.lock();

        let term_handle = session.terminal.term_handle();
        let term = term_handle.lock();
        use alacritty_terminal::grid::Dimensions;
        let screen_lines = term.screen_lines();
        let cols = term.columns();

        let content = term.renderable_content();
        let cursor = CursorData {
            col: content.cursor.point.column.0 as u16,
            row: content.cursor.point.line.0.max(0) as u16,
            visible: content.cursor.shape != alacritty_terminal::vte::ansi::CursorShape::Hidden,
        };
        let mode = content.mode.bits();
        drop(term);

        let cells = snapshot_cells(&session.terminal, screen_lines, cols);
        let title = session.title.lock().clone();
        let output_mode = session.output_mode;

        Some(Response::SessionState {
            id: id.to_string(),
            cols: cols as u16,
            rows: screen_lines as u16,
            cells,
            cursor,
            mode,
            title,
            output_mode,
        })
    }

    /// Handle a single client request and return the response.
    pub(crate) fn handle_request(&self, request: &Request) -> Response {
        match request {
            Request::SpawnSession {
                shell,
                cwd,
                worktree,
                name,
            } => match self.spawn_session(shell.clone(), cwd.clone(), *worktree, name.clone()) {
                Ok((id, session_name)) => Response::SessionSpawned {
                    id,
                    name: session_name,
                },
                Err(e) => Response::Error {
                    message: format!("Failed to spawn session: {e}"),
                },
            },

            Request::KillSession { id } => {
                let mut sessions = self.sessions.lock();
                if let Some(session_arc) = sessions.remove(id) {
                    let session = session_arc.lock();
                    if let Some(ref wt_path) = session.worktree_path {
                        Self::remove_worktree(wt_path);
                    }
                    // Clean up the session event log file.
                    if let Some(si) = session.terminal.state_inference() {
                        let mut guard = si.lock();
                        if let Some(log) = guard.event_log_mut() {
                            thermal_terminal::EventLog::remove(log.path());
                        }
                    }
                    drop(session);
                    // Remove from sidecar (fire-and-forget).
                    let id_for_sidecar = id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = sidecar_remove_entry(&id_for_sidecar).await {
                            warn!("Failed to remove sidecar entry on kill: {e}");
                        }
                    });
                    // Emit semantic events for kill.
                    self.event_bus
                        .session_exited(id, None, "killed".to_string());
                    self.event_bus.session_removed(id);
                    info!(session = %id, "Session killed");
                    Response::Ok
                } else {
                    Response::Error {
                        message: format!("Session not found: {id}"),
                    }
                }
            }

            Request::ListSessions => {
                let sessions = self.list_sessions();
                Response::SessionList { sessions }
            }

            Request::SendInput { id, data } => {
                let sessions = self.sessions.lock();
                match sessions.get(id) {
                    Some(session_arc) => {
                        let session = session_arc.lock();
                        match session.pty.write(data) {
                            Ok(_) => Response::Ok,
                            Err(e) => Response::Error {
                                message: format!("PTY write failed: {e}"),
                            },
                        }
                    }
                    None => Response::Error {
                        message: format!("Session not found: {id}"),
                    },
                }
            }

            Request::SendText { id, text } => {
                let sessions = self.sessions.lock();
                match sessions.get(id) {
                    Some(session_arc) => {
                        let session = session_arc.lock();
                        // Append \r to press Enter, matching kitty @ send-text behavior.
                        let mut payload = text.as_bytes().to_vec();
                        payload.push(b'\r');
                        match session.pty.write(&payload) {
                            Ok(_) => Response::Ok,
                            Err(e) => Response::Error {
                                message: format!("PTY write failed: {e}"),
                            },
                        }
                    }
                    None => Response::Error {
                        message: format!("Session not found: {id}"),
                    },
                }
            }

            Request::GetSessionState { id } => match self.get_session_state(id) {
                Some(state) => state,
                None => Response::Error {
                    message: format!("Session not found: {id}"),
                },
            },

            Request::Attach { id, initial_size } => {
                let sessions = self.sessions.lock();
                match sessions.get(id) {
                    Some(session_arc) => {
                        let session = session_arc.lock();
                        // Apply initial size if provided and no other clients attached.
                        if let Some((cols, rows)) = initial_size
                            && session.attached_count.load(Ordering::Relaxed) == 0
                        {
                            session
                                .terminal
                                .resize(*cols as usize, *rows as usize, 8, 16);
                            let _ = session.pty.resize(*cols, *rows);
                        }
                        session.attached_count.fetch_add(1, Ordering::Relaxed);
                        drop(session);
                        drop(sessions);

                        // Return full snapshot.
                        match self.get_session_state(id) {
                            Some(state) => state,
                            None => Response::Error {
                                message: format!("Session disappeared: {id}"),
                            },
                        }
                    }
                    None => Response::Error {
                        message: format!("Session not found: {id}"),
                    },
                }
            }

            Request::Detach { id } => {
                let sessions = self.sessions.lock();
                match sessions.get(id) {
                    Some(session_arc) => {
                        let session = session_arc.lock();
                        session.attached_count.fetch_sub(1, Ordering::Relaxed);
                        Response::Ok
                    }
                    None => Response::Error {
                        message: format!("Session not found: {id}"),
                    },
                }
            }

            Request::Resize { id, cols, rows } => {
                let sessions = self.sessions.lock();
                match sessions.get(id) {
                    Some(session_arc) => {
                        let session = session_arc.lock();
                        session
                            .terminal
                            .resize(*cols as usize, *rows as usize, 8, 16);
                        // Signal the update broadcaster so it sees the
                        // TermDamage::Full produced by the resize and sends
                        // a SessionState with the new dimensions to clients.
                        session.pty_dirty.store(true, Ordering::Release);
                        match session.pty.resize(*cols, *rows) {
                            Ok(_) => Response::Ok,
                            Err(e) => Response::Error {
                                message: format!("PTY resize failed: {e}"),
                            },
                        }
                    }
                    None => Response::Error {
                        message: format!("Session not found: {id}"),
                    },
                }
            }

            Request::Hello { version } => {
                if *version == protocol::PROTOCOL_VERSION {
                    Response::HelloAck {
                        version: protocol::PROTOCOL_VERSION,
                        capabilities: vec!["events".into(), "streaming".into()],
                    }
                } else {
                    Response::Error {
                        message: format!(
                            "Incompatible protocol version: client={version}, daemon={}",
                            protocol::PROTOCOL_VERSION
                        ),
                    }
                }
            }

            Request::Ping => Response::Pong,

            // SubscribeEvents is handled at the connection level in handle_client.
            // If it reaches here, it means the connection handler didn't intercept it.
            Request::SubscribeEvents { .. } => Response::Error {
                message: "SubscribeEvents must be handled at the connection level".into(),
            },

            // MessageForward is handled at the connection level when a MessageBus
            // is available (e.g. in the TUI). The daemon doesn't own a bus yet.
            Request::MessageForward { .. } => Response::Error {
                message: "MessageForward not supported in daemon mode (bus is internal to TUI)"
                    .into(),
            },
        }
    }

    /// Get a broadcast receiver for a session's updates.
    pub(super) fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<Response>> {
        let sessions = self.sessions.lock();
        sessions.get(id).map(|s| {
            let session = s.lock();
            session.update_tx.subscribe()
        })
    }

    /// Collect current session state for persistence.
    ///
    /// Snapshots all active sessions into a `PersistedState` that can be
    /// written to disk on graceful shutdown.
    pub(crate) fn collect_persisted_state(&self) -> PersistedState {
        let sessions = self.sessions.lock();
        let persisted_sessions: Vec<PersistedSession> = sessions
            .values()
            .map(|s| {
                let session = s.lock();
                let term_handle = session.terminal.term_handle();
                let term = term_handle.lock();
                use alacritty_terminal::grid::Dimensions;
                let cols = term.columns() as u16;
                let rows = term.screen_lines() as u16;
                drop(term);

                let created_secs = session
                    .created_at
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                PersistedSession {
                    id: session.id.clone(),
                    name: session.name.clone(),
                    shell_pid: session.pty.child_pid().as_raw(),
                    shell_command: session.shell_command.clone(),
                    cwd: session.cwd.clone(),
                    cols,
                    rows,
                    title: session.title.lock().clone(),
                    created_at: created_secs,
                    worktree_path: session.worktree_path.clone(),
                }
            })
            .collect();

        PersistedState {
            saved_at: now_epoch(),
            sessions: persisted_sessions,
        }
    }
}
