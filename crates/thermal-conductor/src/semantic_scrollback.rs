//! Semantic scrollback — navigate agent sessions by meaning, not raw output.
//!
//! Wraps a [`SessionLog`] and provides a high-level timeline view grouped by
//! conversation turns, plus O(1) jump-to navigation between tool calls, user
//! messages, and other semantic landmarks.
//!
//! This is a data/navigation API — rendering is a follow-up (GPU view mode).

use std::time::Duration;

use crate::session_log::{SessionEvent, SessionEventType, SessionLog};

// ── Types ────────────────────────────────────────────────────────────────────

/// The kind of grouped timeline entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimelineEntryKind {
    /// A user turn (one or more user messages).
    UserTurn,
    /// An assistant response (text, possibly with thinking).
    AssistantResponse,
    /// A tool call round (ToolUse + ToolResult, possibly with Progress).
    ToolCallRound,
    /// A system/meta event (permission-mode, system, etc.).
    SystemEvent,
}

impl std::fmt::Display for TimelineEntryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserTurn => write!(f, "user"),
            Self::AssistantResponse => write!(f, "assistant"),
            Self::ToolCallRound => write!(f, "tool"),
            Self::SystemEvent => write!(f, "system"),
        }
    }
}

/// A grouped entry in the session timeline.
#[derive(Debug, Clone)]
pub(crate) struct TimelineEntry {
    /// What kind of turn/event this is.
    pub kind: TimelineEntryKind,
    /// Range of event indices in the SessionLog (inclusive start, exclusive end).
    pub event_range: (usize, usize),
    /// Brief label for display (e.g. "User: Hello...", "Bash: ls -la").
    pub label: String,
    /// Timestamp of the first event in the group.
    pub timestamp: String,
    /// Total duration of the group (for tool rounds: use -> result).
    pub duration: Option<Duration>,
}

/// Semantic scrollback navigator.
#[derive(Debug)]
pub(crate) struct SemanticScrollback {
    /// The underlying session log.
    log: SessionLog,
    /// High-level timeline of grouped entries.
    timeline: Vec<TimelineEntry>,
    /// Indices into `timeline` for user turns (for jump-to).
    user_turn_indices: Vec<usize>,
    /// Indices into `timeline` for tool call rounds (for jump-to).
    tool_call_indices: Vec<usize>,
    /// Current position in the timeline.
    cursor: usize,
}

// ── Implementation ───────────────────────────────────────────────────────────

impl SemanticScrollback {
    /// Build a semantic scrollback view from a session log.
    pub fn new(log: SessionLog) -> Self {
        let timeline = build_timeline(log.events());

        let user_turn_indices: Vec<usize> = timeline
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == TimelineEntryKind::UserTurn)
            .map(|(i, _)| i)
            .collect();

