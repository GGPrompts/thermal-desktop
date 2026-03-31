//! Session daemon: owns PTY sessions independently of any frontend window.
//!
//! The daemon listens on a Unix socket and accepts client connections.
//! Each session consists of a `PtySession` + `Terminal` (alacritty_terminal::Term).
//! Frontends connect, attach to sessions, receive screen updates, and send input.
//!
//! Socket path: `/run/user/<uid>/thermal/conductor.sock`

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

use crate::kitty::{
    SidecarEntry, next_unique_name, sidecar_locked_update, sidecar_remove as sidecar_remove_entry, now_epoch,
};
use thermal_terminal::state_inference::{AgentType, InferenceConfig};
use crate::persist::{self, PersistedSession, PersistedState};
use crate::protocol::{
    self, CellData, ColorData, CursorData, DirtyCellData, Request, Response, SessionInfo,
};
use crate::pty::PtySession;
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
    pty_dirty: Arc<AtomicBool>,
    /// Current terminal title.
    title: Arc<Mutex<String>>,
    /// Number of attached frontend clients.
    attached_count: Arc<AtomicU64>,
    created_at: SystemTime,
}

// ── Daemon state ─────────────────────────────────────────────────────────────

/// The session daemon, managing all sessions and client connections.
pub(crate) struct Daemon {
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<Session>>>>>,
    next_id: AtomicU64,
}

