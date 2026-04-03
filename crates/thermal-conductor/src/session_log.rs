//! JSONL session log reader for Claude Code sessions.
//!
//! Loads and parses a CC session JSONL file into typed [`SessionEvent`]s.
//! Each line is a JSON object with a `type` field and optional `timestamp`.
//! Tool call lifecycles are paired by `tool_use_id` to compute durations.
//!
//! # CC JSONL Format
//!
//! ```json
//! {"type":"user","content":"Hello","timestamp":"2026-04-02T00:13:52.232Z","sessionId":"abc"}
//! {"type":"assistant","content":[{"type":"text","text":"I'll help"}],"timestamp":"..."}
//! {"type":"tool_use","tool":"Bash","tool_use_id":"tu_01","input":{...},"timestamp":"..."}
//! {"type":"tool_result","tool":"Bash","tool_use_id":"tu_01","output":"ok","timestamp":"..."}
//! {"type":"thinking","content":"Let me analyze...","timestamp":"..."}
//! {"type":"progress","tool":"Bash","status":"running","message":"...","timestamp":"..."}
//! {"type":"system","content":"...","timestamp":"..."}
//! ```

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

// ── Types ────────────────────────────────────────────────────────────────────

/// The kind of event in a session log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SessionEventType {
    UserMessage,
    AssistantText,
    ToolUse,
    ToolResult,
    Thinking,
    Progress,
    SystemMessage,
}

impl std::fmt::Display for SessionEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserMessage => write!(f, "user"),
            Self::AssistantText => write!(f, "assistant"),
            Self::ToolUse => write!(f, "tool_use"),
            Self::ToolResult => write!(f, "tool_result"),
            Self::Thinking => write!(f, "thinking"),
            Self::Progress => write!(f, "progress"),
            Self::SystemMessage => write!(f, "system"),
        }
    }
}

/// A single parsed event from a CC session JSONL file.
#[derive(Debug, Clone)]
pub(crate) struct SessionEvent {
    /// ISO 8601 timestamp string from the JSONL line.
    pub timestamp: String,
    /// Parsed epoch millis for ordering/duration computation.
    pub epoch_ms: Option<u64>,
    /// Event type classification.
    pub event_type: SessionEventType,
    /// Primary content (message text, tool output, thinking text, etc.).
    pub content: String,
    /// Tool name, if this is a tool-related event.
    pub tool_name: Option<String>,
    /// For ToolResult events: duration from matching ToolUse to this result.
    pub duration: Option<Duration>,
    /// Tool use ID for pairing ToolUse <-> ToolResult.
    pub tool_use_id: Option<String>,
    /// Whether a ToolResult was an error.
    pub is_error: bool,
}

/// A loaded and parsed CC session log.
#[derive(Debug)]
pub(crate) struct SessionLog {
    /// Source file path.
    path: PathBuf,
    /// All parsed events in file order.
    events: Vec<SessionEvent>,
    /// Session ID extracted from filename.
    pub session_id: Option<String>,
}

// ── Raw JSONL envelope ───────────────────────────────────────────────────────

/// Flexible deserialization envelope for CC JSONL lines.
#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    msg_type: Option<String>,

    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

// ── Implementation ───────────────────────────────────────────────────────────

impl SessionLog {
    /// Load and parse a CC JSONL session file.
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("Failed to open session log: {}", path.display()))?;
        let reader = BufReader::new(file);

        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());

        let mut events = Vec::new();

        for line in reader.lines() {
            let line = line.context("Failed to read line from session log")?;
            let trimmed = line.trim();
            if trimmed.is_empty() || !trimmed.starts_with('{') {
                continue;
            }

            if let Some(event) = parse_session_event(trimmed) {
                events.push(event);
            }
        }

        // Pair ToolUse -> ToolResult by tool_use_id to compute durations.
        pair_tool_durations(&mut events);

        Ok(Self {
            path: path.to_path_buf(),
            events,
            session_id,
        })
    }

    /// All parsed events in file order.
    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// Source file path.
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of events.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the log is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

