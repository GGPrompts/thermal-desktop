//! Rich JSONL session viewer — thermal-themed ratatui TUI for subagent transcripts.
//!
//! Replaces raw `tail -f` with a structured, color-coded view of Claude Code
//! session events. Incrementally reads the JSONL file and renders parsed events
//! with thermal palette colors.
//!
//! Launched via `thc view <path>` or spawned by SwarmWatcher for subagent windows.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color as RColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;

use crate::session_log::{parse_session_event, SessionEvent, SessionEventType};

// ── Thermal color constants (from palette.rs) ───────────────────────────────

const USER_COLOR: RColor = RColor::Rgb(0xf5, 0x9e, 0x0b); // ACCENT_WARM
const ASSIST_COLOR: RColor = RColor::Rgb(0xc4, 0xb5, 0xfd); // TEXT
const THINK_COLOR: RColor = RColor::Rgb(0x3b, 0x82, 0xf6); // ACCENT_COOL
const TOOL_COLOR: RColor = RColor::Rgb(0x14, 0xb8, 0xa6); // ACCENT_NEUTRAL
const ERROR_COLOR: RColor = RColor::Rgb(0xef, 0x44, 0x44); // ACCENT_HOT
const MUTED_COLOR: RColor = RColor::Rgb(0x9b, 0x8d, 0xd1); // TEXT_MUTED
const BG_COLOR: RColor = RColor::Rgb(0x0a, 0x00, 0x10); // BG
const OK_COLOR: RColor = RColor::Rgb(0x22, 0xc5, 0x5e); // STATUS_OK

// ── Rendered line ───────────────────────────────────────────────────────────

/// A pre-rendered display line with styling info.
struct DisplayLine {
    spans: Vec<Span<'static>>,
}

// ── Viewer state ────────────────────────────────────────────────────────────

struct ViewerState {
    /// Path to the JSONL file.
    path: PathBuf,
    /// Parsed events from the file.
    events: Vec<SessionEvent>,
    /// Pre-rendered display lines.
    lines: Vec<DisplayLine>,
    /// File read offset (for incremental reads).
    file_offset: u64,
    /// Scroll offset (line index at top of viewport).
    scroll: usize,
    /// Whether auto-follow is active (scroll to bottom on new content).
    following: bool,
    /// Whether the file was modified recently (streaming indicator).
    streaming: bool,
    /// When streaming was last detected.
    last_activity: Instant,
    /// Agent ID extracted from filename (for title).
    agent_id: String,
}

impl ViewerState {
    fn new(path: PathBuf) -> Self {
        let agent_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .strip_prefix("agent-")
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
            })
            .to_string();

        Self {
            path,
            events: Vec::new(),
            lines: Vec::new(),
            file_offset: 0,
            scroll: 0,
            following: true,
            streaming: false,
            last_activity: Instant::now(),
            agent_id,
        }
    }

    /// Read new lines from the JSONL file incrementally.
    fn poll_file(&mut self) -> Result<()> {
        let metadata = std::fs::metadata(&self.path);
        let file_len = match metadata {
            Ok(m) => m.len(),
            Err(_) => return Ok(()), // File doesn't exist yet — wait.
        };

        if file_len <= self.file_offset {
            // No new data (or file truncated — reset).
            if file_len < self.file_offset {
                self.file_offset = 0;
                self.events.clear();
                self.lines.clear();
            }
            return Ok(());
        }

        let file = File::open(&self.path)?;
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(self.file_offset))?;

        let mut new_events = Vec::new();
        let mut buf = String::new();

        loop {
            buf.clear();
            let bytes_read = reader.read_line(&mut buf)?;
            if bytes_read == 0 {
                break;
            }
            self.file_offset += bytes_read as u64;

            let trimmed = buf.trim();
            if trimmed.is_empty() || !trimmed.starts_with('{') {
                continue;
            }

            let parsed = parse_session_event(trimmed);
            new_events.extend(parsed);
        }

        if !new_events.is_empty() {
            self.streaming = true;
            self.last_activity = Instant::now();

            for event in new_events {
                let new_lines = render_event(&event);
                self.lines.extend(new_lines);
                self.events.push(event);
            }
        }

        // Clear streaming indicator after 3 seconds of inactivity.
        if self.streaming && self.last_activity.elapsed() > Duration::from_secs(3) {
            self.streaming = false;
        }

        Ok(())
    }

    /// Total display lines.
    fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Scroll to the bottom.
    fn scroll_to_bottom(&mut self, viewport_height: usize) {
        let total = self.line_count();
        if total > viewport_height {
            self.scroll = total - viewport_height;
        } else {
            self.scroll = 0;
        }
    }

    /// Jump to next tool call event after current scroll position.
    fn jump_next_tool(&mut self, viewport_height: usize) {
        // Find the display line for the next tool event after current scroll.
        let mut line_idx = 0;
        for event in &self.events {
            let n = display_line_count(event);
            if line_idx > self.scroll
                && matches!(event.event_type, SessionEventType::ToolUse | SessionEventType::ToolResult)
            {
                self.scroll = line_idx;
                self.following = false;
                return;
            }
            line_idx += n;
        }
        // Wrap or no-op: scroll to bottom.
        self.scroll_to_bottom(viewport_height);
    }

    /// Jump to previous tool call event before current scroll position.
    fn jump_prev_tool(&mut self) {
        let mut positions = Vec::new();
        let mut line_idx = 0;
        for event in &self.events {
            let n = display_line_count(event);
            if matches!(
                event.event_type,
                SessionEventType::ToolUse | SessionEventType::ToolResult
            ) {
                positions.push(line_idx);
            }
            line_idx += n;
        }
        // Find the last position before current scroll.
        if let Some(&pos) = positions.iter().rev().find(|&&p| p < self.scroll) {
            self.scroll = pos;
            self.following = false;
        }
    }
}

