//! JSONL session log tailer — incrementally reads new lines from the active
//! Claude Code session's JSONL file and converts them to `AgentEvent`s.
//!
//! The GPU window polls this each event loop iteration. When the active
//! `claude_session` changes (new `session_id`), the tailer resolves the
//! corresponding JSONL file under `~/.claude/projects/` and begins tailing it.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use tracing::{debug, trace, warn};

use crate::session_log::{self, SessionEventType};
use crate::structured_output::AgentEvent;

/// Incrementally reads a Claude Code JSONL session file and emits `AgentEvent`s.
pub(crate) struct SessionJsonlTailer {
    /// The session_id we are currently tailing.
    current_session_id: Option<String>,
    /// Resolved JSONL file path for the current session.
    current_path: Option<PathBuf>,
    /// Byte offset into the file — we read from here on each poll.
    read_offset: u64,
}

impl SessionJsonlTailer {
    pub fn new() -> Self {
        Self {
            current_session_id: None,
            current_path: None,
            read_offset: 0,
        }
    }

    /// Poll for new JSONL lines. If the session_id changed, resolve the new
    /// JSONL file and start tailing from the current end (skip history).
    ///
    /// New lines are parsed via `session_log::parse_session_event`, converted
    /// to `AgentEvent`, and sent through `tx`.
    pub fn poll(&mut self, session_id: Option<&str>, tx: &Sender<AgentEvent>) {
        match session_id {
            Some(sid) => {
                // Session changed — resolve the new JSONL file.
                if self.current_session_id.as_deref() != Some(sid) {
                    self.switch_session(sid);
                }
            }
            None => {
                // No active session — clear state.
                if self.current_session_id.is_some() {
                    trace!("JSONL tailer: no active session, clearing");
                    self.current_session_id = None;
                    self.current_path = None;
                    self.read_offset = 0;
                }
                return;
            }
        }

        // Read new lines from the current file.
        let path = match &self.current_path {
            Some(p) => p.clone(),
            None => return,
        };

        self.read_new_lines(&path, tx);
    }

    /// Switch to a new session: resolve JSONL path and seek to end.
    fn switch_session(&mut self, session_id: &str) {
        debug!(session_id, "JSONL tailer: switching to new session");

        self.current_session_id = Some(session_id.to_string());
        self.current_path = None;
        self.read_offset = 0;

        if let Some(path) = resolve_session_jsonl(session_id) {
            debug!(
                session_id,
                path = %path.display(),
                "JSONL tailer: resolved session JSONL"
            );

            // Seek to end of file — we only want new events, not history.
            let offset = std::fs::metadata(&path)
                .map(|m| m.len())
                .unwrap_or(0);

            self.current_path = Some(path);
            self.read_offset = offset;
        } else {
            debug!(
                session_id,
                "JSONL tailer: could not resolve JSONL file (will retry)"
            );
        }
    }

    /// Incrementally read new lines from the JSONL file.
    fn read_new_lines(&mut self, path: &Path, tx: &Sender<AgentEvent>) {
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return,
        };

        let file_len = match file.metadata() {
            Ok(m) => m.len(),
            Err(_) => return,
        };

        if file_len <= self.read_offset {
            // File truncated or no new data — if truncated, reset.
            if file_len < self.read_offset {
                trace!("JSONL tailer: file truncated, resetting offset");
                self.read_offset = file_len;
            }
            return;
        }

        if file.seek(SeekFrom::Start(self.read_offset)).is_err() {
            return;
        }