// ── Parsing helpers ──────────────────────────────────────────────────────────

/// Parse a single JSONL line into a SessionEvent.
fn parse_session_event(line: &str) -> Option<SessionEvent> {
    let raw: RawLine = serde_json::from_str(line).ok()?;
    let msg_type = raw.msg_type.as_deref()?;

    let timestamp = raw.timestamp.clone().unwrap_or_default();
    let epoch_ms = parse_iso8601_epoch_ms(&timestamp);

    let (event_type, content, tool_name) = match msg_type {
        "user" => {
            let content = extract_content_string(&raw.content);
            (SessionEventType::UserMessage, content, None)
        }
        "assistant" => {
            let content = extract_content_string(&raw.content);
            (SessionEventType::AssistantText, content, None)
        }
        "tool_use" => {
            let content = raw
                .input
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default();
            (SessionEventType::ToolUse, content, raw.tool.clone())
        }
        "tool_result" => {
            let content = raw.output.clone().unwrap_or_default();
            (SessionEventType::ToolResult, content, raw.tool.clone())
        }
        "thinking" => {
            let content = extract_content_string(&raw.content);
            (SessionEventType::Thinking, content, None)
        }
        "progress" => {
            let content = raw
                .message
                .clone()
                .or(raw.status.clone())
                .unwrap_or_default();
            (SessionEventType::Progress, content, raw.tool.clone())
        }
        "system" | "permission-mode" | "last-prompt" | "attachment" => {
            let content = extract_content_string(&raw.content);
            (SessionEventType::SystemMessage, content, None)
        }
        _ => return None,
    };

    Some(SessionEvent {
        timestamp,
        epoch_ms,
        event_type,
        content,
        tool_name,
        duration: None,
        tool_use_id: raw.tool_use_id,
        is_error: raw.is_error.unwrap_or(false),
    })
}

/// Extract text content from CC's polymorphic `content` field.
///
/// CC uses either a plain string or an array of `{"type":"text","text":"..."}` objects.
fn extract_content_string(value: &Option<serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    parts.push(text);
                }
            }
            parts.join("")
        }
        _ => String::new(),
    }
}

/// Pair ToolUse events with their ToolResult by `tool_use_id` and set durations.
fn pair_tool_durations(events: &mut [SessionEvent]) {
    // Build a map of tool_use_id -> index of ToolUse event.
    let mut tool_use_times: HashMap<String, u64> = HashMap::new();

    for event in events.iter() {
        if event.event_type == SessionEventType::ToolUse {
            if let (Some(id), Some(ms)) = (&event.tool_use_id, event.epoch_ms) {
                tool_use_times.insert(id.clone(), ms);
            }
        }
    }

    // Now set durations on ToolResult events.
    for event in events.iter_mut() {
        if event.event_type == SessionEventType::ToolResult {
            if let (Some(id), Some(result_ms)) = (&event.tool_use_id, event.epoch_ms) {
                if let Some(&use_ms) = tool_use_times.get(id) {
                    if result_ms >= use_ms {
                        event.duration = Some(Duration::from_millis(result_ms - use_ms));
                    }
                }
            }
        }
    }
}