        let tool_call_indices: Vec<usize> = timeline
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == TimelineEntryKind::ToolCallRound)
            .map(|(i, _)| i)
            .collect();

        Self {
            log,
            timeline,
            user_turn_indices,
            tool_call_indices,
            cursor: 0,
        }
    }

    /// The high-level timeline.
    pub fn timeline(&self) -> &[TimelineEntry] {
        &self.timeline
    }

    /// Access the underlying session log.
    #[allow(dead_code)]
    pub fn log(&self) -> &SessionLog {
        &self.log
    }

    /// Current cursor position in the timeline.
    #[allow(dead_code)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Set cursor to a specific timeline index. Returns false if out of bounds.
    #[allow(dead_code)]
    pub fn set_cursor(&mut self, idx: usize) -> bool {
        if idx < self.timeline.len() {
            self.cursor = idx;
            true
        } else {
            false
        }
    }

    /// Jump to the next tool call round after the current cursor position.
    /// Returns the new cursor position, or `None` if there are no more tool calls.
    pub fn jump_to_next_tool_call(&mut self) -> Option<usize> {
        let target = self
            .tool_call_indices
            .iter()
            .find(|&&idx| idx > self.cursor)
            .copied();
        if let Some(idx) = target {
            self.cursor = idx;
        }
        target
    }

    /// Jump to the previous tool call round before the current cursor position.
    /// Returns the new cursor position, or `None` if there are no earlier tool calls.
    pub fn jump_to_prev_tool_call(&mut self) -> Option<usize> {
        let target = self
            .tool_call_indices
            .iter()
            .rev()
            .find(|&&idx| idx < self.cursor)
            .copied();
        if let Some(idx) = target {
            self.cursor = idx;
        }
        target
    }

    /// Jump to the next user message after the current cursor position.
    pub fn jump_to_next_user_message(&mut self) -> Option<usize> {
        let target = self
            .user_turn_indices
            .iter()
            .find(|&&idx| idx > self.cursor)
            .copied();
        if let Some(idx) = target {
            self.cursor = idx;
        }
        target
    }

    /// Jump to the previous user message before the current cursor position.
    pub fn jump_to_prev_user_message(&mut self) -> Option<usize> {
        let target = self
            .user_turn_indices
            .iter()
            .rev()
            .find(|&&idx| idx < self.cursor)
            .copied();
        if let Some(idx) = target {
            self.cursor = idx;
        }
        target
    }

    /// Full-text search across all event content. Returns timeline indices of matches.
    pub fn search(&self, query: &str) -> Vec<usize> {
        if query.is_empty() {
            return Vec::new();
        }
        let query_lower = query.to_lowercase();
        let events = self.log.events();

        self.timeline
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                // Check if any event in this entry's range contains the query.
                let (start, end) = entry.event_range;
                events[start..end].iter().any(|e| {
                    e.content.to_lowercase().contains(&query_lower)
                        || e.tool_name
                            .as_deref()
                            .map(|n| n.to_lowercase().contains(&query_lower))
                            .unwrap_or(false)
                })
            })
            .map(|(i, _)| i)
            .collect()
    }
}

// ── Timeline builder ─────────────────────────────────────────────────────────

