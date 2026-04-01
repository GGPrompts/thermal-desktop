//! Preview pane: daemon broadcast subscriber and cell-to-line conversion.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::client::DaemonClient;
use crate::protocol::{CellData, DirtyCellData, Response};

use super::format::TEXT_MUTED;
use super::SessionsPage;

// ---------------------------------------------------------------------------
// Preview broadcast subscriber
// ---------------------------------------------------------------------------

/// Shared buffer holding the latest screen state from a daemon broadcast subscription.
#[allow(dead_code)]
pub(super) struct PreviewBuffer {
    /// Row-major flat cell grid (length = cols * rows).
    cells: Vec<CellData>,
    /// Number of columns.
    cols: usize,
    /// The daemon session ID this buffer corresponds to.
    session_id: String,
    /// Sequence number of the latest ScreenUpdate applied.
    seq: u64,
    /// Set to true whenever new data arrives (cleared by the reader).
    dirty: bool,
}

/// Manages a background thread that subscribes to daemon screen updates via `Attach`.
pub(in crate::tui) struct PreviewSubscriber {
    /// Shared buffer with the latest screen state.
    buffer: Arc<Mutex<Option<PreviewBuffer>>>,
    /// Channel to tell the background task which session to attach to.
    session_tx: std::sync::mpsc::Sender<Option<(String, String)>>,
}

impl PreviewSubscriber {
    /// Start the background subscriber thread.
    pub(super) fn spawn() -> Self {
        let buffer: Arc<Mutex<Option<PreviewBuffer>>> = Arc::new(Mutex::new(None));
        let (session_tx, session_rx) = std::sync::mpsc::channel::<Option<(String, String)>>();

        let buffer_clone = Arc::clone(&buffer);
        std::thread::Builder::new()
            .name("preview-subscriber".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!("preview-subscriber: failed to create runtime: {e}");
                        return;
                    }
                };
                rt.block_on(preview_subscriber_loop(buffer_clone, session_rx));
            })
            .expect("spawn preview-subscriber thread");

        Self { buffer, session_tx }
    }

    /// Switch the subscription to a different daemon session.
    pub(super) fn attach(&self, daemon_id: &str, _cwd: &str) {
        let _ = self
            .session_tx
            .send(Some((daemon_id.to_string(), _cwd.to_string())));
    }

    /// Read the current preview content if new data is available.
    pub(super) fn take_if_dirty(&self) -> Option<(Vec<CellData>, usize)> {
        let mut guard = self.buffer.lock().ok()?;
        let buf = guard.as_mut()?;
        if !buf.dirty {
            return None;
        }
        buf.dirty = false;
        Some((buf.cells.clone(), buf.cols))
    }

    /// Read the current preview content regardless of dirty flag.
    pub(super) fn read(&self) -> Option<(Vec<CellData>, usize)> {
        let guard = self.buffer.lock().ok()?;
        let buf = guard.as_ref()?;
        Some((buf.cells.clone(), buf.cols))
    }
}