/// Parse an ISO 8601 timestamp like `"2026-04-02T00:13:52.232Z"` into epoch milliseconds.
///
/// Minimal parser — handles the common CC format without pulling in chrono.
fn parse_iso8601_epoch_ms(s: &str) -> Option<u64> {
    // Expected format: YYYY-MM-DDTHH:MM:SS.mmmZ or YYYY-MM-DDTHH:MM:SSZ
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }

    let year: u64 = s.get(0..4)?.parse().ok()?;
    let month: u64 = s.get(5..7)?.parse().ok()?;
    let day: u64 = s.get(8..10)?.parse().ok()?;
    let hour: u64 = s.get(11..13)?.parse().ok()?;
    let min: u64 = s.get(14..16)?.parse().ok()?;
    let sec: u64 = s.get(17..19)?.parse().ok()?;

    // Optional fractional seconds.
    let millis = if s.len() > 20 && s.as_bytes().get(19) == Some(&b'.') {
        // Find end of fractional part.
        let frac_start = 20;
        let frac_end = s[frac_start..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|i| frac_start + i)
            .unwrap_or(s.len());
        let frac_str = &s[frac_start..frac_end];
        // Pad or truncate to 3 digits for milliseconds.
        let padded = format!("{:0<3}", frac_str);
        padded[..3].parse::<u64>().unwrap_or(0)
    } else {
        0
    };

    // Days from year 1970 to the given year (approximate, ignoring leap seconds).
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }

    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap_year(year) {
            days += 1;
        }
    }
    days += day - 1;

    let total_secs = days * 86400 + hour * 3600 + min * 60 + sec;
    Some(total_secs * 1000 + millis)
}