impl Daemon {
    pub(crate) fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        }
    }

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
        let mut pty = PtySession::spawn_sized(&shell_path, Some(&effective_cwd), 120, 36)
            .with_context(|| format!("Failed to spawn PTY with shell: {shell_path}"))?;
        let pty_output_rx = pty.take_output();

        // Attach agent state inference to the terminal byte processor.
        // Infers agent type from the shell command; state files are written
        // to /tmp/{claude-code,codex,copilot}-state/ for the ClaudeStatePoller.
        {
            let agent_type = AgentType::from_command(&shell_path);
            let child_pid = pty.child_pid().as_raw() as u32;
            terminal.attach_state_inference(InferenceConfig {
                session_id: id.clone(),
                child_pid,
                agent_type,
                working_dir: Some(effective_cwd.clone()),
            });
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
            let existing_names: Vec<String> = sessions
                .values()
                .map(|s| s.lock().name.clone())
                .collect();
            assign_unique_name(&base, &existing_names)
        };

        // Clone values for sidecar before they're moved into Session.
        let sidecar_cwd = cwd_path.clone();
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
        };

        let session_arc = Arc::new(Mutex::new(session));
        self.sessions
            .lock()
            .insert(id.clone(), Arc::clone(&session_arc));

        // Spawn a task to handle terminal events (PtyWrite, title changes, etc.)
        {
            let session_ref = Arc::clone(&session_arc);
            let update_tx = update_tx.clone();
            let title_ref = Arc::clone(&title);
            let session_id = id.clone();

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
                                title: new_title,
                            });
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

            tokio::spawn(async move {
                let wakeup_fd = {
                    use std::os::fd::AsRawFd;
                    wakeup_read.as_raw_fd()
                };
                // Keep the OwnedFd alive for the duration of the task.
                let _wakeup_owner = wakeup_read;

                loop {
                    // Wait a bit before checking dirty flag.
                    tokio::time::sleep(std::time::Duration::from_millis(8)).await;

                    // Drain wakeup pipe.
                    {
                        use std::io::Read;
                        use std::os::fd::FromRawFd;
                        let mut f = unsafe { std::fs::File::from_raw_fd(wakeup_fd) };
                        let mut buf = [0u8; 64];
                        let _ = f.read(&mut buf);
                        std::mem::forget(f);
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
                        }
                        TermDamage::Partial(iter) => {
                            full_redraw = false;
                            let damaged_rows: std::collections::HashSet<usize> =
                                iter.filter(|b| b.is_damaged()).map(|b| b.line).collect();

                            if damaged_rows.is_empty() {
                                term.reset_damage();
                                drop(term);
                                drop(session);
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
                        drop(session);

                        let _ = update_tx.send(Response::SessionState {
                            id: session_id.clone(),
                            cols: cols as u16,
                            rows: screen_lines as u16,
                            cells,
                            cursor,
                            title,
                        });
                    } else {
                        let s = seq_ref.fetch_add(1, Ordering::Relaxed);
                        let _ = update_tx.send(Response::ScreenUpdate {
                            id: session_id.clone(),
                            seq: s,
                            dirty_cells,
                            cursor,
                        });
                    }

                    // Check if the PTY child exited.
                    {
                        let session = session_ref.lock();
                        if session.pty.has_exited() {
                            let _ = update_tx.send(Response::SessionExited {
                                id: session_id.clone(),
                                exit_code: None,
                            });
                            break;
                        }
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
                }
            })
            .collect()
    }

    /// Build a full grid snapshot for a session.
    fn get_session_state(&self, id: &str) -> Option<Response> {
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
        drop(term);

        let cells = snapshot_cells(&session.terminal, screen_lines, cols);
        let title = session.title.lock().clone();

        Some(Response::SessionState {
            id: id.to_string(),
            cols: cols as u16,
            rows: screen_lines as u16,
            cells,
            cursor,
            title,
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
                    drop(session);
                    // Remove from sidecar (fire-and-forget).
                    let id_for_sidecar = id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = sidecar_remove_entry(&id_for_sidecar).await {
                            warn!("Failed to remove sidecar entry on kill: {e}");
                        }
                    });
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

            Request::Ping => Response::Pong,
        }
    }

    /// Get a broadcast receiver for a session's updates.
    fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<Response>> {
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

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Convert an alacritty terminal cell to our wire format `CellData`.
fn cell_to_data(cell: &alacritty_terminal::term::cell::Cell) -> CellData {
    CellData {
        ch: cell.c,
        fg: color_to_data(cell.fg),
        bg: color_to_data(cell.bg),
        flags: cell.flags.bits(),
    }
}

/// Convert an alacritty Rgb/Color to our wire format `ColorData`.
fn color_to_data(color: alacritty_terminal::vte::ansi::Color) -> ColorData {
    // alacritty_terminal::vte::ansi::Color can be Named, Spec, or Indexed.
    // For the wire protocol we resolve to a default RGB value.
    match color {
        alacritty_terminal::vte::ansi::Color::Spec(rgb) => ColorData {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        },
        alacritty_terminal::vte::ansi::Color::Named(name) => {
            // Map named colors to reasonable defaults.
            let (r, g, b) = named_color_rgb(name);
            ColorData { r, g, b }
        }
        alacritty_terminal::vte::ansi::Color::Indexed(idx) => {
            // Use the standard 256-color palette approximation.
            let (r, g, b) = indexed_color_rgb(idx);
            ColorData { r, g, b }
        }
    }
}

/// Map a named color to an approximate RGB value.
fn named_color_rgb(name: alacritty_terminal::vte::ansi::NamedColor) -> (u8, u8, u8) {
    use alacritty_terminal::vte::ansi::NamedColor;
    match name {
        NamedColor::Black => (0, 0, 0),
        NamedColor::Red => (204, 0, 0),
        NamedColor::Green => (78, 154, 6),
        NamedColor::Yellow => (196, 160, 0),
        NamedColor::Blue => (52, 101, 164),
        NamedColor::Magenta => (117, 80, 123),
        NamedColor::Cyan => (6, 152, 154),
        NamedColor::White => (211, 215, 207),
        NamedColor::BrightBlack => (85, 87, 83),
        NamedColor::BrightRed => (239, 41, 41),
        NamedColor::BrightGreen => (138, 226, 52),
        NamedColor::BrightYellow => (252, 233, 79),
        NamedColor::BrightBlue => (114, 159, 207),
        NamedColor::BrightMagenta => (173, 127, 168),
        NamedColor::BrightCyan => (52, 226, 226),
        NamedColor::BrightWhite => (238, 238, 236),
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::Cursor => {
            (211, 215, 207)
        }
        NamedColor::Background => (0, 0, 0),
        NamedColor::DimBlack => (40, 40, 40),
        NamedColor::DimRed => (150, 0, 0),
        NamedColor::DimGreen => (50, 100, 4),
        NamedColor::DimYellow => (140, 110, 0),
        NamedColor::DimBlue => (35, 70, 110),
        NamedColor::DimMagenta => (80, 55, 85),
        NamedColor::DimCyan => (4, 105, 106),
        NamedColor::DimWhite | NamedColor::DimForeground => (150, 152, 147),
    }
}

/// Map a 256-color index to RGB.
fn indexed_color_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0..=15 => {
            // Standard 16 colors — map via named.
            let named = [
                (0, 0, 0),
                (204, 0, 0),
                (78, 154, 6),
                (196, 160, 0),
                (52, 101, 164),
                (117, 80, 123),
                (6, 152, 154),
                (211, 215, 207),
                (85, 87, 83),
                (239, 41, 41),
                (138, 226, 52),
                (252, 233, 79),
                (114, 159, 207),
                (173, 127, 168),
                (52, 226, 226),
                (238, 238, 236),
            ];
            named[idx as usize]
        }
        16..=231 => {
            // 6x6x6 color cube.
            let idx = idx - 16;
            let r = idx / 36;
            let g = (idx % 36) / 6;
            let b = idx % 6;
            let to_val = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            (to_val(r), to_val(g), to_val(b))
        }
        232..=255 => {
            // Grayscale ramp.
            let v = 8 + 10 * (idx - 232);
            (v, v, v)
        }
    }
}

