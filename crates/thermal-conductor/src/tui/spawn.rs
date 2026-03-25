//! Spawn page — profile-based session spawner for the TUI dashboard.
//!
//! Loads profiles from `config/profiles.toml` (or `~/.config/thermal/profiles.toml`).
//! Users select a profile, optionally override fields, then spawn sessions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use thermal_core::{ClaudeStatePoller, palette::ThermalPalette};

use super::TuiPage;
use crate::backend::BackendPreference;
use crate::profiles_config::{Profile, expand_tilde, load_profiles};

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
// Spawn form focus
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    ProfileList,
    CwdField,
    CommandField,
    CountField,
    WorktreeToggle,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::ProfileList => Focus::CwdField,
            Focus::CwdField => Focus::CommandField,
            Focus::CommandField => Focus::CountField,
            Focus::CountField => Focus::WorktreeToggle,
            Focus::WorktreeToggle => Focus::ProfileList,
        }
    }
    fn prev(self) -> Self {
        match self {
            Focus::ProfileList => Focus::WorktreeToggle,
            Focus::CwdField => Focus::ProfileList,
            Focus::CommandField => Focus::CwdField,
            Focus::CountField => Focus::CommandField,
            Focus::WorktreeToggle => Focus::CountField,
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
    /// Whether to create git worktrees for spawned sessions.
    worktree_enabled: bool,
    focus: Focus,
    status_msg: Option<(String, bool)>,
    spawning: Arc<AtomicBool>,
    /// CWD that thc was launched from
    launch_cwd: String,
    /// Set to true by external code to trigger a profile reload on next tick.
    pub(crate) needs_reload: bool,
    /// Which backend to use for spawning.
    backend_pref: BackendPreference,
}

impl SpawnPage {
    pub fn new(backend_pref: BackendPreference) -> Self {
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
        let (cwd, cmd, count, worktree) = if let Some(p) = profiles.first() {
            (
                p.cwd.as_deref().map(expand_tilde).unwrap_or_default(),
                p.command.clone().unwrap_or_default(),
                p.count.to_string(),
                p.git_worktree,
            )
        } else {
            (String::new(), String::new(), "1".into(), false)
        };

        Self {
            profiles,
            default_cwd,
            list_state,
            cwd_input: cwd,
            command_input: cmd,
            count_input: count,
            worktree_enabled: worktree,
            focus: Focus::ProfileList,
            status_msg: None,
            spawning: Arc::new(AtomicBool::new(false)),
            launch_cwd,
            needs_reload: false,
            backend_pref,
        }
    }

    /// Reload profiles from disk.
    pub(crate) fn reload_profiles(&mut self) {
        let (default_cwd, profiles) = load_profiles();
        self.default_cwd = default_cwd;
        self.profiles = profiles;
        // Clamp selection
        if let Some(i) = self.list_state.selected() {
            if i >= self.profiles.len() {
                self.list_state.select(if self.profiles.is_empty() {
                    None
                } else {
                    Some(0)
                });
            }
        }
        self.apply_selected_profile();
    }

    /// Fill editable fields from the selected profile.
    fn apply_selected_profile(&mut self) {
        if let Some(i) = self.list_state.selected() {
            if let Some(p) = self.profiles.get(i) {
                self.cwd_input = p.cwd.as_deref().map(expand_tilde).unwrap_or_default();
                self.command_input = p.command.clone().unwrap_or_default();
                self.count_input = p.count.to_string();
                self.worktree_enabled = p.git_worktree;
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
        if self.spawning.load(Ordering::SeqCst) {
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
        if !cwd.is_empty() {
            let path = std::path::Path::new(&cwd);
            if !path.is_dir() {
                self.status_msg = Some((format!("Not a directory: {}", cwd), true));
                return;
            }
        }

        // Use command_input if provided, else fall back to $SHELL.
        let command = if self.command_input.trim().is_empty() {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
        } else {
            self.command_input.trim().to_string()
        };

        let effective_cwd = if cwd.is_empty() {
            std::env::var("HOME").unwrap_or_else(|_| "/".into())
        } else {
            cwd.clone()
        };

        let profile_name = self
            .list_state
            .selected()
            .and_then(|i| self.profiles.get(i))
            .map(|p| p.name.clone());

        let worktree = self.worktree_enabled;
        let backend_pref = self.backend_pref;

        // Capture display strings before moving values into the spawn closure.
        let display_profile = profile_name.as_deref().unwrap_or("Custom").to_string();
        let display_cwd = if effective_cwd.is_empty() {
            "(default)".to_string()
        } else {
            effective_cwd.clone()
        };

        self.spawning.store(true, Ordering::SeqCst);

        let spawning_flag = Arc::clone(&self.spawning);
        let profile_clone = profile_name.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = rt {
                let _ = rt.block_on(async {
                    let backend = match crate::backend::detect_backend(backend_pref).await {
                        Ok(b) => b,
                        Err(_) => return,
                    };

                    match backend {
                        crate::backend::Backend::Kitty(controller) => {
                            for i in 0..count {
                                let ts = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis();
                                let id = format!("session-{ts}-{i}");

                                // Optionally create a git worktree.
                                let (spawn_cwd, wt_path) = if worktree {
                                    match crate::cmd_create_worktree(&effective_cwd, &id) {
                                        Ok(wt) => (wt.clone(), Some(wt)),
                                        Err(_) => (effective_cwd.clone(), None),
                                    }
                                } else {
                                    (effective_cwd.clone(), None)
                                };

                                let _ = controller
                                    .spawn(
                                        &id,
                                        &command,
                                        &spawn_cwd,
                                        profile_clone.as_deref(),
                                        wt_path.as_deref(),
                                    )
                                    .await;
                            }
                        }
                        crate::backend::Backend::Daemon(mut client) => {
                            for _ in 0..count {
                                let _ = client
                                    .spawn_session(
                                        Some(command.clone()),
                                        Some(effective_cwd.clone()),
                                        worktree,
                                    )
                                    .await;
                            }
                        }
                    }
                });
            }
            spawning_flag.store(false, Ordering::SeqCst);
        });

        self.status_msg = Some((
            format!(
                "Spawning {} x {} in {} (backend: {})",
                count, display_profile, display_cwd, backend_pref,
            ),
            false,
        ));
    }

    fn nav_up(&mut self) {
        if self.profiles.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        let prev = if i == 0 {
            self.profiles.len() - 1
        } else {
            i - 1
        };
        self.list_state.select(Some(prev));
        self.apply_selected_profile();
    }

    fn nav_down(&mut self) {
        if self.profiles.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        let next = if i >= self.profiles.len() - 1 {
            0
        } else {
            i + 1
        };
        self.list_state.select(Some(next));
        self.apply_selected_profile();
    }
}

impl TuiPage for SpawnPage {
    fn title(&self) -> &str {
        "Spawn"
    }

    fn tick(&mut self, _poller: &mut ClaudeStatePoller) {
        if self.needs_reload {
            self.reload_profiles();
            self.needs_reload = false;
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);

        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(28), // profile list
                Constraint::Min(30),    // form
            ])
            .margin(1)
            .split(area);

        // -- Profile list (left panel) --
        let profile_items: Vec<ListItem> = self
            .profiles
            .iter()
            .map(|p| {
                let icon = p.icon.as_deref().unwrap_or(" ");
                let text = format!("{} {}", icon, p.name);
                ListItem::new(text)
            })
            .collect();

        let list_border = if self.focus == Focus::ProfileList {
            ACCENT_COLD
        } else {
            COLD
        };
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
                Constraint::Length(3), // worktree toggle
                Constraint::Length(1), // spacer
                Constraint::Length(1), // hint
                Constraint::Length(2), // status
                Constraint::Min(0),    // rest
                Constraint::Length(1), // footer
            ])
            .split(main_chunks[1]);

        // Title
        let profile_name = self
            .list_state
            .selected()
            .and_then(|i| self.profiles.get(i))
            .map(|p| p.name.as_str())
            .unwrap_or("Custom");
        let title = Paragraph::new(format!("Spawn: {}", profile_name))
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .fg(TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            );
        f.render_widget(title, form_chunks[0]);

        // Helper to render a text input field
        let render_field = |f: &mut Frame,
                            area: Rect,
                            title: &str,
                            value: &str,
                            focused: bool,
                            placeholder: &str| {
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

            let widget = Paragraph::new(display).style(text_style).block(
                Block::default()
                    .title(format!(" {} ", title))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .style(Style::default().bg(BG_SURFACE)),
            );
            f.render_widget(widget, area);
        };

        let cwd_placeholder = format!("(inherits: {})", self.effective_cwd());
        render_field(
            f,
            form_chunks[1],
            "Working Directory",
            &self.cwd_input,
            self.focus == Focus::CwdField,
            &cwd_placeholder,
        );
        render_field(
            f,
            form_chunks[2],
            "Command",
            &self.command_input,
            self.focus == Focus::CommandField,
            "(default: claude)",
        );
        render_field(
            f,
            form_chunks[3],
            "Count (1-16)",
            &self.count_input,
            self.focus == Focus::CountField,
            "1",
        );

        // Worktree toggle
        {
            let focused = self.focus == Focus::WorktreeToggle;
            let border_color = if focused { ACCENT_COLD } else { COLD };
            let indicator = if self.worktree_enabled { "[x]" } else { "[ ]" };
            let label = format!("{} Git worktree per session", indicator);
            let text_color = if focused { TEXT_BRIGHT } else { TEXT };
            let toggle = Paragraph::new(label)
                .style(Style::default().fg(text_color))
                .block(
                    Block::default()
                        .title(" Worktree ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border_color))
                        .style(Style::default().bg(BG_SURFACE)),
                );
            f.render_widget(toggle, form_chunks[4]);
        }

        // Hint
        let hint = Paragraph::new(Line::from(vec![
            Span::styled(
                "Enter",
                Style::default().fg(WARM).add_modifier(Modifier::BOLD),
            ),
            Span::styled(": spawn  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "Tab",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": next field  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "j/k",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": select profile", Style::default().fg(TEXT_MUTED)),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(hint, form_chunks[6]);

        // Status message
        if let Some((ref msg, is_error)) = self.status_msg {
            let color = if is_error { SEARING } else { WARM };
            let status = Paragraph::new(msg.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(color));
            f.render_widget(status, form_chunks[7]);
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
            Focus::WorktreeToggle => match key.code {
                KeyCode::Char(' ') => self.worktree_enabled = !self.worktree_enabled,
                KeyCode::Tab => self.focus = self.focus.next(),
                KeyCode::BackTab => self.focus = self.focus.prev(),
                KeyCode::Enter => self.do_spawn(),
                KeyCode::Esc => {
                    self.focus = Focus::ProfileList;
                    self.status_msg = None;
                }
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
                    Focus::CwdField => {
                        self.cwd_input.pop();
                    }
                    Focus::CommandField => {
                        self.command_input.pop();
                    }
                    Focus::CountField => {
                        self.count_input.pop();
                    }
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
        use crossterm::event::{MouseButton, MouseEventKind};
        match event.kind {
            MouseEventKind::ScrollDown => self.nav_down(),
            MouseEventKind::ScrollUp => self.nav_up(),
            MouseEventKind::Down(MouseButton::Left) => {
                // Page area starts at absolute row 3 (below 3-row tab bar).
                // Layout has margin(1), so content starts at row 3+1=4, col 1.
                // Left panel: cols 1..29 (Length(28)), right panel: cols 29+.
                // Left panel is a List with Borders::ALL: border top + items start at row 5.
                let page_top = 3u16; // tab bar height
                let margin = 1u16;
                let content_top = page_top + margin;
                let content_left = margin;
                let left_panel_width = 28u16;

                let col = event.column;
                let row = event.row;

                if col >= content_left && col < content_left + left_panel_width {
                    // Left panel click — profile list
                    // List has Borders::ALL: +1 top border for items
                    let list_data_start = content_top + 1; // border top
                    if row >= list_data_start {
                        let clicked_idx = (row - list_data_start) as usize;
                        if clicked_idx < self.profiles.len() {
                            self.list_state.select(Some(clicked_idx));
                            self.apply_selected_profile();
                            self.focus = Focus::ProfileList;
                        }
                    }
                } else if col >= content_left + left_panel_width {
                    // Right panel click — form fields
                    // form_chunks layout (each Length(3)):
                    //   [0] title:    content_top .. content_top+3
                    //   [1] cwd:      content_top+3 .. content_top+6
                    //   [2] command:  content_top+6 .. content_top+9
                    //   [3] count:    content_top+9 .. content_top+12
                    //   [4] worktree: content_top+12 .. content_top+15
                    let form_top = content_top;
                    if row >= form_top + 3 && row < form_top + 6 {
                        self.focus = Focus::CwdField;
                    } else if row >= form_top + 6 && row < form_top + 9 {
                        self.focus = Focus::CommandField;
                    } else if row >= form_top + 9 && row < form_top + 12 {
                        self.focus = Focus::CountField;
                    } else if row >= form_top + 12 && row < form_top + 15 {
                        self.focus = Focus::WorktreeToggle;
                    }
                }
            }
            _ => {}
        }
    }

    fn has_text_focus(&self) -> bool {
        matches!(
            self.focus,
            Focus::CwdField | Focus::CommandField | Focus::CountField
        )
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles_config::{ProfileConfig, default_count};

    // ── expand_tilde ──────────────────────────────────────────────────────────

    #[test]
    fn expand_tilde_leaves_absolute_path_unchanged() {
        // Safety: test-only, single-threaded context.
        unsafe {
            std::env::set_var("HOME", "/home/testuser");
        }
        let result = expand_tilde("/absolute/path");
        assert_eq!(result, "/absolute/path");
    }

    #[test]
    fn expand_tilde_expands_home_prefix() {
        unsafe {
            std::env::set_var("HOME", "/home/testuser");
        }
        let result = expand_tilde("~/projects/foo");
        assert_eq!(result, "/home/testuser/projects/foo");
    }

    #[test]
    fn expand_tilde_leaves_tilde_only_unchanged() {
        // "~" without a slash is not expanded — only "~/" prefix is handled.
        let result = expand_tilde("~");
        assert_eq!(result, "~");
    }

    #[test]
    fn expand_tilde_leaves_relative_path_unchanged() {
        let result = expand_tilde("relative/path");
        assert_eq!(result, "relative/path");
    }

    #[test]
    fn expand_tilde_empty_string_unchanged() {
        let result = expand_tilde("");
        assert_eq!(result, "");
    }

    #[test]
    fn expand_tilde_deep_path() {
        unsafe {
            std::env::set_var("HOME", "/home/builder");
        }
        let result = expand_tilde("~/a/b/c/d.toml");
        assert_eq!(result, "/home/builder/a/b/c/d.toml");
    }

    // ── ProfileConfig TOML parsing ────────────────────────────────────────────

    fn parse_config(toml: &str) -> ProfileConfig {
        toml::from_str(toml).expect("TOML should parse")
    }

    #[test]
    fn toml_minimal_profile_required_field_only() {
        let toml = r#"
[[profile]]
name = "My Profile"
"#;
        let cfg = parse_config(toml);
        assert_eq!(cfg.profiles.len(), 1);
        let p = &cfg.profiles[0];
        assert_eq!(p.name, "My Profile");
        assert!(p.command.is_none());
        assert!(p.cwd.is_none());
        assert!(p.icon.is_none());
        assert_eq!(p.count, 1); // default_count()
        assert!(!p.git_worktree); // default false
    }

    #[test]
    fn toml_full_profile_all_fields() {
        let toml = r#"
[[profile]]
name = "Full Profile"
command = "claude"
cwd = "~/projects/myapp"
icon = "🚀"
count = 4
git_worktree = true
"#;
        let cfg = parse_config(toml);
        assert_eq!(cfg.profiles.len(), 1);
        let p = &cfg.profiles[0];
        assert_eq!(p.name, "Full Profile");
        assert_eq!(p.command.as_deref(), Some("claude"));
        assert_eq!(p.cwd.as_deref(), Some("~/projects/myapp"));
        assert_eq!(p.icon.as_deref(), Some("🚀"));
        assert_eq!(p.count, 4);
        assert!(p.git_worktree);
    }

    #[test]
    fn toml_multiple_profiles() {
        let toml = r#"
[[profile]]
name = "Alpha"
count = 1

[[profile]]
name = "Beta"
count = 3
cwd = "/tmp"
"#;
        let cfg = parse_config(toml);
        assert_eq!(cfg.profiles.len(), 2);
        assert_eq!(cfg.profiles[0].name, "Alpha");
        assert_eq!(cfg.profiles[1].name, "Beta");
        assert_eq!(cfg.profiles[1].count, 3);
        assert_eq!(cfg.profiles[1].cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn toml_default_cwd_field() {
        let toml = r#"
default_cwd = "/srv/projects"

[[profile]]
name = "Dev"
"#;
        let cfg = parse_config(toml);
        assert_eq!(cfg.default_cwd.as_deref(), Some("/srv/projects"));
        assert_eq!(cfg.profiles[0].name, "Dev");
    }

    #[test]
    fn toml_empty_profiles_list() {
        let toml = ""; // no profiles table at all
        let cfg: ProfileConfig = toml::from_str(toml).expect("empty TOML should parse");
        assert!(cfg.profiles.is_empty());
        assert!(cfg.default_cwd.is_none());
    }

    #[test]
    fn toml_default_count_is_one() {
        assert_eq!(default_count(), 1);
    }

    #[test]
    fn toml_profile_with_tilde_cwd() {
        let toml = r#"
[[profile]]
name = "Home"
cwd = "~/code"
"#;
        let cfg = parse_config(toml);
        assert_eq!(cfg.profiles[0].cwd.as_deref(), Some("~/code"));
    }

    // ── Focus navigation ──────────────────────────────────────────────────────

    #[test]
    fn focus_next_cycles_forward() {
        assert_eq!(Focus::ProfileList.next(), Focus::CwdField);
        assert_eq!(Focus::CwdField.next(), Focus::CommandField);
        assert_eq!(Focus::CommandField.next(), Focus::CountField);
        assert_eq!(Focus::CountField.next(), Focus::WorktreeToggle);
        assert_eq!(Focus::WorktreeToggle.next(), Focus::ProfileList);
    }

    #[test]
    fn focus_prev_cycles_backward() {
        assert_eq!(Focus::ProfileList.prev(), Focus::WorktreeToggle);
        assert_eq!(Focus::CwdField.prev(), Focus::ProfileList);
        assert_eq!(Focus::CommandField.prev(), Focus::CwdField);
        assert_eq!(Focus::CountField.prev(), Focus::CommandField);
        assert_eq!(Focus::WorktreeToggle.prev(), Focus::CountField);
    }

    #[test]
    fn focus_next_then_prev_returns_to_start() {
        let start = Focus::CwdField;
        assert_eq!(start.next().prev(), start);
    }

    #[test]
    fn focus_full_cycle_via_next() {
        let mut f = Focus::ProfileList;
        for _ in 0..5 {
            f = f.next();
        }
        assert_eq!(f, Focus::ProfileList);
    }

    #[test]
    fn focus_full_cycle_via_prev() {
        let mut f = Focus::ProfileList;
        for _ in 0..5 {
            f = f.prev();
        }
        assert_eq!(f, Focus::ProfileList);
    }

    // ── SpawnPage: effective_cwd resolution ───────────────────────────────────

    /// Build a minimal SpawnPage and override fields without going through the
    /// full load_profiles() I/O path.
    fn make_page_with_cwd_inputs(
        cwd_input: &str,
        default_cwd: Option<&str>,
        launch_cwd: &str,
    ) -> SpawnPage {
        // We call SpawnPage::new() which calls load_profiles() — that may
        // produce a fallback "Custom" profile if no config file exists, which
        // is fine for our purposes.
        SpawnPage {
            profiles: vec![],
            default_cwd: default_cwd.map(String::from),
            list_state: ratatui::widgets::ListState::default(),
            cwd_input: cwd_input.to_string(),
            command_input: String::new(),
            count_input: "1".into(),
            worktree_enabled: false,
            focus: Focus::ProfileList,
            status_msg: None,
            spawning: Arc::new(AtomicBool::new(false)),
            launch_cwd: launch_cwd.to_string(),
            needs_reload: false,
            backend_pref: BackendPreference::Auto,
        }
    }

    #[test]
    fn effective_cwd_uses_cwd_input_when_set() {
        unsafe {
            std::env::set_var("HOME", "/home/testuser");
        }
        let page = make_page_with_cwd_inputs("~/mydir", None, "/launch");
        let cwd = page.effective_cwd();
        assert_eq!(cwd, "/home/testuser/mydir");
    }

    #[test]
    fn effective_cwd_falls_back_to_default_cwd_when_input_empty() {
        unsafe {
            std::env::set_var("HOME", "/home/testuser");
        }
        let page = make_page_with_cwd_inputs("", Some("~/defaults"), "/launch");
        let cwd = page.effective_cwd();
        assert_eq!(cwd, "/home/testuser/defaults");
    }

    #[test]
    fn effective_cwd_falls_back_to_launch_cwd_when_all_empty() {
        let page = make_page_with_cwd_inputs("", None, "/launch/dir");
        let cwd = page.effective_cwd();
        assert_eq!(cwd, "/launch/dir");
    }

    #[test]
    fn effective_cwd_whitespace_only_input_treated_as_empty() {
        // trim() in effective_cwd() means whitespace-only should fall through.
        let page = make_page_with_cwd_inputs("   ", None, "/fallback");
        let cwd = page.effective_cwd();
        assert_eq!(cwd, "/fallback");
    }

    #[test]
    fn effective_cwd_input_takes_priority_over_default_and_launch() {
        unsafe {
            std::env::set_var("HOME", "/home/testuser");
        }
        let page = make_page_with_cwd_inputs("/explicit/path", Some("/default"), "/launch");
        let cwd = page.effective_cwd();
        assert_eq!(cwd, "/explicit/path");
    }
}