fn is_leap_year(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
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

    #[test]
    fn load_empty_file() {
        let f = make_session_file(&[]);
        let log = SessionLog::load(f.path()).unwrap();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn parse_user_message() {
        let f = make_session_file(&[
            r#"{"type":"user","content":"Hello world","timestamp":"2026-04-02T00:13:52.232Z"}"#,
        ]);
        let log = SessionLog::load(f.path()).unwrap();
        assert_eq!(log.len(), 1);
        let e = &log.events()[0];
        assert_eq!(e.event_type, SessionEventType::UserMessage);
        assert_eq!(e.content, "Hello world");
        assert!(e.epoch_ms.is_some());
    }

    #[test]
    fn parse_assistant_with_content_array() {
        let f = make_session_file(&[
            r#"{"type":"assistant","content":[{"type":"text","text":"I'll help you"}],"timestamp":"2026-04-02T00:14:00.000Z"}"#,
        ]);
        let log = SessionLog::load(f.path()).unwrap();
        assert_eq!(log.len(), 1);
        let e = &log.events()[0];
        assert_eq!(e.event_type, SessionEventType::AssistantText);
        assert_eq!(e.content, "I'll help you");
    }

    #[test]
    fn parse_tool_use_and_result() {
        let f = make_session_file(&[
            r#"{"type":"tool_use","tool":"Bash","tool_use_id":"tu_01","input":{"command":"ls"},"timestamp":"2026-04-02T00:14:00.000Z"}"#,
            r#"{"type":"tool_result","tool":"Bash","tool_use_id":"tu_01","output":"file.rs","timestamp":"2026-04-02T00:14:02.500Z"}"#,
        ]);
        let log = SessionLog::load(f.path()).unwrap();
        assert_eq!(log.len(), 2);

        let use_event = &log.events()[0];
        assert_eq!(use_event.event_type, SessionEventType::ToolUse);
        assert_eq!(use_event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(use_event.tool_use_id.as_deref(), Some("tu_01"));

        let result_event = &log.events()[1];
        assert_eq!(result_event.event_type, SessionEventType::ToolResult);
        assert_eq!(result_event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(result_event.content, "file.rs");
        // Duration should be 2500ms.
        assert_eq!(result_event.duration, Some(Duration::from_millis(2500)));
    }

    #[test]
    fn parse_thinking_event() {
        let f = make_session_file(&[
            r#"{"type":"thinking","content":"Let me analyze the code...","timestamp":"2026-04-02T00:15:00.000Z"}"#,
        ]);
        let log = SessionLog::load(f.path()).unwrap();
        assert_eq!(log.len(), 1);
        let e = &log.events()[0];
        assert_eq!(e.event_type, SessionEventType::Thinking);
        assert_eq!(e.content, "Let me analyze the code...");
    }

    #[test]
    fn parse_progress_event() {
        let f = make_session_file(&[
            r#"{"type":"progress","tool":"Bash","status":"running","message":"Compiling...","timestamp":"2026-04-02T00:15:01.000Z"}"#,
        ]);
        let log = SessionLog::load(f.path()).unwrap();
        assert_eq!(log.len(), 1);
        let e = &log.events()[0];
        assert_eq!(e.event_type, SessionEventType::Progress);
        assert_eq!(e.content, "Compiling...");
        assert_eq!(e.tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn parse_system_message() {
        let f = make_session_file(&[
            r#"{"type":"system","content":"Session started","timestamp":"2026-04-02T00:10:00.000Z"}"#,
        ]);
        let log = SessionLog::load(f.path()).unwrap();
        assert_eq!(log.len(), 1);
        let e = &log.events()[0];
        assert_eq!(e.event_type, SessionEventType::SystemMessage);
    }

    #[test]
    fn unknown_type_skipped() {
        let f = make_session_file(&[r#"{"type":"unknown_future_type","data":"something"}"#]);
        let log = SessionLog::load(f.path()).unwrap();
        assert!(log.is_empty());
    }

    #[test]
    fn invalid_json_skipped() {
        let f = make_session_file(&[
            "not json at all",
            r#"{"type":"user","content":"valid","timestamp":"2026-04-02T00:10:00.000Z"}"#,
        ]);
        let log = SessionLog::load(f.path()).unwrap();
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn tool_result_error_flag() {
        let f = make_session_file(&[
            r#"{"type":"tool_result","tool":"Bash","tool_use_id":"tu_02","output":"command not found","is_error":true,"timestamp":"2026-04-02T00:14:05.000Z"}"#,
        ]);
        let log = SessionLog::load(f.path()).unwrap();
        assert!(log.events()[0].is_error);
    }

    #[test]
    fn iso8601_parsing() {
        let ms = parse_iso8601_epoch_ms("2026-04-02T00:13:52.232Z").unwrap();
        // Sanity check: should be somewhere around ~1.77 trillion ms.
        assert!(ms > 1_700_000_000_000);
        assert!(ms < 1_800_000_000_000);
    }

    #[test]
    fn iso8601_no_fractional() {
        let ms = parse_iso8601_epoch_ms("2026-04-02T00:13:52Z").unwrap();
        assert!(ms > 1_700_000_000_000);
    }

    #[test]
    fn iso8601_invalid() {
        assert!(parse_iso8601_epoch_ms("").is_none());
        assert!(parse_iso8601_epoch_ms("not a date").is_none());
        assert!(parse_iso8601_epoch_ms("2026").is_none());
    }

    #[test]
    fn session_id_from_path() {
        let f = make_session_file(&[]);
        let log = SessionLog::load(f.path()).unwrap();
        // tempfile generates a random name — just check it's set.
        assert!(log.session_id.is_some());
    }

    #[test]
    fn multiple_tool_pairs() {
        let f = make_session_file(&[
            r#"{"type":"tool_use","tool":"Read","tool_use_id":"tu_a","input":{},"timestamp":"2026-04-02T00:14:00.000Z"}"#,
            r#"{"type":"tool_use","tool":"Bash","tool_use_id":"tu_b","input":{},"timestamp":"2026-04-02T00:14:01.000Z"}"#,
            r#"{"type":"tool_result","tool":"Read","tool_use_id":"tu_a","output":"content","timestamp":"2026-04-02T00:14:03.000Z"}"#,
            r#"{"type":"tool_result","tool":"Bash","tool_use_id":"tu_b","output":"ok","timestamp":"2026-04-02T00:14:05.000Z"}"#,
        ]);
        let log = SessionLog::load(f.path()).unwrap();
        assert_eq!(log.len(), 4);

        // Read took 3 seconds.
        assert_eq!(log.events()[2].duration, Some(Duration::from_millis(3000)));
        // Bash took 4 seconds.
        assert_eq!(log.events()[3].duration, Some(Duration::from_millis(4000)));
    }
}