        let reader = BufReader::new(&file);
        let mut events_sent = 0u32;
        let mut bytes_consumed: u64 = 0;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                // IO error or incomplete line at EOF — stop here so we
                // retry the partial content on the next poll.
                Err(_) => break,
            };
            // Account for the line content + newline delimiter.
            bytes_consumed += line.len() as u64 + 1;

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Parse using session_log's parser (handles nested CC format).
            let session_events = session_log::parse_session_event(trimmed);

            for se in session_events {
                if let Some(agent_event) = session_event_to_agent_event(&se) {
                    if tx.send(agent_event).is_err() {
                        // Receiver dropped — stop sending.
                        warn!("JSONL tailer: agent_event_tx receiver dropped");
                        return;
                    }
                    events_sent += 1;
                }
            }
        }

        // Advance only by fully-consumed lines so partial lines at EOF
        // are retried on the next poll instead of being silently dropped.
        self.read_offset += bytes_consumed;

        if events_sent > 0 {
            trace!(events_sent, "JSONL tailer: sent agent events");
        }
    }
}

// ── Conversion ──────────────────────────────────────────────────────────────

/// Convert a `SessionEvent` (from session_log.rs) to an `AgentEvent`
/// (consumed by the overlay manager).
fn session_event_to_agent_event(se: &session_log::SessionEvent) -> Option<AgentEvent> {
    match se.event_type {
        SessionEventType::UserMessage => Some(AgentEvent::UserMessage {
            content: se.content.clone(),
        }),
        SessionEventType::AssistantText => Some(AgentEvent::AssistantMessage {
            content: se.content.clone(),
        }),
        SessionEventType::ToolUse => {
            // content is the pretty-printed input JSON.
            let input = serde_json::from_str(&se.content).unwrap_or(serde_json::Value::Null);
            Some(AgentEvent::ToolUse {
                tool: se.tool_name.clone().unwrap_or_default(),
                input,
                tool_use_id: se.tool_use_id.clone(),
            })
        }
        SessionEventType::ToolResult => Some(AgentEvent::ToolResult {
            tool: se.tool_name.clone().unwrap_or_default(),
            output: se.content.clone(),
            is_error: se.is_error,
            tool_use_id: se.tool_use_id.clone(),
        }),
        SessionEventType::Progress => Some(AgentEvent::Progress {
            tool: se.tool_name.clone().unwrap_or_default(),
            status: String::new(),
            message: Some(se.content.clone()),
            tool_use_id: se.tool_use_id.clone(),
        }),
        SessionEventType::Thinking => Some(AgentEvent::Thinking {
            content: se.content.clone(),
        }),
        SessionEventType::SystemMessage => {
            // System messages don't have a corresponding AgentEvent variant.
            None
        }
    }
}

// ── JSONL path resolution ───────────────────────────────────────────────────

