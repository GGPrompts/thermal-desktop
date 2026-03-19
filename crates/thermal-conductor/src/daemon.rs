//! Session daemon: owns PTY sessions independently of any frontend window.
//!
//! The daemon listens on a Unix socket and accepts client connections.
//! Each session consists of a `PtySession` + `Terminal` (alacritty_terminal::Term).
//! Frontends connect, attach to sessions, receive screen updates, and send input.
//!
//! Socket path: `/run/user/<uid>/thermal/conductor.sock`

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

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
    terminal: Terminal,
    pty: PtySession,
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
    #[allow(dead_code)]
    created_at: SystemTime,
}

// ── Daemon state ─────────────────────────────────────────────────────────────

/// The session daemon, managing all sessions and client connections.
struct Daemon {
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<Session>>>>>,
    next_id: AtomicU64,
}

impl Daemon {
    fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        }
    }

    /// Spawn a new PTY session and register it.
    fn spawn_session(
        &self,
        shell: Option<String>,
        _cwd: Option<String>,
    ) -> Result<String> {
        let shell_path =
            shell.unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));

        let id_num = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("session-{id_num}");

        let mut pty = PtySession::spawn(&shell_path)
            .with_context(|| format!("Failed to spawn PTY with shell: {shell_path}"))?;

        let terminal = Terminal::with_size(120, 36);
        let pty_output_rx = pty.take_output();

        // Shared dirty flag for the byte processor.
        let pty_dirty = Arc::new(AtomicBool::new(false));

        // Wakeup pipe for the byte processor to signal the update loop.
        let (wakeup_read, wakeup_write) =
            nix::unistd::pipe().context("Failed to create wakeup pipe")?;

        // Set read end to non-blocking.
        {
            use nix::fcntl::{fcntl, FcntlArg, OFlag};
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

        let session = Session {
            id: id.clone(),
            terminal,
            pty,
            update_tx: update_tx.clone(),
            seq: Arc::clone(&seq),
            pty_dirty: Arc::clone(&pty_dirty),
            title: Arc::clone(&title),
            attached_count: Arc::clone(&attached_count),
            created_at: SystemTime::now(),
        };

        let session_arc = Arc::new(Mutex::new(session));
        self.sessions.lock().insert(id.clone(), Arc::clone(&session_arc));

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

                    match term.damage() {
                        TermDamage::Full => {
                            full_redraw = true;
                            dirty_cells = Vec::new();
                        }
                        TermDamage::Partial(iter) => {
                            full_redraw = false;
                            let damaged_rows: std::collections::HashSet<usize> = iter
                                .filter(|b| b.is_damaged())
                                .map(|b| b.line)
                                .collect();

                            if damaged_rows.is_empty() {
                                term.reset_damage();
                                drop(term);
                                drop(session);
                                continue;
                            }

                            let content = term.renderable_content();
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

                    let content = term.renderable_content();
                    let cursor = CursorData {
                        col: content.cursor.point.column.0 as u16,
                        row: content.cursor.point.line.0.max(0) as u16,
                        visible: content.cursor.shape != alacritty_terminal::vte::ansi::CursorShape::Hidden,
                    };

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

        info!(session = %id, "Session spawned");
        Ok(id)
    }

    /// Get a list of all sessions.
    fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock();
        sessions
            .values()
            .map(|s| {
                let session = s.lock();
                let term_handle = session.terminal.term_handle();
                let term = term_handle.lock();
                use alacritty_terminal::grid::Dimensions;
                SessionInfo {
                    id: session.id.clone(),
                    shell_pid: session.pty.child_pid().as_raw(),
                    cols: term.columns() as u16,
                    rows: term.screen_lines() as u16,
                    title: session.title.lock().clone(),
                    exited: session.pty.has_exited(),
                    attached_clients: session.attached_count.load(Ordering::Relaxed) as usize,
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
    fn handle_request(&self, request: &Request) -> Response {
        match request {
            Request::SpawnSession { shell, cwd } => {
                match self.spawn_session(shell.clone(), cwd.clone()) {
                    Ok(id) => Response::SessionSpawned { id },
                    Err(e) => Response::Error {
                        message: format!("Failed to spawn session: {e}"),
                    },
                }
            }

            Request::KillSession { id } => {
                let mut sessions = self.sessions.lock();
                if sessions.remove(id).is_some() {
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
                        if let Some((cols, rows)) = initial_size {
                            if session.attached_count.load(Ordering::Relaxed) == 0 {
                                session.terminal.resize(
                                    *cols as usize,
                                    *rows as usize,
                                    8,
                                    16,
                                );
                                let _ = session.pty.resize(*cols, *rows);
                            }
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

/// Create a full grid snapshot from a Terminal.
fn snapshot_cells(terminal: &Terminal, screen_lines: usize, cols: usize) -> Vec<CellData> {
    let term_handle = terminal.term_handle();
    let term = term_handle.lock();
    let content = term.renderable_content();

    let mut grid = vec![
        CellData {
            ch: ' ',
            fg: ColorData { r: 211, g: 215, b: 207 },
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
    let mut update_rx: Option<broadcast::Receiver<Response>> = None;

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
            if let Some(rx) = daemon.subscribe(id) {
                update_rx = Some(rx);
                attached_session = Some(id.clone());

                // Spawn a task to forward broadcasts to the client channel.
                let client_tx_clone = client_tx.clone();
                let mut rx = daemon.subscribe(id).unwrap();
                tokio::spawn(async move {
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
                });
            }
        }

        let response = daemon.handle_request(&request);
        if client_tx.send(response).await.is_err() {
            break;
        }
    }

    // Clean up: detach from session if attached.
    if let Some(id) = attached_session {
        let sessions = daemon.sessions.lock();
        if let Some(session_arc) = sessions.get(&id) {
            let session = session_arc.lock();
            session.attached_count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    // Drop the update receiver.
    drop(update_rx);
    drop(client_tx);
    let _ = writer_handle.await;
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Run the session daemon.
///
/// This is an async function that runs until interrupted (SIGTERM/SIGINT).
/// It binds a Unix socket and accepts client connections.
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

    // Install shutdown handler.
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

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
            _ = &mut shutdown => {
                info!("Shutdown signal received");
                break;
            }
        }
    }

    // Clean up socket.
    let _ = std::fs::remove_file(&socket_path);
    info!("Daemon shut down");
    Ok(())
}
