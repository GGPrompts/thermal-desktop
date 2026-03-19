//! Spawn page — profile-based session spawner for the TUI dashboard.
//!
//! Loads profiles from `config/profiles.toml` (or `~/.config/thermal/profiles.toml`).
//! Users select a profile, optionally override fields, then spawn sessions.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};
use serde::Deserialize;

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
const TEXT: Color = pal(ThermalPalette::TEXT);
const TEXT_BRIGHT: Color = pal(ThermalPalette::TEXT_BRIGHT);
const TEXT_MUTED: Color = pal(ThermalPalette::TEXT_MUTED);
const COLD: Color = pal(ThermalPalette::COLD);
const ACCENT_COLD: Color = pal(ThermalPalette::ACCENT_COLD);
const WARM: Color = pal(ThermalPalette::WARM);
const SEARING: Color = pal(ThermalPalette::SEARING);

// ---------------------------------------------------------------------------
// Profile config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct ProfileConfig {
    #[serde(default)]
    default_cwd: Option<String>,
    #[serde(default, rename = "profile")]
    profiles: Vec<Profile>,
}

#[derive(Debug, Clone, Deserialize)]
struct Profile {
    name: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default = "default_count")]
    count: u32,
}

fn default_count() -> u32 {
    1
}

fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home, &path[1..]);
        }
    }
    path.to_string()
}

/// Load profiles from config file. Search order:
/// 1. ./config/profiles.toml (dev)
/// 2. ~/.config/thermal/profiles.toml (user)
fn load_profiles() -> (Option<String>, Vec<Profile>) {
    let candidates = [
        "config/profiles.toml".to_string(),
        expand_tilde("~/.config/thermal/profiles.toml"),
    ];

    for path in &candidates {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(config) = toml::from_str::<ProfileConfig>(&content) {
                return (config.default_cwd, config.profiles);
            }
        }
    }

    // Fallback: single "Custom" profile
    (None, vec![Profile {
        name: "Custom".into(),
        command: None,
        cwd: None,
        icon: Some("⚡".into()),
        count: 1,
    }])
}

// ---------------------------------------------------------------------------
// Spawn form focus
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    ProfileList,
    CwdField,
    CommandField,
    CountField,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::ProfileList => Focus::CwdField,
            Focus::CwdField => Focus::CommandField,
            Focus::CommandField => Focus::CountField,
            Focus::CountField => Focus::ProfileList,
        }
    }
    fn prev(self) -> Self {
        match self {
            Focus::ProfileList => Focus::CountField,
            Focus::CwdField => Focus::ProfileList,
            Focus::CommandField => Focus::CwdField,
            Focus::CountField => Focus::CommandField,
        }
    }
}

// ---------------------------------------------------------------------------
// Spawn page state
// ---------------------------------------------------------------------------

pub struct SpawnPage {
    profiles: Vec<Profile>,
    default_cwd: Option<String>,
    list_state: ListState,
    /// Editable fields (override profile values)
    cwd_input: String,
    command_input: String,
    count_input: String,
    focus: Focus,
    status_msg: Option<(String, bool)>,
    spawning: bool,
    /// CWD that thc was launched from
    launch_cwd: String,
}

impl SpawnPage {
    pub fn new() -> Self {
        let (default_cwd, profiles) = load_profiles();
        let launch_cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_default();

        let mut list_state = ListState::default();
        if !profiles.is_empty() {
            list_state.select(Some(0));
        }

        // Initialize fields from first profile
        let (cwd, cmd, count) = if let Some(p) = profiles.first() {
            (
                p.cwd.as_deref().map(expand_tilde).unwrap_or_default(),
                p.command.clone().unwrap_or_default(),
                p.count.to_string(),
            )
        } else {
            (String::new(), String::new(), "1".into())
        };

        Self {
            profiles,
            default_cwd,
            list_state,
            cwd_input: cwd,
            command_input: cmd,
            count_input: count,
            focus: Focus::ProfileList,
            status_msg: None,
            spawning: false,
            launch_cwd,
        }
    }

