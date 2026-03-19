//! Spawn page — interactive form for spawning new therminal sessions.
//!
//! Provides a project directory picker, session name field, and optional
//! initial prompt, then calls the existing `thc spawn` logic via `DaemonClient`.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use thermal_core::{palette::ThermalPalette, ClaudeStatePoller};

use super::TuiPage;

// ---------------------------------------------------------------------------
// Palette helpers
// ---------------------------------------------------------------------------

const fn pal(c: [f32; 4]) -> Color {
    Color::Rgb(
        (c[0] * 255.0) as u8,
        (c[1] * 255.0) as u8,
        (c[2] * 255.0) as u8,
    )
}

const BG: Color = pal(ThermalPalette::BG);
const BG_SURFACE: Color = pal(ThermalPalette::BG_SURFACE);
const TEXT_BRIGHT: Color = pal(ThermalPalette::TEXT_BRIGHT);
const TEXT_MUTED: Color = pal(ThermalPalette::TEXT_MUTED);
const COLD: Color = pal(ThermalPalette::COLD);
const ACCENT_COLD: Color = pal(ThermalPalette::ACCENT_COLD);
const WARM: Color = pal(ThermalPalette::WARM);
const SEARING: Color = pal(ThermalPalette::SEARING);

// ---------------------------------------------------------------------------
// Spawn form fields
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Project,
    Count,
}

impl Field {
    fn next(self) -> Self {
        match self {
            Field::Project => Field::Count,
            Field::Count => Field::Project,
        }
    }

    fn prev(self) -> Self {
        self.next() // only 2 fields, so next == prev
    }
}

// ---------------------------------------------------------------------------
// Spawn page state
// ---------------------------------------------------------------------------

pub struct SpawnPage {
    /// Current project directory input.
    project_input: String,
    /// Number of sessions to spawn (as text for editing).
    count_input: String,
    /// Which field is focused.
    focused: Field,
    /// Status message (success / error feedback).
    status_msg: Option<(String, bool)>, // (message, is_error)
    /// Whether a spawn is in progress.
    spawning: bool,
}

impl SpawnPage {
    pub fn new() -> Self {
        // Default project to current working directory.
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_default();

        Self {
            project_input: cwd,
            count_input: "1".to_string(),
            focused: Field::Project,
            status_msg: None,
            spawning: false,
        }
    }

    /// Attempt to spawn sessions via the daemon client.
    fn do_spawn(&mut self) {
        if self.spawning {
            return;
        }

        let count: u32 = match self.count_input.parse() {
            Ok(n) if n >= 1 && n <= 16 => n,
            _ => {
                self.status_msg = Some(("Count must be 1-16".into(), true));
                return;
            }
        };

        let project = if self.project_input.trim().is_empty() {
            None
        } else {
            let path = std::path::Path::new(self.project_input.trim());
            if !path.is_dir() {
                self.status_msg = Some((format!("Not a directory: {}", self.project_input), true));
                return;
            }
            Some(self.project_input.trim().to_string())
        };

        self.spawning = true;
        self.status_msg = Some((format!("Spawning {} session(s)...", count), false));

        // Spawn in a background thread using a blocking tokio client call.
        let project_clone = project.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = rt {
                let _ = rt.block_on(async {
                    match crate::client::DaemonClient::connect().await {
                        Ok(Some(mut client)) => {
                            for _ in 0..count {
                                let _ = client.spawn_session(None, project_clone.clone()).await;
                            }
                        }
                        _ => {}
                    }
                });
            }
        });

        self.status_msg = Some((
            format!(
                "Spawned {} session{}{}",
                count,
                if count == 1 { "" } else { "s" },
                project
                    .as_ref()
                    .map(|p| format!(" in {}", p))
                    .unwrap_or_default()
            ),
            false,
        ));
        self.spawning = false;
    }
}

impl TuiPage for SpawnPage {
    fn title(&self) -> &str {
        "Spawn"
    }

