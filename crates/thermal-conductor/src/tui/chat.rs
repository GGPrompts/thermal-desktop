//! Chat page — read-only message bus log for the TUI.
//!
//! Connects to `messages.sock` and displays the message stream as a scrollable
//! chat log. Input has moved to the Sessions tab inline chat bar.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
};

use thermal_core::{
    ClaudeStatePoller,
    message::{AgentId, Message, MessageType},
    palette::ThermalPalette,
};

use super::TuiPage;

// ---------------------------------------------------------------------------
// Palette
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

// Agent type → color mapping for badges.
fn agent_color(agent_type: &str) -> Color {
    match agent_type {
        "claude" => pal(ThermalPalette::ACCENT_WARM),
        "codex" => pal(ThermalPalette::ACCENT_COOL),
        "copilot" => pal(ThermalPalette::ACCENT_HOT),
        "user" => pal(ThermalPalette::WARM),
        "dispatcher" => pal(ThermalPalette::MILD),
        "system" => TEXT_MUTED,
        _ => TEXT,
    }
}

// ---------------------------------------------------------------------------
// Socket path
// ---------------------------------------------------------------------------

fn messages_socket_path() -> PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    PathBuf::from(format!("/run/user/{uid}/thermal/messages.sock"))
}

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

/// Non-blocking connection to the messages daemon.
struct BusConnection {
    reader: BufReader<UnixStream>,
    /// Buffer for partial line reads.
    line_buf: String,
}

impl BusConnection {
    /// Attempt to connect and send a Subscribe message.
    fn connect(since_seq: u64) -> Option<Self> {
        let path = messages_socket_path();
        let stream = UnixStream::connect(&path).ok()?;
        stream.set_nonblocking(true).ok()?;

        // Build subscribe message.
        let subscribe = Message {
            seq: 0,
            ts: 0,
            from: AgentId::new("user", "tui"),
            to: AgentId::new("daemon", "bus"),
            context_id: None,
            project: None,
            content: String::new(),
            msg_type: MessageType::Subscribe {
                since_seq: if since_seq > 0 { Some(since_seq) } else { None },
            },
            metadata: Default::default(),
        };

        let mut json = serde_json::to_string(&subscribe).ok()?;
        json.push('\n');

        // Briefly set blocking for the subscribe write.
        stream.set_nonblocking(false).ok()?;
        let mut write_stream = stream.try_clone().ok()?;
        write_stream.write_all(json.as_bytes()).ok()?;
        stream.set_nonblocking(true).ok()?;

        Some(Self {
            reader: BufReader::new(stream),
            line_buf: String::new(),
        })
    }

    /// Try to read any available messages (non-blocking).
    fn poll(&mut self) -> Vec<Message> {
        let mut msgs = Vec::new();
        loop {
            self.line_buf.clear();
            match self.reader.read_line(&mut self.line_buf) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = self.line_buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(msg) = serde_json::from_str::<Message>(trimmed) {
                        msgs.push(msg);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break, // Connection lost
            }
        }
        msgs
    }

}

// ---------------------------------------------------------------------------
// Chat display entry
// ---------------------------------------------------------------------------

#[allow(dead_code)]
struct ChatEntry {
    timestamp: String,
    from: AgentId,
    content: String,
    msg_type: MessageType,
    seq: u64,
    project: Option<String>,
}

impl ChatEntry {
    fn from_message(msg: &Message) -> Self {
        // Convert ms timestamp to HH:MM:SS.
        let secs = msg.ts / 1000;
        let h = (secs / 3600) % 24;
        let m = (secs / 60) % 60;
        let s = secs % 60;
        let timestamp = format!("{h:02}:{m:02}:{s:02}");

        Self {
            timestamp,
            from: msg.from.clone(),
            content: msg.content.clone(),
            msg_type: msg.msg_type.clone(),
            seq: msg.seq,
            project: msg.project.clone(),
        }
    }

    /// Render as a styled Line for display (fully owned, no borrows).
    fn to_line(&self) -> Line<'static> {
        let color = agent_color(&self.from.agent_type);

        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(
                format!("[{}] ", self.timestamp),
                Style::default().fg(TEXT_MUTED),
            ),
            Span::styled(
                format!("@{}", self.from),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ];

