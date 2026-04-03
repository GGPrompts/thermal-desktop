//! Structured JSON output parser for AI agent sessions.
//!
//! When an agent (e.g. Claude Code) is launched with `--output-format json`,
//! its PTY output consists of JSONL — one JSON object per line with a `type`
//! field identifying the message kind.
//!
//! This module parses those JSON lines into typed [`AgentEvent`] variants that
//! can drive rich widget rendering in a future GPU overlay (therm-cgos).
//!
//! # CC JSON message types
//!
//! The `type` field values emitted by Claude Code `--output-format json`:
//! - `"user"` — User message
//! - `"assistant"` — Assistant text response
//! - `"tool_use"` — Tool invocation request
//! - `"tool_result"` — Tool execution result
//! - `"progress"` — Progress/status update for a running tool
//! - `"thinking"` — Model thinking/reasoning content

use serde::Deserialize;

/// A parsed event from an agent's structured JSON output stream.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AgentEvent {
    /// User message sent to the agent.
    UserMessage { content: String },
    /// Assistant text response.
    AssistantMessage { content: String },
    /// Tool invocation request from the agent.
    ToolUse {
        tool: String,
        input: serde_json::Value,
    },
    /// Result of a tool execution.
    ToolResult {
        tool: String,
        output: String,
        is_error: bool,
    },
    /// Progress update for a running tool.
    Progress {
        tool: String,
        status: String,
        message: Option<String>,
    },
    /// Model thinking/reasoning content.
    Thinking { content: String },
}

/// Raw JSON envelope — just enough structure to route by `type`.
#[derive(Deserialize)]
struct RawEnvelope {
    #[serde(rename = "type")]
    msg_type: String,

    // Optional fields — present depending on `type`.
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    tool: Option<String>,
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

/// Parse a single line of agent JSON output into an [`AgentEvent`].
///
/// Returns `None` if the line is not valid JSON, lacks a `type` field,
/// or has an unrecognized type. Unknown types are silently ignored so
/// that new CC message types don't break existing parsing.
pub(crate) fn parse_agent_event(line: &str) -> Option<AgentEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.starts_with('{') {
        return None;
    }

    let envelope: RawEnvelope = serde_json::from_str(trimmed).ok()?;

    match envelope.msg_type.as_str() {
        "user" => Some(AgentEvent::UserMessage {
            content: extract_content_string(&envelope.content),
        }),
        "assistant" => Some(AgentEvent::AssistantMessage {
            content: extract_content_string(&envelope.content),
        }),
        "tool_use" => Some(AgentEvent::ToolUse {
            tool: envelope.tool.unwrap_or_default(),
            input: envelope.input.unwrap_or(serde_json::Value::Null),
        }),
        "tool_result" => Some(AgentEvent::ToolResult {
            tool: envelope.tool.unwrap_or_default(),
            output: envelope.output.unwrap_or_default(),
            is_error: envelope.is_error.unwrap_or(false),
        }),
        "progress" => Some(AgentEvent::Progress {
            tool: envelope.tool.unwrap_or_default(),
            status: envelope.status.unwrap_or_default(),
            message: envelope.message,
        }),
        "thinking" => Some(AgentEvent::Thinking {
            content: extract_content_string(&envelope.content),
        }),
        _ => {
            tracing::trace!(msg_type = %envelope.msg_type, "Unknown agent JSON message type");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_message() {
        let line = r#"{"type":"user","content":"Hello agent"}"#;
        let event = parse_agent_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::UserMessage {
                content: "Hello agent".into()
            }
        );
    }

    #[test]
    fn parse_assistant_message() {
        let line = r#"{"type":"assistant","content":"I'll help you with that."}"#;
        let event = parse_agent_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::AssistantMessage {
                content: "I'll help you with that.".into()
            }
        );
    }

    #[test]
    fn parse_tool_use() {
        let line = r#"{"type":"tool_use","tool":"Read","input":{"file_path":"/tmp/test.rs"}}"#;
        let event = parse_agent_event(line).unwrap();
        match event {
            AgentEvent::ToolUse { tool, input } => {
                assert_eq!(tool, "Read");
                assert_eq!(input["file_path"], "/tmp/test.rs");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_tool_result() {
        let line = r#"{"type":"tool_result","tool":"Bash","output":"ok","is_error":false}"#;
        let event = parse_agent_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::ToolResult {
                tool: "Bash".into(),
                output: "ok".into(),
                is_error: false,
            }
        );
    }

    #[test]
    fn parse_tool_result_error() {
        let line =
            r#"{"type":"tool_result","tool":"Bash","output":"command not found","is_error":true}"#;
        let event = parse_agent_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::ToolResult {
                tool: "Bash".into(),
                output: "command not found".into(),
                is_error: true,
            }
        );
    }

    #[test]
    fn parse_progress() {
        let line =
            r#"{"type":"progress","tool":"Bash","status":"running","message":"Compiling..."}"#;
        let event = parse_agent_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::Progress {
                tool: "Bash".into(),
                status: "running".into(),
                message: Some("Compiling...".into()),
            }
        );
    }

    #[test]
    fn parse_thinking() {
        let line = r#"{"type":"thinking","content":"Let me analyze the code..."}"#;
        let event = parse_agent_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::Thinking {
                content: "Let me analyze the code...".into()
            }
        );
    }

    #[test]
    fn parse_unknown_type_returns_none() {
        let line = r#"{"type":"system_info","version":"1.0"}"#;
        assert!(parse_agent_event(line).is_none());
    }

    #[test]
    fn parse_invalid_json_returns_none() {
        assert!(parse_agent_event("not json").is_none());
        assert!(parse_agent_event("").is_none());
        assert!(parse_agent_event("  ").is_none());
    }

    #[test]
    fn parse_non_object_json_returns_none() {
        assert!(parse_agent_event("[1,2,3]").is_none());
        assert!(parse_agent_event(r#""string""#).is_none());
    }

    #[test]
    fn parse_missing_content_defaults_empty() {
        let line = r#"{"type":"user"}"#;
        let event = parse_agent_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::UserMessage {
                content: String::new()
            }
        );
    }

    #[test]
    fn parse_with_leading_whitespace() {
        let line = r#"  {"type":"assistant","content":"trimmed"}"#;
        let event = parse_agent_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::AssistantMessage {
                content: "trimmed".into()
            }
        );
    }

    #[test]
    fn parse_array_content() {
        let line = r#"{"type":"assistant","content":[{"type":"text","text":"Hello "},{"type":"text","text":"world"}]}"#;
        let event = parse_agent_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::AssistantMessage {
                content: "Hello world".into()
            }
        );
    }

    #[test]
    fn parse_thinking_array_content() {
        let line = r#"{"type":"thinking","content":[{"type":"text","text":"Let me think..."}]}"#;
        let event = parse_agent_event(line).unwrap();
        assert_eq!(
            event,
            AgentEvent::Thinking {
                content: "Let me think...".into()
            }
        );
    }
}
