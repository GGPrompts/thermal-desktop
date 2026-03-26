//! Profiles page — combined profile manager and session launcher.
//!
//! Two sub-modes within a single tab:
//! - **Launch**: Select a profile, override fields, spawn sessions
//! - **Edit**: Create, modify, clone, delete profiles (with icon picker)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use thermal_core::{ClaudeStatePoller, palette::ThermalPalette};

use super::TuiPage;
use crate::backend::BackendPreference;
use crate::profiles_config::{Profile, expand_tilde, load_profiles, save_profiles};

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
// Curated emoji grid for icon picker
// ---------------------------------------------------------------------------

const ICON_GRID: &[&str] = &[
    // Row 1: fire/heat themed
    "\u{1f525}", // fire
    "\u{2728}",  // sparkles
    "\u{26a1}",  // lightning
    "\u{1f680}", // rocket
    "\u{2b50}",  // star
    "\u{1f31f}", // glowing star
    "\u{1f4a5}", // collision
    "\u{1f300}", // cyclone
    // Row 2: tech/tools
    "\u{1f916}",         // robot
    "\u{1f527}",         // wrench
    "\u{2699}\u{fe0f}",  // gear
    "\u{1f6e0}\u{fe0f}", // hammer+wrench
    "\u{1f4bb}",         // laptop
    "\u{1f5a5}\u{fe0f}", // desktop
    "\u{2328}\u{fe0f}",  // keyboard
    "\u{1f50c}",         // plug
    // Row 3: science/nature
    "\u{1f9ea}",         // test tube
    "\u{1f52c}",         // microscope
    "\u{1f9ec}",         // dna
    "\u{1f30d}",         // earth
    "\u{1f30a}",         // wave
    "\u{2744}\u{fe0f}",  // snowflake
    "\u{1f321}\u{fe0f}", // thermometer
    "\u{1f308}",         // rainbow
    // Row 4: symbols/misc
    "\u{1f4e6}", // package
    "\u{1f4cb}", // clipboard
    "\u{1f4c1}", // folder
    "\u{1f4dd}", // memo
    "\u{1f50d}", // magnifying glass
    "\u{1f512}", // lock
    "\u{1f513}", // unlock
    "\u{1f4ac}", // speech bubble
    // Row 5: animals/fun
    "\u{1f40d}", // snake
    "\u{1f980}", // crab (Rust!)
    "\u{1f41d}", // bee
    "\u{1f427}", // penguin
    "\u{1f431}", // cat
    "\u{1f43a}", // wolf
    "\u{1f985}", // eagle
    "\u{1f409}", // dragon
];

const ICON_GRID_COLS: usize = 8;

// ---------------------------------------------------------------------------
// Sub-mode and focus
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Launch,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    ProfileList,
    // Edit-only fields
    NameField,
    IconField,
    // Shared fields (both modes)
    CwdField,
    CommandField,
    CountField,
    WorktreeToggle,
}

impl Focus {
    fn next(self, mode: Mode) -> Self {
        match mode {
            Mode::Launch => match self {
                Focus::ProfileList => Focus::CwdField,
                Focus::CwdField => Focus::CommandField,
                Focus::CommandField => Focus::CountField,
                Focus::CountField => Focus::WorktreeToggle,
                Focus::WorktreeToggle => Focus::ProfileList,
                _ => Focus::ProfileList,
            },
            Mode::Edit => match self {
                Focus::ProfileList => Focus::NameField,
                Focus::NameField => Focus::CwdField,
                Focus::CwdField => Focus::CommandField,
                Focus::CommandField => Focus::CountField,
                Focus::CountField => Focus::WorktreeToggle,
                Focus::WorktreeToggle => Focus::IconField,
                Focus::IconField => Focus::ProfileList,
            },
        }
    }

