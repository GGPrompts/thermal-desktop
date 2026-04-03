//! SCTK KeyboardHandler and PointerHandler trait impls for ConductorWindow.

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::TermMode;
use smithay_client_toolkit::seat::{
    keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
    pointer::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, PointerEvent, PointerEventKind, PointerHandler},
};
use wayland_client::{
    Connection, QueueHandle,
    protocol::{wl_keyboard, wl_pointer, wl_surface},
};

use crate::agent_graph::GRAPH_OVERLAY_HEIGHT;
use crate::agent_timeline::TIMELINE_BAR_HEIGHT;
use crate::input;

use super::ConductorWindow;

// ── Keyboard handler ──────────────────────────────────────────────────────────

impl KeyboardHandler for ConductorWindow {
    fn enter(
        &mut self,
        _: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _keysyms: &[Keysym],
    ) {
        // DECSET 1004 focus reporting: send CSI I (focus in)
        if self.current_term_mode().contains(TermMode::FOCUS_IN_OUT) {
            self.write_session(b"\x1b[I");
        }
        self.record_focus_in();
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        // DECSET 1004 focus reporting: send CSI O (focus out)
        if self.current_term_mode().contains(TermMode::FOCUS_IN_OUT) {
            self.write_session(b"\x1b[O");
        }
        self.record_focus_out();
        self.reset_keyboard_state();
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        // ── Modal overlay input capture ────────────────────────────────
        // When a modal widget (e.g. permission dialog) is on the focus
        // stack, it captures ALL keyboard input. Window-level shortcuts
        // (Ctrl+Shift+Q) still work — modal only intercepts after those.
        if self.overlay.has_modal() {
            // For now, 'y'/'n'/Escape dismiss the modal.
            // A full implementation would route to the widget's handler.
            match event.keysym {
                Keysym::y | Keysym::Y | Keysym::n | Keysym::N | Keysym::Escape => {
                    if let Some(widget) = self.overlay.pop_modal() {
                        tracing::info!(
                            id = widget.id,
                            key = ?event.keysym,
                            "modal widget dismissed by keypress"
                        );
                    }
                    self.dirty = true;
                }
                _ => {
                    // Swallow all other input while modal is active.
                    tracing::trace!(key = ?event.keysym, "input swallowed by modal overlay");
                }
            }
            return;
        }

        // ── Window close: Ctrl+Shift+Q ─────────────────────────────────
        if self.modifiers.ctrl
            && self.modifiers.shift
            && matches!(event.keysym, Keysym::Q | Keysym::q)
        {
            tracing::info!("Ctrl+Shift+Q: closing window");
            self.exit = true;
            return;
        }

        // ── Clipboard: Ctrl+Shift+C (copy) / Ctrl+Shift+V (paste) ──────
        if self.modifiers.ctrl && self.modifiers.shift {
            match event.keysym {
                Keysym::C | Keysym::c => {
                    self.clipboard_copy();
                    self.dirty = true;
                    return;
                }
                Keysym::V | Keysym::v => {
                    self.clipboard_paste();
                    self.dirty = true;
                    return;
                }
                _ => {}
            }
        }

        // ── Agent timeline toggle: Ctrl+Shift+T ────────────────────────
        if self.modifiers.ctrl
            && self.modifiers.shift
            && matches!(event.keysym, Keysym::T | Keysym::t)
        {
            self.agent_timeline.toggle();
            // Recalculate terminal grid to account for the timeline bar and graph.
            let mut effective_h = self.height;
            if self.agent_timeline.visible {
                effective_h = effective_h.saturating_sub(TIMELINE_BAR_HEIGHT);
            }
            if self.agent_graph.visible {
                effective_h = effective_h.saturating_sub(GRAPH_OVERLAY_HEIGHT);
            }
            let (cols, rows) = self.grid_renderer.grid_size(self.width, effective_h);
            self.terminal.resize(
                cols,
                rows,
                self.grid_renderer.cell_width as u16,
                self.grid_renderer.cell_height as u16,
            );
            self.resize_session(cols as u16, rows as u16);
            self.dirty = true;
            return;
        }

        // ── Agent graph toggle: F3 ───────────────────────────────────────
        if matches!(event.keysym, Keysym::F3) {
            self.agent_graph.toggle();
            // Recalculate terminal grid to account for the graph overlay.
            let effective_h = if self.agent_graph.visible {
                self.height.saturating_sub(GRAPH_OVERLAY_HEIGHT)
            } else {
                self.height
            };
            // Also account for timeline if it's visible.
            let effective_h = if self.agent_timeline.visible {
                effective_h.saturating_sub(TIMELINE_BAR_HEIGHT)
            } else {
                effective_h
            };
            let (cols, rows) = self.grid_renderer.grid_size(self.width, effective_h);
            self.terminal.resize(
                cols,
                rows,
                self.grid_renderer.cell_width as u16,
                self.grid_renderer.cell_height as u16,
            );
            self.resize_session(cols as u16, rows as u16);
            self.dirty = true;
            return;
        }

        // ── Cross-pane inject: Ctrl+Shift+Enter ─────────────────────────
        // Sends the current selection to all other thermal-conductor windows.
        if self.modifiers.ctrl
            && self.modifiers.shift
            && matches!(event.keysym, Keysym::Return | Keysym::KP_Enter)
        {
            self.inject_selection();
            return;
        }

        // ── Context continuation: Ctrl+Shift+N ──────────────────────────
        // Spawns a new continuation session when the context window is saturated.
        if self.modifiers.ctrl
            && self.modifiers.shift
            && matches!(event.keysym, Keysym::N | Keysym::n)
        {
            self.spawn_continuation();
            return;
        }

        // ── Font size: Ctrl+Plus (increase), Ctrl+Minus (decrease), Ctrl+0 (reset)
        if self.modifiers.ctrl && !self.modifiers.shift {
            let font_changed = match event.keysym {
                Keysym::plus | Keysym::equal | Keysym::KP_Add => {
                    self.grid_renderer.font_config.increase()
                }
                Keysym::minus | Keysym::KP_Subtract => self.grid_renderer.font_config.decrease(),
                Keysym::_0 | Keysym::KP_0 => self.grid_renderer.font_config.reset(),
                _ => false,
            };
            if font_changed {
                tracing::info!(
                    font_size = self.grid_renderer.font_config.font_size,
                    "Font size changed"
                );
                self.grid_renderer.update_font_metrics();
                // Recalculate terminal grid dimensions for the new cell size.
                let mut effective_h = self.height;
                if self.agent_timeline.visible {
                    effective_h = effective_h.saturating_sub(TIMELINE_BAR_HEIGHT);
                }
                if self.agent_graph.visible {
                    effective_h = effective_h.saturating_sub(GRAPH_OVERLAY_HEIGHT);
                }
                let (cols, rows) = self.grid_renderer.grid_size(self.width, effective_h);
                self.terminal.resize(
                    cols,
                    rows,
                    self.grid_renderer.cell_width as u16,
                    self.grid_renderer.cell_height as u16,
                );
                self.resize_session(cols as u16, rows as u16);
                self.dirty = true;
                return;
            }
        }

        // ── Scrollback navigation (Shift+PageUp/Down/Home/End) ──────────
        // These are intercepted BEFORE encode_key so they never reach the PTY.
        if self.modifiers.shift {
            let scroll = match event.keysym {
                Keysym::Page_Up => Some(Scroll::PageUp),
                Keysym::Page_Down => Some(Scroll::PageDown),
                Keysym::Home => Some(Scroll::Top),
                Keysym::End => Some(Scroll::Bottom),
                _ => None,
            };
            if let Some(scroll) = scroll {
                let term_handle = self.terminal.term_handle();
                let mut term = term_handle.lock();
                term.scroll_display(scroll);
                self.dirty = true;
                return;
            }
        }

        // Encode the key press into bytes and send to the session.
        // Uses kitty keyboard protocol when the terminal has it enabled.
        if let Some(bytes) = self.encode_key_event(&event, input::KeyEventType::Press) {
            self.write_session(&bytes);
        }

        // Start key repeat for this key. Modifier-only keys don't repeat.
        if self
            .encode_key_event(&event, input::KeyEventType::Press)
            .is_some()
        {
            self.repeat_key = Some(event);
            self.repeat_next = Some(std::time::Instant::now() + self.repeat_delay);
        }

        // Don't set dirty here — the PTY echo will set pty_dirty and
        // trigger a render when the shell response arrives, avoiding an
        // unnecessary extra GPU frame on every keypress.
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        // Stop key repeat when any key is released.
        self.repeat_key = None;
        self.repeat_next = None;

        // When kitty keyboard protocol with REPORT_EVENTS is active, send
        // a release event to the terminal application.
        let flags = self.kitty_flags();
        if flags.contains(input::KittyFlags::REPORT_EVENTS) {
            if let Some(bytes) = input::encode_key_kitty(
                &event,
                &self.modifiers,
                flags,
                input::KeyEventType::Release,
            ) {
                self.write_session(&bytes);
            }
        }
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _layout: u32,
    ) {
        self.modifiers = modifiers;
    }
}

