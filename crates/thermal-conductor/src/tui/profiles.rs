//! Profiles page — CRUD editor for spawn profiles in the TUI dashboard.
//!
//! Left panel: profile list with New/Clone/Delete actions.
//! Right panel: edit form with name, cwd, command, count, worktree toggle, icon picker.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

use thermal_core::{palette::ThermalPalette, ClaudeStatePoller};

use super::TuiPage;
use crate::profiles_config::{Profile, load_profiles, save_profiles};

// ---------------------------------------------------------------------------
// Palette helpers (same constants as spawn.rs)
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
    "\u{1f916}", // robot
    "\u{1f527}", // wrench
    "\u{2699}\u{fe0f}",  // gear
    "\u{1f6e0}\u{fe0f}", // hammer+wrench
    "\u{1f4bb}", // laptop
    "\u{1f5a5}\u{fe0f}", // desktop
    "\u{2328}\u{fe0f}",  // keyboard
    "\u{1f50c}", // plug
    // Row 3: science/nature
    "\u{1f9ea}", // test tube
    "\u{1f52c}", // microscope
    "\u{1f9ec}", // dna
    "\u{1f30d}", // earth
    "\u{1f30a}", // wave
    "\u{2744}\u{fe0f}",  // snowflake
    "\u{1f321}\u{fe0f}", // thermometer
    "\u{1f308}", // rainbow
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
// Focus and form field tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    ProfileList,
    NameField,
    CwdField,
    CommandField,
    CountField,
    WorktreeToggle,
    IconField,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::ProfileList => Focus::NameField,
            Focus::NameField => Focus::CwdField,
            Focus::CwdField => Focus::CommandField,
            Focus::CommandField => Focus::CountField,
            Focus::CountField => Focus::WorktreeToggle,
            Focus::WorktreeToggle => Focus::IconField,
            Focus::IconField => Focus::ProfileList,
        }
    }
    fn prev(self) -> Self {
        match self {
            Focus::ProfileList => Focus::IconField,
            Focus::NameField => Focus::ProfileList,
            Focus::CwdField => Focus::NameField,
            Focus::CommandField => Focus::CwdField,
            Focus::CountField => Focus::CommandField,
            Focus::WorktreeToggle => Focus::CountField,
            Focus::IconField => Focus::WorktreeToggle,
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
    focus: Focus,
    /// Editable form fields for the selected profile.
    name_input: String,
    cwd_input: String,
    command_input: String,
    count_input: String,
    worktree_enabled: bool,
    icon_input: String,
    /// Icon picker overlay state.
    icon_picker_open: bool,
    icon_picker_index: usize,
    /// Status/feedback message.
    status_msg: Option<(String, bool)>,
    /// Whether profiles have been modified since last save.
    dirty: bool,
    /// Signal to the Spawn page that profiles changed on disk.
    pub(crate) profiles_changed: bool,
}

impl ProfilesPage {
    pub fn new() -> Self {
        let (default_cwd, profiles) = load_profiles();
        let mut list_state = ListState::default();
        if !profiles.is_empty() {
            list_state.select(Some(0));
        }

        let (name, cwd, cmd, count, worktree, icon) = if let Some(p) = profiles.first() {
            (
                p.name.clone(),
                p.cwd.clone().unwrap_or_default(),
                p.command.clone().unwrap_or_default(),
                p.count.to_string(),
                p.git_worktree,
                p.icon.clone().unwrap_or_default(),
            )
        } else {
            (String::new(), String::new(), String::new(), "1".into(), false, String::new())
        };

        Self {
            profiles,
            default_cwd,
            list_state,
            focus: Focus::ProfileList,
            name_input: name,
            cwd_input: cwd,
            command_input: cmd,
            count_input: count,
            worktree_enabled: worktree,
            icon_input: icon,
            icon_picker_open: false,
            icon_picker_index: 0,
            status_msg: None,
            dirty: false,
            profiles_changed: false,
        }
    }

    /// Copy form fields into the currently selected profile in memory.
    fn apply_form_to_profile(&mut self) {
        if let Some(i) = self.list_state.selected() {
            if let Some(p) = self.profiles.get_mut(i) {
                p.name = self.name_input.clone();
                p.cwd = if self.cwd_input.is_empty() { None } else { Some(self.cwd_input.clone()) };
                p.command = if self.command_input.is_empty() { None } else { Some(self.command_input.clone()) };
                p.count = self.count_input.parse().unwrap_or(1).max(1).min(16);
                p.git_worktree = self.worktree_enabled;
                p.icon = if self.icon_input.is_empty() { None } else { Some(self.icon_input.clone()) };
                self.dirty = true;
            }
        }
    }

    /// Load form fields from the currently selected profile.
    fn load_form_from_profile(&mut self) {
        if let Some(i) = self.list_state.selected() {
            if let Some(p) = self.profiles.get(i) {
                self.name_input = p.name.clone();
                self.cwd_input = p.cwd.clone().unwrap_or_default();
                self.command_input = p.command.clone().unwrap_or_default();
                self.count_input = p.count.to_string();
                self.worktree_enabled = p.git_worktree;
                self.icon_input = p.icon.clone().unwrap_or_default();
            }
        }
        self.status_msg = None;
    }

    fn do_save(&mut self) {
        // Apply current form to the selected profile first.
        self.apply_form_to_profile();

        match save_profiles(self.default_cwd.as_deref(), &self.profiles) {
            Ok(()) => {
                self.dirty = false;
                self.profiles_changed = true;
                self.status_msg = Some(("Profiles saved".into(), false));
            }
            Err(e) => {
                self.status_msg = Some((e, true));
            }
        }
    }

    fn new_profile(&mut self) {
        // Save current form before switching
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
            // Save current form before cloning
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
            let new_idx = if i >= self.profiles.len() { self.profiles.len() - 1 } else { i };
            self.list_state.select(Some(new_idx));
            self.load_form_from_profile();
            self.dirty = true;
            self.status_msg = Some(("Profile deleted".into(), false));
        }
    }

    fn nav_up(&mut self) {
        if self.profiles.is_empty() { return; }
        // Save current form before navigating
        self.apply_form_to_profile();
        let i = self.list_state.selected().unwrap_or(0);
        let prev = if i == 0 { self.profiles.len() - 1 } else { i - 1 };
        self.list_state.select(Some(prev));
        self.load_form_from_profile();
    }

    fn nav_down(&mut self) {
        if self.profiles.is_empty() { return; }
        // Save current form before navigating
        self.apply_form_to_profile();
        let i = self.list_state.selected().unwrap_or(0);
        let next = if i >= self.profiles.len() - 1 { 0 } else { i + 1 };
        self.list_state.select(Some(next));
        self.load_form_from_profile();
    }

    /// Render the icon picker overlay centered on the screen.
    fn render_icon_picker(&self, f: &mut Frame, area: Rect) {
        let rows = (ICON_GRID.len() + ICON_GRID_COLS - 1) / ICON_GRID_COLS;
        // Each icon cell is 4 chars wide, plus borders + padding
        let picker_w = (ICON_GRID_COLS as u16 * 4) + 4;
        let picker_h = rows as u16 + 4; // +2 for borders, +1 title, +1 hint

        // Center the popup
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

        // Render grid rows
        let grid_area = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1));
        for (idx, icon) in ICON_GRID.iter().enumerate() {
            let row = idx / ICON_GRID_COLS;
            let col = idx % ICON_GRID_COLS;
            let cell_x = grid_area.x + (col as u16 * 4) + 1;
            let cell_y = grid_area.y + row as u16;
            if cell_y >= grid_area.y + grid_area.height || cell_x >= grid_area.x + grid_area.width {
                continue;
            }
            let cell_area = Rect::new(cell_x, cell_y, 3.min(grid_area.x + grid_area.width - cell_x), 1);

            let style = if idx == self.icon_picker_index {
                Style::default().bg(ACCENT_COLD).fg(TEXT_BRIGHT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };
            f.render_widget(Paragraph::new(*icon).style(style), cell_area);
        }

        // Hint at bottom
        let hint_y = grid_area.y + grid_area.height;
        if hint_y < inner.y + inner.height {
            let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
            let hint = Paragraph::new(Line::from(vec![
                Span::styled("Arrows", Style::default().fg(ACCENT_COLD).add_modifier(Modifier::BOLD)),
                Span::styled(": navigate  ", Style::default().fg(TEXT_MUTED)),
                Span::styled("Enter", Style::default().fg(WARM).add_modifier(Modifier::BOLD)),
                Span::styled(": select  ", Style::default().fg(TEXT_MUTED)),
                Span::styled("Esc", Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD)),
                Span::styled(": cancel", Style::default().fg(TEXT_MUTED)),
            ])).alignment(Alignment::Center);
            f.render_widget(hint, hint_area);
        }
    }
}