/// Build the grouped timeline from raw events.
///
/// Grouping rules:
/// - Consecutive UserMessage events -> single UserTurn
/// - Consecutive Thinking + AssistantText events -> single AssistantResponse
/// - ToolUse + Progress* + ToolResult (same tool_use_id) -> single ToolCallRound
/// - SystemMessage -> standalone SystemEvent
fn build_timeline(events: &[SessionEvent]) -> Vec<TimelineEntry> {
    let mut timeline = Vec::new();
    let mut i = 0;

    while i < events.len() {
        let event = &events[i];

        match event.event_type {
            SessionEventType::UserMessage => {
                // Consume consecutive user messages.
                let start = i;
                while i < events.len()
                    && events[i].event_type == SessionEventType::UserMessage
                {
                    i += 1;
                }
                let label = make_label("User", &events[start].content, 60);
                timeline.push(TimelineEntry {
                    kind: TimelineEntryKind::UserTurn,
                    event_range: (start, i),
                    label,
                    timestamp: events[start].timestamp.clone(),
                    duration: None,
                });
            }
            SessionEventType::Thinking | SessionEventType::AssistantText => {
                // Consume consecutive thinking + assistant text.
                let start = i;
                while i < events.len()
                    && matches!(
                        events[i].event_type,
                        SessionEventType::Thinking | SessionEventType::AssistantText
                    )
                {
                    i += 1;
                }
                // Use the first assistant text for the label, or first thinking.
                let label_event = events[start..i]
                    .iter()
                    .find(|e| e.event_type == SessionEventType::AssistantText)
                    .unwrap_or(&events[start]);
                let label = make_label("Assistant", &label_event.content, 60);
                timeline.push(TimelineEntry {
                    kind: TimelineEntryKind::AssistantResponse,
                    event_range: (start, i),
                    label,
                    timestamp: events[start].timestamp.clone(),
                    duration: None,
                });
            }
            SessionEventType::ToolUse => {
                // Consume ToolUse + Progress* + ToolResult for same tool_use_id.
                let start = i;
                let tool_use_id = event.tool_use_id.clone();
                let tool_name = event.tool_name.clone().unwrap_or_else(|| "?".into());
                i += 1;

                // Consume Progress and ToolResult events.
                while i < events.len() {
                    match events[i].event_type {
                        SessionEventType::Progress => {
                            i += 1;
                        }
                        SessionEventType::ToolResult => {
                            // Match by tool_use_id if available.
                            if tool_use_id.is_some()
                                && events[i].tool_use_id == tool_use_id
                            {
                                i += 1;
                                break;
                            } else if tool_use_id.is_none() {
                                // No tool_use_id — consume next ToolResult.
                                i += 1;
                                break;
                            } else {
                                break;
                            }
                        }
                        _ => break,
                    }
                }

                let duration = events[start..i]
                    .iter()
                    .rev()
                    .find_map(|e| e.duration);

                let label = make_label(&tool_name, &events[start].content, 60);
                timeline.push(TimelineEntry {
                    kind: TimelineEntryKind::ToolCallRound,
                    event_range: (start, i),
                    label,
                    timestamp: events[start].timestamp.clone(),
                    duration,
                });
            }
            SessionEventType::ToolResult => {
                // Orphan tool result (no preceding ToolUse in sequence).
                let tool_name = event.tool_name.clone().unwrap_or_else(|| "?".into());
                let label = make_label(&format!("{tool_name} result"), &event.content, 60);
                timeline.push(TimelineEntry {
                    kind: TimelineEntryKind::ToolCallRound,
                    event_range: (i, i + 1),
                    label,
                    timestamp: event.timestamp.clone(),
                    duration: event.duration,
                });
                i += 1;
            }
            SessionEventType::Progress => {
                // Orphan progress (no preceding ToolUse in sequence).
                let tool_name = event.tool_name.clone().unwrap_or_else(|| "?".into());
                let label = make_label(&format!("{tool_name} progress"), &event.content, 60);
                timeline.push(TimelineEntry {
                    kind: TimelineEntryKind::ToolCallRound,
                    event_range: (i, i + 1),
                    label,
                    timestamp: event.timestamp.clone(),
                    duration: None,
                });
                i += 1;
            }
            SessionEventType::SystemMessage => {
                let label = make_label("System", &event.content, 60);
                timeline.push(TimelineEntry {
                    kind: TimelineEntryKind::SystemEvent,
                    event_range: (i, i + 1),
                    label,
                    timestamp: event.timestamp.clone(),
                    duration: None,
                });
                i += 1;
            }
        }
    }

    timeline
}

