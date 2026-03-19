//! Sessions page — absorbed from thermal-monitor.
//!
//! Shows all Claude sessions with subagent nesting, context %, mouse scroll,
//! kitty attach, and a history popup overlay.

use std::collections::{HashMap, VecDeque};
use std::process::Command;
use std::time::Instant;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};

use thermal_core::{
    palette::ThermalPalette,
    ClaudeSessionState, ClaudeStatePoller, ClaudeStatus,
};

use super::TuiPage;

// ---------------------------------------------------------------------------
// Palette helpers (same as thermal-monitor)
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

fn status_color(status: &ClaudeStatus) -> Color {
    match status {
        ClaudeStatus::Idle => pal(ThermalPalette::COLD),
        ClaudeStatus::Processing => pal(ThermalPalette::WARM),
        ClaudeStatus::ToolUse => pal(ThermalPalette::HOT),
        ClaudeStatus::AwaitingInput => pal(ThermalPalette::SEARING),
    }
}

fn status_label(status: &ClaudeStatus) -> &'static str {
    match status {
        ClaudeStatus::Idle => "IDLE",
        ClaudeStatus::Processing => "RUNNING",
        ClaudeStatus::ToolUse => "TOOL USE",
        ClaudeStatus::AwaitingInput => "AWAITING",
    }
}

/// Color for context percentage thresholds.
fn ctx_color(pct: f32) -> Color {
    if pct < 50.0 {
        Color::Green
    } else if pct < 75.0 {
        Color::Yellow
    } else if pct < 90.0 {
        Color::Rgb(249, 115, 22) // orange
    } else {
        Color::Red
    }
}

// ---------------------------------------------------------------------------
// Activity formatting
// ---------------------------------------------------------------------------