/// Generate a display name from the shell path.
///
/// Extracts the basename of the shell binary (e.g. "/bin/zsh" -> "zsh",
/// "/usr/bin/bash" -> "bash"). Falls back to "session-N" if the path
/// has no recognizable basename.
fn generate_name_from_shell(shell_path: &str, id_num: u64) -> String {
    std::path::Path::new(shell_path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("session-{id_num}"))
}

/// Assign a unique name given a base and a list of existing names.
///
/// - If `base` is not taken, returns it as-is (e.g. "zsh").
/// - If taken, appends a dedup suffix: "zsh-2", "zsh-3", etc.
///
/// Delegates to the shared `next_unique_name()` helper in kitty.rs.
fn assign_unique_name(base: &str, existing: &[String]) -> String {
    next_unique_name(base, |candidate| existing.iter().any(|n| n == candidate))
}

/// Create a full grid snapshot from a Terminal.
fn snapshot_cells(terminal: &Terminal, screen_lines: usize, cols: usize) -> Vec<CellData> {
    let term_handle = terminal.term_handle();
    let term = term_handle.lock();
    let content = term.renderable_content();

    let mut grid = vec![
        CellData {
            ch: ' ',
            fg: ColorData {
                r: 211,
                g: 215,
                b: 207
            },
            bg: ColorData { r: 0, g: 0, b: 0 },
            flags: 0,
        };
        screen_lines * cols
    ];

    for indexed in content.display_iter {
        let point = indexed.point;
        let cell = indexed.cell;
        let viewport_line = point.line.0 + content.display_offset as i32;
        let row = match usize::try_from(viewport_line) {
            Ok(r) if r < screen_lines => r,
            _ => continue,
        };
        let col = point.column.0;
        if col < cols {
            grid[row * cols + col] = cell_to_data(cell);
        }
    }

    grid
}

// ── Client connection handler ────────────────────────────────────────────────