/// Background async loop: listens for session switch commands and runs
/// the attach/stream cycle for each.
async fn preview_subscriber_loop(
    buffer: Arc<Mutex<Option<PreviewBuffer>>>,
    session_rx: std::sync::mpsc::Receiver<Option<(String, String)>>,
) {
    let mut current_session: Option<String> = None;
    let mut client: Option<DaemonClient> = None;

    loop {
        let switch = if client.is_some() && current_session.is_some() {
            match session_rx.try_recv() {
                Ok(cmd) => Some(cmd),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        } else {
            match session_rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => return,
            }
        };

        if let Some(cmd) = switch {
            match cmd {
                Some((daemon_id, _cwd)) => {
                    if client.is_none() {
                        match DaemonClient::connect().await {
                            Ok(Some(c)) => client = Some(c),
                            Ok(None) => {
                                tracing::warn!("preview-subscriber: daemon not running");
                                continue;
                            }
                            Err(e) => {
                                tracing::warn!("preview-subscriber: connect error: {e}");
                                continue;
                            }
                        }
                    }

                    let c = client.as_mut().unwrap();
                    if let Err(e) = c
                        .send(crate::protocol::Request::Attach {
                            id: daemon_id.clone(),
                            initial_size: None,
                        })
                        .await
                    {
                        tracing::warn!("preview-subscriber: attach send error: {e}");
                        client = None;
                        continue;
                    }

                    match tokio::time::timeout(std::time::Duration::from_secs(3), c.recv()).await {
                        Ok(Some(Response::SessionState { cols, cells, .. })) => {
                            let mut guard = buffer.lock().unwrap_or_else(|p| p.into_inner());
                            *guard = Some(PreviewBuffer {
                                cells,
                                cols: cols as usize,
                                session_id: daemon_id.clone(),
                                seq: 0,
                                dirty: true,
                            });
                            current_session = Some(daemon_id);
                        }
                        Ok(Some(Response::Error { message })) => {
                            tracing::warn!("preview-subscriber: attach error: {message}");
                            current_session = None;
                        }
                        Ok(Some(_)) => {
                            tracing::warn!("preview-subscriber: unexpected response to attach");
                            current_session = None;
                        }
                        Ok(None) => {
                            tracing::warn!(
                                "preview-subscriber: connection closed during attach"
                            );
                            client = None;
                            current_session = None;
                        }
                        Err(_) => {
                            tracing::warn!("preview-subscriber: attach timed out");
                            current_session = None;
                        }
                    }
                }
                None => {
                    let mut guard = buffer.lock().unwrap_or_else(|p| p.into_inner());
                    *guard = None;
                    current_session = None;
                }
            }
            continue;
        }

        // No switch command — try to read the next broadcast update.
        if let Some(c) = client.as_mut() {
            match tokio::time::timeout(std::time::Duration::from_millis(50), c.recv()).await {
                Ok(Some(Response::ScreenUpdate { dirty_cells, .. })) => {
                    let mut guard = buffer.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(ref mut buf) = *guard {
                        apply_dirty_cells(&mut buf.cells, buf.cols, &dirty_cells);
                        buf.seq += 1;
                        buf.dirty = true;
                    }
                }
                Ok(Some(Response::SessionState { cols, cells, .. })) => {
                    let mut guard = buffer.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(ref mut buf) = *guard {
                        buf.cells = cells;
                        buf.cols = cols as usize;
                        buf.seq += 1;
                        buf.dirty = true;
                    }
                }
                Ok(Some(Response::SessionExited { .. })) => {
                    let mut guard = buffer.lock().unwrap_or_else(|p| p.into_inner());
                    *guard = None;
                    current_session = None;
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    tracing::info!("preview-subscriber: connection closed");
                    client = None;
                    current_session = None;
                }
                Err(_) => {}
            }
        }
    }
}