/// Extract just the filename from a path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Build the activity string from session state.
fn format_activity(s: &ClaudeSessionState) -> String {
    if s.status == ClaudeStatus::Idle || s.status == ClaudeStatus::AwaitingInput {
        return "\u{2705} Ready".into();
    }

    let tool_name = s.current_tool.as_deref().unwrap_or("");
    if tool_name.is_empty() {
        return "\u{26A1} Processing".into();
    }

    let trunc = |s: &str, n: usize| -> String {
        if s.chars().count() > n {
            format!("{}...", s.chars().take(n).collect::<String>())
        } else {
            s.to_string()
        }
    };
    let detail = s
        .details
        .as_ref()
        .and_then(|d| d.args.as_ref())
        .map(|a| {
            if let Some(fp) = &a.file_path {
                basename(fp).to_string()
            } else if let Some(cmd) = &a.command {
                trunc(cmd, 20)
            } else if let Some(pat) = &a.pattern {
                pat.clone()
            } else if let Some(desc) = &a.description {
                trunc(desc, 20)
            } else {
                String::new()
            }
        })
        .unwrap_or_default();

    let (emoji, label) = match tool_name {
        "Read" => ("\u{1F4D6}", "Read"),
        "Write" => ("\u{1F4DD}", "Write"),
        "Edit" => ("\u{270F}\u{FE0F}", "Edit"),
        "Bash" => ("\u{1F53A}", "Bash"),
        "Glob" => ("\u{1F50D}", "Glob"),
        "Grep" => ("\u{1F50E}", "Grep"),
        "Task" | "Agent" => ("\u{1F916}", "Task"),
        "WebFetch" => ("\u{1F310}", "Fetch"),
        "WebSearch" => ("\u{1F50D}", "Search"),
        other => ("", other),
    };

    let mut result = if emoji.is_empty() {
        label.to_string()
    } else if detail.is_empty() {
        format!("{} {}", emoji, label)
    } else {
        format!("{} {}: {}", emoji, label, detail)
    };

    if let Some(n) = s.subagent_count {
        if n > 0 {
            result.push_str(&format!(" \u{1F916}\u{00D7}{}", n));
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Relative timestamps
// ---------------------------------------------------------------------------

fn relative_time(iso: &str) -> String {
    parse_secs_ago(iso)
        .map(|s| {
            let s = s.max(0);
            if s < 60 {
                format!("{}s", s)
            } else if s < 3600 {
                format!("{}m", s / 60)
            } else {
                format!("{}h", s / 3600)
            }
        })
        .unwrap_or_else(|| "-".into())
}

fn parse_secs_ago(iso: &str) -> Option<i64> {
    let s = iso.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, mo, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let time = time.split('.').next()?;
    let time = time.split('+').next()?;
    let mut t = time.split(':');
    let (h, mi, sc): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next().and_then(|s| s.parse().ok()).unwrap_or(0),
    );
    let (mut yr, mut mn) = (y, mo);
    if mn <= 2 {
        yr -= 1;
        mn += 12;
    }
    let days = 365 * yr + yr / 4 - yr / 100 + yr / 400 + (153 * (mn - 3) + 2) / 5 + day - 719469;
    let ts = days * 86400 + h * 3600 + mi * 60 + sc;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(now - ts)
}

// ---------------------------------------------------------------------------
// History tracking
// ---------------------------------------------------------------------------

const MAX_HISTORY: usize = 12;

struct HistoryEntry {
    text: String,
    timestamp: Instant,
}

// ---------------------------------------------------------------------------
// Display ordering -- parents first, subagents nested underneath
// ---------------------------------------------------------------------------

struct DisplayRow {
    session: ClaudeSessionState,
    is_subagent: bool,
    is_last_child: bool,
}

fn build_display_order(sessions: &[ClaudeSessionState]) -> Vec<DisplayRow> {
    let mut parents: Vec<&ClaudeSessionState> = sessions
        .iter()
        .filter(|s| s.parent_session_id.is_none())
        .collect();
    parents.sort_by(|a, b| a.session_id.cmp(&b.session_id));

    let mut rows = Vec::with_capacity(sessions.len());

    for parent in &parents {
        rows.push(DisplayRow {
            session: (*parent).clone(),
            is_subagent: false,
            is_last_child: false,
        });

        let mut children: Vec<&ClaudeSessionState> = sessions
            .iter()
            .filter(|s| s.parent_session_id.as_deref() == Some(&parent.session_id))
            .collect();
        children.sort_by(|a, b| a.session_id.cmp(&b.session_id));

        let child_count = children.len();
        for (i, child) in children.into_iter().enumerate() {
            rows.push(DisplayRow {
                session: child.clone(),
                is_subagent: true,
                is_last_child: i == child_count - 1,
            });
        }
    }

    // Orphan subagents
    for s in sessions {
        if s.parent_session_id.is_some()
            && !parents
                .iter()
                .any(|p| Some(p.session_id.as_str()) == s.parent_session_id.as_deref())
        {
            rows.push(DisplayRow {
                session: s.clone(),
                is_subagent: true,
                is_last_child: true,
            });
        }
    }

    rows
}

// ---------------------------------------------------------------------------
// Sessions page state
// ---------------------------------------------------------------------------

pub struct SessionsPage {
    sessions: Vec<ClaudeSessionState>,
    display_rows: Vec<DisplayRow>,
    table_state: TableState,
    prev_state: HashMap<String, (ClaudeStatus, Option<String>)>,
    cached_context_pct: HashMap<String, f32>,
    history: HashMap<String, VecDeque<HistoryEntry>>,
    history_popup: Option<String>,
}

impl SessionsPage {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            display_rows: Vec::new(),
            table_state: TableState::default(),
            prev_state: HashMap::new(),
            cached_context_pct: HashMap::new(),
            history: HashMap::new(),
            history_popup: None,
        }
    }

    fn update_from_poller(&mut self, poller: &mut ClaudeStatePoller) {
        let updated = poller.poll();
        if !updated.is_empty() {
            self.sessions = updated;
        }
        // Cache context_percent
        for s in &mut self.sessions {
            if let Some(pct) = s.context_percent {
                self.cached_context_pct
                    .insert(s.session_id.clone(), pct);
            } else if let Some(&cached) = self.cached_context_pct.get(&s.session_id) {
                s.context_percent = Some(cached);
            }
        }
        self.display_rows = build_display_order(&self.sessions);
        self.clamp_selection();
        self.update_history();
    }

    fn update_history(&mut self) {
        let now = Instant::now();
        for s in &self.sessions {
            if s.parent_session_id.is_some() {
                continue;
            }
            let current = (s.status.clone(), s.current_tool.clone());
            let changed = match self.prev_state.get(&s.session_id) {
                Some(prev) => *prev != current,
                None => true,
            };
            if changed {
                let activity = format_activity(s);
                let entries = self.history.entry(s.session_id.clone()).or_default();
                entries.push_back(HistoryEntry {
                    text: activity,
                    timestamp: now,
                });
                while entries.len() > MAX_HISTORY {
                    entries.pop_front();
                }
                self.prev_state.insert(s.session_id.clone(), current);
            }
        }
    }

    fn force_refresh(&mut self, poller: &mut ClaudeStatePoller) {
        self.sessions = poller.get_all();
        self.display_rows = build_display_order(&self.sessions);
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.display_rows.is_empty() {
            self.table_state.select(None);
        } else if let Some(i) = self.table_state.selected() {
            if i >= self.display_rows.len() {
                self.table_state
                    .select(Some(self.display_rows.len() - 1));
            }
        }
    }

    pub fn nav_down(&mut self) {
        if self.display_rows.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let next = if i >= self.display_rows.len() - 1 {
            0
        } else {
            i + 1
        };
        self.table_state.select(Some(next));
    }

    pub fn nav_up(&mut self) {
        if self.display_rows.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let prev = if i == 0 {
            self.display_rows.len() - 1
        } else {
            i - 1
        };
        self.table_state.select(Some(prev));
    }

    pub fn attach_selected(&self) {
        if let Some(i) = self.table_state.selected() {
            if let Some(row) = self.display_rows.get(i) {
                let target = if let Some(ref parent) = row.session.parent_session_id {
                    parent.as_str()
                } else {
                    &row.session.session_id
                };
                let _ = Command::new("kitty")
                    .args([
                        "@",
                        "focus-window",
                        "--match",
                        &format!("pid:{}", row.session.pid.unwrap_or(0)),
                    ])
                    .status()
                    .or_else(|_| {
                        Command::new("tmux")
                            .args(["switch-client", "-t", target])
                            .status()
                    });
            }
        }
    }

    pub fn toggle_history(&mut self) {
        if self.history_popup.is_some() {
            self.history_popup = None;
        } else if let Some(i) = self.table_state.selected() {
            if let Some(row) = self.display_rows.get(i) {
                let target = row
                    .session
                    .parent_session_id
                    .as_ref()
                    .unwrap_or(&row.session.session_id)
                    .clone();
                self.history_popup = Some(target);
            }
        }
    }

    pub fn dismiss_history(&mut self) {
        self.history_popup = None;
    }

    /// Returns true if the history popup overlay is currently visible.
    #[allow(dead_code)]
    pub fn has_history_popup(&self) -> bool {
        self.history_popup.is_some()
    }
}

