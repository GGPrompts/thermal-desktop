//! thermal-monitor — a ratatui TUI for watching active Claude Code sessions.
//!
//! Reads session state from `/tmp/claude-code-state/` via thermal-core's
//! `ClaudeStatePoller` and renders a thermal-styled dashboard table.

use std::io;
use std::process::Command;
use std::time::Duration;

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Terminal,
};

use thermal_core::{
    palette::{self, ThermalPalette},
    ClaudeSessionState, ClaudeStatePoller, ClaudeStatus,
};

// ---------------------------------------------------------------------------
// Palette helpers
// ---------------------------------------------------------------------------

/// Convert a ThermalPalette `[f32; 4]` constant to a ratatui `Color::Rgb`.
const fn pal(c: [f32; 4]) -> Color {
    Color::Rgb(
        (c[0] * 255.0) as u8,
        (c[1] * 255.0) as u8,
        (c[2] * 255.0) as u8,
    )
}

/// Convert a thermal-core `palette::Color` to ratatui `Color::Rgb`.
fn tc(c: palette::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

const BG: Color = pal(ThermalPalette::BG);
const BG_SURFACE: Color = pal(ThermalPalette::BG_SURFACE);
const TEXT: Color = pal(ThermalPalette::TEXT);
const TEXT_BRIGHT: Color = pal(ThermalPalette::TEXT_BRIGHT);
const TEXT_MUTED: Color = pal(ThermalPalette::TEXT_MUTED);
const COLD: Color = pal(ThermalPalette::COLD);
const ACCENT_COLD: Color = pal(ThermalPalette::ACCENT_COLD);

/// Map a `ClaudeStatus` to a ratatui color.
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

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct App {
    poller: ClaudeStatePoller,
    sessions: Vec<ClaudeSessionState>,
    table_state: TableState,
    should_quit: bool,
}

impl App {
    fn new() -> anyhow::Result<Self> {
        let poller = ClaudeStatePoller::new()?;
        let sessions = poller.get_all();
        let mut table_state = TableState::default();
        if !sessions.is_empty() {
            table_state.select(Some(0));
        }
        Ok(Self {
            poller,
            sessions,
            table_state,
            should_quit: false,
        })
    }

    fn tick(&mut self) {
        let updated = self.poller.poll();
        if !updated.is_empty() {
            self.sessions = updated;
            self.clamp_selection();
        }
    }

    fn force_refresh(&mut self) {
        self.sessions = self.poller.get_all();
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.sessions.is_empty() {
            self.table_state.select(None);
        } else if let Some(i) = self.table_state.selected() {
            if i >= self.sessions.len() {
                self.table_state.select(Some(self.sessions.len() - 1));
            }
        }
    }

    fn nav_down(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let next = if i >= self.sessions.len() - 1 { 0 } else { i + 1 };
        self.table_state.select(Some(next));
    }

    fn nav_up(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let prev = if i == 0 { self.sessions.len() - 1 } else { i - 1 };
        self.table_state.select(Some(prev));
    }

    fn attach_selected(&self) {
        if let Some(i) = self.table_state.selected() {
            if let Some(session) = self.sessions.get(i) {
                let _ = Command::new("tmux")
                    .args(["switch-client", "-t", &session.session_id])
                    .status();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UI rendering
// ---------------------------------------------------------------------------

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(5),   // table
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    // Background
    f.render_widget(Block::default().style(Style::default().bg(BG)), f.area());

    // -- Header --
    let total = app.sessions.len();
    let active = app
        .sessions
        .iter()
        .filter(|s| s.status != ClaudeStatus::Idle)
        .count();
    let header_text = format!("THERMAL MONITOR  [{} active / {} total]", active, total);
    let header = Paragraph::new(header_text)
        .alignment(Alignment::Center)
        .style(
            Style::default()
                .fg(TEXT_BRIGHT)
                .bg(BG_SURFACE)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(COLD))
                .style(Style::default().bg(BG_SURFACE)),
        );
    f.render_widget(header, chunks[0]);

    // -- Session table --
    let header_cells = ["Session", "Status", "Tool", "Ctx%", "Directory", "Updated"]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            )
        });
    let header_row = Row::new(header_cells).height(1);

    let rows: Vec<Row> = app
        .sessions
        .iter()
        .map(|s| {
            let color = status_color(&s.status);
            let label = status_label(&s.status);

            let tool = s
                .current_tool
                .as_deref()
                .unwrap_or("-")
                .to_string();

            let ctx = match s.context_percent {
                Some(pct) => format!("{:.0}%", pct),
                None => "-".to_string(),
            };
            let ctx_color = s
                .context_percent
                .map(|pct| tc(palette::thermal_gradient(pct / 100.0)))
                .unwrap_or(TEXT_MUTED);

            let dir = s
                .working_dir
                .as_deref()
                .unwrap_or("-");
            // Shorten home prefix
            let short_dir = dir
                .strip_prefix("/home/builder/")
                .map(|p| format!("~/{}", p))
                .unwrap_or_else(|| dir.to_string());

            let updated = s
                .last_updated
                .as_deref()
                .and_then(|ts| ts.split('T').nth(1))
                .and_then(|t| t.split('.').next())
                .unwrap_or("-")
                .to_string();

            // Short session ID
            let short_id = if s.session_id.len() > 12 {
                &s.session_id[..12]
            } else {
                &s.session_id
            };

            Row::new(vec![
                Cell::from(short_id.to_string()).style(Style::default().fg(TEXT)),
                Cell::from(label).style(Style::default().fg(color)),
                Cell::from(tool).style(Style::default().fg(TEXT)),
                Cell::from(ctx).style(Style::default().fg(ctx_color)),
                Cell::from(short_dir).style(Style::default().fg(TEXT_MUTED)),
                Cell::from(updated).style(Style::default().fg(TEXT_MUTED)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),     // Session
            Constraint::Length(10),     // Status
            Constraint::Length(12),     // Tool
            Constraint::Length(6),      // Ctx%
            Constraint::Percentage(40), // Directory
            Constraint::Length(10),     // Updated
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(COLD))
            .style(Style::default().bg(BG)),
    )
    .row_highlight_style(
        Style::default()
            .bg(BG_SURFACE)
            .add_modifier(Modifier::BOLD),
    );

    f.render_stateful_widget(table, chunks[1], &mut app.table_state);

    // -- Footer --
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(
            " q",
            Style::default()
                .fg(ACCENT_COLD)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(": quit  ", Style::default().fg(TEXT_MUTED)),
        Span::styled(
            "j/k",
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
            "r",
            Style::default()
                .fg(ACCENT_COLD)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(": refresh", Style::default().fg(TEXT_MUTED)),
    ]))
    .style(Style::default().bg(BG));
    f.render_widget(footer, chunks[2]);
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("thermal_monitor=info")
        .init();

    // Terminal setup
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new()?;

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        // Poll for events with 250ms timeout (tick rate)
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                    KeyCode::Char('j') | KeyCode::Down => app.nav_down(),
                    KeyCode::Char('k') | KeyCode::Up => app.nav_up(),
                    KeyCode::Enter => app.attach_selected(),
                    KeyCode::Char('r') => app.force_refresh(),
                    _ => {}
                }
            }
        }

        app.tick();

        if app.should_quit {
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