    fn prev(self, mode: Mode) -> Self {
        match mode {
            Mode::Launch => match self {
                Focus::ProfileList => Focus::WorktreeToggle,
                Focus::CwdField => Focus::ProfileList,
                Focus::CommandField => Focus::CwdField,
                Focus::CountField => Focus::CommandField,
                Focus::WorktreeToggle => Focus::CountField,
                _ => Focus::ProfileList,
            },
            Mode::Edit => match self {
                Focus::ProfileList => Focus::IconField,
                Focus::NameField => Focus::ProfileList,
                Focus::CwdField => Focus::NameField,
                Focus::CommandField => Focus::CwdField,
                Focus::CountField => Focus::CommandField,
                Focus::WorktreeToggle => Focus::CountField,
                Focus::IconField => Focus::WorktreeToggle,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Profiles page state
// ---------------------------------------------------------------------------

pub struct ProfilesPage {
    profiles: Vec<Profile>,
    default_cwd: Option<String>,
    list_state: ListState,
    mode: Mode,
    focus: Focus,

    // Shared form fields (used in both modes)
    cwd_input: String,
    command_input: String,
    count_input: String,
    worktree_enabled: bool,

    // Edit-only form fields
    name_input: String,
    icon_input: String,
    icon_picker_open: bool,
    icon_picker_index: usize,
    dirty: bool,

    // Launch-only state
    spawning: Arc<AtomicBool>,
    launch_cwd: String,
    backend_pref: BackendPreference,

    // Shared state
    status_msg: Option<(String, bool)>,
}

impl ProfilesPage {
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

        let (name, cwd, cmd, count, worktree, icon) = if let Some(p) = profiles.first() {
            (
                p.name.clone(),
                p.cwd.as_deref().map(expand_tilde).unwrap_or_default(),
                p.command.clone().unwrap_or_default(),
                p.count.to_string(),
                p.git_worktree,
                p.icon.clone().unwrap_or_default(),
            )
        } else {
            (
                String::new(),
                String::new(),
                String::new(),
                "1".into(),
                false,
                String::new(),
            )
        };

        Self {
            profiles,
            default_cwd,
            list_state,
            mode: Mode::Launch,
            focus: Focus::ProfileList,
            cwd_input: cwd,
            command_input: cmd,
            count_input: count,
            worktree_enabled: worktree,
            name_input: name,
            icon_input: icon,
            icon_picker_open: false,
            icon_picker_index: 0,
            dirty: false,
            spawning: Arc::new(AtomicBool::new(false)),
            launch_cwd,
            backend_pref,
            status_msg: None,
        }
    }

    // -- Mode switching -------------------------------------------------------

    fn switch_to_launch(&mut self) {
        if self.mode == Mode::Launch {
            return;
        }
        // Save any in-progress edits
        self.apply_form_to_profile();
        self.mode = Mode::Launch;
        // Snap focus to ProfileList if it's on an edit-only field
        if matches!(self.focus, Focus::NameField | Focus::IconField) {
            self.focus = Focus::ProfileList;
        }
        self.status_msg = None;
    }

    fn switch_to_edit(&mut self) {
        if self.mode == Mode::Edit {
            return;
        }
        self.mode = Mode::Edit;
        // Load edit fields from current profile
        self.load_form_from_profile();
        // Snap focus to ProfileList if already there, otherwise keep
        self.status_msg = None;
    }

    // -- Profile loading/saving -----------------------------------------------

    /// Load all form fields from the currently selected profile.
    fn load_form_from_profile(&mut self) {
        if let Some(i) = self.list_state.selected()
            && let Some(p) = self.profiles.get(i)
        {
            self.name_input = p.name.clone();
            self.icon_input = p.icon.clone().unwrap_or_default();
            self.cwd_input = p.cwd.as_deref().map(expand_tilde).unwrap_or_default();
            self.command_input = p.command.clone().unwrap_or_default();
            self.count_input = p.count.to_string();
            self.worktree_enabled = p.git_worktree;
        }
        self.status_msg = None;
    }

    /// Load launch-mode fields only (CWD, command, count, worktree) from the selected profile.
    fn apply_selected_profile(&mut self) {
        if let Some(i) = self.list_state.selected()
            && let Some(p) = self.profiles.get(i)
        {
            self.cwd_input = p.cwd.as_deref().map(expand_tilde).unwrap_or_default();
            self.command_input = p.command.clone().unwrap_or_default();
            self.count_input = p.count.to_string();
            self.worktree_enabled = p.git_worktree;
            self.status_msg = None;
        }
    }

    /// Copy form fields into the currently selected profile in memory (edit mode).
    fn apply_form_to_profile(&mut self) {
        if let Some(i) = self.list_state.selected()
            && let Some(p) = self.profiles.get_mut(i)
        {
            p.name = self.name_input.clone();
            p.cwd = if self.cwd_input.is_empty() {
                None
            } else {
                Some(self.cwd_input.clone())
            };
            p.command = if self.command_input.is_empty() {
                None
            } else {
                Some(self.command_input.clone())
            };
            p.count = self.count_input.parse().unwrap_or(1).clamp(1, 16);
            p.git_worktree = self.worktree_enabled;
            p.icon = if self.icon_input.is_empty() {
                None
            } else {
                Some(self.icon_input.clone())
            };
            self.dirty = true;
        }
    }

    fn do_save(&mut self) {
        self.apply_form_to_profile();

        match save_profiles(self.default_cwd.as_deref(), &self.profiles) {
            Ok(()) => {
                self.dirty = false;
                self.status_msg = Some(("Profiles saved".into(), false));
            }
            Err(e) => {
                self.status_msg = Some((e, true));
            }
        }
    }

    // -- CWD resolution (launch mode) -----------------------------------------

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

    // -- Spawn (launch mode) --------------------------------------------------

    fn do_spawn(&mut self) {
        if self.spawning.load(Ordering::SeqCst) {
            return;
        }

        let count: u32 = match self.count_input.parse() {
            Ok(n) if (1..=16).contains(&n) => n,
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
                rt.block_on(async {
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

    // -- Profile CRUD (edit mode) ---------------------------------------------

    fn new_profile(&mut self) {
        self.apply_form_to_profile();

        let p = Profile {
            name: "New Profile".into(),
            command: None,
            cwd: None,
            icon: Some("\u{26a1}".into()),
            count: 1,
            git_worktree: false,
        };
        self.profiles.push(p);
        let idx = self.profiles.len() - 1;
        self.list_state.select(Some(idx));
        self.load_form_from_profile();
        self.focus = Focus::NameField;
        self.dirty = true;
    }

    fn clone_profile(&mut self) {
        if let Some(i) = self.list_state.selected() {
            self.apply_form_to_profile();

            if let Some(p) = self.profiles.get(i).cloned() {
                let mut cloned = p;
                cloned.name = format!("{} (copy)", cloned.name);
                self.profiles.push(cloned);
                let idx = self.profiles.len() - 1;
                self.list_state.select(Some(idx));
                self.load_form_from_profile();
                self.focus = Focus::NameField;
                self.dirty = true;
            }
        }
    }

    fn delete_profile(&mut self) {
        if let Some(i) = self.list_state.selected() {
            if self.profiles.len() <= 1 {
                self.status_msg = Some(("Cannot delete last profile".into(), true));
                return;
            }
            self.profiles.remove(i);
            let new_idx = if i >= self.profiles.len() {
                self.profiles.len() - 1
            } else {
                i
            };
            self.list_state.select(Some(new_idx));
            self.load_form_from_profile();
            self.dirty = true;
            self.status_msg = Some(("Profile deleted".into(), false));
        }
    }

    // -- Navigation -----------------------------------------------------------

    fn nav_up(&mut self) {
        if self.profiles.is_empty() {
            return;
        }
        if self.mode == Mode::Edit {
            self.apply_form_to_profile();
        }
        let i = self.list_state.selected().unwrap_or(0);
        let prev = if i == 0 {
            self.profiles.len() - 1
        } else {
            i - 1
        };
        self.list_state.select(Some(prev));
        if self.mode == Mode::Edit {
            self.load_form_from_profile();
        } else {
            self.apply_selected_profile();
        }
    }

    fn nav_down(&mut self) {
        if self.profiles.is_empty() {
            return;
        }
        if self.mode == Mode::Edit {
            self.apply_form_to_profile();
        }
        let i = self.list_state.selected().unwrap_or(0);
        let next = if i >= self.profiles.len() - 1 {
            0
        } else {
            i + 1
        };
        self.list_state.select(Some(next));
        if self.mode == Mode::Edit {
            self.load_form_from_profile();
        } else {
            self.apply_selected_profile();
        }
    }

    // -- Icon picker rendering ------------------------------------------------

    fn render_icon_picker(&self, f: &mut Frame, area: Rect) {
        let rows = ICON_GRID.len().div_ceil(ICON_GRID_COLS);
        let picker_w = (ICON_GRID_COLS as u16 * 4) + 4;
        let picker_h = rows as u16 + 4;

        let x = area.x + area.width.saturating_sub(picker_w) / 2;
        let y = area.y + area.height.saturating_sub(picker_h) / 2;
        let popup_area = Rect::new(x, y, picker_w.min(area.width), picker_h.min(area.height));

        f.render_widget(Clear, popup_area);

        let block = Block::default()
            .title(" Pick Icon ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT_COLD))
            .style(Style::default().bg(BG));
        let inner = block.inner(popup_area);
        f.render_widget(block, popup_area);

        let grid_area = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        );
        for (idx, icon) in ICON_GRID.iter().enumerate() {
            let row = idx / ICON_GRID_COLS;
            let col = idx % ICON_GRID_COLS;
            let cell_x = grid_area.x + (col as u16 * 4) + 1;
            let cell_y = grid_area.y + row as u16;
            if cell_y >= grid_area.y + grid_area.height || cell_x >= grid_area.x + grid_area.width {
                continue;
            }
            let cell_area = Rect::new(
                cell_x,
                cell_y,
                3.min(grid_area.x + grid_area.width - cell_x),
                1,
            );

            let style = if idx == self.icon_picker_index {
                Style::default()
                    .bg(ACCENT_COLD)
                    .fg(TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };
            f.render_widget(Paragraph::new(*icon).style(style), cell_area);
        }

        let hint_y = grid_area.y + grid_area.height;
        if hint_y < inner.y + inner.height {
            let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
            let hint = Paragraph::new(Line::from(vec![
                Span::styled(
                    "Arrows",
                    Style::default()
                        .fg(ACCENT_COLD)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(": navigate  ", Style::default().fg(TEXT_MUTED)),
                Span::styled(
                    "Enter",
                    Style::default().fg(WARM).add_modifier(Modifier::BOLD),
                ),
                Span::styled(": select  ", Style::default().fg(TEXT_MUTED)),
                Span::styled(
                    "Esc",
                    Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(": cancel", Style::default().fg(TEXT_MUTED)),
            ]))
            .alignment(Alignment::Center);
            f.render_widget(hint, hint_area);
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

fn render_field(
    f: &mut Frame,
    area: Rect,
    title: &str,
    value: &str,
    focused: bool,
    placeholder: &str,
) {
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
}

fn render_mode_bar(f: &mut Frame, area: Rect, mode: Mode) {
    let launch_style = if mode == Mode::Launch {
        Style::default()
            .fg(TEXT_BRIGHT)
            .bg(BG_SURFACE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT_MUTED)
    };
    let edit_style = if mode == Mode::Edit {
        Style::default()
            .fg(TEXT_BRIGHT)
            .bg(BG_SURFACE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT_MUTED)
    };

    let mode_line = Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(" Launch ", launch_style),
        Span::styled("  \u{2502}  ", Style::default().fg(COLD)),
        Span::styled(" Edit ", edit_style),
    ]);
    f.render_widget(Paragraph::new(mode_line), area);
}

// ---------------------------------------------------------------------------
// TuiPage implementation
// ---------------------------------------------------------------------------

impl TuiPage for ProfilesPage {
    fn title(&self) -> &str {
        "Profiles"
    }

    fn tick(&mut self, _poller: &mut ClaudeStatePoller) {}

    fn render(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);

        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(30), // profile list
                Constraint::Min(30),    // form
            ])
            .margin(1)
            .split(area);

        // -- Left panel: profile list (shared between modes) --
        let profile_items: Vec<ListItem> = self
            .profiles
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let icon = p.icon.as_deref().unwrap_or(" ");
                let dirty_mark =
                    if self.mode == Mode::Edit && self.dirty && self.list_state.selected() == Some(i)
                    {
                        "*"
                    } else {
                        ""
                    };
                let text = format!("{} {}{}", icon, p.name, dirty_mark);
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
            .highlight_symbol("\u{25b8} ")
            .style(Style::default().fg(TEXT));

        f.render_stateful_widget(profile_list, main_chunks[0], &mut self.list_state);

        // -- Right panel: mode-dependent form --
        match self.mode {
            Mode::Launch => self.render_launch_form(f, main_chunks[1]),
            Mode::Edit => self.render_edit_form(f, main_chunks[1]),
        }

        // Icon picker overlay (rendered last)
        if self.icon_picker_open {
            self.render_icon_picker(f, area);
        }
    }

    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        _poller: &mut ClaudeStatePoller,
    ) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};

        // Icon picker overlay intercepts all keys when open
        if self.icon_picker_open {
            self.handle_icon_picker_key(key);
            return false;
        }

        // Ctrl+S saves (edit mode, works from any focus)
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.mode == Mode::Edit {
                self.do_save();
            }
            return false;
        }

        match self.mode {
            Mode::Launch => self.handle_launch_key(key),
            Mode::Edit => self.handle_edit_key(key),
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
                let col = event.column;
                let row = event.row;

                if self.icon_picker_open {
                    self.icon_picker_open = false;
                    return;
                }

                let page_top = 3u16;
                let margin = 1u16;
                let content_top = page_top + margin;
                let content_left = margin;
                let left_panel_width = 30u16;

                if col >= content_left && col < content_left + left_panel_width {
                    // Left panel click — profile list
                    let list_data_start = content_top + 1;
                    if row >= list_data_start {
                        let clicked_idx = (row - list_data_start) as usize;
                        if clicked_idx < self.profiles.len() {
                            if self.mode == Mode::Edit {
                                self.apply_form_to_profile();
                            }
                            self.list_state.select(Some(clicked_idx));
                            if self.mode == Mode::Edit {
                                self.load_form_from_profile();
                            } else {
                                self.apply_selected_profile();
                            }
                            self.focus = Focus::ProfileList;
                        }
                    }
                } else if col >= content_left + left_panel_width {
                    // Right panel click — mode bar or form fields
                    let mode_bar_row = content_top;
                    if row == mode_bar_row {
                        // Click on mode bar: detect which label was clicked
                        // Mode bar layout: "  [Launch]  |  [Edit]"
                        // "Launch" starts around col offset +2, "Edit" around +15
                        let right_start = content_left + left_panel_width;
                        let relative_col = col.saturating_sub(right_start);
                        if relative_col >= 2 && relative_col < 10 {
                            self.switch_to_launch();
                        } else if relative_col >= 13 && relative_col < 19 {
                            self.switch_to_edit();
                        }
                    } else {
                        // Form field clicks (mode-dependent)
                        let form_top = content_top + 2; // after mode bar + spacer
                        self.handle_form_click(row, form_top);
                    }
                }
            }
            _ => {}
        }
    }

    fn has_text_focus(&self) -> bool {
        match self.mode {
            Mode::Launch => matches!(
                self.focus,
                Focus::CwdField | Focus::CommandField | Focus::CountField
            ),
            Mode::Edit => matches!(
                self.focus,
                Focus::NameField | Focus::CwdField | Focus::CommandField | Focus::CountField
            ),
        }
    }

}

// ---------------------------------------------------------------------------
// Rendering sub-methods
// ---------------------------------------------------------------------------

impl ProfilesPage {
    fn render_launch_form(&mut self, f: &mut Frame, area: Rect) {
        let form_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // mode bar
                Constraint::Length(1), // spacer
                Constraint::Length(3), // title "Spawn: ProfileName"
                Constraint::Length(3), // cwd field
                Constraint::Length(3), // command field
                Constraint::Length(3), // count field
                Constraint::Length(3), // worktree toggle
                Constraint::Length(1), // spacer
                Constraint::Length(1), // hint
                Constraint::Length(2), // status
                Constraint::Min(0),   // rest
            ])
            .split(area);

        render_mode_bar(f, form_chunks[0], self.mode);

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
        f.render_widget(title, form_chunks[2]);

        let cwd_placeholder = format!("(inherits: {})", self.effective_cwd());
        render_field(
            f,
            form_chunks[3],
            "Working Directory",
            &self.cwd_input,
            self.focus == Focus::CwdField,
            &cwd_placeholder,
        );
        render_field(
            f,
            form_chunks[4],
            "Command",
            &self.command_input,
            self.focus == Focus::CommandField,
            "(default: claude)",
        );
        render_field(
            f,
            form_chunks[5],
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
            f.render_widget(toggle, form_chunks[6]);
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
            Span::styled(": select  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "e",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": edit profiles", Style::default().fg(TEXT_MUTED)),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(hint, form_chunks[8]);

        // Status message
        if let Some((ref msg, is_error)) = self.status_msg {
            let color = if is_error { SEARING } else { WARM };
            let status = Paragraph::new(msg.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(color));
            f.render_widget(status, form_chunks[9]);
        }
    }

    fn render_edit_form(&mut self, f: &mut Frame, area: Rect) {
        let form_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // mode bar
                Constraint::Length(1), // spacer
                Constraint::Length(3), // name field
                Constraint::Length(3), // icon field
                Constraint::Length(3), // cwd field
                Constraint::Length(3), // command field
                Constraint::Length(3), // count field
                Constraint::Length(3), // worktree toggle
                Constraint::Length(1), // spacer
                Constraint::Length(2), // hints
                Constraint::Length(2), // status
                Constraint::Min(0),   // rest
            ])
            .split(area);

        render_mode_bar(f, form_chunks[0], self.mode);

        render_field(
            f,
            form_chunks[2],
            "Name",
            &self.name_input,
            self.focus == Focus::NameField,
            "(required)",
        );

        // Icon field
        {
            let focused = self.focus == Focus::IconField;
            let border_color = if focused { ACCENT_COLD } else { COLD };
            let icon_display = if self.icon_input.is_empty() {
                "(none \u{2014} press Enter to pick)".to_string()
            } else if focused {
                format!("{}  (Enter to change)", self.icon_input)
            } else {
                self.icon_input.clone()
            };
            let text_style = if focused {
                Style::default().fg(TEXT_BRIGHT)
            } else if self.icon_input.is_empty() {
                Style::default().fg(TEXT_MUTED)
            } else {
                Style::default().fg(TEXT)
            };
            let widget = Paragraph::new(icon_display).style(text_style).block(
                Block::default()
                    .title(" Icon ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .style(Style::default().bg(BG_SURFACE)),
            );
            f.render_widget(widget, form_chunks[3]);
        }

        render_field(
            f,
            form_chunks[4],
            "Working Directory",
            &self.cwd_input,
            self.focus == Focus::CwdField,
            "(optional)",
        );
        render_field(
            f,
            form_chunks[5],
            "Command",
            &self.command_input,
            self.focus == Focus::CommandField,
            "(default: claude)",
        );
        render_field(
            f,
            form_chunks[6],
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
            f.render_widget(toggle, form_chunks[7]);
        }

        // Hints
        let hint = Paragraph::new(Line::from(vec![
            Span::styled(
                "Ctrl+S",
                Style::default().fg(WARM).add_modifier(Modifier::BOLD),
            ),
            Span::styled(": save  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "n",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": new  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "c",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": clone  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "d",
                Style::default().fg(SEARING).add_modifier(Modifier::BOLD),
            ),
            Span::styled(": delete  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "Tab",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": next  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "l",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": launch", Style::default().fg(TEXT_MUTED)),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(hint, form_chunks[9]);

        // Status message
        if let Some((ref msg, is_error)) = self.status_msg {
            let color = if is_error { SEARING } else { WARM };
            let status = Paragraph::new(msg.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(color));
            f.render_widget(status, form_chunks[10]);
        }
    }

    fn handle_form_click(&mut self, row: u16, form_top: u16) {
        match self.mode {
            Mode::Launch => {
                // Layout: [0] title(3), [1] cwd(3), [2] command(3), [3] count(3), [4] worktree(3)
                let field_top = form_top + 3; // after the title row
                if row >= field_top && row < field_top + 3 {
                    self.focus = Focus::CwdField;
                } else if row >= field_top + 3 && row < field_top + 6 {
                    self.focus = Focus::CommandField;
                } else if row >= field_top + 6 && row < field_top + 9 {
                    self.focus = Focus::CountField;
                } else if row >= field_top + 9 && row < field_top + 12 {
                    self.focus = Focus::WorktreeToggle;
                }
            }
            Mode::Edit => {
                // Layout: [0] name(3), [1] icon(3), [2] cwd(3), [3] command(3), [4] count(3), [5] worktree(3)
                if row >= form_top && row < form_top + 3 {
                    self.focus = Focus::NameField;
                } else if row >= form_top + 3 && row < form_top + 6 {
                    self.focus = Focus::IconField;
                } else if row >= form_top + 6 && row < form_top + 9 {
                    self.focus = Focus::CwdField;
                } else if row >= form_top + 9 && row < form_top + 12 {
                    self.focus = Focus::CommandField;
                } else if row >= form_top + 12 && row < form_top + 15 {
                    self.focus = Focus::CountField;
                } else if row >= form_top + 15 && row < form_top + 18 {
                    self.focus = Focus::WorktreeToggle;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Key handling sub-methods
// ---------------------------------------------------------------------------

impl ProfilesPage {
    fn handle_icon_picker_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Esc => {
                self.icon_picker_open = false;
            }
            KeyCode::Enter => {
                if self.icon_picker_index < ICON_GRID.len() {
                    self.icon_input = ICON_GRID[self.icon_picker_index].to_string();
                    self.dirty = true;
                }
                self.icon_picker_open = false;
            }
            KeyCode::Up => {
                if self.icon_picker_index >= ICON_GRID_COLS {
                    self.icon_picker_index -= ICON_GRID_COLS;
                }
            }
            KeyCode::Down => {
                let next = self.icon_picker_index + ICON_GRID_COLS;
                if next < ICON_GRID.len() {
                    self.icon_picker_index = next;
                }
            }
            KeyCode::Left => {
                if self.icon_picker_index > 0 {
                    self.icon_picker_index -= 1;
                }
            }
            KeyCode::Right => {
                if self.icon_picker_index + 1 < ICON_GRID.len() {
                    self.icon_picker_index += 1;
                }
            }
            _ => {}
        }
    }

    fn handle_launch_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;

        match self.focus {
            Focus::ProfileList => match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.nav_down(),
                KeyCode::Char('k') | KeyCode::Up => self.nav_up(),
                KeyCode::Char('e') => self.switch_to_edit(),
                KeyCode::Enter => self.do_spawn(),
                KeyCode::Tab => self.focus = self.focus.next(self.mode),
                KeyCode::BackTab => self.focus = self.focus.prev(self.mode),
                KeyCode::Esc => self.status_msg = None,
                _ => {}
            },
            Focus::WorktreeToggle => match key.code {
                KeyCode::Char(' ') => self.worktree_enabled = !self.worktree_enabled,
                KeyCode::Tab => self.focus = self.focus.next(self.mode),
                KeyCode::BackTab => self.focus = self.focus.prev(self.mode),
                KeyCode::Enter => self.do_spawn(),
                KeyCode::Esc => {
                    self.focus = Focus::ProfileList;
                    self.status_msg = None;
                }
                _ => {}
            },
            _ => match key.code {
                KeyCode::Tab => self.focus = self.focus.next(self.mode),
                KeyCode::BackTab => self.focus = self.focus.prev(self.mode),
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
    }

    fn handle_edit_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;

        match self.focus {
            Focus::ProfileList => match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.nav_down(),
                KeyCode::Char('k') | KeyCode::Up => self.nav_up(),
                KeyCode::Char('l') => self.switch_to_launch(),
                KeyCode::Char('n') => self.new_profile(),
                KeyCode::Char('c') => self.clone_profile(),
                KeyCode::Char('d') => self.delete_profile(),
                KeyCode::Tab => self.focus = self.focus.next(self.mode),
                KeyCode::BackTab => self.focus = self.focus.prev(self.mode),
                KeyCode::Enter => self.focus = Focus::NameField,
                KeyCode::Esc => self.status_msg = None,
                _ => {}
            },
            Focus::WorktreeToggle => match key.code {
                KeyCode::Char(' ') | KeyCode::Enter => {
                    self.worktree_enabled = !self.worktree_enabled;
                    self.dirty = true;
                }
                KeyCode::Tab => {
                    self.apply_form_to_profile();
                    self.focus = self.focus.next(self.mode);
                }
                KeyCode::BackTab => {
                    self.apply_form_to_profile();
                    self.focus = self.focus.prev(self.mode);
                }
                KeyCode::Esc => {
                    self.apply_form_to_profile();
                    self.focus = Focus::ProfileList;
                    self.status_msg = None;
                }
                _ => {}
            },
            Focus::IconField => match key.code {
                KeyCode::Enter => {
                    self.icon_picker_open = true;
                    self.icon_picker_index = ICON_GRID
                        .iter()
                        .position(|&i| i == self.icon_input)
                        .unwrap_or(0);
                }
                KeyCode::Backspace => {
                    self.icon_input.clear();
                    self.dirty = true;
                }
                KeyCode::Tab => {
                    self.apply_form_to_profile();
                    self.focus = self.focus.next(self.mode);
                }
                KeyCode::BackTab => {
                    self.apply_form_to_profile();
                    self.focus = self.focus.prev(self.mode);
                }
                KeyCode::Esc => {
                    self.apply_form_to_profile();
                    self.focus = Focus::ProfileList;
                    self.status_msg = None;
                }
                _ => {}
            },
            // Text input fields: Name, CWD, Command, Count
            _ => match key.code {
                KeyCode::Tab => {
                    self.apply_form_to_profile();
                    self.focus = self.focus.next(self.mode);
                }
                KeyCode::BackTab => {
                    self.apply_form_to_profile();
                    self.focus = self.focus.prev(self.mode);
                }
                KeyCode::Esc => {
                    self.apply_form_to_profile();
                    self.focus = Focus::ProfileList;
                    self.status_msg = None;
                }
                KeyCode::Backspace => {
                    match self.focus {
                        Focus::NameField => {
                            self.name_input.pop();
                        }
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
                    }
                    self.dirty = true;
                }
                KeyCode::Char(c) => {
                    match self.focus {
                        Focus::NameField => self.name_input.push(c),
                        Focus::CwdField => self.cwd_input.push(c),
                        Focus::CommandField => self.command_input.push(c),
                        Focus::CountField => {
                            if c.is_ascii_digit() && self.count_input.len() < 2 {
                                self.count_input.push(c);
                            }
                        }
                        _ => {}
                    }
                    self.dirty = true;
                }
                _ => {}
            },
        }
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
        assert_eq!(p.count, 1);
        assert!(!p.git_worktree);
    }

    #[test]
    fn toml_full_profile_all_fields() {
        let toml = "
[[profile]]
name = \"Full Profile\"
command = \"claude\"
cwd = \"~/projects/myapp\"
icon = \"\u{1f680}\"
count = 4
git_worktree = true
";
        let cfg = parse_config(toml);
        assert_eq!(cfg.profiles.len(), 1);
        let p = &cfg.profiles[0];
        assert_eq!(p.name, "Full Profile");
        assert_eq!(p.command.as_deref(), Some("claude"));
        assert_eq!(p.cwd.as_deref(), Some("~/projects/myapp"));
        assert_eq!(p.icon.as_deref(), Some("\u{1f680}"));
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
        let toml = "";
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

    // ── Focus navigation (launch mode) ───────────────────────────────────────

    #[test]
    fn focus_launch_next_cycles_forward() {
        assert_eq!(Focus::ProfileList.next(Mode::Launch), Focus::CwdField);
        assert_eq!(Focus::CwdField.next(Mode::Launch), Focus::CommandField);
        assert_eq!(Focus::CommandField.next(Mode::Launch), Focus::CountField);
        assert_eq!(Focus::CountField.next(Mode::Launch), Focus::WorktreeToggle);
        assert_eq!(Focus::WorktreeToggle.next(Mode::Launch), Focus::ProfileList);
    }

    #[test]
    fn focus_launch_prev_cycles_backward() {
        assert_eq!(Focus::ProfileList.prev(Mode::Launch), Focus::WorktreeToggle);
        assert_eq!(Focus::CwdField.prev(Mode::Launch), Focus::ProfileList);
        assert_eq!(Focus::CommandField.prev(Mode::Launch), Focus::CwdField);
        assert_eq!(Focus::CountField.prev(Mode::Launch), Focus::CommandField);
        assert_eq!(
            Focus::WorktreeToggle.prev(Mode::Launch),
            Focus::CountField
        );
    }

    #[test]
    fn focus_launch_full_cycle_via_next() {
        let mut f = Focus::ProfileList;
        for _ in 0..5 {
            f = f.next(Mode::Launch);
        }
        assert_eq!(f, Focus::ProfileList);
    }

    #[test]
    fn focus_launch_full_cycle_via_prev() {
        let mut f = Focus::ProfileList;
        for _ in 0..5 {
            f = f.prev(Mode::Launch);
        }
        assert_eq!(f, Focus::ProfileList);
    }

    #[test]
    fn focus_launch_next_then_prev_returns_to_start() {
        let start = Focus::CwdField;
        assert_eq!(start.next(Mode::Launch).prev(Mode::Launch), start);
    }

    // ── Focus navigation (edit mode) ─────────────────────────────────────────

    #[test]
    fn focus_edit_next_cycles_all_fields() {
        let mut f = Focus::ProfileList;
        let expected = [
            Focus::NameField,
            Focus::CwdField,
            Focus::CommandField,
            Focus::CountField,
            Focus::WorktreeToggle,
            Focus::IconField,
            Focus::ProfileList,
        ];
        for e in &expected {
            f = f.next(Mode::Edit);
            assert_eq!(f, *e);
        }
    }

    #[test]
    fn focus_edit_prev_cycles_all_fields() {
        let mut f = Focus::ProfileList;
        let expected = [
            Focus::IconField,
            Focus::WorktreeToggle,
            Focus::CountField,
            Focus::CommandField,
            Focus::CwdField,
            Focus::NameField,
            Focus::ProfileList,
        ];
        for e in &expected {
            f = f.prev(Mode::Edit);
            assert_eq!(f, *e);
        }
    }

    #[test]
    fn focus_edit_next_then_prev_round_trips() {
        let start = Focus::CwdField;
        assert_eq!(start.next(Mode::Edit).prev(Mode::Edit), start);
    }

    // ── Icon grid ─────────────────────────────────────────────────────────────

    #[test]
    fn icon_grid_has_expected_size() {
        assert!(ICON_GRID.len() >= 32);
        assert!(ICON_GRID.len() <= 48);
    }

    #[test]
    fn icon_grid_cols_divides_evenly_ish() {
        assert_eq!(ICON_GRID_COLS, 8);
    }

    // ── effective_cwd resolution ─────────────────────────────────────────────

    fn make_page_with_cwd_inputs(
        cwd_input: &str,
        default_cwd: Option<&str>,
        launch_cwd: &str,
    ) -> ProfilesPage {
        ProfilesPage {
            profiles: vec![],
            default_cwd: default_cwd.map(String::from),
            list_state: ListState::default(),
            mode: Mode::Launch,
            focus: Focus::ProfileList,
            cwd_input: cwd_input.to_string(),
            command_input: String::new(),
            count_input: "1".into(),
            worktree_enabled: false,
            name_input: String::new(),
            icon_input: String::new(),
            icon_picker_open: false,
            icon_picker_index: 0,
            dirty: false,
            spawning: Arc::new(AtomicBool::new(false)),
            launch_cwd: launch_cwd.to_string(),
            backend_pref: BackendPreference::Auto,
            status_msg: None,
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