impl TuiPage for SessionsPage {
    fn title(&self) -> &str {
        "Sessions"
    }

    fn tick(&mut self, poller: &mut ClaudeStatePoller) {
        self.update_from_poller(poller);
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),   // table
                Constraint::Length(1), // footer
            ])
            .split(area);

        // Background
        f.render_widget(
            Block::default().style(Style::default().bg(BG)),
            area,
        );

        // -- Session table header info --
        let parent_count = self.display_rows.iter().filter(|r| !r.is_subagent).count();
        let active = self
            .display_rows
            .iter()
            .filter(|r| !r.is_subagent && r.session.status != ClaudeStatus::Idle)
            .count();
        let subagent_count = self.display_rows.iter().filter(|r| r.is_subagent).count();
        let block_title = if subagent_count > 0 {
            format!(
                " Sessions [{} active / {}, {} subagents] ",
                active, parent_count, subagent_count
            )
        } else {
            format!(" Sessions [{} active / {}] ", active, parent_count)
        };

        let header_cells =
            ["Session", "Status", "Activity", "Ctx%", "Directory", "Updated"]
                .iter()
                .map(|h| {
                    Cell::from(*h).style(
                        Style::default()
                            .fg(ACCENT_COLD)
                            .add_modifier(Modifier::BOLD),
                    )
                });
        let header_row = Row::new(header_cells).height(1);

        let rows: Vec<Row> = self
            .display_rows
            .iter()
            .map(|row| {
                let s = &row.session;
                let color = status_color(&s.status);
                let label = status_label(&s.status);
                let activity = format_activity(s);

                let (ctx_str, ctx_c) = match s.context_percent {
                    Some(pct) => (format!("{:.0}%", pct), ctx_color(pct)),
                    None => ("-".into(), TEXT_MUTED),
                };

                let dir = s.working_dir.as_deref().unwrap_or("-");
                let short_dir = dir
                    .strip_prefix("/home/builder/")
                    .map(|p| format!("~/{}", p))
                    .unwrap_or_else(|| dir.to_string());

                let updated = s
                    .last_updated
                    .as_deref()
                    .map(|ts| relative_time(ts))
                    .unwrap_or_else(|| "-".into());

                if row.is_subagent {
                    let tree = if row.is_last_child {
                        "\u{2514}\u{2500}"
                    } else {
                        "\u{251C}\u{2500}"
                    };
                    let agent_label = s
                        .agent_id
                        .as_deref()
                        .map(|id| if id.len() > 8 { &id[..8] } else { id })
                        .unwrap_or("agent");
                    let id_str = format!("{} {}", tree, agent_label);

                    Row::new(vec![
                        Cell::from(id_str).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(label).style(Style::default().fg(color)),
                        Cell::from(activity).style(Style::default().fg(TEXT)),
                        Cell::from(ctx_str).style(Style::default().fg(ctx_c)),
                        Cell::from(short_dir).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(updated).style(Style::default().fg(TEXT_MUTED)),
                    ])
                } else {
                    let short_id = if s.session_id.len() > 12 {
                        &s.session_id[..12]
                    } else {
                        &s.session_id
                    };

                    Row::new(vec![
                        Cell::from(short_id.to_string())
                            .style(Style::default().fg(TEXT)),
                        Cell::from(label).style(Style::default().fg(color)),
                        Cell::from(activity).style(Style::default().fg(TEXT_BRIGHT)),
                        Cell::from(ctx_str).style(Style::default().fg(ctx_c)),
                        Cell::from(short_dir).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(updated).style(Style::default().fg(TEXT_MUTED)),
                    ])
                }
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(14),
                Constraint::Length(10),
                Constraint::Length(28),
                Constraint::Length(6),
                Constraint::Percentage(30),
                Constraint::Length(6),
            ],
        )
        .header(header_row)
        .block(
            Block::default()
                .title(block_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(COLD))
                .style(Style::default().bg(BG)),
        )
        .row_highlight_style(
            Style::default()
                .bg(BG_SURFACE)
                .add_modifier(Modifier::BOLD),
        );

        f.render_stateful_widget(table, chunks[0], &mut self.table_state);

        // -- Footer --
        let footer = Paragraph::new(Line::from(vec![
            Span::styled(
                " j/k",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": navigate  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "Enter",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": attach  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "h",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": history  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "r",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": refresh", Style::default().fg(TEXT_MUTED)),
        ]))
        .style(Style::default().bg(BG));
        f.render_widget(footer, chunks[1]);

        // -- History popup overlay --
        if let Some(ref sid) = self.history_popup {
            self.render_history_popup(f, sid);
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent, poller: &mut ClaudeStatePoller) -> bool {
        use crossterm::event::KeyCode;

        if self.history_popup.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('h') => self.dismiss_history(),
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.nav_down(),
            KeyCode::Char('k') | KeyCode::Up => self.nav_up(),
            KeyCode::Enter => self.attach_selected(),
            KeyCode::Char('h') => self.toggle_history(),
            KeyCode::Char('r') => self.force_refresh(poller),
            _ => {}
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

impl SessionsPage {
    fn render_history_popup(&self, f: &mut Frame, session_id: &str) {
        let area = f.area();
        let popup_w = (area.width as u32 * 60 / 100).min(72) as u16;
        let popup_h = 16u16.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(popup_w)) / 2;
        let y = (area.height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect::new(x, y, popup_w, popup_h);

        f.render_widget(Clear, popup_area);

        let short_id = if session_id.len() > 16 {
            &session_id[..16]
        } else {
            session_id
        };
        let title = format!(" History: {} ", short_id);

        let now = Instant::now();
        let lines: Vec<Line> = self
            .history
            .get(session_id)
            .map(|entries| {
                entries
                    .iter()
                    .rev()
                    .map(|e| {
                        let ago = now.duration_since(e.timestamp).as_secs();
                        let rel = if ago < 60 {
                            format!("{}s ago", ago)
                        } else if ago < 3600 {
                            format!("{}m ago", ago / 60)
                        } else {
                            format!("{}h ago", ago / 3600)
                        };
                        Line::from(vec![
                            Span::styled(
                                format!("{:>7}  ", rel),
                                Style::default().fg(TEXT_MUTED),
                            ),
                            Span::styled(&e.text, Style::default().fg(TEXT_BRIGHT)),
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default();

        let content = if lines.is_empty() {
            Paragraph::new("  No history yet.")
                .style(Style::default().fg(TEXT_MUTED).bg(BG))
        } else {
            Paragraph::new(lines)
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: true })
        };

        let popup = content.block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT_COLD))
                .style(Style::default().bg(BG)),
        );

        f.render_widget(popup, popup_area);
    }
}