impl TuiPage for ProfilesPage {
    fn title(&self) -> &str {
        "Profiles"
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
                Constraint::Length(30), // profile list
                Constraint::Min(35),   // edit form
            ])
            .margin(1)
            .split(area);

        // -- Left panel: profile list --
        let profile_items: Vec<ListItem> = self.profiles.iter().enumerate().map(|(i, p)| {
            let icon = p.icon.as_deref().unwrap_or(" ");
            let dirty_mark = if self.dirty && self.list_state.selected() == Some(i) { "*" } else { "" };
            let text = format!("{} {}{}", icon, p.name, dirty_mark);
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
            .highlight_symbol("\u{25b8} ")
            .style(Style::default().fg(TEXT));

        f.render_stateful_widget(profile_list, main_chunks[0], &mut self.list_state);

        // -- Right panel: edit form --
        let form_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
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
            .split(main_chunks[1]);

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

        render_field(f, form_chunks[0], "Name", &self.name_input, self.focus == Focus::NameField, "(required)");

        // Icon field — show current icon + hint
        {
            let focused = self.focus == Focus::IconField;
            let border_color = if focused { ACCENT_COLD } else { COLD };
            let icon_display = if self.icon_input.is_empty() {
                "(none — press Enter to pick)".to_string()
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
            let widget = Paragraph::new(icon_display)
                .style(text_style)
                .block(
                    Block::default()
                        .title(" Icon ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border_color))
                        .style(Style::default().bg(BG_SURFACE)),
                );
            f.render_widget(widget, form_chunks[1]);
        }

        render_field(f, form_chunks[2], "Working Directory", &self.cwd_input, self.focus == Focus::CwdField, "(optional)");
        render_field(f, form_chunks[3], "Command", &self.command_input, self.focus == Focus::CommandField, "(default: claude)");
        render_field(f, form_chunks[4], "Count (1-16)", &self.count_input, self.focus == Focus::CountField, "1");

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
            f.render_widget(toggle, form_chunks[5]);
        }

        // Hints
        let hint = Paragraph::new(Line::from(vec![
            Span::styled("Ctrl+S", Style::default().fg(WARM).add_modifier(Modifier::BOLD)),
            Span::styled(": save  ", Style::default().fg(TEXT_MUTED)),
            Span::styled("n", Style::default().fg(ACCENT_COLD).add_modifier(Modifier::BOLD)),
            Span::styled(": new  ", Style::default().fg(TEXT_MUTED)),
            Span::styled("c", Style::default().fg(ACCENT_COLD).add_modifier(Modifier::BOLD)),
            Span::styled(": clone  ", Style::default().fg(TEXT_MUTED)),
            Span::styled("d", Style::default().fg(SEARING).add_modifier(Modifier::BOLD)),
            Span::styled(": delete  ", Style::default().fg(TEXT_MUTED)),
            Span::styled("Tab", Style::default().fg(ACCENT_COLD).add_modifier(Modifier::BOLD)),
            Span::styled(": next field", Style::default().fg(TEXT_MUTED)),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(hint, form_chunks[7]);

        // Status message
        if let Some((ref msg, is_error)) = self.status_msg {
            let color = if is_error { SEARING } else { WARM };
            let status = Paragraph::new(msg.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(color));
            f.render_widget(status, form_chunks[8]);
        }

        // Icon picker overlay (rendered last so it's on top)
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
            return false;
        }

        // Global: Ctrl+S saves
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.do_save();
            return false;
        }

        match self.focus {
            Focus::ProfileList => match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.nav_down(),
                KeyCode::Char('k') | KeyCode::Up => self.nav_up(),
                KeyCode::Char('n') => self.new_profile(),
                KeyCode::Char('c') => self.clone_profile(),
                KeyCode::Char('d') => self.delete_profile(),
                KeyCode::Tab => self.focus = self.focus.next(),
                KeyCode::BackTab => self.focus = self.focus.prev(),
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
                    self.focus = self.focus.next();
                }
                KeyCode::BackTab => {
                    self.apply_form_to_profile();
                    self.focus = self.focus.prev();
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
                    // Try to find current icon in grid to pre-select
                    self.icon_picker_index = ICON_GRID.iter()
                        .position(|&i| i == self.icon_input)
                        .unwrap_or(0);
                }
                KeyCode::Backspace => {
                    self.icon_input.clear();
                    self.dirty = true;
                }
                KeyCode::Tab => {
                    self.apply_form_to_profile();
                    self.focus = self.focus.next();
                }
                KeyCode::BackTab => {
                    self.apply_form_to_profile();
                    self.focus = self.focus.prev();
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
                    self.focus = self.focus.next();
                }
                KeyCode::BackTab => {
                    self.apply_form_to_profile();
                    self.focus = self.focus.prev();
                }
                KeyCode::Esc => {
                    self.apply_form_to_profile();
                    self.focus = Focus::ProfileList;
                    self.status_msg = None;
                }
                KeyCode::Backspace => {
                    match self.focus {
                        Focus::NameField => { self.name_input.pop(); }
                        Focus::CwdField => { self.cwd_input.pop(); }
                        Focus::CommandField => { self.command_input.pop(); }
                        Focus::CountField => { self.count_input.pop(); }
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

                // Icon picker overlay intercepts clicks when open
                if self.icon_picker_open {
                    // The picker is centered on the full terminal area.
                    // Approximate: compute the popup area the same way as render_icon_picker.
                    // We use the same math but with a rough terminal size estimate.
                    // Since we don't have the frame area here, use the click coords
                    // to determine if they land on an icon cell.
                    // The picker grid: each icon cell is 4 chars wide, rows are 1 char tall.
                    // This is best-effort — we check if the click is inside the popup
                    // by recomputing from ICON_GRID dimensions.
                    let _grid_rows = (ICON_GRID.len() + ICON_GRID_COLS - 1) / ICON_GRID_COLS;
                    // We don't know the exact terminal size, but we can use a reasonable
                    // fallback. The icon picker is centered, so approximate with 80x24 min.
                    // In practice, clicks that land on icons will work for most terminal sizes.
                    // A simpler approach: just check relative to a reasonable center.
                    // For robustness, accept any click when picker is open — if it's outside
                    // the grid, just close the picker.
                    // Actually, let's just close the picker on any click outside.
                    // We can't perfectly replicate the centering logic without the frame area,
                    // so we'll keep it simple.
                    self.icon_picker_open = false;
                    return;
                }

                // Page area starts at absolute row 3 (below 3-row tab bar).
                // Layout has margin(1), so content starts at row 4, col 1.
                let page_top = 3u16;
                let margin = 1u16;
                let content_top = page_top + margin;
                let content_left = margin;
                let left_panel_width = 30u16;

                if col >= content_left && col < content_left + left_panel_width {
                    // Left panel click — profile list
                    let list_data_start = content_top + 1; // +1 for Borders::ALL top
                    if row >= list_data_start {
                        let clicked_idx = (row - list_data_start) as usize;
                        if clicked_idx < self.profiles.len() {
                            self.apply_form_to_profile();
                            self.list_state.select(Some(clicked_idx));
                            self.load_form_from_profile();
                            self.focus = Focus::ProfileList;
                        }
                    }
                } else if col >= content_left + left_panel_width {
                    // Right panel click — form fields
                    // form_chunks layout (each Length(3)):
                    //   [0] name:     content_top .. content_top+3
                    //   [1] icon:     content_top+3 .. content_top+6
                    //   [2] cwd:      content_top+6 .. content_top+9
                    //   [3] command:  content_top+9 .. content_top+12
                    //   [4] count:    content_top+12 .. content_top+15
                    //   [5] worktree: content_top+15 .. content_top+18
                    let form_top = content_top;
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
            _ => {}
        }
    }

    fn has_text_focus(&self) -> bool {
        matches!(self.focus, Focus::NameField | Focus::CwdField | Focus::CommandField | Focus::CountField)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_next_cycles_all_fields() {
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
            f = f.next();
            assert_eq!(f, *e);
        }
    }

    #[test]
    fn focus_prev_cycles_all_fields() {
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
            f = f.prev();
            assert_eq!(f, *e);
        }
    }

    #[test]
    fn focus_next_then_prev_round_trips() {
        let start = Focus::CwdField;
        assert_eq!(start.next().prev(), start);
    }

    #[test]
    fn icon_grid_has_expected_size() {
        // We promised ~40 curated emoji.
        assert!(ICON_GRID.len() >= 32);
        assert!(ICON_GRID.len() <= 48);
    }

    #[test]
    fn icon_grid_cols_divides_evenly_ish() {
        // Last row may be partial, but ICON_GRID_COLS should be reasonable.
        assert_eq!(ICON_GRID_COLS, 8);
    }
}
