//! Input routing for thermal-conductor.
//!
//! Translates winit keyboard/mouse events into `InputAction` commands that the
//! main loop can dispatch to the appropriate pane.

use winit::event::{ElementState, MouseButton, WindowEvent};

/// High-level input actions produced by `InputHandler::handle`.
#[derive(Debug)]
#[allow(dead_code)]
pub enum InputAction {
    /// Forward a key sequence to a specific pane via `tmux send-keys`.
    SendKeys { pane_idx: usize, keys: String },
    /// Move keyboard focus to a different pane.
    FocusPane { idx: usize },
    /// Scroll pane content by `lines` (positive = down, negative = up).
    Scroll { pane_idx: usize, lines: i32 },
    /// Quit the application.
    Quit,
}

/// Converts winit `WindowEvent`s into `InputAction`s.
#[allow(dead_code)]
pub struct InputHandler {
    /// Which pane currently has keyboard focus.
    pub focused_pane: usize,
    /// Current scrollback offset for the focused pane (lines from bottom).
    pub scrollback_offset: i32,
    /// Last known cursor position in physical pixels.
    cursor_pos: (f32, f32),
}

#[allow(dead_code)]
impl InputHandler {
    pub fn new() -> Self {
        Self {
            focused_pane: 0,
            scrollback_offset: 0,
            cursor_pos: (0.0, 0.0),
        }
    }

    /// Handle a `WindowEvent`. Returns `Some(InputAction)` if the event maps
    /// to something the main loop should act on, or `None` to ignore it.
    pub fn handle(&mut self, event: &WindowEvent) -> Option<InputAction> {
        match event {
            // ── Keyboard ─────────────────────────────────────────────────────
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return None;
                }

                use winit::keyboard::{Key, NamedKey};
                match &event.logical_key {
                    // Note: Escape is NOT quit — it's used by terminal apps (vim, readline, etc.).
                    // Ctrl-Q (winit sends \x11, ASCII 17) quits thermal-conductor.
                    Key::Character(c) if c.as_str() == "\x11" => Some(InputAction::Quit),
                    Key::Named(NamedKey::Enter) => Some(InputAction::SendKeys {
                        pane_idx: self.focused_pane,
                        keys: "\r".to_owned(),
                    }),
                    Key::Named(NamedKey::Backspace) => Some(InputAction::SendKeys {
                        pane_idx: self.focused_pane,
                        keys: "\x7f".to_owned(),
                    }),
                    Key::Named(NamedKey::Tab) => Some(InputAction::SendKeys {
                        pane_idx: self.focused_pane,
                        keys: "\t".to_owned(),
                    }),
                    Key::Named(NamedKey::ArrowUp) => Some(InputAction::SendKeys {
                        pane_idx: self.focused_pane,
                        keys: "\x1b[A".to_owned(),
                    }),
                    Key::Named(NamedKey::ArrowDown) => Some(InputAction::SendKeys {
                        pane_idx: self.focused_pane,
                        keys: "\x1b[B".to_owned(),
                    }),
                    Key::Named(NamedKey::ArrowRight) => Some(InputAction::SendKeys {
                        pane_idx: self.focused_pane,
                        keys: "\x1b[C".to_owned(),
                    }),
                    Key::Named(NamedKey::ArrowLeft) => Some(InputAction::SendKeys {
                        pane_idx: self.focused_pane,
                        keys: "\x1b[D".to_owned(),
                    }),
                    Key::Character(text) => Some(InputAction::SendKeys {
                        pane_idx: self.focused_pane,
                        keys: text.to_string(),
                    }),
                    _ => None,
                }
            }

            // ── Mouse cursor tracking ─────────────────────────────────────────
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x as f32, position.y as f32);
                None
            }

            // ── Mouse click — focus pane ──────────────────────────────────────
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => Some(InputAction::FocusPane {
                idx: self.focused_pane, // caller updates via LayoutEngine::pane_at
            }),

            // ── Scroll wheel ─────────────────────────────────────────────────
            WindowEvent::MouseWheel { delta, .. } => {
                use winit::event::MouseScrollDelta;
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => {
                        if *y > 0.0 { -3 } else { 3 }
                    }
                    MouseScrollDelta::PixelDelta(pos) => {
                        if pos.y > 0.0 { -3 } else { 3 }
                    }
                };
                Some(InputAction::Scroll {
                    pane_idx: self.focused_pane,
                    lines,
                })
            }

            _ => None,
        }
    }

    /// Returns the current cursor position in physical pixels.
    pub fn cursor_pos(&self) -> (f32, f32) {
        self.cursor_pos
    }

    /// Handle a mouse click by querying the layout engine for which pane was
    /// clicked, then updating `focused_pane` and the layout.
    ///
    /// Returns a `FocusPane` action with the correct pane index, or `None` if
    /// the click did not land on any pane.
    pub fn handle_click(
        &mut self,
        layout: &mut crate::layout::LayoutEngine,
    ) -> Option<InputAction> {
        let (x, y) = self.cursor_pos;
        if let Some(idx) = layout.pane_at(x, y) {
            layout.set_focused(idx);
            self.focused_pane = idx;
            Some(InputAction::FocusPane { idx })
        } else {
            None
        }
    }
}

impl Default for InputHandler {
    fn default() -> Self {
        Self::new()
    }
}