/// Handle a single client connection.
async fn handle_client(daemon: Arc<Daemon>, stream: UnixStream) {
    let (mut reader, mut writer) = stream.into_split();
    let mut attached_session: Option<String> = None;

    // Cancellation token for the broadcast forwarder task. When the client
    // detaches or re-attaches to a different session, we abort the old
    // forwarder so we don't duplicate messages.
    let mut forwarder_handle: Option<tokio::task::JoinHandle<()>> = None;

    // Spawn a task to forward update broadcasts to this client.
    let (client_tx, mut client_rx) = mpsc::channel::<Response>(64);

    // Writer task: sends responses to the client socket.
    let writer_handle = tokio::spawn(async move {
        while let Some(response) = client_rx.recv().await {
            match protocol::encode_frame(&response) {
                Ok(frame) => {
                    if let Err(e) = writer.write_all(&frame).await {
                        warn!("Failed to write to client: {e}");
                        break;
                    }
                }
                Err(e) => {
                    error!("Failed to encode response: {e}");
                }
            }
        }
    });

    loop {
        // Read the next request from the client.
        let payload = match protocol::read_frame(&mut reader).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                info!("Client disconnected");
                break;
            }
            Err(e) => {
                warn!("Client read error: {e}");
                break;
            }
        };

        let request: Request = match protocol::decode_payload(&payload) {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to decode client request: {e}");
                let _ = client_tx
                    .send(Response::Error {
                        message: format!("Invalid request: {e}"),
                    })
                    .await;
                continue;
            }
        };

        // Handle attach specially — subscribe to the session's broadcast.
        if let Request::Attach { ref id, .. } = request {
            // If already attached to a session, detach from it first.
            if let Some(ref prev_id) = attached_session {
                // Abort the old forwarder task to stop duplicate messages.
                if let Some(handle) = forwarder_handle.take() {
                    handle.abort();
                }
                // Decrement the old session's attached count.
                let sessions = daemon.sessions.lock();
                if let Some(session_arc) = sessions.get(prev_id) {
                    let session = session_arc.lock();
                    session.attached_count.fetch_sub(1, Ordering::Relaxed);
                }
            }

            // Subscribe to the session's broadcast channel. Only one
            // subscription is created and moved into the forwarder task.
            if let Some(mut rx) = daemon.subscribe(id) {
                attached_session = Some(id.clone());

                // Spawn a task to forward broadcasts to the client channel.
                let client_tx_clone = client_tx.clone();
                forwarder_handle = Some(tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(response) => {
                                if client_tx_clone.send(response).await.is_err() {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Client lagged, skipped {n} updates");
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        }
                    }
                }));
            }
        }

        // Handle detach — clean up forwarder and attached count.
        if let Request::Detach { ref id } = request {
            if attached_session.as_deref() == Some(id) {
                if let Some(handle) = forwarder_handle.take() {
                    handle.abort();
                }
                attached_session = None;
            }
        }

        let response = daemon.handle_request(&request);
        if client_tx.send(response).await.is_err() {
            break;
        }
    }

    // Clean up: abort forwarder and detach from session if attached.
    if let Some(handle) = forwarder_handle.take() {
        handle.abort();
    }
    if let Some(id) = attached_session {
        let sessions = daemon.sessions.lock();
        if let Some(session_arc) = sessions.get(&id) {
            let session = session_arc.lock();
            session.attached_count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    drop(client_tx);
    let _ = writer_handle.await;
}

// ── Public entry points ──────────────────────────────────────────────────────

/// Run the session daemon on a given `UnixListener` until the `shutdown` receiver
/// fires.
///
/// This is the core accept loop, factored out so that tests, alternative entry
/// points, and `run_daemon()` can supply their own socket path and shutdown
/// signal. An optional pre-created `Daemon` can be passed in; if `None`, a
/// fresh one is created.
pub async fn run_daemon_on(
    listener: UnixListener,
    mut shutdown: tokio::sync::mpsc::Receiver<()>,
    daemon: Option<Arc<Daemon>>,
) -> Result<Arc<Daemon>> {
    let daemon = daemon.unwrap_or_else(|| Arc::new(Daemon::new()));

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _addr)) => {
                        info!("Client connected");
                        let daemon_clone = Arc::clone(&daemon);
                        tokio::spawn(handle_client(daemon_clone, stream));
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {e}");
                    }
                }
            }
            _ = shutdown.recv() => {
                info!("Shutdown signal received");
                break;
            }
        }
    }

    Ok(daemon)
}