// ── Event rendering ─────────────────────────────────────────────────────────

/// How many display lines an event will take.
fn display_line_count(event: &SessionEvent) -> usize {
    match event.event_type {
        SessionEventType::UserMessage => {
            // Header + content lines (max 5) + blank.
            let content_lines = event.content.lines().count().min(5).max(1);
            1 + content_lines + 1
        }
        SessionEventType::AssistantText => {
            // Content lines (max 4) + blank.
            let content_lines = event.content.lines().count().min(4).max(1);
            content_lines + 1
        }
        SessionEventType::Thinking => 1, // Single collapsed line.
        SessionEventType::ToolUse => 1,  // Tool header line.
        SessionEventType::ToolResult => {
            // Result summary line + blank.
            2
        }
        SessionEventType::Progress => 1,
        SessionEventType::SystemMessage => 1,
    }
}

/// Render a single event into display lines.
fn render_event(event: &SessionEvent) -> Vec<DisplayLine> {
    let mut lines = Vec::new();

    match event.event_type {
        SessionEventType::UserMessage => {
            // Timestamp extraction (HH:MM:SS).
            let ts = format_time(&event.timestamp);

            // Header line with gold accent.
            lines.push(DisplayLine {
                spans: vec![
                    Span::styled("  ", Style::default()),
                    Span::styled("┃ ", Style::default().fg(USER_COLOR)),
                    Span::styled("USER", Style::default().fg(USER_COLOR).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  {ts}"), Style::default().fg(MUTED_COLOR)),
                ],
            });

            // Content lines (max 5).
            for line in event.content.lines().take(5) {
                let text = truncate_line(line, 120);
                lines.push(DisplayLine {
                    spans: vec![
                        Span::styled("  ", Style::default()),
                        Span::styled("┃ ", Style::default().fg(USER_COLOR)),
                        Span::styled(text, Style::default().fg(RColor::Rgb(0xe9, 0xe0, 0xff))),
                    ],
                });
            }
            if event.content.lines().count() > 5 {
                let remaining = event.content.lines().count() - 5;
                lines.push(DisplayLine {
                    spans: vec![
                        Span::styled("  ", Style::default()),
                        Span::styled("┃ ", Style::default().fg(USER_COLOR)),
                        Span::styled(
                            format!("  ...{remaining} more lines"),
                            Style::default().fg(MUTED_COLOR),
                        ),
                    ],
                });
            }

            // Blank separator.
            lines.push(DisplayLine {
                spans: vec![Span::raw("")],
            });
        }

        SessionEventType::AssistantText => {
            for line in event.content.lines().take(4) {
                let text = truncate_line(line, 120);
                lines.push(DisplayLine {
                    spans: vec![
                        Span::styled("    ", Style::default()),
                        Span::styled(text, Style::default().fg(ASSIST_COLOR)),
                    ],
                });
            }
            if event.content.lines().count() > 4 {
                let remaining = event.content.lines().count() - 4;
                lines.push(DisplayLine {
                    spans: vec![
                        Span::styled("    ", Style::default()),
                        Span::styled(
                            format!("...{remaining} more lines"),
                            Style::default().fg(MUTED_COLOR),
                        ),
                    ],
                });
            }
            // Blank separator.
            lines.push(DisplayLine {
                spans: vec![Span::raw("")],
            });
        }

        SessionEventType::Thinking => {
            let preview = event
                .content
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect::<String>();
            lines.push(DisplayLine {
                spans: vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        "▸ Thinking",
                        Style::default()
                            .fg(THINK_COLOR)
                            .add_modifier(Modifier::DIM),
                    ),
                    Span::styled(
                        format!("  {preview}"),
                        Style::default()
                            .fg(MUTED_COLOR)
                            .add_modifier(Modifier::DIM),
                    ),
                ],
            });
        }

        SessionEventType::ToolUse => {
            let tool = event.tool_name.as_deref().unwrap_or("?");
            // Extract a meaningful summary from the input JSON.
            let summary = extract_tool_summary(tool, &event.content);
            let tool_style = tool_color_style(tool);

            lines.push(DisplayLine {
                spans: vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(format!("▸ {tool}"), tool_style.add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  {summary}"), Style::default().fg(MUTED_COLOR)),
                ],
            });
        }

        SessionEventType::ToolResult => {
            let tool = event.tool_name.as_deref().unwrap_or("?");
            let duration_str = event
                .duration
                .map(|d| format!("[{:.1}s]", d.as_secs_f64()))
                .unwrap_or_default();

            let (icon, color) = if event.is_error {
                ("✗", ERROR_COLOR)
            } else {
                ("✓", OK_COLOR)
            };

            // First line of output (truncated).
            let preview = event
                .content
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect::<String>();

            let total_lines = event.content.lines().count();
            let line_info = if total_lines > 1 {
                format!(" ({total_lines} lines)")
            } else {
                String::new()
            };

            lines.push(DisplayLine {
                spans: vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(format!("{icon} "), Style::default().fg(color)),
                    Span::styled(preview, Style::default().fg(MUTED_COLOR)),
                    Span::styled(line_info, Style::default().fg(MUTED_COLOR).add_modifier(Modifier::DIM)),
                    Span::styled(
                        format!("  {duration_str}"),
                        Style::default().fg(MUTED_COLOR).add_modifier(Modifier::DIM),
                    ),
                ],
            });

            // Blank separator after tool result.
            lines.push(DisplayLine {
                spans: vec![Span::raw("")],
            });
        }

        SessionEventType::Progress => {
            let tool = event.tool_name.as_deref().unwrap_or("?");
            let msg = truncate_line(&event.content, 80);
            lines.push(DisplayLine {
                spans: vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(
                        format!("⋯ {tool}: {msg}"),
                        Style::default()
                            .fg(MUTED_COLOR)
                            .add_modifier(Modifier::DIM),
                    ),
                ],
            });
        }

        SessionEventType::SystemMessage => {
            let msg = truncate_line(&event.content, 80);
            lines.push(DisplayLine {
                spans: vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        format!("◆ {msg}"),
                        Style::default()
                            .fg(MUTED_COLOR)
                            .add_modifier(Modifier::DIM),
                    ),
                ],
            });
        }
    }

    lines
}