        // Show project tag if present.
        if let Some(ref proj) = self.project {
            spans.push(Span::styled(
                format!(" [{proj}]"),
                Style::default().fg(COLD),
            ));
        }

        // Add type badge for non-AgentMsg types.
        match &self.msg_type {
            MessageType::AgentMsg => {}
            MessageType::TaskStatus { task_id, state } => {
                spans.push(Span::styled(
                    format!(" [task:{task_id} {state:?}]"),
                    Style::default().fg(WARM),
                ));
            }
            MessageType::RingOverflow { oldest_available } => {
                spans.push(Span::styled(
                    format!(" [overflow: oldest={oldest_available}]"),
                    Style::default().fg(SEARING),
                ));
            }
            MessageType::Ack { ref_seq } => {
                spans.push(Span::styled(
                    format!(" [ack:{ref_seq}]"),
                    Style::default().fg(TEXT_MUTED),
                ));
            }
            MessageType::Subscribe { .. } => {
                spans.push(Span::styled(
                    String::from(" [subscribe]"),
                    Style::default().fg(ACCENT_COLD),
                ));
            }
        }

        spans.push(Span::styled(String::from(" > "), Style::default().fg(TEXT_MUTED)));
        spans.push(Span::styled(self.content.clone(), Style::default().fg(TEXT_BRIGHT)));

        Line::from(spans)
    }
}

// ---------------------------------------------------------------------------
// ChatPage
// ---------------------------------------------------------------------------

/// Maximum messages to keep in the scrollback buffer.
const MAX_SCROLLBACK: usize = 1000;

pub struct ChatPage {
    /// Chat message entries.
    entries: VecDeque<ChatEntry>,
    /// Connection to messages.sock (None if not connected).
    conn: Option<BusConnection>,
    /// Highest seen sequence number (for replay on reconnect).
    last_seq: u64,
    /// Last connection attempt time (for retry throttling).
    last_connect_attempt: Option<Instant>,

    /// Scroll offset from bottom (0 = pinned to bottom).
    scroll_offset: usize,
    /// Whether the user has scrolled up (disables auto-scroll).
    scroll_pinned: bool,

    /// Filter mode active.
    filter_active: bool,
    /// Filter text.
    filter_text: String,

    /// Status message with error flag and timestamp.
    status_msg: Option<(String, bool, Instant)>,
}

impl ChatPage {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            conn: None,
            last_seq: 0,
            last_connect_attempt: None,
            scroll_offset: 0,
            scroll_pinned: true,
            filter_active: false,
            filter_text: String::new(),
            status_msg: None,
        }
    }

    /// Attempt to connect to the message bus.
    fn try_connect(&mut self) {
        // Throttle connection attempts to every 3 seconds.
        if let Some(last) = self.last_connect_attempt {
            if last.elapsed().as_secs() < 3 {
                return;
            }
        }
        self.last_connect_attempt = Some(Instant::now());

        match BusConnection::connect(self.last_seq) {
            Some(conn) => {
                self.conn = Some(conn);
                self.status_msg = Some((
                    "Connected to message bus".into(),
                    false,
                    Instant::now(),
                ));
            }
            None => {
                // Not an error — daemon may not be running.
            }
        }
    }

    /// Poll for new messages from the bus.
    fn poll_messages(&mut self) {
        let msgs = if let Some(ref mut conn) = self.conn {
            let msgs = conn.poll();
            if msgs.is_empty() {
                // Check if connection is still alive by looking at poll result.
                // On actual EOF the next poll will also return empty.
                return;
            }
            msgs
        } else {
            return;
        };

        for msg in msgs {
            if msg.seq > self.last_seq {
                self.last_seq = msg.seq;
            }
            // Skip subscribe/ack control messages from display.
            match &msg.msg_type {
                MessageType::Subscribe { .. } | MessageType::Ack { .. } => continue,
                _ => {}
            }
            self.entries.push_back(ChatEntry::from_message(&msg));
        }

        // Trim scrollback.
        while self.entries.len() > MAX_SCROLLBACK {
            self.entries.pop_front();
            // Adjust scroll offset if needed.
            if self.scroll_offset > 0 {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
        }
    }

    /// Get filtered entries for display.
    fn visible_entries(&self) -> Vec<&ChatEntry> {
        if !self.filter_active || self.filter_text.is_empty() {
            return self.entries.iter().collect();
        }
        let filter = self.filter_text.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                e.from.agent_type.to_lowercase().contains(&filter)
                    || e.from.key.to_lowercase().contains(&filter)
                    || e.project
                        .as_ref()
                        .is_some_and(|p| p.to_lowercase().contains(&filter))
            })
            .collect()
    }

    /// Handle character input for the filter field.
    fn handle_char(&mut self, ch: char) {
        if self.filter_active {
            self.filter_text.push(ch);
        }
    }

    /// Handle backspace for the filter field.
    fn handle_backspace(&mut self) {
        if self.filter_active {
            self.filter_text.pop();
        }
    }
}