/// Run the session daemon.
///
/// This is an async function that runs until interrupted (SIGTERM/SIGINT).
/// It binds a Unix socket and accepts client connections. It also registers
/// a D-Bus interface (`org.thermal.Conductor`) on the session bus so that
/// thermal-bar, thermal-hud, and other components can discover sessions
/// without a direct Unix socket connection.
pub async fn run_daemon() -> Result<()> {
    let socket_path = protocol::socket_path();
    info!(path = %socket_path.display(), "Starting session daemon");

    // Ensure parent directory exists.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create socket directory: {}", parent.display()))?;
    }

    // Remove stale socket if present.
    if socket_path.exists() {
        info!("Removing stale socket");
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("Failed to remove stale socket: {}", socket_path.display()))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Failed to bind Unix socket: {}", socket_path.display()))?;

    info!(path = %socket_path.display(), "Daemon listening");

    let daemon = Arc::new(Daemon::new());

    // ── Session recovery from previous daemon instance ──────────────────
    match persist::recover_sessions() {
        Ok(recovered) if !recovered.is_empty() => {
            let alive_count = recovered.iter().filter(|r| r.shell_alive).count();
            let dead_count = recovered.len() - alive_count;
            info!(
                total = recovered.len(),
                alive = alive_count,
                dead = dead_count,
                "Recovered sessions from previous daemon"
            );

            for r in &recovered {
                if r.shell_alive {
                    // The shell is still running, but we've lost the PTY master fd.
                    // Log as orphaned — future versions can re-adopt via fd passing.
                    warn!(
                        id = %r.session.id,
                        name = %r.session.name,
                        pid = r.session.shell_pid,
                        "Orphaned session: shell alive but PTY master lost — \
                         cannot re-attach (run `kill {}` to clean up)",
                        r.session.shell_pid
                    );
                } else {
                    info!(
                        id = %r.session.id,
                        name = %r.session.name,
                        pid = r.session.shell_pid,
                        "Previous session shell has exited — no recovery needed"
                    );
                }
            }
        }
        Ok(_) => {
            // No state file or empty — fresh start.
        }
        Err(e) => {
            warn!("Failed to recover sessions from previous daemon: {e}");
        }
    }

    // Register D-Bus interface on the session bus.
    let dbus_interface = crate::dbus_interface::ConductorInterface::new(Arc::clone(&daemon));
    let _dbus_conn = match zbus::connection::Builder::session()
        .and_then(|b| b.name(crate::dbus_interface::BUS_NAME))
        .and_then(|b| b.serve_at(crate::dbus_interface::OBJECT_PATH, dbus_interface))
    {
        Ok(builder) => match builder.build().await {
            Ok(conn) => {
                info!(
                    name = crate::dbus_interface::BUS_NAME,
                    path = crate::dbus_interface::OBJECT_PATH,
                    "D-Bus interface registered"
                );
                Some(conn)
            }
            Err(e) => {
                warn!("Failed to connect to D-Bus session bus: {e} — running without D-Bus");
                None
            }
        },
        Err(e) => {
            warn!("Failed to build D-Bus connection: {e} — running without D-Bus");
            None
        }
    };

    // Bridge ctrl_c into an mpsc channel so we can reuse run_daemon_on().
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = shutdown_tx.send(()).await;
    });

    // Delegate to the shared accept loop.
    let daemon = run_daemon_on(listener, shutdown_rx, Some(daemon)).await?;

    // ── Persist session state before shutdown ─────────────────────────────
    let state = daemon.collect_persisted_state();
    if !state.sessions.is_empty() {
        match persist::save_state(&state) {
            Ok(()) => info!(
                sessions = state.sessions.len(),
                "Session state persisted for recovery"
            ),
            Err(e) => error!("Failed to persist session state: {e}"),
        }
    } else {
        // No sessions to save — clean up any stale state file.
        let _ = persist::remove_state_file();
    }

    // Clean up socket.
    let _ = std::fs::remove_file(&socket_path);
    info!("Daemon shut down");
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use std::path::PathBuf;
    use tokio::net::UnixListener;

    /// Spawn a daemon on a temporary socket, connect a client, spawn a session,
    /// list sessions, verify the session appears, then shut everything down.
    #[tokio::test]
    async fn daemon_spawn_and_list() {
        // Use tempfile::tempdir() so the socket lives in a guaranteed-writable
        // directory (works in sandboxed environments where /tmp may not be
        // accessible). The `_dir` binding keeps the directory alive for the
        // duration of the test.
        let _dir = tempfile::tempdir().expect("Failed to create temp dir");
        let sock_path = _dir.path().join("test.sock");

        let listener = UnixListener::bind(&sock_path).expect("Failed to bind test socket");

        // Shutdown channel.
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

        // Spawn daemon in background.
        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon_on(listener, shutdown_rx, None).await;
        });

        // Give the daemon a moment to start accepting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect a client.
        let mut client = DaemonClient::connect_to(PathBuf::from(&sock_path))
            .await
            .expect("connect_to failed")
            .expect("Expected Some(client), daemon should be running");

        // Ping the daemon.
        client.ping().await.expect("Ping failed");

        // Spawn a session.
        let session_id = client
            .spawn_session(Some("/bin/sh".to_string()), None, false)
            .await
            .expect("spawn_session failed");
        assert!(
            session_id.starts_with("session-"),
            "Unexpected session id: {session_id}"
        );

        // List sessions and verify our session is there.
        let sessions = client.list_sessions().await.expect("list_sessions failed");
        assert_eq!(sessions.len(), 1, "Expected exactly one session");
        assert_eq!(sessions[0].id, session_id);
        assert_eq!(sessions[0].shell_command, "/bin/sh");
        assert!(sessions[0].is_alive, "Session should be alive");

        // Kill the session.
        client
            .kill_session(&session_id)
            .await
            .expect("kill_session failed");

        // Verify the session is gone.
        let sessions = client
            .list_sessions()
            .await
            .expect("list_sessions after kill failed");
        assert!(sessions.is_empty(), "Expected no sessions after kill");

        // Shut down the daemon.
        let _ = shutdown_tx.send(()).await;
        let _ = daemon_handle.await;

        // Socket file is cleaned up when `_dir` is dropped.
    }

    /// Helper: spin up a daemon on a temp socket, return (shutdown_tx, sock_path, _dir).
    async fn setup_daemon() -> (
        tokio::sync::mpsc::Sender<()>,
        PathBuf,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let sock_path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&sock_path).expect("Failed to bind test socket");
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

        tokio::spawn(async move {
            let _ = run_daemon_on(listener, shutdown_rx, None).await;
        });

        // Wait for daemon to start accepting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        (shutdown_tx, sock_path, dir)
    }

    /// Helper: connect a client to the given socket path.
    async fn connect_client(sock_path: &std::path::Path) -> DaemonClient {
        DaemonClient::connect_to(PathBuf::from(sock_path))
            .await
            .expect("connect_to failed")
            .expect("Expected Some(client)")
    }

    /// Attach to a session and verify we get a SessionState snapshot back.
    #[tokio::test]
    async fn attach_returns_session_state() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        let session_id = client
            .spawn_session(Some("/bin/sh".to_string()), None, false)
            .await
            .expect("spawn_session failed");

        let resp = client
            .attach(&session_id, Some((80, 24)))
            .await
            .expect("attach failed");

        match resp {
            Response::SessionState { id, cols, rows, .. } => {
                assert_eq!(id, session_id);
                // Daemon applies the initial size when no other client is attached.
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
            }
            other => panic!("Expected SessionState, got: {other:?}"),
        }

        let _ = shutdown_tx.send(()).await;
    }

    /// Verify that attached clients receive streamed ScreenUpdate messages
    /// when input is sent to the session's PTY.
    #[tokio::test]
    async fn attach_streams_screen_updates() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        let session_id = client
            .spawn_session(Some("/bin/sh".to_string()), None, false)
            .await
            .expect("spawn_session failed");

        // Attach to get initial state.
        let _ = client
            .attach(&session_id, Some((80, 24)))
            .await
            .expect("attach failed");

        // Take the response receiver so we can read streamed updates.
        let mut rx = client.take_response_rx();

        // Send input that will produce output (echo).
        let tx = client.request_tx_clone();
        tx.send(Request::SendInput {
            id: session_id.clone(),
            data: b"echo hello\n".to_vec(),
        })
        .await
        .expect("send input");

        // We should receive at least one ScreenUpdate or SessionState within
        // a reasonable timeout (the daemon polls every 8ms + processing).
        let mut got_update = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Some(Response::ScreenUpdate { .. })) => {
                    got_update = true;
                    break;
                }
                Ok(Some(Response::SessionState { .. })) => {
                    got_update = true;
                    break;
                }
                Ok(Some(Response::Ok)) => {
                    // SendInput acknowledgment — keep waiting for the screen update.
                    continue;
                }
                Ok(Some(_other)) => {
                    // Some other response — keep waiting.
                    continue;
                }
                Ok(None) => break,
                Err(_timeout) => continue,
            }
        }
        assert!(got_update, "Expected to receive a screen update after sending input");

        let _ = shutdown_tx.send(()).await;
    }

    /// Verify that detach decrements the attached count and works cleanly.
    #[tokio::test]
    async fn detach_decrements_attached_count() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        let session_id = client
            .spawn_session(Some("/bin/sh".to_string()), None, false)
            .await
            .expect("spawn_session failed");

        // Attach.
        let _ = client
            .attach(&session_id, Some((80, 24)))
            .await
            .expect("attach failed");

        // List sessions to verify attached count is 1.
        // Need a second client for listing since the first has its rx taken.
        let mut client2 = connect_client(&sock_path).await;
        let sessions = client2.list_sessions().await.expect("list failed");
        assert_eq!(sessions[0].connected_client_count, 1);

        // Detach.
        client.detach(&session_id).await.expect("detach failed");

        // Verify attached count went back to 0.
        let sessions = client2.list_sessions().await.expect("list after detach failed");
        assert_eq!(sessions[0].connected_client_count, 0);

        let _ = shutdown_tx.send(()).await;
    }

    /// Two clients can attach to the same session simultaneously.
    #[tokio::test]
    async fn two_clients_attach_to_same_session() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client1 = connect_client(&sock_path).await;
        let mut client2 = connect_client(&sock_path).await;

        let session_id = client1
            .spawn_session(Some("/bin/sh".to_string()), None, false)
            .await
            .expect("spawn_session failed");

        // Both attach.
        let resp1 = client1
            .attach(&session_id, Some((80, 24)))
            .await
            .expect("client1 attach failed");
        assert!(matches!(resp1, Response::SessionState { .. }));

        let resp2 = client2
            .attach(&session_id, None)
            .await
            .expect("client2 attach failed");
        assert!(matches!(resp2, Response::SessionState { .. }));

        // List to verify both are counted.
        let mut list_client = connect_client(&sock_path).await;
        let sessions = list_client.list_sessions().await.expect("list failed");
        assert_eq!(sessions[0].connected_client_count, 2);

        let _ = shutdown_tx.send(()).await;
    }

    /// Resize request changes the PTY dimensions.
    #[tokio::test]
    async fn resize_session_changes_dimensions() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        let session_id = client
            .spawn_session(Some("/bin/sh".to_string()), None, false)
            .await
            .expect("spawn_session failed");

        // Resize to a specific size.
        client
            .resize(&session_id, 100, 50)
            .await
            .expect("resize failed");

        // Allow the terminal to process the resize.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Verify via session list.
        let sessions = client.list_sessions().await.expect("list failed");
        assert_eq!(sessions[0].cols, 100);
        assert_eq!(sessions[0].rows, 50);

        let _ = shutdown_tx.send(()).await;
    }

    /// Sending input to a non-existent session returns an error.
    #[tokio::test]
    async fn send_input_to_nonexistent_session_errors() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        let resp = client
            .request(Request::SendInput {
                id: "nonexistent".to_string(),
                data: b"hello".to_vec(),
            })
            .await
            .expect("request failed");

        assert!(
            matches!(resp, Response::Error { ref message } if message.contains("not found")),
            "Expected error for nonexistent session, got: {resp:?}"
        );

        let _ = shutdown_tx.send(()).await;
    }

    /// Client disconnect properly cleans up attached count.
    #[tokio::test]
    async fn client_disconnect_cleanup() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;

        let session_id;
        {
            let mut client = connect_client(&sock_path).await;

            session_id = client
                .spawn_session(Some("/bin/sh".to_string()), None, false)
                .await
                .expect("spawn_session failed");

            let _ = client
                .attach(&session_id, Some((80, 24)))
                .await
                .expect("attach failed");
            // client drops here — connection closes.
        }

        // Give the daemon time to process the disconnect.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify count went back to 0.
        let mut check_client = connect_client(&sock_path).await;
        let sessions = check_client.list_sessions().await.expect("list failed");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].connected_client_count, 0);

        let _ = shutdown_tx.send(()).await;
    }

    /// Get session state returns a full grid snapshot.
    #[tokio::test]
    async fn get_session_state_returns_snapshot() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        let session_id = client
            .spawn_session(Some("/bin/sh".to_string()), None, false)
            .await
            .expect("spawn_session failed");

        let resp = client
            .get_session_state(&session_id)
            .await
            .expect("get_session_state failed");

        match resp {
            Response::SessionState {
                id,
                cols,
                rows,
                cells,
                ..
            } => {
                assert_eq!(id, session_id);
                assert!(cols > 0);
                assert!(rows > 0);
                // Grid should have cols * rows cells.
                assert_eq!(cells.len(), cols as usize * rows as usize);
            }
            other => panic!("Expected SessionState, got: {other:?}"),
        }

        let _ = shutdown_tx.send(()).await;
    }

    // ── Name generation unit tests ──────────────────────────────────────────

    #[test]
    fn generate_name_from_shell_basename() {
        assert_eq!(generate_name_from_shell("/bin/zsh", 1), "zsh");
        assert_eq!(generate_name_from_shell("/usr/bin/bash", 2), "bash");
        assert_eq!(generate_name_from_shell("/bin/sh", 3), "sh");
    }

    #[test]
    fn generate_name_from_shell_bare_name() {
        assert_eq!(generate_name_from_shell("fish", 4), "fish");
    }

    #[test]
    fn generate_name_from_shell_empty_fallback() {
        // Empty path should fall back to session-N.
        assert_eq!(generate_name_from_shell("", 5), "session-5");
    }

    #[test]
    fn generate_name_from_shell_trailing_slash_fallback() {
        // A path like "/" has no file_name, should fall back.
        assert_eq!(generate_name_from_shell("/", 6), "session-6");
    }

    // ── Unique name assignment unit tests ───────────────────────────────────

    #[test]
    fn assign_unique_name_first_is_bare() {
        let existing: Vec<String> = vec![];
        assert_eq!(assign_unique_name("zsh", &existing), "zsh");
    }

    #[test]
    fn assign_unique_name_dedup_second() {
        let existing = vec!["zsh".to_string()];
        assert_eq!(assign_unique_name("zsh", &existing), "zsh-2");
    }

    #[test]
    fn assign_unique_name_dedup_third() {
        let existing = vec!["zsh".to_string(), "zsh-2".to_string()];
        assert_eq!(assign_unique_name("zsh", &existing), "zsh-3");
    }

    #[test]
    fn assign_unique_name_different_bases_no_conflict() {
        let existing = vec!["zsh".to_string()];
        assert_eq!(assign_unique_name("bash", &existing), "bash");
    }

    #[test]
    fn assign_unique_name_gap_fills_first_available() {
        // "zsh" and "zsh-3" taken but not "zsh-2".
        let existing = vec!["zsh".to_string(), "zsh-3".to_string()];
        assert_eq!(assign_unique_name("zsh", &existing), "zsh-2");
    }

    // ── Integration: spawn returns name ─────────────────────────────────────

    #[tokio::test]
    async fn spawn_session_returns_name() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        let (session_id, name) = client
            .spawn_session_named(Some("/bin/sh".to_string()), None, false, None)
            .await
            .expect("spawn_session_named failed");

        assert!(session_id.starts_with("session-"));
        // Auto-generated name from "/bin/sh" should be "sh".
        assert_eq!(name, "sh");

        let _ = shutdown_tx.send(()).await;
    }

    #[tokio::test]
    async fn spawn_session_with_explicit_name() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        let (_id, name) = client
            .spawn_session_named(
                Some("/bin/sh".to_string()),
                None,
                false,
                Some("opus".to_string()),
            )
            .await
            .expect("spawn_session_named failed");

        assert_eq!(name, "opus");

        let _ = shutdown_tx.send(()).await;
    }

    #[tokio::test]
    async fn spawn_session_dedup_names() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        // Spawn two sessions with the same shell — names should be deduped.
        let (_id1, name1) = client
            .spawn_session_named(Some("/bin/sh".to_string()), None, false, None)
            .await
            .expect("first spawn failed");
        let (_id2, name2) = client
            .spawn_session_named(Some("/bin/sh".to_string()), None, false, None)
            .await
            .expect("second spawn failed");

        assert_eq!(name1, "sh");
        assert_eq!(name2, "sh-2");

        let _ = shutdown_tx.send(()).await;
    }

    #[tokio::test]
    async fn spawn_session_dedup_explicit_names() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        let (_id1, name1) = client
            .spawn_session_named(
                Some("/bin/sh".to_string()),
                None,
                false,
                Some("opus".to_string()),
            )
            .await
            .expect("first spawn failed");
        let (_id2, name2) = client
            .spawn_session_named(
                Some("/bin/sh".to_string()),
                None,
                false,
                Some("opus".to_string()),
            )
            .await
            .expect("second spawn failed");

        assert_eq!(name1, "opus");
        assert_eq!(name2, "opus-2");

        let _ = shutdown_tx.send(()).await;
    }

    #[tokio::test]
    async fn list_sessions_includes_name() {
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        let (_id, _name) = client
            .spawn_session_named(
                Some("/bin/sh".to_string()),
                None,
                false,
                Some("sonnet".to_string()),
            )
            .await
            .expect("spawn failed");

        let sessions = client.list_sessions().await.expect("list_sessions failed");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name.as_deref(), Some("sonnet"));

        let _ = shutdown_tx.send(()).await;
    }

    #[tokio::test]
    async fn backward_compat_spawn_session_still_works() {
        // The old spawn_session (without name) should still work and
        // return a session ID (name is auto-generated but not returned
        // by the old API).
        let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
        let mut client = connect_client(&sock_path).await;

        let session_id = client
            .spawn_session(Some("/bin/sh".to_string()), None, false)
            .await
            .expect("spawn_session failed");

        assert!(session_id.starts_with("session-"));

        // The session should have a name in the list.
        let sessions = client.list_sessions().await.expect("list failed");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name.as_deref(), Some("sh"));

        let _ = shutdown_tx.send(()).await;
    }
}