// ── Pointer handler (mouse selection + primary paste) ─────────────────────────

impl PointerHandler for ConductorWindow {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        // Check if the terminal program wants mouse events (SGR mouse mode).
        let mode = self.current_term_mode();
        let mouse_mode = mode.contains(TermMode::MOUSE_REPORT_CLICK)
            || mode.contains(TermMode::MOUSE_DRAG)
            || mode.contains(TermMode::MOUSE_MOTION);

        for event in events {
            // Set the cursor shape on pointer enter — the Wayland protocol
            // requires re-setting the cursor on every enter event.
            if let PointerEventKind::Enter { serial } = event.kind {
                self.pointer_enter_serial = serial;
                if let Some(ref device) = self.cursor_shape_device {
                    device.set_shape(serial, super::CursorShape::Text);
                }
            }

            if let PointerEventKind::Leave { .. } = event.kind {
                self.reset_pointer_state();
                if let Some(ref device) = self.cursor_shape_device {
                    device.set_shape(self.pointer_enter_serial, super::CursorShape::Default);
                }
            }

            let (px, py) = event.position;
            let (col, line, _side) = self.pixel_to_grid(px, py);
            let cx = col.0 + 1; // SGR is 1-based
            let cy = line.0 + 1;

            // ── Ctrl+Click: open hyperlink ──────────────────────────────
            // Intercept Ctrl+Left-Click before mouse mode or selection
            // handling. If the clicked cell has a hyperlink, open it via
            // xdg-open and consume the event (matching kitty behavior).
            if let PointerEventKind::Press { button, .. } = event.kind {
                if button == BTN_LEFT && self.modifiers.ctrl {
                    let row = line.0 as usize;
                    let col_idx = col.0;
                    if let Some(url) = self.hyperlink_at(row, col_idx) {
                        self.open_url(&url);
                        continue; // Consume the click — don't start selection or forward SGR
                    }
                }
            }

            if mouse_mode {
                // Forward mouse events to session as SGR escape sequences.
                // Format: \x1b[<btn;col;row M (press) or m (release)
                let sgr = match event.kind {
                    PointerEventKind::Press { button, .. } => {
                        let btn = match button {
                            BTN_LEFT => 0,
                            BTN_MIDDLE => 1,
                            BTN_RIGHT => 2,
                            _ => continue,
                        };
                        Some(format!("\x1b[<{btn};{cx};{cy}M"))
                    }
                    PointerEventKind::Release { button, .. } => {
                        let btn = match button {
                            BTN_LEFT => 0,
                            BTN_MIDDLE => 1,
                            BTN_RIGHT => 2,
                            _ => continue,
                        };
                        Some(format!("\x1b[<{btn};{cx};{cy}m"))
                    }
                    PointerEventKind::Motion { .. } => {
                        // Motion reporting (mode 1003) or drag (1002 + button held)
                        if self.mouse_left_held {
                            Some(format!("\x1b[<32;{cx};{cy}M"))
                        } else {
                            if mode.contains(TermMode::MOUSE_MOTION) {
                                Some(format!("\x1b[<35;{cx};{cy}M"))
                            } else {
                                None
                            }
                        }
                    }
                    PointerEventKind::Axis { vertical, .. } => {
                        // Scroll: button 64 (up) / 65 (down) in SGR mode.
                        let btn = if vertical.discrete > 0 { 65 } else { 64 };
                        let steps = vertical.discrete.unsigned_abs().max(1);
                        let mut seq = String::new();
                        for _ in 0..steps {
                            seq.push_str(&format!("\x1b[<{btn};{cx};{cy}M"));
                        }
                        Some(seq)
                    }
                    _ => None,
                };

                if let Some(seq) = sgr {
                    self.write_session(seq.as_bytes());
                    self.dirty = true;
                }

                // Track left button state for drag reporting.
                match event.kind {
                    PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                        self.mouse_left_held = true;
                    }
                    PointerEventKind::Release { button, .. } if button == BTN_LEFT => {
                        self.mouse_left_held = false;
                    }
                    _ => {}
                }
            } else {
                // No mouse mode — use mouse for selection and scroll.
                match event.kind {
                    PointerEventKind::Press { button, .. } => {
                        if button == BTN_LEFT {
                            self.selection_start(col, line, _side);
                            self.mouse_left_held = true;
                            self.dirty = true;
                        } else if button == BTN_MIDDLE {
                            self.primary_paste();
                            self.dirty = true;
                        }
                    }
                    PointerEventKind::Release { button, .. } => {
                        if button == BTN_LEFT {
                            self.mouse_left_held = false;
                            self.selection_finalize();
                            self.dirty = true;
                        }
                    }
                    PointerEventKind::Motion { .. } => {
                        if self.mouse_left_held {
                            self.selection_update(col, line, _side);
                            self.dirty = true;
                        }
                    }
                    PointerEventKind::Axis { vertical, .. } => {
                        // Scroll the terminal scrollback when not in mouse mode.
                        let th = self.terminal.term_handle();
                        let mut t = th.lock();
                        if vertical.discrete > 0 {
                            t.scroll_display(Scroll::Delta(-3));
                        } else if vertical.discrete < 0 {
                            t.scroll_display(Scroll::Delta(3));
                        }
                        self.dirty = true;
                    }
                    _ => {}
                }
            }
        }
    }
}
