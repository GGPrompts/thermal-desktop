//! Daemon reader task and helpers for applying daemon session state to
//! the local alacritty terminal.

use std::os::fd::AsRawFd;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;

use crate::protocol::Response;
use crate::terminal::{Terminal, ThermalEventListener};
use crate::window::session_mode::SharedDaemonClient;

/// Spawn a tokio task that reads daemon responses and applies screen updates
/// to the local terminal. Signals the wakeup pipe so the render loop wakes.
///
/// In client mode the daemon streams `ScreenUpdate` and `SessionExited`
/// messages after an `Attach`. This task processes them in the background,
/// writing dirty cells into the Term and setting the pty_dirty flag.
pub(super) fn spawn_daemon_reader_task(
    terminal: &Terminal,
    mut response_rx: tokio::sync::mpsc::Receiver<Response>,
    pty_dirty: Arc<AtomicBool>,
    force_full_redraw: Arc<AtomicBool>,
    synced_term_mode: Arc<AtomicU32>,
    exit_requested: Arc<AtomicBool>,
    pending_title: Arc<Mutex<Option<String>>>,
    wakeup_write: std::os::fd::OwnedFd,
    shared_client: SharedDaemonClient,
    session_id: String,
) {
    let term_handle = terminal.term_handle();
    let wakeup_write_fd = wakeup_write.as_raw_fd();

    tokio::spawn(async move {
        // Keep the OwnedFd alive for the lifetime of the task.
        let _wakeup_owner = wakeup_write;

        tracing::info!("Daemon reader task started — streaming screen updates");

        'outer: loop {
            while let Some(response) = response_rx.recv().await {
                match response {
                    Response::ScreenUpdate {
                        dirty_cells,
                        cursor,
                        mode,
                        ..
                    } => {
                        // Apply dirty cells incrementally to the local term.
                        let mut term = term_handle.lock();
                        let (screen_lines, screen_cols) = {
                            (term.screen_lines(), term.columns())
                        };

                        for dc in &dirty_cells {
                            let row = dc.row as usize;
                            let col = dc.col as usize;
                            if row >= screen_lines || col >= screen_cols {
                                continue;
                            }
                            let point =
                                Point::new(alacritty_terminal::index::Line(row as i32), Column(col));
                            let grid_cell = &mut term.grid_mut()[point];
                            grid_cell.c = dc.cell.ch;
                            grid_cell.flags = Flags::from_bits_truncate(dc.cell.flags);
                            grid_cell.fg = alacritty_terminal::vte::ansi::Color::Spec(
                                alacritty_terminal::vte::ansi::Rgb {
                                    r: dc.cell.fg.r,
                                    g: dc.cell.fg.g,
                                    b: dc.cell.fg.b,
                                },
                            );
                            grid_cell.bg = alacritty_terminal::vte::ansi::Color::Spec(
                                alacritty_terminal::vte::ansi::Rgb {
                                    r: dc.cell.bg.r,
                                    g: dc.cell.bg.g,
                                    b: dc.cell.bg.b,
                                },
                            );
                        }

                        // Update cursor position.
                        if cursor.visible {
                            term.grid_mut().cursor.point = Point::new(
                                alacritty_terminal::index::Line(cursor.row as i32),
                                Column(cursor.col as usize),
                            );
                        }

                        drop(term);

                        // Signal render loop: new content available.
                        synced_term_mode.store(mode, Ordering::Release);
                        force_full_redraw.store(true, Ordering::Release);
                        pty_dirty.store(true, Ordering::Release);
                        wake_render_loop(wakeup_write_fd);

                        tracing::trace!(
                            dirty = dirty_cells.len(),
                            "Applied incremental screen update from daemon"
                        );
                    }

                    Response::SessionState {
                        cols,
                        rows,
                        ref cells,
                        ref cursor,
                        mode,
                        ..
                    } => {
                        // Full redraw — the daemon sends this when damage is too
                        // large for an incremental update.
                        let mut term = term_handle.lock();

                        // Resize if needed.
                        let current_cols = term.columns();
                        let current_rows = term.screen_lines();
                        if current_cols != cols as usize || current_rows != rows as usize {
                            use crate::terminal::ConductorTerminalSize;
                            let size = ConductorTerminalSize::new(cols as usize, rows as usize);
                            term.resize(size);
                        }

                        // Apply all cells.
                        let safe_cols = term.columns();
                        for (i, cell_data) in cells.iter().enumerate() {
                            let row = i / (cols as usize);
                            let col = i % (cols as usize);
                            if row < rows as usize && col < safe_cols {
                                let point = Point::new(
                                    alacritty_terminal::index::Line(row as i32),
                                    Column(col),
                                );
                                let grid_cell = &mut term.grid_mut()[point];
                                grid_cell.c = cell_data.ch;
                                grid_cell.flags = Flags::from_bits_truncate(cell_data.flags);
                                grid_cell.fg = alacritty_terminal::vte::ansi::Color::Spec(
                                    alacritty_terminal::vte::ansi::Rgb {
                                        r: cell_data.fg.r,
                                        g: cell_data.fg.g,
                                        b: cell_data.fg.b,
                                    },
                                );
                                grid_cell.bg = alacritty_terminal::vte::ansi::Color::Spec(
                                    alacritty_terminal::vte::ansi::Rgb {
                                        r: cell_data.bg.r,
                                        g: cell_data.bg.g,
                                        b: cell_data.bg.b,
                                    },
                                );
                            }
                        }

                        // Position the cursor.
                        if cursor.visible {
                            term.grid_mut().cursor.point = Point::new(
                                alacritty_terminal::index::Line(cursor.row as i32),
                                Column(cursor.col as usize),
                            );
                        }

                        drop(term);

                        synced_term_mode.store(mode, Ordering::Release);
                        force_full_redraw.store(true, Ordering::Release);
                        pty_dirty.store(true, Ordering::Release);
                        wake_render_loop(wakeup_write_fd);

                        tracing::debug!(
                            cols,
                            rows,
                            cells = cells.len(),
                            "Applied full session state from daemon (streamed)"
                        );
                    }

                    Response::SessionExited {
                        id,
                        exit_code,
                        reason,
                    } => {
                        tracing::info!(
                            session_id = %id,
                            ?exit_code,
                            %reason,
                            "Daemon reports session exited"
                        );
                        exit_requested.store(true, Ordering::Release);
                        // Wake the render loop so it checks exit promptly.
                        wake_render_loop(wakeup_write_fd);
                        break 'outer;
                    }

                    Response::TitleChanged { title, .. } => {
                        tracing::debug!(title = %title, "Daemon title changed — forwarding to window");
                        if let Ok(mut guard) = pending_title.lock() {
                            *guard = Some(title);
                        }
                        // Wake the render loop so it picks up the title promptly.
                        wake_render_loop(wakeup_write_fd);
                    }

                    // Ignore request-response messages (Ok, Pong, Error, etc.)
                    // that may arrive before the stream settles.
                    other => {
                        tracing::trace!(?other, "Daemon reader: ignoring non-stream response");
                    }
                }
            }

            // The recv loop exited (returned None) — connection lost.
            // If we already received SessionExited, we're done.
            if exit_requested.load(Ordering::Acquire) {
                break 'outer;
            }

            // ── Reconnect loop ────────────────────────────────────────
            tracing::warn!("Connection lost, attempting reconnect...");

            // Visual indicator: set title to "Reconnecting..." so the user
            // sees the state in the title bar.
            if let Ok(mut guard) = pending_title.lock() {
                *guard = Some("thermal \u{2014} Reconnecting...".to_string());
            }
            wake_render_loop(wakeup_write_fd);

            let reconnected = {
                let mut client = shared_client.lock().await;
                match client.reconnect().await {
                    Ok(true) => {
                        tracing::info!("Reconnected to daemon, re-attaching to session {session_id}");
                        // Re-attach to the session. If the daemon restarted and
                        // the session no longer exists, this will fail.
                        match client.attach(&session_id, None).await {
                            Ok(attach_resp) => {
                                // Grab the fresh response stream.
                                let new_rx = client.take_response_rx();
                                Some((new_rx, Some(attach_resp)))
                            }
                            Err(e) => {
                                tracing::error!(
                                    session_id = %session_id,
                                    error = %e,
                                    "Re-attach failed — session may no longer exist after daemon restart"
                                );
                                None
                            }
                        }
                    }
                    Ok(false) => {
                        tracing::warn!("Daemon socket gone — cannot reconnect");
                        None
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Reconnect failed after all retries");
                        None
                    }
                }
            };

            match reconnected {
                Some((new_rx, attach_resp)) => {
                    response_rx = new_rx;

                    // If the attach returned a full SessionState, apply it so
                    // the terminal isn't stale after reconnect.
                    if let Some(ref resp @ Response::SessionState { mode, .. }) = attach_resp {
                        synced_term_mode.store(mode, Ordering::Release);
                        // Use the standalone apply helper with the term_handle.
                        apply_session_state_with_handle(&term_handle, resp);
                        force_full_redraw.store(true, Ordering::Release);
                        pty_dirty.store(true, Ordering::Release);
                    }

                    // Restore title to indicate success.
                    if let Ok(mut guard) = pending_title.lock() {
                        *guard = Some(format!(
                            "thermal \u{2014} session {}",
                            &session_id[..session_id.len().min(8)]
                        ));
                    }
                    wake_render_loop(wakeup_write_fd);

                    tracing::info!("Reconnect successful — resuming screen updates");
                    // Continue the outer loop, which will re-enter the recv loop
                    // with the new response_rx.
                }
                None => {
                    // Reconnect failed — exit cleanly.
                    tracing::warn!(
                        "Daemon reader: reconnect failed — signaling exit"
                    );
                    exit_requested.store(true, Ordering::Release);
                    wake_render_loop(wakeup_write_fd);
                    break 'outer;
                }
            }
        }

        tracing::info!("Daemon reader task exiting");
    });
}