/// Extract a meaningful summary from tool input JSON.
fn extract_tool_summary(tool: &str, input_json: &str) -> String {
    // Parse the JSON input to find key fields.
    let value: serde_json::Value = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(_) => return truncate_line(input_json, 60),
    };

    match tool {
        "Bash" => value
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| truncate_line(s, 80))
            .unwrap_or_default(),
        "Read" => value
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| shorten_path(s))
            .unwrap_or_default(),
        "Write" => value
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| shorten_path(s))
            .unwrap_or_default(),
        "Edit" => value
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| shorten_path(s))
            .unwrap_or_default(),
        "Glob" => value
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| truncate_line(s, 60))
            .unwrap_or_default(),
        "Grep" => value
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| truncate_line(s, 60))
            .unwrap_or_default(),
        "Agent" => value
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| truncate_line(s, 60))
            .unwrap_or_default(),
        _ => {
            // Generic: show first string field value.
            if let Some(obj) = value.as_object() {
                for (_k, v) in obj.iter() {
                    if let Some(s) = v.as_str() {
                        if !s.is_empty() {
                            return truncate_line(s, 60);
                        }
                    }
                }
            }
            String::new()
        }
    }
}

/// Get the color style for a tool name.
fn tool_color_style(tool: &str) -> Style {
    match tool {
        "Bash" => Style::default().fg(USER_COLOR),    // Gold for shell commands.
        "Read" | "Write" | "Edit" | "Glob" | "Grep" => Style::default().fg(TOOL_COLOR), // Teal for file ops.
        "Agent" => Style::default().fg(THINK_COLOR),  // Blue for subagent spawns.
        _ => Style::default().fg(TOOL_COLOR),
    }
}

