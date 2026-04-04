//! Rich JSONL session viewer — thermal-themed ratatui TUI for subagent transcripts.
//!
//! Replaces raw `tail -f` with a structured, color-coded view of Claude Code
//! session events. Incrementally reads the JSONL file and renders parsed events
//! with thermal palette colors.
//!
//! Launched via `thc view <path>` or spawned by SwarmWatcher for subagent windows.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color as RColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;
use tracing::debug;

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

/// Threshold after which idle stream is considered fully complete (not just paused).
const COMPLETION_THRESHOLD: Duration = Duration::from_secs(10);

// ── Rendered line ───────────────────────────────────────────────────────────

/// A pre-rendered display line with styling info.
struct DisplayLine {
    spans: Vec<Span<'static>>,
    /// Index of the source event that produced this line (for expansion mapping).
    event_index: usize,
    /// Whether this line is an expandable "+N more lines" indicator.
    is_expandable: bool,
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
    /// Current terminal width (updated each frame).
    terminal_width: u16,
    /// Last content area width used for rendering lines (excludes scrollbar).
    rendered_width: u16,
    /// Whether the final report has been expanded (done once when stream stops).
    final_expanded: bool,
    /// Set of event indices that the user has manually expanded.
    expanded_events: HashSet<usize>,
    /// Whether all events are expanded (toggle-all state).
    all_expanded: bool,
    /// Whether the session is considered fully complete (no changes for COMPLETION_THRESHOLD).
    completed: bool,
    /// Whether the TTS completion announcement has been fired (one-shot guard).
    completion_announced: bool,
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
            terminal_width: 120,
            rendered_width: 0,
            final_expanded: false,
            expanded_events: HashSet::new(),
            all_expanded: false,
            completed: false,
            completion_announced: false,
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
            self.final_expanded = false; // New content — reset expansion.