/// Write a single byte to the wakeup pipe to unblock the poll() in the
/// render loop. Errors are silently ignored (pipe full is harmless).
pub(super) fn wake_render_loop(wakeup_write_fd: i32) {
    let _ = nix::unistd::write(
        unsafe { std::os::fd::BorrowedFd::borrow_raw(wakeup_write_fd) },
        &[1u8],
    );
}

/// Apply a `SessionState` response from the daemon to the local alacritty Term.
///
/// This paints the initial grid contents received on attach so the first
/// frame renders the correct terminal state.
pub(super) fn apply_session_state_to_term(terminal: &Terminal, response: &Response) {
    if let Response::SessionState {
        cols,
        rows,
        cells,
        cursor,
        ..
    } = response
    {
        let term_handle = terminal.term_handle();
        let mut term = term_handle.lock();

        // Resize the term to match the daemon's grid if needed.
        let current_cols = term.columns();
        let current_rows = term.screen_lines();
        if current_cols != *cols as usize || current_rows != *rows as usize {
            use crate::terminal::ConductorTerminalSize;
            let size = ConductorTerminalSize::new(*cols as usize, *rows as usize);
            term.resize(size);
        }

        // Apply cells to the grid.
        let safe_cols = term.columns();
        for (i, cell_data) in cells.iter().enumerate() {
            let row = i / (*cols as usize);
            let col = i % (*cols as usize);
            if row < *rows as usize && col < safe_cols {
                let point = Point::new(alacritty_terminal::index::Line(row as i32), Column(col));
                let grid_cell = &mut term.grid_mut()[point];
                grid_cell.c = cell_data.ch;
                grid_cell.flags = Flags::from_bits_truncate(cell_data.flags);
                // Map protocol colors to alacritty Color.
                grid_cell.fg = alacritty_terminal::vte::ansi::Color::Spec(
                    alacritty_terminal::vte::ansi::Rgb {
                        r: cell_data.fg.r,
                        g: cell_data.fg.g,
                        b: cell_data.fg.b,
                    },
                );
                grid_cell.bg = alacritty_terminal::vte::ansi::Color::Spec(
                    alacritty_terminal::vte::ansi::Rgb {
                        r: cell_data.bg.r,
                        g: cell_data.bg.g,
                        b: cell_data.bg.b,
                    },
                );
            }
        }

        // Position the cursor.
        if cursor.visible {
            term.grid_mut().cursor.point = Point::new(
                alacritty_terminal::index::Line(cursor.row as i32),
                Column(cursor.col as usize),
            );
        }

        tracing::debug!(
            cols,
            rows,
            cells = cells.len(),
            "Applied daemon session state to local term"
        );
    }
}