/// Resolve the JSONL file path for a Claude Code session ID.
///
/// Claude Code stores session transcripts at:
/// `~/.claude/projects/{project-hash}/{session-uuid}.jsonl`
///
/// We search all project directories since the state file doesn't tell us
/// which project hash to use.
fn resolve_session_jsonl(session_id: &str) -> Option<PathBuf> {
    let projects_dir = home_dir().join(".claude").join("projects");
    if !projects_dir.exists() {
        return None;
    }

    let filename = format!("{session_id}.jsonl");

    let entries = std::fs::read_dir(&projects_dir).ok()?;
    for entry in entries.flatten() {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }

        let candidate = project_dir.join(&filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    None
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/home/builder".into()))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tailer_starts_empty() {
        let tailer = SessionJsonlTailer::new();
        assert!(tailer.current_session_id.is_none());
        assert!(tailer.current_path.is_none());
        assert_eq!(tailer.read_offset, 0);
    }

    #[test]
    fn tailer_clears_on_no_session() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut tailer = SessionJsonlTailer::new();
        tailer.current_session_id = Some("old".into());
        tailer.poll(None, &tx);
        assert!(tailer.current_session_id.is_none());
    }

    #[test]
    fn session_event_conversion_user() {
        let se = session_log::SessionEvent {
            timestamp: String::new(),

            event_type: SessionEventType::UserMessage,
            content: "hello".into(),
            tool_name: None,

            tool_use_id: None,
            is_error: false,
        };
        let ae = session_event_to_agent_event(&se).unwrap();
        assert_eq!(ae, AgentEvent::UserMessage { content: "hello".into() });
    }

    #[test]
    fn session_event_conversion_tool_use() {
        let se = session_log::SessionEvent {
            timestamp: String::new(),

            event_type: SessionEventType::ToolUse,
            content: r#"{"command": "ls"}"#.into(),
            tool_name: Some("Bash".into()),

            tool_use_id: Some("tu_01".into()),
            is_error: false,
        };
        let ae = session_event_to_agent_event(&se).unwrap();
        match ae {
            AgentEvent::ToolUse { tool, input, tool_use_id } => {
                assert_eq!(tool, "Bash");
                assert_eq!(input["command"], "ls");
                assert_eq!(tool_use_id.as_deref(), Some("tu_01"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn session_event_conversion_tool_result() {
        let se = session_log::SessionEvent {
            timestamp: String::new(),

            event_type: SessionEventType::ToolResult,
            content: "ok".into(),
            tool_name: Some("Bash".into()),

            tool_use_id: Some("tu_01".into()),
            is_error: true,
        };
        let ae = session_event_to_agent_event(&se).unwrap();
        assert_eq!(ae, AgentEvent::ToolResult {
            tool: "Bash".into(),
            output: "ok".into(),
            is_error: true,
            tool_use_id: Some("tu_01".into()),
        });
    }

    #[test]
    fn session_event_conversion_system_is_none() {
        let se = session_log::SessionEvent {
            timestamp: String::new(),

            event_type: SessionEventType::SystemMessage,
            content: "system info".into(),
            tool_name: None,

            tool_use_id: None,
            is_error: false,
        };
        assert!(session_event_to_agent_event(&se).is_none());
    }

    #[test]
    fn tailer_reads_new_lines_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        // Write initial content.
        {
            let mut f = File::create(&path).unwrap();
            writeln!(f, r#"{{"type":"user","content":"old message","timestamp":"2026-04-02T00:00:00Z"}}"#).unwrap();
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let mut tailer = SessionJsonlTailer::new();

        // Simulate being set up already (skip resolve since we don't have ~/.claude/).
        tailer.current_session_id = Some("test".into());
        let initial_len = std::fs::metadata(&path).unwrap().len();
        tailer.read_offset = initial_len; // skip existing content
        tailer.current_path = Some(path.clone());

        // Append a new line.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, r#"{{"type":"user","content":"new message","timestamp":"2026-04-02T00:01:00Z"}}"#).unwrap();
        }

        // Poll should pick up the new line.
        tailer.read_new_lines(&path, &tx);

        let event = rx.try_recv().unwrap();
        assert_eq!(event, AgentEvent::UserMessage { content: "new message".into() });

        // No more events.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn tailer_handles_nested_assistant_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        let (tx, rx) = std::sync::mpsc::channel();
        let mut tailer = SessionJsonlTailer::new();
        tailer.current_session_id = Some("test".into());
        tailer.current_path = Some(path.clone());
        tailer.read_offset = 0;

        // Write a nested-format assistant message with tool_use.
        {
            let mut f = File::create(&path).unwrap();
            writeln!(f, r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"Let me check."}},{{"type":"tool_use","name":"Bash","id":"tu_abc","input":{{"command":"ls"}}}}]}},"timestamp":"2026-04-02T00:14:00.000Z"}}"#).unwrap();
        }

        tailer.read_new_lines(&path, &tx);

        // Should get two events: AssistantMessage + ToolUse.
        let ev1 = rx.try_recv().unwrap();
        assert_eq!(ev1, AgentEvent::AssistantMessage { content: "Let me check.".into() });

        let ev2 = rx.try_recv().unwrap();
        match ev2 {
            AgentEvent::ToolUse { tool, tool_use_id, .. } => {
                assert_eq!(tool, "Bash");
                assert_eq!(tool_use_id.as_deref(), Some("tu_abc"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        assert!(rx.try_recv().is_err());
    }
}
