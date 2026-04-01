//! Key and mouse input handling for the sessions page.

use ratatui::layout::Rect;
use thermal_core::ClaudeStatePoller;

use super::{FocusedPanel, SessionsPage};

impl SessionsPage {
    /// Handle a key event. Return `true` if the app should quit.
    pub(super) fn handle_key_sessions(
        &mut self,
        key: crossterm::event::KeyEvent,
        poller: &mut ClaudeStatePoller,
    ) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // History popup intercepts all keys.
        if self.history_popup.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('h') => self.dismiss_history(),
                _ => {}
            }
            return false;
        }

        // Chat input mode.
        if self.focused_panel == FocusedPanel::Chat {
            // Autocomplete navigation intercepts certain keys.
            if self.autocomplete_active && !self.autocomplete_items.is_empty() {
                match key.code {
                    KeyCode::Tab | KeyCode::Down => {
                        self.autocomplete_index =
                            (self.autocomplete_index + 1) % self.autocomplete_items.len();
                        return false;
                    }
                    KeyCode::Up => {
                        self.autocomplete_index = if self.autocomplete_index == 0 {
                            self.autocomplete_items.len() - 1
                        } else {
                            self.autocomplete_index - 1
                        };
                        return false;
                    }
                    KeyCode::Enter => {
                        self.accept_autocomplete();
                        return false;
                    }
                    KeyCode::Esc => {
                        self.autocomplete_active = false;
                        return false;
                    }
                    _ => {}
                }
            }

            match key.code {
                KeyCode::Esc => {
                    self.focused_panel = FocusedPanel::AgentList;
                    self.autocomplete_active = false;
                }
                KeyCode::Enter => {
                    self.send_chat_input();
                    self.autocomplete_active = false;
                }
                KeyCode::Backspace => {
                    self.chat_handle_backspace();
                    self.update_autocomplete();
                }
                KeyCode::Left => {
                    if self.chat_cursor > 0 {
                        self.chat_cursor = self.chat_input[..self.chat_cursor]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                    }
                    self.update_autocomplete();
                }
                KeyCode::Right => {
                    if self.chat_cursor < self.chat_input.len() {
                        self.chat_cursor = self.chat_input[self.chat_cursor..]
                            .char_indices()
                            .nth(1)
                            .map(|(i, _)| self.chat_cursor + i)
                            .unwrap_or(self.chat_input.len());
                    }
                    self.update_autocomplete();
                }
                KeyCode::Up => {
                    if !self.chat_history.is_empty() {
                        match self.chat_history_index {
                            None => {
                                self.chat_saved_input = self.chat_input.clone();
                                self.chat_history_index = Some(0);
                                self.chat_input = self.chat_history[0].clone();
                            }
                            Some(idx) if idx < self.chat_history.len() - 1 => {
                                let new_idx = idx + 1;
                                self.chat_history_index = Some(new_idx);
                                self.chat_input = self.chat_history[new_idx].clone();
                            }
                            _ => {}
                        }
                        self.chat_cursor = self.chat_input.len();
                    }
                    self.autocomplete_active = false;
                }
                KeyCode::Down => {
                    match self.chat_history_index {
                        Some(0) => {
                            self.chat_history_index = None;
                            self.chat_input = self.chat_saved_input.clone();
                        }
                        Some(idx) if idx > 0 => {
                            let new_idx = idx - 1;
                            self.chat_history_index = Some(new_idx);
                            self.chat_input = self.chat_history[new_idx].clone();
                        }
                        _ => {}
                    }
                    self.chat_cursor = self.chat_input.len();
                    self.autocomplete_active = false;
                }
                KeyCode::Home => self.chat_cursor = 0,
                KeyCode::End => self.chat_cursor = self.chat_input.len(),
                KeyCode::Char(ch) => {
                    self.chat_handle_char(ch);
                    self.update_autocomplete();
                }
                _ => {}
            }
            return false;
        }

        // Preview panel mode.
        if self.focused_panel == FocusedPanel::Preview {
            match key.code {
                KeyCode::Esc => {
                    self.focused_panel = FocusedPanel::AgentList;
                }
                KeyCode::Tab => {
                    self.focused_panel = self.focused_panel.next();
                }
                KeyCode::BackTab => {
                    self.focused_panel = self.focused_panel.prev();
                }
                KeyCode::PageUp => {
                    self.preview_scroll = self.preview_scroll.saturating_sub(10).max(1);
                    self.preview_pinned = true;
                }
                KeyCode::PageDown => {
                    self.preview_scroll =
                        (self.preview_scroll + 10).min(self.preview_content.len());
                    self.preview_pinned = self.preview_scroll < self.preview_content.len();
                }
                KeyCode::Home => {
                    self.preview_scroll = 1;
                    self.preview_pinned = true;
                }
                KeyCode::End => {
                    self.preview_scroll = self.preview_content.len();
                    self.preview_pinned = false;
                }
                _ => {}
            }
            return false;
        }

        // Normal navigation mode (AgentList focused).

        if ctrl && key.code == KeyCode::Char('a') {
            self.select_all();
            return false;
        }
        if ctrl && key.code == KeyCode::Char('d') {
            self.deselect_all();
            return false;
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.nav_down();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.nav_up();
            }
            KeyCode::Char('f') => {
                self.focus_selected_window();
            }
            KeyCode::Char(' ') => self.toggle_select_current(),
            KeyCode::Enter => self.attach_selected(),
            KeyCode::Char('/') => {
                self.focused_panel = FocusedPanel::Chat;
            }
            KeyCode::Tab => {
                self.focused_panel = self.focused_panel.next();
            }
            KeyCode::BackTab => {
                self.focused_panel = self.focused_panel.prev();
            }
            KeyCode::Char('h') => self.toggle_history(),
            KeyCode::Char('r') => self.force_refresh(poller),
            KeyCode::Char('s') => self.save_session_as_profile(),
            KeyCode::PageUp => {
                self.preview_scroll = self.preview_scroll.saturating_sub(10).max(1);
                self.preview_pinned = true;
            }
            KeyCode::PageDown => {
                self.preview_scroll = (self.preview_scroll + 10).min(self.preview_content.len());
                self.preview_pinned = self.preview_scroll < self.preview_content.len();
            }
            KeyCode::Home => {
                self.preview_scroll = 1;
                self.preview_pinned = true;
            }
            KeyCode::End => {
                self.preview_scroll = self.preview_content.len();
                self.preview_pinned = false;
            }
            _ => {}
        }
        false
    }

    /// Handle a mouse event.
    pub(super) fn handle_mouse_sessions(
        &mut self,
        event: crossterm::event::MouseEvent,
        _poller: &mut ClaudeStatePoller,
    ) {
        use crossterm::event::{MouseButton, MouseEventKind};

        let row = event.row;
        let col = event.column;

        fn hit(rect: Rect, col: u16, row: u16) -> bool {
            col >= rect.x
                && col < rect.x + rect.width
                && row >= rect.y
                && row < rect.y + rect.height
        }

        match event.kind {
            MouseEventKind::ScrollDown => {
                if hit(self.panel_rect_preview, col, row) {
                    self.preview_scroll = (self.preview_scroll + 3).min(self.preview_content.len());
                    self.preview_pinned = self.preview_scroll < self.preview_content.len();
                    self.focused_panel = FocusedPanel::Preview;
                } else if hit(self.panel_rect_agent, col, row) {
                    self.nav_down();
                }
            }
            MouseEventKind::ScrollUp => {
                if hit(self.panel_rect_preview, col, row) {
                    self.preview_scroll = self.preview_scroll.saturating_sub(3).max(1);
                    self.preview_pinned = true;
                    self.focused_panel = FocusedPanel::Preview;
                } else if hit(self.panel_rect_agent, col, row) {
                    self.nav_up();
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if hit(self.panel_rect_agent, col, row) {
                    self.focused_panel = FocusedPanel::AgentList;
                    let data_start = self.panel_rect_agent.y + 1 + 1;
                    if row >= data_start {
                        let clicked_row = (row - data_start) as usize;
                        if clicked_row < self.display_rows.len() {
                            self.table_state.select(Some(clicked_row));
                        }
                    }
                } else if hit(self.panel_rect_preview, col, row) {
                    self.focused_panel = FocusedPanel::Preview;
                } else if hit(self.panel_rect_chat, col, row) {
                    self.focused_panel = FocusedPanel::Chat;
                }
            }
            _ => {}
        }
    }
}