/// Truncate a single line to max chars.
fn truncate_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or(s);
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(max - 1).collect();
        format!("{truncated}…")
    }
}

/// Shorten a file path for display (keep last 2-3 components).
fn shorten_path(path: &str) -> String {
    let p = Path::new(path);
    let components: Vec<_> = p.components().collect();
    if components.len() <= 3 {
        path.to_string()
    } else {
        let tail: PathBuf = components[components.len() - 3..].iter().collect();
        format!("…/{}", tail.display())
    }
}

/// Extract HH:MM:SS from an ISO 8601 timestamp.
fn format_time(ts: &str) -> String {
    // "2026-04-02T00:13:52.232Z" -> "00:13:52"
    if ts.len() >= 19 {
        ts[11..19].to_string()
    } else {
        ts.to_string()
    }
}

// ── UI rendering ────────────────────────────────────────────────────────────

fn render_ui(f: &mut Frame, state: &mut ViewerState) {
    let area = f.area();

    // Layout: title bar (1) + content + footer (1).
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    // ── Title bar ───────────────────────────────────────────────────────
    let streaming_indicator = if state.streaming {
        Span::styled(" ● ", Style::default().fg(ERROR_COLOR))
    } else {
        Span::styled(" ○ ", Style::default().fg(MUTED_COLOR))
    };

    let follow_indicator = if state.following {
        Span::styled(" [follow] ", Style::default().fg(OK_COLOR))
    } else {
        Span::styled("", Style::default())
    };

    let title_line = Line::from(vec![
        Span::styled(
            format!(" swarm:{}", &state.agent_id[..state.agent_id.len().min(12)]),
            Style::default()
                .fg(TOOL_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" — {} events", state.events.len()),
            Style::default().fg(MUTED_COLOR),
        ),
        streaming_indicator,
        follow_indicator,
    ]);

    let title = Paragraph::new(title_line).style(Style::default().bg(RColor::Rgb(0x12, 0x08, 0x22)));
    f.render_widget(title, chunks[0]);

    // ── Content area ────────────────────────────────────────────────────
    let content_area = chunks[1];
    let viewport_height = content_area.height as usize;

    // Auto-follow: scroll to bottom when following.
    if state.following {
        state.scroll_to_bottom(viewport_height);
    }

    // Build visible lines.
    let total_lines = state.line_count();
    let visible_start = state.scroll;
    let visible_end = (visible_start + viewport_height).min(total_lines);

    let mut display_lines: Vec<Line> = Vec::new();
    for i in visible_start..visible_end {
        if let Some(dl) = state.lines.get(i) {
            display_lines.push(Line::from(dl.spans.clone()));
        }
    }

    // Pad with empty lines if content is shorter than viewport.
    while display_lines.len() < viewport_height {
        display_lines.push(Line::from(""));
    }

    let content = Paragraph::new(display_lines).style(Style::default().bg(BG_COLOR));
    f.render_widget(content, content_area);

    // ── Scrollbar ───────────────────────────────────────────────────────
    if total_lines > viewport_height {
        let mut scrollbar_state =
            ScrollbarState::new(total_lines.saturating_sub(viewport_height)).position(state.scroll);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::default().fg(MUTED_COLOR));
        f.render_stateful_widget(scrollbar, content_area, &mut scrollbar_state);
    }

    // ── Footer ──────────────────────────────────────────────────────────
    let footer_line = Line::from(vec![
        Span::styled(" q", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled(":quit  ", Style::default().fg(MUTED_COLOR)),
        Span::styled("↑↓", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled(":scroll  ", Style::default().fg(MUTED_COLOR)),
        Span::styled("PgUp/Dn", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled(":page  ", Style::default().fg(MUTED_COLOR)),
        Span::styled("t", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled(":next-tool  ", Style::default().fg(MUTED_COLOR)),
        Span::styled("T", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled(":prev-tool  ", Style::default().fg(MUTED_COLOR)),
        Span::styled("f", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled(":follow  ", Style::default().fg(MUTED_COLOR)),
        Span::styled("G", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled(":bottom", Style::default().fg(MUTED_COLOR)),
    ]);

    let footer = Paragraph::new(footer_line).style(Style::default().bg(RColor::Rgb(0x12, 0x08, 0x22)));
    f.render_widget(footer, chunks[2]);
}

// ── Main loop ───────────────────────────────────────────────────────────────

/// Run the viewer TUI for a JSONL file.
pub async fn run(path: PathBuf) -> Result<()> {
    // Set up terminal.
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let mut state = ViewerState::new(path);

    // Initial file read.
    state.poll_file()?;

    let poll_interval = Duration::from_millis(250);

    loop {
        // Render.
        terminal.draw(|f| render_ui(f, &mut state))?;

        // Poll for events with timeout (allows periodic file polling).
        if event::poll(poll_interval)? {
            match event::read()? {
                Event::Key(key) => {
                    let viewport_height = terminal.size()?.height.saturating_sub(2) as usize;

                    match (key.code, key.modifiers) {
                        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => break,
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,

                        // Scroll up.
                        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                            state.scroll = state.scroll.saturating_sub(1);
                            state.following = false;
                        }

                        // Scroll down.
                        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                            let max_scroll = state.line_count().saturating_sub(viewport_height);
                            state.scroll = (state.scroll + 1).min(max_scroll);
                            // Re-enable follow if we scrolled to bottom.
                            if state.scroll >= max_scroll {
                                state.following = true;
                            }
                        }

                        // Page up.
                        (KeyCode::PageUp, _) => {
                            state.scroll = state.scroll.saturating_sub(viewport_height);
                            state.following = false;
                        }

                        // Page down.
                        (KeyCode::PageDown, _) => {
                            let max_scroll = state.line_count().saturating_sub(viewport_height);
                            state.scroll = (state.scroll + viewport_height).min(max_scroll);
                            if state.scroll >= max_scroll {
                                state.following = true;
                            }
                        }

                        // Home.
                        (KeyCode::Home, _) | (KeyCode::Char('g'), _) => {
                            state.scroll = 0;
                            state.following = false;
                        }

                        // End / Go to bottom.
                        (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
                            state.scroll_to_bottom(viewport_height);
                            state.following = true;
                        }

                        // Follow toggle.
                        (KeyCode::Char('f'), _) => {
                            state.following = !state.following;
                            if state.following {
                                state.scroll_to_bottom(viewport_height);
                            }
                        }

                        // Next tool call.
                        (KeyCode::Char('t'), _) => {
                            state.jump_next_tool(viewport_height);
                        }

                        // Previous tool call.
                        (KeyCode::Char('T'), _) => {
                            state.jump_prev_tool();
                        }

                        _ => {}
                    }
                }
                Event::Mouse(mouse) => {
                    use crossterm::event::MouseEventKind;
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            state.scroll = state.scroll.saturating_sub(3);
                            state.following = false;
                        }
                        MouseEventKind::ScrollDown => {
                            let viewport_height =
                                terminal.size()?.height.saturating_sub(2) as usize;
                            let max_scroll = state.line_count().saturating_sub(viewport_height);
                            state.scroll = (state.scroll + 3).min(max_scroll);
                            if state.scroll >= max_scroll {
                                state.following = true;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        // Poll file for new content.
        state.poll_file()?;
    }

    // Restore terminal.
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;

    Ok(())
}
