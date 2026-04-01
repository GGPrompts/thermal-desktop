//! Clipboard, selection, and hyperlink handling for ConductorWindow.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;

use super::ConductorWindow;

/// Strip ANSI escape sequences from clipboard data to prevent terminal injection
/// when pasting without bracketed paste mode. Preserves normal text including
/// tabs, newlines, and carriage returns.
fn sanitize_paste(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            // ESC — skip the entire escape sequence
            0x1b => {
                i += 1;
                if i >= input.len() {
                    break;
                }
                match input[i] {
                    // CSI sequence: ESC [ ... final_byte
                    b'[' => {
                        i += 1;
                        while i < input.len() && !(0x40..=0x7E).contains(&input[i]) {
                            i += 1;
                        }
                        if i < input.len() {
                            i += 1; // skip final byte
                        }
                    }
                    // OSC sequence: ESC ] ... ST (ESC \ or BEL)
                    b']' => {
                        i += 1;
                        while i < input.len() {
                            if input[i] == 0x07 {
                                i += 1;
                                break;
                            }
                            if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'\\' {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    // Two-character sequence (e.g., ESC c for RIS)
                    _ => {
                        i += 1;
                    }
                }
            }
            // Allow tab, newline, carriage return
            b'\t' | b'\n' | b'\r' => {
                out.push(input[i]);
                i += 1;
            }
            // Strip other C0 control chars (0x00-0x1F except the above, and 0x7F)
            c if c < 0x20 || c == 0x7F => {
                i += 1;
            }
            // Pass through everything else (printable ASCII + UTF-8)
            _ => {
                out.push(input[i]);
                i += 1;
            }
        }
    }
    out
}

impl ConductorWindow {
    /// Copy the current terminal selection to the Wayland clipboard via `wl-copy`.
    pub(super) fn clipboard_copy(&self) {
        let term_handle = self.terminal.term_handle();
        let term = term_handle.lock();
        let text = term.selection_to_string();
        drop(term);

        if let Some(text) = text {
            if text.is_empty() {
                tracing::debug!("Clipboard copy: selection is empty");
                return;
            }
            // Shell out to wl-copy for clipboard access.
            match std::process::Command::new("wl-copy")
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                Ok(mut child) => {
                    if let Some(ref mut stdin) = child.stdin {
                        use std::io::Write;
                        let _ = stdin.write_all(text.as_bytes());
                    }
                    let _ = child.wait();
                    tracing::debug!(len = text.len(), "Clipboard copy: text sent to wl-copy");
                }
                Err(e) => {
                    tracing::warn!("Failed to run wl-copy: {} (is wl-clipboard installed?)", e);
                }
            }
        } else {
            tracing::debug!("Clipboard copy: no selection");
        }
    }

    // ── Mouse selection helpers ──────────────────────────────────────────────

    /// Convert pixel coordinates to terminal grid position (col, line, side).
    ///
    /// The `side` indicates whether the click was on the left or right half
    /// of the cell, which alacritty_terminal uses for precise selection edges.
    pub(super) fn pixel_to_grid(&self, px: f64, py: f64) -> (Column, Line, Side) {
        let padding_x = self.grid_renderer.padding_x();
        let padding_y = self.grid_renderer.padding_y();
        let cell_w = self.grid_renderer.cell_width as f64;
        let cell_h = self.grid_renderer.cell_height as f64;

        let x = (px - padding_x as f64).max(0.0);
        let y = (py - padding_y as f64).max(0.0);

        let col = (x / cell_w) as usize;
        let row = (y / cell_h) as i32;

        // Determine which side of the cell the click is on.
        let cell_x_offset = x - (col as f64 * cell_w);
        let side = if cell_x_offset < cell_w / 2.0 {
            Side::Left
        } else {
            Side::Right
        };

        // Clamp to grid bounds.
        let term_handle = self.terminal.term_handle();
        let term = term_handle.lock();
        let max_col = term.columns().saturating_sub(1);
        let max_row = term.screen_lines() as i32 - 1;
        drop(term);

        let col = col.min(max_col);
        let row = row.min(max_row);

        (Column(col), Line(row), side)
    }

    /// Start a new text selection at the given grid position.
    pub(super) fn selection_start(&mut self, col: Column, line: Line, side: Side) {
        let point = Point::new(line, col);
        let selection = Selection::new(SelectionType::Simple, point, side);
        let term_handle = self.terminal.term_handle();
        let mut term = term_handle.lock();
        term.selection = Some(selection);
        tracing::debug!(?point, ?side, "Selection started");
    }

    /// Update the end point of an in-progress selection.
    pub(super) fn selection_update(&mut self, col: Column, line: Line, side: Side) {
        let point = Point::new(line, col);
        let term_handle = self.terminal.term_handle();
        let mut term = term_handle.lock();
        if let Some(ref mut sel) = term.selection {
            sel.update(point, side);
        }
    }

    /// Finalize the selection: extract text and set primary selection via wl-copy.
    pub(super) fn selection_finalize(&self) {
        let term_handle = self.terminal.term_handle();
        let term = term_handle.lock();
        let text = term.selection_to_string();
        drop(term);

        if let Some(ref text) = text {
            if text.is_empty() {
                return;
            }
            // Set primary selection via wl-copy --primary.
            match std::process::Command::new("wl-copy")
                .arg("--primary")
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                Ok(mut child) => {
                    if let Some(ref mut stdin) = child.stdin {
                        use std::io::Write;
                        let _ = stdin.write_all(text.as_bytes());
                    }
                    let _ = child.wait();
                    tracing::debug!(
                        len = text.len(),
                        "Primary selection set via wl-copy --primary"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to run wl-copy --primary: {} (is wl-clipboard installed?)",
                        e
                    );
                }
            }
        }
    }

    /// Clear any active selection.
    #[allow(dead_code)]
    pub(super) fn selection_clear(&self) {
        let term_handle = self.terminal.term_handle();
        let mut term = term_handle.lock();
        term.selection = None;
    }

    // ── Hyperlink click handling ────────────────────────────────────────────

    /// Look up the hyperlink URL at the given grid position.
    /// Returns `Some(url)` if the cell has an associated hyperlink.
    pub(super) fn hyperlink_at(&self, row: usize, col: usize) -> Option<String> {
        self.grid_renderer.hyperlink_map.get(&(row, col)).cloned()
    }

    /// Open a URL via `xdg-open` (spawned detached, non-blocking).
    /// Only http:// and https:// URIs are allowed — other schemes (file://,
    /// javascript:, data:, etc.) are rejected to prevent local file access
    /// and code execution via crafted terminal hyperlinks.
    pub(super) fn open_url(&self, url: &str) {
        // Validate URI scheme — only allow http(s)
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            tracing::warn!(url, "Rejected hyperlink with disallowed URI scheme");
            return;
        }

        tracing::info!(url, "Opening hyperlink via xdg-open");
        match std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => {
                tracing::debug!(url, "xdg-open spawned successfully");
            }
            Err(e) => {
                tracing::warn!(url, error = %e, "Failed to spawn xdg-open");
            }
        }
    }

    /// Paste from the primary selection (middle-click) into the session.
    pub(super) fn primary_paste(&self) {
        let output = match std::process::Command::new("wl-paste")
            .arg("--primary")
            .arg("--no-newline")
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    "Failed to run wl-paste --primary: {} (is wl-clipboard installed?)",
                    e
                );
                return;
            }
        };

        if !output.status.success() {
            tracing::debug!(
                "wl-paste --primary returned non-zero (primary selection may be empty)"
            );
            return;
        }

        let text = &output.stdout;
        if text.is_empty() {
            return;
        }

        // Check if the terminal has bracketed paste mode enabled.
        let bracketed = self.current_term_mode().contains(TermMode::BRACKETED_PASTE);

        if bracketed {
            let mut payload = Vec::with_capacity(text.len() + 12);
            payload.extend_from_slice(b"\x1b[200~");
            payload.extend_from_slice(text);
            payload.extend_from_slice(b"\x1b[201~");
            self.write_session(&payload);
        } else {
            // Sanitize control sequences to prevent terminal injection.
            let sanitized = sanitize_paste(text);
            self.write_session(&sanitized);
        }

        tracing::debug!(
            len = text.len(),
            bracketed,
            "Primary paste: sent to session"
        );
    }

    /// Paste from the Wayland clipboard into the session, with bracketed paste
    /// support when the terminal has DECSET 2004 enabled.
    pub(super) fn clipboard_paste(&self) {
        // Read clipboard contents via wl-paste.
        let output = match std::process::Command::new("wl-paste")
            .arg("--no-newline")
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("Failed to run wl-paste: {} (is wl-clipboard installed?)", e);
                return;
            }
        };

        if !output.status.success() {
            tracing::debug!("wl-paste returned non-zero (clipboard may be empty)");
            return;
        }

        let text = &output.stdout;
        if text.is_empty() {
            return;
        }

        // Check if the terminal has bracketed paste mode enabled (DECSET 2004).
        let bracketed = self.current_term_mode().contains(TermMode::BRACKETED_PASTE);

        if bracketed {
            // Wrap paste in bracketed paste escape sequences:
            //   \x1b[200~ ... \x1b[201~
            let mut payload = Vec::with_capacity(text.len() + 12);
            payload.extend_from_slice(b"\x1b[200~");
            payload.extend_from_slice(text);
            payload.extend_from_slice(b"\x1b[201~");
            self.write_session(&payload);
        } else {
            // Sanitize control sequences to prevent terminal injection.
            let sanitized = sanitize_paste(text);
            self.write_session(&sanitized);
        }

        tracing::debug!(
            len = text.len(),
            bracketed,
            "Clipboard paste: sent to session"
        );
    }
}