    /// Fill editable fields from the selected profile.
    fn apply_selected_profile(&mut self) {
        if let Some(i) = self.list_state.selected() {
            if let Some(p) = self.profiles.get(i) {
                self.cwd_input = p.cwd.as_deref().map(expand_tilde).unwrap_or_default();
                self.command_input = p.command.clone().unwrap_or_default();
                self.count_input = p.count.to_string();
                self.status_msg = None;
            }
        }
    }

    /// Resolve the effective CWD: field > profile > default_cwd > launch_cwd
    fn effective_cwd(&self) -> String {
        let input = self.cwd_input.trim();
        if !input.is_empty() {
            return expand_tilde(input);
        }
        if let Some(ref def) = self.default_cwd {
            return expand_tilde(def);
        }
        self.launch_cwd.clone()
    }

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

        let cwd = self.effective_cwd();
        let project = if cwd.is_empty() {
            None
        } else {
            let path = std::path::Path::new(&cwd);
            if !path.is_dir() {
                self.status_msg = Some((format!("Not a directory: {}", cwd), true));
                return;
            }
            Some(cwd.clone())
        };

        self.spawning = true;

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

        let profile_name = self.list_state.selected()
            .and_then(|i| self.profiles.get(i))
            .map(|p| p.name.as_str())
            .unwrap_or("Custom");

        self.status_msg = Some((
            format!(
                "Spawned {} x {} in {}",
                count,
                profile_name,
                project.as_deref().unwrap_or("(default)"),
            ),
            false,
        ));
        self.spawning = false;
    }

    fn nav_up(&mut self) {
        if self.profiles.is_empty() { return; }
        let i = self.list_state.selected().unwrap_or(0);
        let prev = if i == 0 { self.profiles.len() - 1 } else { i - 1 };
        self.list_state.select(Some(prev));
        self.apply_selected_profile();
    }

    fn nav_down(&mut self) {
        if self.profiles.is_empty() { return; }
        let i = self.list_state.selected().unwrap_or(0);
        let next = if i >= self.profiles.len() - 1 { 0 } else { i + 1 };
        self.list_state.select(Some(next));
        self.apply_selected_profile();
    }
}

impl TuiPage for SpawnPage {
    fn title(&self) -> &str {
        "Spawn"
    }

    fn tick(&mut self, _poller: &mut ClaudeStatePoller) {}