impl TuiPage for ChatPage {
    fn title(&self) -> &str {
        "Messages"
    }

    fn tick(&mut self, _poller: &mut ClaudeStatePoller) {
        // Ensure we have a connection.
        if self.conn.is_none() {
            self.try_connect();
        }

        // Poll for new messages.
        self.poll_messages();

        // Clear status message after 4 seconds.
        if let Some((_, _, when)) = &self.status_msg {
            if when.elapsed().as_secs() >= 4 {
                self.status_msg = None;
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);

        let connected = self.conn.is_some();

        // Layout: title | messages | filter bar (optional) | status
        let filter_height = if self.filter_active { 1 } else { 0 };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),            // title + connection status
                Constraint::Min(5),               // messages
                Constraint::Length(filter_height), // filter bar
                Constraint::Length(1),             // status / hints
            ])
            .margin(1)
            .split(area);

        // -- Title row --
        let conn_indicator = if connected {
            Span::styled(
                " [connected]",
                Style::default().fg(Color::Rgb(0, 200, 80)),
            )
        } else {
            Span::styled(
                " [disconnected]",
                Style::default().fg(Color::Rgb(200, 50, 50)),
            )
        };

        let msg_count = self.entries.len();
        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                "Message Bus",
                Style::default()
                    .fg(TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
            conn_indicator,
            Span::styled(
                format!("  ({msg_count} messages)"),
                Style::default().fg(TEXT_MUTED),
            ),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(title, chunks[0]);

        // -- Messages area --
        // Collect lines eagerly to avoid borrowing self during scroll mutation.
        let lines: Vec<Line> = {
            let visible = self.visible_entries();
            visible.iter().map(|e| e.to_line()).collect()
        };
        let total_lines = lines.len();

        // Calculate scroll position.
        let msg_area_height = chunks[1].height.saturating_sub(2) as usize; // border
        let max_scroll = total_lines.saturating_sub(msg_area_height);

        if self.scroll_pinned {
            self.scroll_offset = 0;
        }
        let scroll_pos = if self.scroll_pinned {
            max_scroll
        } else {
            max_scroll.saturating_sub(self.scroll_offset)
        };

        let messages = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll_pos as u16, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(COLD))
                    .title(" Messages ")
                    .title_style(Style::default().fg(ACCENT_COLD))
                    .style(Style::default().bg(BG)),
            );
        f.render_widget(messages, chunks[1]);

        // Scrollbar.
        if total_lines > msg_area_height {
            let mut scrollbar_state =
                ScrollbarState::new(max_scroll).position(scroll_pos);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("\u{25b2}"))
                .end_symbol(Some("\u{25bc}"));
            f.render_stateful_widget(scrollbar, chunks[1], &mut scrollbar_state);
        }

        // -- Filter bar --
        if self.filter_active {
            let filter = Paragraph::new(Line::from(vec![
                Span::styled(
                    " Filter: ",
                    Style::default().fg(WARM).add_modifier(Modifier::BOLD),
                ),
                Span::styled(&self.filter_text, Style::default().fg(TEXT_BRIGHT)),
                Span::styled("_", Style::default().fg(TEXT_BRIGHT)),
            ]))
            .style(Style::default().bg(BG_SURFACE));
            f.render_widget(filter, chunks[2]);
        }

        // -- Status / hints --
        let status_line = if let Some((ref msg, is_error, _)) = self.status_msg {
            let color = if is_error { SEARING } else { WARM };
            Line::from(Span::styled(msg.as_str(), Style::default().fg(color)))
        } else {
            Line::from(vec![
                Span::styled(
                    "Ctrl+F",
                    Style::default().fg(WARM).add_modifier(Modifier::BOLD),
                ),
                Span::styled(": filter  ", Style::default().fg(TEXT_MUTED)),
                Span::styled(
                    "PgUp/PgDn",
                    Style::default()
                        .fg(ACCENT_COLD)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(": scroll  ", Style::default().fg(TEXT_MUTED)),
                Span::styled(
                    "End",
                    Style::default()
                        .fg(ACCENT_COLD)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(": latest", Style::default().fg(TEXT_MUTED)),
            ])
        };
        let status = Paragraph::new(status_line).alignment(Alignment::Center);
        f.render_widget(status, chunks[3]);
    }

    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        _poller: &mut ClaudeStatePoller,
    ) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Ctrl+F toggles filter in any state.
        if ctrl && key.code == KeyCode::Char('f') {
            self.filter_active = !self.filter_active;
            if !self.filter_active {
                self.filter_text.clear();
            }
            return false;
        }

        // If filter bar is active, handle filter input.
        if self.filter_active {
            match key.code {
                KeyCode::Esc => {
                    self.filter_active = false;
                    self.filter_text.clear();
                }
                KeyCode::Backspace => self.handle_backspace(),
                KeyCode::Char(ch) => self.handle_char(ch),
                _ => {}
            }
            return false;
        }

        // Navigation keys (read-only log — no input bar).
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_pinned = false;
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.scroll_offset > 0 {
                    self.scroll_offset -= 1;
                } else {
                    self.scroll_pinned = true;
                }
            }
            KeyCode::PageUp => {
                self.scroll_pinned = false;
                self.scroll_offset = self.scroll_offset.saturating_add(20);
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(20);
                if self.scroll_offset == 0 {
                    self.scroll_pinned = true;
                }
            }
            KeyCode::Home => {
                self.scroll_pinned = false;
                self.scroll_offset = self.entries.len();
            }
            KeyCode::End => {
                self.scroll_pinned = true;
                self.scroll_offset = 0;
            }
            KeyCode::Char('g') => {
                // 'g' = go to top
                self.scroll_pinned = false;
                self.scroll_offset = self.entries.len();
            }
            KeyCode::Char('G') => {
                // 'G' = go to bottom
                self.scroll_pinned = true;
                self.scroll_offset = 0;
            }
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
            MouseEventKind::ScrollUp => {
                self.scroll_pinned = false;
                self.scroll_offset = self.scroll_offset.saturating_add(3);
            }
            MouseEventKind::ScrollDown => {
                if self.scroll_offset > 0 {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                }
                if self.scroll_offset == 0 {
                    self.scroll_pinned = true;
                }
            }
            _ => {}
        }
    }

    fn has_text_focus(&self) -> bool {
        self.filter_active
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sample_message(seq: u64, agent_type: &str, content: &str) -> Message {
        Message {
            seq,
            ts: 1711500000000 + seq * 1000,
            from: AgentId::new(agent_type, "test"),
            to: AgentId::new("user", "tui"),
            context_id: None,
            project: None,
            content: content.into(),
            msg_type: MessageType::AgentMsg,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn chat_page_title() {
        let page = ChatPage::new();
        assert_eq!(page.title(), "Messages");
    }

    #[test]
    fn chat_page_starts_disconnected() {
        let page = ChatPage::new();
        assert!(page.conn.is_none());
        assert_eq!(page.entries.len(), 0);
        assert_eq!(page.last_seq, 0);
    }

    #[test]
    fn chat_page_default_not_focused() {
        let page = ChatPage::new();
        assert!(!page.has_text_focus());
        assert!(!page.filter_active);
    }

    #[test]
    fn chat_entry_from_message_formats_timestamp() {
        let msg = sample_message(1, "claude", "hello");
        let entry = ChatEntry::from_message(&msg);
        // 1711500001 seconds = some time in HH:MM:SS
        assert!(!entry.timestamp.is_empty());
        assert!(entry.timestamp.contains(':'));
    }

    #[test]
    fn chat_entry_preserves_content() {
        let msg = sample_message(42, "codex", "build complete");
        let entry = ChatEntry::from_message(&msg);
        assert_eq!(entry.content, "build complete");
        assert_eq!(entry.seq, 42);
        assert_eq!(entry.from.agent_type, "codex");
    }

    #[test]
    fn agent_color_returns_distinct_colors() {
        let claude = agent_color("claude");
        let codex = agent_color("codex");
        let copilot = agent_color("copilot");
        let user = agent_color("user");
        // At minimum claude and codex should differ.
        assert_ne!(claude, codex);
        assert_ne!(copilot, user);
    }

    #[test]
    fn chat_page_filter_input() {
        let mut page = ChatPage::new();
        page.filter_active = true;

        // Type some text into filter.
        page.handle_char('c');
        page.handle_char('l');
        assert_eq!(page.filter_text, "cl");

        // Backspace.
        page.handle_backspace();
        assert_eq!(page.filter_text, "c");
    }

    #[test]
    fn chat_page_filter_entries() {
        let mut page = ChatPage::new();

        // Add some entries.
        let msg1 = sample_message(1, "claude", "hello");
        let msg2 = sample_message(2, "codex", "world");
        let msg3 = sample_message(3, "claude", "bye");
        page.entries.push_back(ChatEntry::from_message(&msg1));
        page.entries.push_back(ChatEntry::from_message(&msg2));
        page.entries.push_back(ChatEntry::from_message(&msg3));

        // No filter — all visible.
        assert_eq!(page.visible_entries().len(), 3);

        // Filter by "claude".
        page.filter_active = true;
        page.filter_text = "claude".into();
        assert_eq!(page.visible_entries().len(), 2);

        // Filter by "codex".
        page.filter_text = "codex".into();
        assert_eq!(page.visible_entries().len(), 1);
    }

    #[test]
    fn chat_page_scrollback_limit() {
        let mut page = ChatPage::new();

        // Fill beyond MAX_SCROLLBACK.
        for i in 0..MAX_SCROLLBACK + 100 {
            let msg = sample_message(i as u64, "claude", "msg");
            page.entries.push_back(ChatEntry::from_message(&msg));
        }

        // Manually trim (normally done in poll_messages).
        while page.entries.len() > MAX_SCROLLBACK {
            page.entries.pop_front();
        }
        assert_eq!(page.entries.len(), MAX_SCROLLBACK);
    }

    #[test]
    fn messages_socket_path_is_under_thermal() {
        let path = messages_socket_path();
        let path_str = path.to_str().unwrap();
        assert!(path_str.ends_with("/thermal/messages.sock"));
    }

    #[test]
    fn chat_entry_to_line_has_spans() {
        let msg = sample_message(1, "claude", "hello world");
        let entry = ChatEntry::from_message(&msg);
        let line = entry.to_line();
        // Should have at least timestamp, agent, separator, content.
        assert!(line.spans.len() >= 4);
    }

    #[test]
    fn chat_entry_task_status_badge() {
        let msg = Message {
            seq: 1,
            ts: 1711500000000,
            from: AgentId::new("dispatcher", "main"),
            to: AgentId::new("user", "tui"),
            context_id: None,
            project: Some("thermal-desktop".into()),
            content: "Task done".into(),
            msg_type: MessageType::TaskStatus {
                task_id: "t-1".into(),
                state: thermal_core::message::TaskState::Completed,
            },
            metadata: HashMap::new(),
        };
        let entry = ChatEntry::from_message(&msg);
        let line = entry.to_line();
        // Should have project tag + task badge.
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("[thermal-desktop]"));
        assert!(text.contains("[task:t-1"));
    }
}