            // Use rendered_width (accounts for scrollbar); fall back to
            // terminal_width on first render before layout is known.
            let width = if self.rendered_width > 0 {
                self.rendered_width as usize
            } else {
                self.terminal_width as usize
            };
            for event in new_events {
                let idx = self.events.len();
                let expanded = self.all_expanded || self.expanded_events.contains(&idx);
                let new_lines = render_event(&event, width, expanded, idx);
                self.lines.extend(new_lines);
                self.events.push(event);
            }
        }

        // Clear streaming indicator after 3 seconds of inactivity.
        if self.streaming && self.last_activity.elapsed() > Duration::from_secs(3) {
            self.streaming = false;
        }

        // Expand the final assistant text when the stream goes quiet.
        // This shows the full subagent report instead of the "+N lines" collapsed view.
        if !self.streaming && !self.final_expanded && !self.events.is_empty() {
            self.expand_final_report();
        }

        // Mark session as completed after COMPLETION_THRESHOLD of inactivity.
        if !self.streaming
            && !self.completed
            && !self.events.is_empty()
            && self.last_activity.elapsed() > COMPLETION_THRESHOLD
        {
            self.completed = true;
            debug!(agent = %self.agent_id, "Session marked as completed");

            // Re-render to apply the final summary highlight.
            self.rerender_all_lines();

            // Announce completion via TTS (one-shot).
            if !self.completion_announced {
                self.completion_announced = true;
                self.announce_completion();
            }
        }

        // Reset completion state if new content arrives (stream resumed).
        if self.streaming && self.completed {
            self.completed = false;
            // Don't reset completion_announced — only announce once per session.
            self.rerender_all_lines(); // Remove highlight styling.
        }

        Ok(())
    }

    /// Extract the final summary text (last AssistantText event content).
    fn final_summary_text(&self) -> Option<&str> {
        self.events
            .iter()
            .rev()
            .find(|e| e.event_type == SessionEventType::AssistantText)
            .map(|e| e.content.as_str())
    }

    /// Truncate text to approximately 2 sentences for TTS readout.
    fn truncate_for_tts(text: &str) -> String {
        // Find sentence boundaries (. ! ?) and take first 2.
        let mut sentence_count = 0;
        let mut end_pos = 0;

        for (i, ch) in text.char_indices() {
            if ch == '.' || ch == '!' || ch == '?' {
                // Check it's not part of a number/abbreviation (crude heuristic).
                let next_char = text[i + ch.len_utf8()..].chars().next();
                if next_char.is_none() || next_char == Some(' ') || next_char == Some('\n') {
                    sentence_count += 1;
                    end_pos = i + ch.len_utf8();
                    if sentence_count >= 2 {
                        break;
                    }
                }
            }
        }

        if sentence_count >= 1 && end_pos > 0 {
            text[..end_pos].trim().to_string()
        } else {
            // No sentence boundary found — take first 200 chars.
            let limit = text.char_indices().nth(200).map(|(i, _)| i).unwrap_or(text.len());
            text[..limit].trim().to_string()
        }
    }

    /// Fire `thc say` with the final summary (non-blocking subprocess).
    fn announce_completion(&self) {
        let summary = match self.final_summary_text() {
            Some(text) if !text.is_empty() => text,
            _ => return,
        };

        let tts_text = Self::truncate_for_tts(summary);
        if tts_text.is_empty() {
            return;
        }

        let agent_label = &self.agent_id[..self.agent_id.len().min(8)];
        let announcement = format!("Agent {} complete. {}", agent_label, tts_text);

        debug!(agent = %self.agent_id, text_len = announcement.len(), "Announcing completion via TTS");

        // spawn() is already non-blocking — no thread needed.
        match StdCommand::new("thc")
            .args(["say", &announcement])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_child) => {
                // Fire-and-forget — don't wait for TTS to finish.
            }
            Err(e) => {
                // TTS is best-effort; don't crash the viewer.
                debug!("thc say failed: {e}");
            }
        }
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
        let mut line_idx = 0;
        for (i, event) in self.events.iter().enumerate() {
            let expanded = self.is_event_expanded(i);
            let n = display_line_count(event, expanded);
            if line_idx > self.scroll
                && matches!(event.event_type, SessionEventType::ToolUse | SessionEventType::ToolResult)
            {
                self.scroll = line_idx;
                self.following = false;
                return;
            }
            line_idx += n;
        }
        self.scroll_to_bottom(viewport_height);
    }

    /// Jump to previous tool call event before current scroll position.
    fn jump_prev_tool(&mut self) {
        let mut positions = Vec::new();
        let mut line_idx = 0;
        for (i, event) in self.events.iter().enumerate() {
            let expanded = self.is_event_expanded(i);
            let n = display_line_count(event, expanded);
            if matches!(
                event.event_type,
                SessionEventType::ToolUse | SessionEventType::ToolResult
            ) {
                positions.push(line_idx);
            }
            line_idx += n;
        }
        if let Some(&pos) = positions.iter().rev().find(|&&p| p < self.scroll) {
            self.scroll = pos;
            self.following = false;
        }
    }

    /// Whether event at index should be rendered expanded.
    fn is_event_expanded(&self, idx: usize) -> bool {
        // User manual toggle takes priority.
        if self.all_expanded || self.expanded_events.contains(&idx) {
            return true;
        }
        if !self.final_expanded {
            return false;
        }
        // Find the last AssistantText event — that's the final report.
        let last_assistant = self
            .events
            .iter()
            .rposition(|e| e.event_type == SessionEventType::AssistantText);
        last_assistant == Some(idx)
    }

    /// Toggle expansion for the event that produced the display line at `display_idx`.
    /// Returns true if a toggle actually happened.
    fn toggle_expansion_at(&mut self, display_idx: usize, viewport_height: usize) -> bool {
        let line = match self.lines.get(display_idx) {
            Some(l) => l,
            None => return false,
        };
        if !line.is_expandable {
            return false;
        }
        let event_idx = line.event_index;
        if self.expanded_events.contains(&event_idx) {
            self.expanded_events.remove(&event_idx);
        } else {
            self.expanded_events.insert(event_idx);
        }
        // Remember current scroll position relative to the toggled line.
        let old_scroll = self.scroll;
        self.rerender_all_lines();
        // If we collapsed something above the viewport, adjust scroll to keep context.
        let new_total = self.line_count();
        if old_scroll > new_total.saturating_sub(viewport_height) {
            self.scroll = new_total.saturating_sub(viewport_height);
        } else {
            self.scroll = old_scroll;
        }
        true
    }

    /// Toggle expand/collapse all events.
    fn toggle_all_expansion(&mut self, viewport_height: usize) {
        self.all_expanded = !self.all_expanded;
        if !self.all_expanded {
            self.expanded_events.clear();
        }
        let old_scroll = self.scroll;
        self.rerender_all_lines();
        let new_total = self.line_count();
        if old_scroll > new_total.saturating_sub(viewport_height) {
            self.scroll = new_total.saturating_sub(viewport_height);
        } else {
            self.scroll = old_scroll;
        }
    }

    /// Find the display line index that the cursor is on (top of viewport + offset).
    /// For now, use the first visible expandable line, or support Enter on any line
    /// that belongs to a collapsible event. We use the "current line" = scroll position.
    fn cursor_display_line(&self, viewport_height: usize) -> usize {
        // The "cursor" is conceptually the first visible line.
        // Users scroll to a "+N more lines" indicator and press Enter.
        // We search visible lines for the first expandable one from the top.
        let visible_start = self.scroll;
        let visible_end = (visible_start + viewport_height).min(self.line_count());
        for i in visible_start..visible_end {
            if let Some(dl) = self.lines.get(i) {
                if dl.is_expandable {
                    return i;
                }
            }
        }
        visible_start
    }

    /// Expand the final assistant text event to show the full report.
    fn expand_final_report(&mut self) {
        self.final_expanded = true;
        self.rerender_all_lines();

        // Auto-scroll to the final summary if follow mode is active.
        // (Scroll will be applied in the render loop via `following`.)
    }

    /// Check if a given event index is the last AssistantText event.
    fn is_last_assistant_text(&self, idx: usize) -> bool {
        self.events
            .iter()
            .rposition(|e| e.event_type == SessionEventType::AssistantText)
            == Some(idx)
    }

    /// Re-render all display lines (e.g., after width change or expansion toggle).
    fn rerender_all_lines(&mut self) {
        // Use rendered_width if available; fall back to terminal_width
        // before the first frame when rendered_width is still 0.
        let width = if self.rendered_width > 0 {
            self.rendered_width as usize
        } else {
            self.terminal_width as usize
        };
        self.lines.clear();
        for (i, event) in self.events.iter().enumerate() {
            let expanded = self.is_event_expanded(i);
            let highlight = self.completed && self.is_last_assistant_text(i);
            let new_lines = if highlight {
                render_event_highlighted(event, width, expanded, i)
            } else {
                render_event(event, width, expanded, i)
            };
            self.lines.extend(new_lines);
        }
    }

    /// Update the effective content width and re-render if it changed.
    fn update_content_width(&mut self, content_area_width: u16, has_scrollbar: bool) {
        let effective = if has_scrollbar {
            content_area_width.saturating_sub(1)
        } else {
            content_area_width
        };
        if effective != self.rendered_width {
            self.rendered_width = effective;
            self.rerender_all_lines();
        }
    }
}