/// Like [`apply_session_state_to_term`] but takes a `FairMutex<Term>` handle
/// directly, for use inside the daemon reader task (which doesn't have a
/// `&Terminal` reference).
fn apply_session_state_with_handle(
    term_handle: &Arc<FairMutex<Term<ThermalEventListener>>>,
    response: &Response,
) {
    if let Response::SessionState {
        cols,
        rows,
        cells,
        cursor,
        ..
    } = response
    {
        let mut term = term_handle.lock();

        let current_cols = term.columns();
        let current_rows = term.screen_lines();
        if current_cols != *cols as usize || current_rows != *rows as usize {
            use crate::terminal::ConductorTerminalSize;
            let size = ConductorTerminalSize::new(*cols as usize, *rows as usize);
            term.resize(size);
        }

        let safe_cols = term.columns();
        for (i, cell_data) in cells.iter().enumerate() {
            let row = i / (*cols as usize);
            let col = i % (*cols as usize);
            if row < *rows as usize && col < safe_cols {
                let point = Point::new(alacritty_terminal::index::Line(row as i32), Column(col));
                let grid_cell = &mut term.grid_mut()[point];
                grid_cell.c = cell_data.ch;
                grid_cell.flags = Flags::from_bits_truncate(cell_data.flags);
                grid_cell.fg = alacritty_terminal::vte::ansi::Color::Spec(
                    alacritty_terminal::vte::ansi::Rgb {
                        r: cell_data.fg.r,
                        g: cell_data.fg.g,
                        b: cell_data.fg.b,
                    },
                );
                grid_cell.bg = alacritty_terminal::vte::ansi::Color::Spec(
                    alacritty_terminal::vte::ansi::Rgb {
                        r: cell_data.bg.r,
                        g: cell_data.bg.g,
                        b: cell_data.bg.b,
                    },
                );
            }
        }

        if cursor.visible {
            term.grid_mut().cursor.point = Point::new(
                alacritty_terminal::index::Line(cursor.row as i32),
                Column(cursor.col as usize),
            );
        }

        tracing::debug!(
            cols,
            rows,
            cells = cells.len(),
            "Applied daemon session state to local term (reconnect)"
        );
    }
}