/// Create a display label like "Tool: first line of content...".
fn make_label(prefix: &str, content: &str, max_content_len: usize) -> String {
    let first_line = content.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        prefix.to_string()
    } else if first_line.len() <= max_content_len {
        format!("{prefix}: {first_line}")
    } else {
        let truncated: String = first_line.chars().take(max_content_len - 3).collect();
        format!("{prefix}: {truncated}...")
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_session_file(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        f.flush().unwrap();
        f
    }

    fn load_scrollback(lines: &[&str]) -> SemanticScrollback {
        let f = make_session_file(lines);
        let log = SessionLog::load(f.path()).unwrap();
        SemanticScrollback::new(log)
    }

    #[test]
    fn empty_session() {
        let sb = load_scrollback(&[]);
        assert!(sb.timeline().is_empty());
    }

    #[test]
    fn basic_conversation_timeline() {
        let sb = load_scrollback(&[
            r#"{"type":"user","content":"Hello","timestamp":"2026-04-02T00:00:00.000Z"}"#,
            r#"{"type":"thinking","content":"Let me think...","timestamp":"2026-04-02T00:00:01.000Z"}"#,
            r#"{"type":"assistant","content":"Hi there!","timestamp":"2026-04-02T00:00:02.000Z"}"#,
            r#"{"type":"tool_use","tool":"Bash","tool_use_id":"tu_1","input":{"command":"ls"},"timestamp":"2026-04-02T00:00:03.000Z"}"#,
            r#"{"type":"tool_result","tool":"Bash","tool_use_id":"tu_1","output":"file.rs","timestamp":"2026-04-02T00:00:05.000Z"}"#,
            r#"{"type":"assistant","content":"Here are the files.","timestamp":"2026-04-02T00:00:06.000Z"}"#,
        ]);

        let tl = sb.timeline();
        assert_eq!(tl.len(), 4);
        assert_eq!(tl[0].kind, TimelineEntryKind::UserTurn);
        assert_eq!(tl[1].kind, TimelineEntryKind::AssistantResponse);
        assert_eq!(tl[2].kind, TimelineEntryKind::ToolCallRound);
        assert_eq!(tl[3].kind, TimelineEntryKind::AssistantResponse);
    }

    #[test]
    fn tool_round_groups_use_and_result() {
        let sb = load_scrollback(&[
            r#"{"type":"tool_use","tool":"Read","tool_use_id":"tu_a","input":{},"timestamp":"2026-04-02T00:00:00.000Z"}"#,
            r#"{"type":"progress","tool":"Read","status":"running","timestamp":"2026-04-02T00:00:01.000Z"}"#,
            r#"{"type":"tool_result","tool":"Read","tool_use_id":"tu_a","output":"content","timestamp":"2026-04-02T00:00:03.000Z"}"#,
        ]);

        let tl = sb.timeline();
        assert_eq!(tl.len(), 1);
        assert_eq!(tl[0].kind, TimelineEntryKind::ToolCallRound);
        assert_eq!(tl[0].event_range, (0, 3));
        assert_eq!(tl[0].duration, Some(Duration::from_millis(3000)));
    }

    #[test]
    fn jump_to_next_tool_call() {
        let mut sb = load_scrollback(&[
            r#"{"type":"user","content":"Hello","timestamp":"2026-04-02T00:00:00.000Z"}"#,
            r#"{"type":"assistant","content":"Hi","timestamp":"2026-04-02T00:00:01.000Z"}"#,
            r#"{"type":"tool_use","tool":"Bash","tool_use_id":"tu_1","input":{},"timestamp":"2026-04-02T00:00:02.000Z"}"#,
            r#"{"type":"tool_result","tool":"Bash","tool_use_id":"tu_1","output":"ok","timestamp":"2026-04-02T00:00:03.000Z"}"#,
            r#"{"type":"tool_use","tool":"Read","tool_use_id":"tu_2","input":{},"timestamp":"2026-04-02T00:00:04.000Z"}"#,
            r#"{"type":"tool_result","tool":"Read","tool_use_id":"tu_2","output":"ok","timestamp":"2026-04-02T00:00:05.000Z"}"#,
        ]);

        // Timeline: [UserTurn, AssistantResponse, ToolCallRound(Bash), ToolCallRound(Read)]
        assert_eq!(sb.cursor(), 0);

        // Jump to first tool call.
        let pos = sb.jump_to_next_tool_call();
        assert_eq!(pos, Some(2));

        // Jump to second tool call.
        let pos = sb.jump_to_next_tool_call();
        assert_eq!(pos, Some(3));

        // No more tool calls.
        let pos = sb.jump_to_next_tool_call();
        assert_eq!(pos, None);
    }

    #[test]
    fn jump_to_prev_tool_call() {
        let mut sb = load_scrollback(&[
            r#"{"type":"user","content":"Hello","timestamp":"2026-04-02T00:00:00.000Z"}"#,
            r#"{"type":"tool_use","tool":"Bash","tool_use_id":"tu_1","input":{},"timestamp":"2026-04-02T00:00:01.000Z"}"#,
            r#"{"type":"tool_result","tool":"Bash","tool_use_id":"tu_1","output":"ok","timestamp":"2026-04-02T00:00:02.000Z"}"#,
            r#"{"type":"tool_use","tool":"Read","tool_use_id":"tu_2","input":{},"timestamp":"2026-04-02T00:00:03.000Z"}"#,
            r#"{"type":"tool_result","tool":"Read","tool_use_id":"tu_2","output":"ok","timestamp":"2026-04-02T00:00:04.000Z"}"#,
        ]);

        // Move to end.
        sb.set_cursor(2); // ToolCallRound(Read)

        let pos = sb.jump_to_prev_tool_call();
        assert_eq!(pos, Some(1));

        let pos = sb.jump_to_prev_tool_call();
        assert_eq!(pos, None);
    }

    #[test]
    fn jump_to_user_messages() {
        let mut sb = load_scrollback(&[
            r#"{"type":"user","content":"First question","timestamp":"2026-04-02T00:00:00.000Z"}"#,
            r#"{"type":"assistant","content":"Answer","timestamp":"2026-04-02T00:00:01.000Z"}"#,
            r#"{"type":"user","content":"Second question","timestamp":"2026-04-02T00:00:02.000Z"}"#,
            r#"{"type":"assistant","content":"Another answer","timestamp":"2026-04-02T00:00:03.000Z"}"#,
        ]);

        // Timeline: [UserTurn, AssistantResponse, UserTurn, AssistantResponse]
        let pos = sb.jump_to_next_user_message();
        assert_eq!(pos, Some(2));

        let pos = sb.jump_to_next_user_message();
        assert_eq!(pos, None);

        let pos = sb.jump_to_prev_user_message();
        assert_eq!(pos, Some(0));
    }

    #[test]
    fn search_finds_matching_entries() {
        let sb = load_scrollback(&[
            r#"{"type":"user","content":"Tell me about Rust generics","timestamp":"2026-04-02T00:00:00.000Z"}"#,
            r#"{"type":"assistant","content":"Generics in Rust allow polymorphism","timestamp":"2026-04-02T00:00:01.000Z"}"#,
            r#"{"type":"tool_use","tool":"Read","tool_use_id":"tu_1","input":{},"timestamp":"2026-04-02T00:00:02.000Z"}"#,
            r#"{"type":"tool_result","tool":"Read","tool_use_id":"tu_1","output":"fn foo<T>() {}","timestamp":"2026-04-02T00:00:03.000Z"}"#,
            r#"{"type":"user","content":"What about lifetimes?","timestamp":"2026-04-02T00:00:04.000Z"}"#,
        ]);

        // Search for "generics" should match user turn and assistant response.
        let results = sb.search("generics");
        assert_eq!(results.len(), 2);

        // Search for "Read" should match the tool call round (by tool name).
        let results = sb.search("Read");
        assert_eq!(results.len(), 1);

        // Case-insensitive.
        let results = sb.search("RUST");
        assert!(!results.is_empty());

        // Empty query returns nothing.
        let results = sb.search("");
        assert!(results.is_empty());

        // No match.
        let results = sb.search("zzzznonexistent");
        assert!(results.is_empty());
    }

    #[test]
    fn consecutive_user_messages_grouped() {
        let sb = load_scrollback(&[
            r#"{"type":"user","content":"Part 1","timestamp":"2026-04-02T00:00:00.000Z"}"#,
            r#"{"type":"user","content":"Part 2","timestamp":"2026-04-02T00:00:01.000Z"}"#,
            r#"{"type":"assistant","content":"Reply","timestamp":"2026-04-02T00:00:02.000Z"}"#,
        ]);

        let tl = sb.timeline();
        assert_eq!(tl.len(), 2);
        assert_eq!(tl[0].kind, TimelineEntryKind::UserTurn);
        assert_eq!(tl[0].event_range, (0, 2)); // Both user messages grouped.
    }

    #[test]
    fn system_events_standalone() {
        let sb = load_scrollback(&[
            r#"{"type":"system","content":"Session init","timestamp":"2026-04-02T00:00:00.000Z"}"#,
            r#"{"type":"user","content":"Hello","timestamp":"2026-04-02T00:00:01.000Z"}"#,
        ]);

        let tl = sb.timeline();
        assert_eq!(tl.len(), 2);
        assert_eq!(tl[0].kind, TimelineEntryKind::SystemEvent);
        assert_eq!(tl[1].kind, TimelineEntryKind::UserTurn);
    }
}