// ── Event rendering ─────────────────────────────────────────────────────────

/// How many display lines an event will take.
fn display_line_count(event: &SessionEvent, expanded: bool) -> usize {
    match event.event_type {
        SessionEventType::UserMessage => {
            let max = if expanded { 100 } else { 5 };
            let content_lines = event.content.lines().count().min(max).max(1);
            let overflow = if event.content.lines().count() > max { 1 } else { 0 };
            1 + content_lines + overflow + 1
        }
        SessionEventType::AssistantText => {
            let max = if expanded { 200 } else { 4 };
            let content_lines = event.content.lines().count().min(max).max(1);
            let overflow = if event.content.lines().count() > max { 1 } else { 0 };
            content_lines + overflow + 1
        }
        SessionEventType::Thinking => 1,
        SessionEventType::ToolUse => 1,
        SessionEventType::ToolResult => 2,
        SessionEventType::Progress => 1,
        SessionEventType::SystemMessage => 1,
    }
}

/// Render a single event into display lines.
/// `max_width` is the usable terminal width (columns minus margins).
/// `expanded` controls whether assistant text shows all lines (true) or is collapsed (false).
/// `event_idx` is the index of this event in the events vec (for expansion mapping).
/// `highlighted` adds a visual accent (used for the final summary after completion).
fn render_event(event: &SessionEvent, max_width: usize, expanded: bool, event_idx: usize) -> Vec<DisplayLine> {
    render_event_inner(event, max_width, expanded, event_idx, false)
}