    fn tick(&mut self, _poller: &mut ClaudeStatePoller) {
        // Nothing to poll for the spawn page.
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        // Background
        f.render_widget(
            Block::default().style(Style::default().bg(BG)),
            area,
        );

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),  // title area
                Constraint::Length(3),  // project field
                Constraint::Length(3),  // count field
                Constraint::Length(2),  // spacer
                Constraint::Length(3),  // submit hint
                Constraint::Length(2),  // status message
                Constraint::Min(0),    // rest
                Constraint::Length(1), // footer
            ])
            .margin(1)
            .split(area);

        // Title
        let title = Paragraph::new("Spawn New Therminal Session")
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .fg(TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            );
        f.render_widget(title, chunks[0]);

        // Project directory field
        let project_style = if self.focused == Field::Project {
            Style::default().fg(TEXT_BRIGHT)
        } else {
            Style::default().fg(TEXT_MUTED)
        };
        let project_border = if self.focused == Field::Project {
            ACCENT_COLD
        } else {
            COLD
        };
        let cursor_suffix = if self.focused == Field::Project {
            "\u{2588}" // block cursor
        } else {
            ""
        };
        let project_field = Paragraph::new(format!("{}{}", self.project_input, cursor_suffix))
            .style(project_style)
            .block(
                Block::default()
                    .title(" Project Directory ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(project_border))
                    .style(Style::default().bg(BG_SURFACE)),
            );
        f.render_widget(project_field, chunks[1]);

        // Count field
        let count_style = if self.focused == Field::Count {
            Style::default().fg(TEXT_BRIGHT)
        } else {
            Style::default().fg(TEXT_MUTED)
        };
        let count_border = if self.focused == Field::Count {
            ACCENT_COLD
        } else {
            COLD
        };
        let count_cursor = if self.focused == Field::Count {
            "\u{2588}"
        } else {
            ""
        };
        let count_field = Paragraph::new(format!("{}{}", self.count_input, count_cursor))
            .style(count_style)
            .block(
                Block::default()
                    .title(" Count (1-16) ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(count_border))
                    .style(Style::default().bg(BG_SURFACE)),
            );
        f.render_widget(count_field, chunks[2]);

        // Submit hint
        let submit_hint = Paragraph::new(Line::from(vec![
            Span::styled(
                "  Enter",
                Style::default().fg(WARM).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to spawn  |  ",
                Style::default().fg(TEXT_MUTED),
            ),
            Span::styled(
                "Tab",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to switch fields", Style::default().fg(TEXT_MUTED)),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(submit_hint, chunks[4]);

        // Status message
        if let Some((ref msg, is_error)) = self.status_msg {
            let color = if is_error { SEARING } else { WARM };
            let status = Paragraph::new(msg.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(color));
            f.render_widget(status, chunks[5]);
        }

        // Footer
        let footer = Paragraph::new(Line::from(vec![
            Span::styled(
                " Tab",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": next field  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "Enter",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": spawn  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "Esc",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": clear status", Style::default().fg(TEXT_MUTED)),
        ]))
        .style(Style::default().bg(BG));
        f.render_widget(footer, chunks[7]);
    }

    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        _poller: &mut ClaudeStatePoller,
    ) -> bool {
        use crossterm::event::KeyCode;

        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.focused = if key.code == KeyCode::BackTab {
                    self.focused.prev()
                } else {
                    self.focused.next()
                };
            }
            KeyCode::Enter => {
                self.do_spawn();
            }
            KeyCode::Esc => {
                self.status_msg = None;
            }
            KeyCode::Backspace => match self.focused {
                Field::Project => {
                    self.project_input.pop();
                }
                Field::Count => {
                    self.count_input.pop();
                }
            },
            KeyCode::Char(c) => match self.focused {
                Field::Project => {
                    self.project_input.push(c);
                }
                Field::Count => {
                    if c.is_ascii_digit() && self.count_input.len() < 2 {
                        self.count_input.push(c);
                    }
                }
            },
            _ => {}
        }
        false
    }

    fn handle_mouse(
        &mut self,
        _event: crossterm::event::MouseEvent,
        _poller: &mut ClaudeStatePoller,
    ) {
        // No mouse handling for spawn page yet.
    }
}