    fn render(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(
            Block::default().style(Style::default().bg(BG)),
            area,
        );

        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(28), // profile list
                Constraint::Min(30),   // form
            ])
            .margin(1)
            .split(area);

        // -- Profile list (left panel) --
        let profile_items: Vec<ListItem> = self.profiles.iter().map(|p| {
            let icon = p.icon.as_deref().unwrap_or(" ");
            let text = format!("{} {}", icon, p.name);
            ListItem::new(text)
        }).collect();

        let list_border = if self.focus == Focus::ProfileList { ACCENT_COLD } else { COLD };
        let profile_list = List::new(profile_items)
            .block(
                Block::default()
                    .title(" Profiles ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(list_border))
                    .style(Style::default().bg(BG)),
            )
            .highlight_style(
                Style::default()
                    .bg(BG_SURFACE)
                    .fg(TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▸ ")
            .style(Style::default().fg(TEXT));

        f.render_stateful_widget(profile_list, main_chunks[0], &mut self.list_state);

        // -- Form (right panel) --
        let form_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // title
                Constraint::Length(3), // cwd field
                Constraint::Length(3), // command field
                Constraint::Length(3), // count field
                Constraint::Length(2), // spacer
                Constraint::Length(1), // hint
                Constraint::Length(2), // status
                Constraint::Min(0),   // rest
                Constraint::Length(1), // footer
            ])
            .split(main_chunks[1]);

        // Title
        let profile_name = self.list_state.selected()
            .and_then(|i| self.profiles.get(i))
            .map(|p| p.name.as_str())
            .unwrap_or("Custom");
        let title = Paragraph::new(format!("Spawn: {}", profile_name))
            .alignment(Alignment::Center)
            .style(Style::default().fg(TEXT_BRIGHT).add_modifier(Modifier::BOLD));
        f.render_widget(title, form_chunks[0]);

        // Helper to render a text input field
        let render_field = |f: &mut Frame, area: Rect, title: &str, value: &str, focused: bool, placeholder: &str| {
            let border_color = if focused { ACCENT_COLD } else { COLD };
            let text_style = if focused {
                Style::default().fg(TEXT_BRIGHT)
            } else if value.is_empty() {
                Style::default().fg(TEXT_MUTED)
            } else {
                Style::default().fg(TEXT)
            };

            let display = if value.is_empty() && !focused {
                placeholder.to_string()
            } else if focused {
                format!("{}\u{2588}", value)
            } else {
                value.to_string()
            };

            let widget = Paragraph::new(display)
                .style(text_style)
                .block(
                    Block::default()
                        .title(format!(" {} ", title))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border_color))
                        .style(Style::default().bg(BG_SURFACE)),
                );
            f.render_widget(widget, area);
        };

        let cwd_placeholder = format!("(inherits: {})", self.effective_cwd());
        render_field(f, form_chunks[1], "Working Directory", &self.cwd_input, self.focus == Focus::CwdField, &cwd_placeholder);
        render_field(f, form_chunks[2], "Command", &self.command_input, self.focus == Focus::CommandField, "(default: claude)");
        render_field(f, form_chunks[3], "Count (1-16)", &self.count_input, self.focus == Focus::CountField, "1");

        // Hint
        let hint = Paragraph::new(Line::from(vec![
            Span::styled("Enter", Style::default().fg(WARM).add_modifier(Modifier::BOLD)),
            Span::styled(": spawn  ", Style::default().fg(TEXT_MUTED)),
            Span::styled("Tab", Style::default().fg(ACCENT_COLD).add_modifier(Modifier::BOLD)),
            Span::styled(": next field  ", Style::default().fg(TEXT_MUTED)),
            Span::styled("j/k", Style::default().fg(ACCENT_COLD).add_modifier(Modifier::BOLD)),
            Span::styled(": select profile", Style::default().fg(TEXT_MUTED)),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(hint, form_chunks[5]);

        // Status message
        if let Some((ref msg, is_error)) = self.status_msg {
            let color = if is_error { SEARING } else { WARM };
            let status = Paragraph::new(msg.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(color));
            f.render_widget(status, form_chunks[6]);
        }
    }

    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        _poller: &mut ClaudeStatePoller,
    ) -> bool {
        use crossterm::event::KeyCode;

        match self.focus {
            Focus::ProfileList => match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.nav_down(),
                KeyCode::Char('k') | KeyCode::Up => self.nav_up(),
                KeyCode::Enter => self.do_spawn(),
                KeyCode::Tab => self.focus = self.focus.next(),
                KeyCode::BackTab => self.focus = self.focus.prev(),
                KeyCode::Esc => self.status_msg = None,
                _ => {}
            },
            _ => match key.code {
                KeyCode::Tab => self.focus = self.focus.next(),
                KeyCode::BackTab => self.focus = self.focus.prev(),
                KeyCode::Enter => self.do_spawn(),
                KeyCode::Esc => {
                    self.focus = Focus::ProfileList;
                    self.status_msg = None;
                }
                KeyCode::Backspace => match self.focus {
                    Focus::CwdField => { self.cwd_input.pop(); }
                    Focus::CommandField => { self.command_input.pop(); }
                    Focus::CountField => { self.count_input.pop(); }
                    _ => {}
                },
                KeyCode::Char(c) => match self.focus {
                    Focus::CwdField => self.cwd_input.push(c),
                    Focus::CommandField => self.command_input.push(c),
                    Focus::CountField => {
                        if c.is_ascii_digit() && self.count_input.len() < 2 {
                            self.count_input.push(c);
                        }
                    }
                    _ => {}
                },
                _ => {}
            },
        }
        false
    }

    fn handle_mouse(
        &mut self,
        event: crossterm::event::MouseEvent,
        _poller: &mut ClaudeStatePoller,
    ) {
        use crossterm::event::MouseEventKind;
        match event.kind {
            MouseEventKind::ScrollDown => self.nav_down(),
            MouseEventKind::ScrollUp => self.nav_up(),
            _ => {}
        }
    }
}