fn render_event_highlighted(event: &SessionEvent, max_width: usize, expanded: bool, event_idx: usize) -> Vec<DisplayLine> {
    render_event_inner(event, max_width, expanded, event_idx, true)
}

fn render_event_inner(event: &SessionEvent, max_width: usize, expanded: bool, event_idx: usize, highlighted: bool) -> Vec<DisplayLine> {
    let mut lines = Vec::new();
    // Content area width after accounting for left margin/gutter (~6 chars).
    let content_width = max_width.saturating_sub(6);

    // Helper: create a plain (non-expandable) display line for this event.
    macro_rules! dl {
        ($spans:expr) => {
            DisplayLine { spans: $spans, event_index: event_idx, is_expandable: false }
        };
    }

    match event.event_type {
        SessionEventType::UserMessage => {
            let ts = format_time(&event.timestamp);

            lines.push(dl!(vec![
                Span::styled("  ", Style::default()),
                Span::styled("┃ ", Style::default().fg(USER_COLOR)),
                Span::styled("USER", Style::default().fg(USER_COLOR).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {ts}"), Style::default().fg(MUTED_COLOR)),
            ]));

            let max_lines = if expanded { 100 } else { 5 };
            for line in event.content.lines().take(max_lines) {
                let text = truncate_line(line, content_width);
                lines.push(dl!(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled("┃ ", Style::default().fg(USER_COLOR)),
                    Span::styled(text, Style::default().fg(RColor::Rgb(0xe9, 0xe0, 0xff))),
                ]));
            }
            let total = event.content.lines().count();
            if total > max_lines {
                let remaining = total - max_lines;
                lines.push(DisplayLine {
                    spans: vec![
                        Span::styled("  ", Style::default()),
                        Span::styled("┃ ", Style::default().fg(USER_COLOR)),
                        Span::styled(
                            format!("  ▸ +{remaining} more lines"),
                            Style::default().fg(TOOL_COLOR),
                        ),
                    ],
                    event_index: event_idx,
                    is_expandable: true,
                });
            } else if expanded && total > 5 {
                // Show a collapse indicator when expanded.
                lines.push(DisplayLine {
                    spans: vec![
                        Span::styled("  ", Style::default()),
                        Span::styled("┃ ", Style::default().fg(USER_COLOR)),
                        Span::styled(
                            "  ▾ collapse".to_string(),
                            Style::default().fg(TOOL_COLOR),
                        ),
                    ],
                    event_index: event_idx,
                    is_expandable: true,
                });
            }

            lines.push(dl!(vec![Span::raw("")]));
        }

        SessionEventType::AssistantText => {
            // Highlighted final summary gets an accent left border.
            let (gutter, text_color) = if highlighted {
                (" \u{2503} ", OK_COLOR) // ┃ in green for final summary
            } else {
                ("    ", ASSIST_COLOR)
            };

            // Insert a separator line before highlighted summary.
            if highlighted {
                lines.push(dl!(vec![
                    Span::styled(
                        " \u{2501}\u{2501}\u{2501} Final Summary \u{2501}\u{2501}\u{2501}",
                        Style::default()
                            .fg(OK_COLOR)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
            }

            let max_lines = if expanded { 200 } else { 4 };
            for line in event.content.lines().take(max_lines) {
                let text = truncate_line(line, content_width);
                lines.push(dl!(vec![
                    Span::styled(gutter.to_string(), Style::default().fg(text_color)),
                    Span::styled(text, Style::default().fg(text_color)),
                ]));
            }
            let total = event.content.lines().count();
            if total > max_lines {
                let remaining = total - max_lines;
                lines.push(DisplayLine {
                    spans: vec![
                        Span::styled(gutter.to_string(), Style::default().fg(text_color)),
                        Span::styled(
                            format!("\u{25b8} +{remaining} more lines"),
                            Style::default().fg(TOOL_COLOR),
                        ),
                    ],
                    event_index: event_idx,
                    is_expandable: true,
                });
            } else if expanded && total > 4 {
                lines.push(DisplayLine {
                    spans: vec![
                        Span::styled(gutter.to_string(), Style::default().fg(text_color)),
                        Span::styled(
                            "\u{25be} collapse".to_string(),
                            Style::default().fg(TOOL_COLOR),
                        ),
                    ],
                    event_index: event_idx,
                    is_expandable: true,
                });
            }
            lines.push(dl!(vec![Span::raw("")]));
        }

        SessionEventType::Thinking => {
            let preview = truncate_line(
                event.content.lines().next().unwrap_or(""),
                content_width.saturating_sub(14),
            );
            lines.push(dl!(vec![
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
            ]));
        }

        SessionEventType::ToolUse => {
            let tool = event.tool_name.as_deref().unwrap_or("?");
            let summary = extract_tool_summary(tool, &event.content, content_width.saturating_sub(tool.len() + 6));
            let tool_style = tool_color_style(tool);

            lines.push(dl!(vec![
                Span::styled("  ", Style::default()),
                Span::styled(format!("▸ {tool}"), tool_style.add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {summary}"), Style::default().fg(MUTED_COLOR)),
            ]));
        }

        SessionEventType::ToolResult => {
            let duration_str = String::new();

            let (icon, color) = if event.is_error {
                ("✗", ERROR_COLOR)
            } else {
                ("✓", OK_COLOR)
            };

            let preview_width = content_width.saturating_sub(6);
            let preview = truncate_line(
                event.content.lines().next().unwrap_or(""),
                preview_width,
            );

            let total_lines = event.content.lines().count();
            let line_info = if total_lines > 1 {
                format!(" +{} lines", total_lines - 1)
            } else {
                String::new()
            };

            lines.push(dl!(vec![
                Span::styled("    ", Style::default()),
                Span::styled(format!("{icon} "), Style::default().fg(color)),
                Span::styled(preview, Style::default().fg(MUTED_COLOR)),
                Span::styled(line_info, Style::default().fg(MUTED_COLOR).add_modifier(Modifier::DIM)),
                Span::styled(
                    format!("  {duration_str}"),
                    Style::default().fg(MUTED_COLOR).add_modifier(Modifier::DIM),
                ),
            ]));

            lines.push(dl!(vec![Span::raw("")]));
        }

        SessionEventType::Progress => {
            let tool = event.tool_name.as_deref().unwrap_or("?");
            let msg = truncate_line(&event.content, content_width.saturating_sub(tool.len() + 4));
            lines.push(dl!(vec![
                Span::styled("    ", Style::default()),
                Span::styled(
                    format!("⋯ {tool}: {msg}"),
                    Style::default()
                        .fg(MUTED_COLOR)
                        .add_modifier(Modifier::DIM),
                ),
            ]));
        }

        SessionEventType::SystemMessage => {
            let msg = truncate_line(&event.content, content_width.saturating_sub(4));
            lines.push(dl!(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("◆ {msg}"),
                    Style::default()
                        .fg(MUTED_COLOR)
                        .add_modifier(Modifier::DIM),
                ),
            ]));
        }
    }

    lines
}

/// Extract a meaningful summary from tool input JSON.
fn extract_tool_summary(tool: &str, input_json: &str, max_width: usize) -> String {
    let value: serde_json::Value = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(_) => return truncate_line(input_json, max_width),
    };

    match tool {
        "Bash" => value
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| truncate_line(s, max_width))
            .unwrap_or_default(),
        "Read" | "Write" | "Edit" => value
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| shorten_path(s))
            .unwrap_or_default(),
        "Glob" | "Grep" => value
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| truncate_line(s, max_width))
            .unwrap_or_default(),
        "Agent" => value
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| truncate_line(s, max_width))
            .unwrap_or_default(),
        _ => {
            if let Some(obj) = value.as_object() {
                for (_k, v) in obj.iter() {
                    if let Some(s) = v.as_str() {
                        if !s.is_empty() {
                            return truncate_line(s, max_width);
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
    let status_indicator = if state.completed {
        Span::styled(
            " \u{2713} Complete ",
            Style::default()
                .fg(OK_COLOR)
                .add_modifier(Modifier::BOLD),
        )
    } else if state.streaming {
        Span::styled(" \u{25cf} ", Style::default().fg(ERROR_COLOR))
    } else {
        Span::styled(" \u{25cb} ", Style::default().fg(MUTED_COLOR))
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
            format!(" \u{2014} {} events", state.events.len()),
            Style::default().fg(MUTED_COLOR),
        ),
        status_indicator,
        follow_indicator,
    ]);

    let title = Paragraph::new(title_line).style(Style::default().bg(RColor::Rgb(0x12, 0x08, 0x22)));
    f.render_widget(title, chunks[0]);

    // ── Content area ────────────────────────────────────────────────────
    let content_area = chunks[1];
    let viewport_height = content_area.height as usize;

    // Clear the entire content area first to prevent stale characters
    // from previous frames (scrollbar tracks, gutter fills) lingering.
    let bg_clear = Block::default().style(Style::default().bg(BG_COLOR));
    f.render_widget(bg_clear, content_area);

    // Always reserve 1 column for the scrollbar gutter to avoid width
    // oscillation: adding/removing the scrollbar column triggers rerender
    // which can change line count, toggling scrollbar presence each frame.
    state.update_content_width(content_area.width, true);

    let total_lines = state.line_count();
    let has_scrollbar = total_lines > viewport_height;

    // Auto-follow: scroll to bottom when following.
    if state.following {
        state.scroll_to_bottom(viewport_height);
    }

    // Build visible lines.
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

    // Text area always excludes the scrollbar column.
    let text_area = Rect {
        width: content_area.width.saturating_sub(1),
        ..content_area
    };

    let content = Paragraph::new(display_lines).style(Style::default().bg(BG_COLOR));
    f.render_widget(content, text_area);

    // ── Scrollbar ───────────────────────────────────────────────────────
    if has_scrollbar {
        let mut scrollbar_state =
            ScrollbarState::new(total_lines.saturating_sub(viewport_height)).position(state.scroll);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::default().fg(MUTED_COLOR));
        f.render_stateful_widget(scrollbar, content_area, &mut scrollbar_state);
    }

    // ── Footer ──────────────────────────────────────────────────────────
    let footer_line = if state.completed {
        Line::from(vec![
            Span::styled(
                " \u{2713} Session complete ",
                Style::default()
                    .fg(OK_COLOR)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("\u{2014} ", Style::default().fg(MUTED_COLOR)),
            Span::styled("q", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled(" to close  ", Style::default().fg(MUTED_COLOR)),
            Span::styled("\u{2191}\u{2193}", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled(":scroll  ", Style::default().fg(MUTED_COLOR)),
            Span::styled("e", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled(":expand-all  ", Style::default().fg(MUTED_COLOR)),
            Span::styled("G", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled(":bottom", Style::default().fg(MUTED_COLOR)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" q", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled(":quit  ", Style::default().fg(MUTED_COLOR)),
            Span::styled("\u{2191}\u{2193}", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
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
            Span::styled(":bottom  ", Style::default().fg(MUTED_COLOR)),
            Span::styled("\u{21b5}", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled(":expand  ", Style::default().fg(MUTED_COLOR)),
            Span::styled("e", Style::default().fg(TOOL_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled(":expand-all", Style::default().fg(MUTED_COLOR)),
        ])
    };

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
        // Update terminal width for dynamic truncation.
        state.terminal_width = terminal.size()?.width;

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

                        // Expand/collapse: Enter or Space toggles the nearest
                        // expandable line visible in the viewport.
                        (KeyCode::Enter, _) | (KeyCode::Char(' '), _) => {
                            let target = state.cursor_display_line(viewport_height);
                            state.toggle_expansion_at(target, viewport_height);
                        }

                        // Toggle-all expand/collapse.
                        (KeyCode::Char('e'), _) => {
                            state.toggle_all_expansion(viewport_height);
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