/// Apply incremental dirty cells to a flat row-major cell grid.
fn apply_dirty_cells(cells: &mut [CellData], cols: usize, dirty: &[DirtyCellData]) {
    if cols == 0 {
        return;
    }
    for dc in dirty {
        let idx = dc.row as usize * cols + dc.col as usize;
        if idx < cells.len() {
            cells[idx] = dc.cell.clone();
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon cell grid -> ratatui Line conversion
// ---------------------------------------------------------------------------

/// Alacritty terminal cell flag bits.
const FLAG_INVERSE: u16 = 0b0000_0000_0000_0001;
const FLAG_BOLD: u16 = 0b0000_0000_0000_0010;
const FLAG_ITALIC: u16 = 0b0000_0000_0000_0100;
const FLAG_UNDERLINE: u16 = 0b0000_0000_0000_1000;
const FLAG_DIM: u16 = 0b0000_0000_1000_0000;
const FLAG_STRIKEOUT: u16 = 0b0000_0010_0000_0000;

/// Convert a flat grid of `CellData` into styled ratatui `Line`s.
pub(super) fn cells_to_lines(cells: &[CellData], cols: usize) -> Vec<Line<'static>> {
    if cols == 0 {
        return Vec::new();
    }

    cells
        .chunks(cols)
        .map(|row_cells| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut run_text = String::new();
            let mut run_style: Option<Style> = None;

            for cell in row_cells {
                let style = cell_style(cell);
                if run_style.as_ref() == Some(&style) {
                    run_text.push(cell.ch);
                } else {
                    if let Some(prev_style) = run_style.take() {
                        spans.push(Span::styled(std::mem::take(&mut run_text), prev_style));
                    }
                    run_text.push(cell.ch);
                    run_style = Some(style);
                }
            }
            if let Some(style) = run_style {
                spans.push(Span::styled(run_text, style));
            }

            Line::from(spans)
        })
        .collect()
}

/// Map a single `CellData` to a ratatui `Style`.
fn cell_style(cell: &CellData) -> Style {
    let flags = cell.flags;

    let (fg_r, fg_g, fg_b, bg_r, bg_g, bg_b) = if flags & FLAG_INVERSE != 0 {
        (
            cell.bg.r, cell.bg.g, cell.bg.b, cell.fg.r, cell.fg.g, cell.fg.b,
        )
    } else {
        (
            cell.fg.r, cell.fg.g, cell.fg.b, cell.bg.r, cell.bg.g, cell.bg.b,
        )
    };

    let mut style = Style::default()
        .fg(Color::Rgb(fg_r, fg_g, fg_b))
        .bg(Color::Rgb(bg_r, bg_g, bg_b));

    let mut modifier = Modifier::empty();
    if flags & FLAG_BOLD != 0 {
        modifier |= Modifier::BOLD;
    }
    if flags & FLAG_ITALIC != 0 {
        modifier |= Modifier::ITALIC;
    }
    if flags & FLAG_UNDERLINE != 0 {
        modifier |= Modifier::UNDERLINED;
    }
    if flags & FLAG_DIM != 0 {
        modifier |= Modifier::DIM;
    }
    if flags & FLAG_STRIKEOUT != 0 {
        modifier |= Modifier::CROSSED_OUT;
    }
    if !modifier.is_empty() {
        style = style.add_modifier(modifier);
    }

    style
}

// ---------------------------------------------------------------------------
// Kitty window scanning
// ---------------------------------------------------------------------------

/// Scan all kitty instances via `/tmp/kitty-thc-*` sockets and build a
/// cwd -> (socket_path, window_id) map across all instances.
pub(super) fn scan_all_kitty_windows() -> HashMap<String, (String, i64)> {
    let mut map = HashMap::new();
    let sockets: Vec<_> = std::fs::read_dir("/tmp")
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("kitty-thc-"))
                .unwrap_or(false)
        })
        .filter_map(|e| {
            let canonical = std::fs::canonicalize(e.path()).ok()?;
            // Verify resolved path is still under /tmp to prevent symlink redirect.
            if !canonical.starts_with("/tmp") {
                return None;
            }
            Some(format!("unix:{}", canonical.display()))
        })
        .collect();

    for socket in &sockets {
        let output = match Command::new("kitty")
            .args(["@", "--to", socket, "ls"])
            .output()
        {
            Ok(o) if o.status.success() => o,
            _ => continue,
        };
        let text = match String::from_utf8(output.stdout) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let data: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let empty = Vec::new();
        for oswin in data.as_array().unwrap_or(&empty) {
            for tab in oswin["tabs"].as_array().unwrap_or(&empty) {
                for win in tab["windows"].as_array().unwrap_or(&empty) {
                    let wid = win["id"].as_i64().unwrap_or(-1);
                    if wid < 0 {
                        continue;
                    }
                    let entry = (socket.clone(), wid);
                    if let Some(cwd) = win["cwd"].as_str() {
                        if !cwd.is_empty() {
                            map.entry(cwd.to_string()).or_insert_with(|| entry.clone());
                        }
                    }
                    if let Some(procs) = win["foreground_processes"].as_array() {
                        for proc in procs {
                            if let Some(cwd) = proc["cwd"].as_str() {
                                if !cwd.is_empty() {
                                    map.entry(cwd.to_string())
                                        .or_insert_with(|| entry.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    map
}

// ---------------------------------------------------------------------------
// SessionsPage preview methods
// ---------------------------------------------------------------------------

impl SessionsPage {
    /// Build a diagnostic line for the preview pane.
    pub(super) fn preview_diagnostic(msg: &str) -> Vec<Line<'static>> {
        vec![Line::from(Span::styled(
            msg.to_string(),
            Style::default().fg(TEXT_MUTED),
        ))]
    }

    /// Fetch terminal content for the selected session's preview pane.
    pub(super) fn fetch_preview(&mut self) {
        let selected = self.table_state.selected().and_then(|i| {
            self.display_rows.get(i).map(|row| {
                let cwd = row.session.working_dir.clone().unwrap_or_default();
                (row.session.session_id.clone(), cwd)
            })
        });

        let (session_id, cwd) = match selected {
            Some((id, cwd)) if !cwd.is_empty() => (id, cwd),
            Some((id, _)) => {
                self.preview_content = Self::preview_diagnostic(&format!(
                    "(no working_dir for session {})",
                    &id[..id.len().min(12)]
                ));
                self.preview_scroll = self.preview_content.len();
                self.last_preview_session = Some(id);
                return;
            }
            _ => {
                self.preview_content =
                    Self::preview_diagnostic("(select a session with arrow keys)");
                self.preview_scroll = self.preview_content.len();
                self.last_preview_session = None;
                return;
            }
        };

        let selection_changed = self.last_preview_session.as_deref() != Some(&session_id);
        if selection_changed {
            self.last_preview_session = Some(session_id.clone());
            self.preview_scroll = 0;
            self.preview_pinned = false;
            self.last_preview_update = None;
            self.preview_attached_daemon_id = None;
        }

        if let Some(last) = self.last_preview_update {
            if last.elapsed() < std::time::Duration::from_millis(500) {
                return;
            }
        }
        self.last_preview_update = Some(Instant::now());

        match self.backend_pref {
            crate::backend::BackendPreference::Daemon => {
                self.fetch_preview_daemon(&cwd);
                return;
            }
            crate::backend::BackendPreference::Auto => {
                if self.fetch_preview_kitty(&cwd) {
                    return;
                }
                self.fetch_preview_daemon(&cwd);
                return;
            }
            crate::backend::BackendPreference::Kitty => {
                self.fetch_preview_kitty(&cwd);
            }
        }
    }

    /// Fetch preview via `kitty @ get-text`.
    pub(super) fn fetch_preview_kitty(&mut self, cwd: &str) -> bool {
        let resolved = self.resolve_kitty_window(cwd);

        let (socket, window_id) = match resolved {
            Some(pair) => pair,
            None => return false,
        };

        let match_arg = format!("id:{window_id}");
        let result = Command::new("kitty")
            .args([
                "@",
                "--to",
                &socket,
                "get-text",
                "--extent=screen",
                "--match",
                &match_arg,
            ])
            .output();

        match result {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                self.preview_content = text
                    .lines()
                    .map(|l| {
                        Line::from(Span::styled(l.to_string(), Style::default().fg(TEXT_MUTED)))
                    })
                    .collect();
                if !self.preview_pinned {
                    self.preview_scroll = self.preview_content.len();
                }
            }
            _ => {
                self.preview_content = Self::preview_diagnostic(&format!(
                    "(preview failed for kitty window {window_id})"
                ));
                self.preview_scroll = self.preview_content.len();
            }
        }
        true
    }

    /// Fetch preview via daemon broadcast subscription.
    pub(super) fn fetch_preview_daemon(&mut self, cwd: &str) {
        let stale = self
            .last_daemon_ls
            .map(|t| t.elapsed() >= std::time::Duration::from_secs(3))
            .unwrap_or(true);

        if stale {
            self.last_daemon_ls = Some(Instant::now());
            self.refresh_daemon_session_map();
        }

        let daemon_id = match self.daemon_session_map.get(cwd) {
            Some(id) => id.clone(),
            None => {
                self.preview_content =
                    Self::preview_diagnostic(&format!("(no daemon session for cwd: {cwd})"));
                self.preview_scroll = self.preview_content.len();
                return;
            }
        };

        if self.preview_subscriber.is_none() {
            self.preview_subscriber = Some(PreviewSubscriber::spawn());
        }
        let sub = self.preview_subscriber.as_ref().unwrap();

        let need_attach = self
            .preview_attached_daemon_id
            .as_deref()
            .map(|id| id != daemon_id)
            .unwrap_or(true);

        if need_attach {
            sub.attach(&daemon_id, cwd);
            self.preview_attached_daemon_id = Some(daemon_id.clone());
        }

        if let Some((cells, cols)) = sub.take_if_dirty() {
            self.preview_content = cells_to_lines(&cells, cols);
            if !self.preview_pinned {
                self.preview_scroll = self.preview_content.len();
            }
        } else if self.preview_content.is_empty() || need_attach {
            if let Some((cells, cols)) = sub.read() {
                self.preview_content = cells_to_lines(&cells, cols);
                if !self.preview_pinned {
                    self.preview_scroll = self.preview_content.len();
                }
            }
        }
    }

    /// Refresh the daemon cwd -> session ID map.
    pub(super) fn refresh_daemon_session_map(&mut self) {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };

        let sessions = rt.block_on(async {
            let mut client = match DaemonClient::connect().await {
                Ok(Some(c)) => c,
                _ => return Vec::new(),
            };
            client.list_sessions().await.unwrap_or_default()
        });

        self.daemon_session_map.clear();
        for info in sessions {
            if info.is_alive && !info.cwd.is_empty() {
                self.daemon_session_map.entry(info.cwd).or_insert(info.id);
            }
        }
    }

    /// Look up the kitty (socket, window_id) for a given cwd.
    pub(super) fn resolve_kitty_window(&mut self, cwd: &str) -> Option<(String, i64)> {
        let stale = self
            .last_kitty_ls
            .map(|t| t.elapsed() >= std::time::Duration::from_secs(3))
            .unwrap_or(true);

        if stale {
            self.last_kitty_ls = Some(Instant::now());
            self.kitty_window_map = scan_all_kitty_windows();
        }

        self.kitty_window_map.get(cwd).cloned()
    }
}
