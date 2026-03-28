//! Wire protocol types for the thermal-messages bus.
//!
//! Shared by all message bus components: daemon, TUI chat, `td` CLI,
//! and the dispatcher bridge.  Follows the same serde patterns used by
//! `thermal-conductor`'s `protocol.rs`.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── AgentId ────────────────────────────────────────────────────────────────────

/// Identifies a participant on the message bus.
///
/// Display format: `"type/key"` (e.g. `"claude/proj-abc"`, `"user/alice"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId {
    /// Agent kind — e.g. `"claude"`, `"codex"`, `"copilot"`, `"user"`, `"dispatcher"`.
    pub agent_type: String,
    /// Instance key — e.g. a session id or username.
    pub key: String,
}

impl AgentId {
    /// Convenience constructor.
    pub fn new(agent_type: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            agent_type: agent_type.into(),
            key: key.into(),
        }
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.agent_type, self.key)
    }
}

/// Error returned when parsing an `AgentId` from a string that does not
/// contain exactly one `/` separator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseAgentIdError(pub String);

impl fmt::Display for ParseAgentIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid AgentId '{}': expected format 'type/key'",
            self.0
        )
    }
}

impl std::error::Error for ParseAgentIdError {}

impl FromStr for AgentId {
    type Err = ParseAgentIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (agent_type, key) = s
            .split_once('/')
            .ok_or_else(|| ParseAgentIdError(s.to_string()))?;
        if agent_type.is_empty() || key.is_empty() {
            return Err(ParseAgentIdError(s.to_string()));
        }
        Ok(Self {
            agent_type: agent_type.to_string(),
            key: key.to_string(),
        })
    }
}

// ── TaskState ──────────────────────────────────────────────────────────────────

/// Lifecycle state for a tracked task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskState {
    Submitted,
    Working,
    Completed,
    Failed,
    InputRequired,
}

// ── MessageType ────────────────────────────────────────────────────────────────

/// Discriminated message kind.  Serialized with `#[serde(tag = "type")]` so the
/// JSON representation includes a `"type"` field for easy routing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessageType {
    /// A regular agent-to-agent (or agent-to-user) message.
    AgentMsg,
    /// Subscribe to the message stream, optionally replaying from a sequence number.
    Subscribe { since_seq: Option<u64> },
    /// Acknowledge receipt of a prior message.
    Ack { ref_seq: u64 },
    /// Server-side notification that the ring buffer has overflowed.
    RingOverflow { oldest_available: u64 },
    /// Task lifecycle status update.
    TaskStatus { task_id: String, state: TaskState },
}

// ── Message ────────────────────────────────────────────────────────────────────

/// A single message on the thermal message bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Monotonically increasing sequence number assigned by the daemon.
    pub seq: u64,
    /// Unix-epoch timestamp in milliseconds.
    pub ts: u64,
    /// Sender.
    pub from: AgentId,
    /// Recipient (or broadcast target).
    pub to: AgentId,
    /// Optional conversation / context thread id.
    pub context_id: Option<String>,
    /// Optional project scope.
    pub project: Option<String>,
    /// Human-readable message body.
    pub content: String,
    /// The message kind — flattened so `"type"` appears at the top level.
    #[serde(flatten)]
    pub msg_type: MessageType,
    /// Arbitrary extension metadata.
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── AgentId Display / FromStr ────────────────────────────────────────────

    #[test]
    fn agent_id_display() {
        let id = AgentId::new("claude", "proj-abc");
        assert_eq!(id.to_string(), "claude/proj-abc");
    }

    #[test]
    fn agent_id_from_str_valid() {
        let id: AgentId = "codex/session-1".parse().unwrap();
        assert_eq!(id.agent_type, "codex");
        assert_eq!(id.key, "session-1");
    }

    #[test]
    fn agent_id_from_str_no_slash() {
        let result: Result<AgentId, _> = "no-slash".parse();
        assert!(result.is_err());
    }

    #[test]
    fn agent_id_from_str_empty_type() {
        let result: Result<AgentId, _> = "/key".parse();
        assert!(result.is_err());
    }

    #[test]
    fn agent_id_from_str_empty_key() {
        let result: Result<AgentId, _> = "type/".parse();
        assert!(result.is_err());
    }

    #[test]
    fn agent_id_from_str_multiple_slashes() {
        // split_once takes the first slash — remainder becomes key.
        let id: AgentId = "user/alice/extra".parse().unwrap();
        assert_eq!(id.agent_type, "user");
        assert_eq!(id.key, "alice/extra");
    }

    #[test]
    fn agent_id_round_trip_display_parse() {
        let original = AgentId::new("dispatcher", "main");
        let parsed: AgentId = original.to_string().parse().unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn agent_id_equality() {
        let a = AgentId::new("claude", "x");
        let b = AgentId::new("claude", "x");
        let c = AgentId::new("claude", "y");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // ── TaskState serde ─────────────────────────────────────────────────────

    #[test]
    fn task_state_json_round_trip() {
        for state in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::InputRequired,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let decoded: TaskState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, decoded);
        }
    }

    // ── MessageType serde ───────────────────────────────────────────────────

    #[test]
    fn message_type_agent_msg_json() {
        let mt = MessageType::AgentMsg;
        let json = serde_json::to_string(&mt).unwrap();
        assert!(json.contains(r#""type":"AgentMsg"#));
        let decoded: MessageType = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, MessageType::AgentMsg);
    }

    #[test]
    fn message_type_subscribe_json() {
        let mt = MessageType::Subscribe {
            since_seq: Some(42),
        };
        let json = serde_json::to_string(&mt).unwrap();
        let decoded: MessageType = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, mt);
    }

    #[test]
    fn message_type_subscribe_none_json() {
        let mt = MessageType::Subscribe { since_seq: None };
        let json = serde_json::to_string(&mt).unwrap();
        let decoded: MessageType = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, mt);
    }

    #[test]
    fn message_type_ack_json() {
        let mt = MessageType::Ack { ref_seq: 99 };
        let json = serde_json::to_string(&mt).unwrap();
        let decoded: MessageType = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, mt);
    }

    #[test]
    fn message_type_ring_overflow_json() {
        let mt = MessageType::RingOverflow {
            oldest_available: 500,
        };
        let json = serde_json::to_string(&mt).unwrap();
        let decoded: MessageType = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, mt);
    }

    #[test]
    fn message_type_task_status_json() {
        let mt = MessageType::TaskStatus {
            task_id: "task-001".into(),
            state: TaskState::Working,
        };
        let json = serde_json::to_string(&mt).unwrap();
        let decoded: MessageType = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, mt);
    }

    // ── Full Message serde ──────────────────────────────────────────────────

    fn sample_message() -> Message {
        Message {
            seq: 1,
            ts: 1711500000000,
            from: AgentId::new("claude", "sess-1"),
            to: AgentId::new("user", "alice"),
            context_id: Some("ctx-abc".into()),
            project: Some("thermal-desktop".into()),
            content: "Build complete.".into(),
            msg_type: MessageType::AgentMsg,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn message_json_round_trip() {
        let msg = sample_message();
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn message_json_has_flattened_type() {
        let msg = sample_message();
        let val: Value = serde_json::to_value(&msg).unwrap();
        // `type` should appear at the top level thanks to #[serde(flatten)]
        assert_eq!(val["type"], "AgentMsg");
    }

    #[test]
    fn message_with_metadata_round_trip() {
        let mut msg = sample_message();
        msg.metadata
            .insert("tool".into(), Value::String("cargo".into()));
        msg.metadata
            .insert("exit_code".into(), Value::Number(0.into()));
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.metadata["tool"], "cargo");
        assert_eq!(decoded.metadata["exit_code"], 0);
    }

    #[test]
    fn message_with_task_status_round_trip() {
        let msg = Message {
            seq: 5,
            ts: 1711500001000,
            from: AgentId::new("dispatcher", "main"),
            to: AgentId::new("claude", "sess-2"),
            context_id: None,
            project: None,
            content: "Task failed".into(),
            msg_type: MessageType::TaskStatus {
                task_id: "t-99".into(),
                state: TaskState::Failed,
            },
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.seq, 5);
        assert_eq!(
            decoded.msg_type,
            MessageType::TaskStatus {
                task_id: "t-99".into(),
                state: TaskState::Failed,
            }
        );
    }

    #[test]
    fn message_subscribe_round_trip() {
        let msg = Message {
            seq: 0,
            ts: 1711500000000,
            from: AgentId::new("user", "bob"),
            to: AgentId::new("daemon", "bus"),
            context_id: None,
            project: None,
            content: String::new(),
            msg_type: MessageType::Subscribe {
                since_seq: Some(100),
            },
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.msg_type,
            MessageType::Subscribe {
                since_seq: Some(100)
            }
        );
    }

    #[test]
    fn message_empty_optional_fields() {
        let msg = Message {
            seq: 0,
            ts: 0,
            from: AgentId::new("a", "b"),
            to: AgentId::new("c", "d"),
            context_id: None,
            project: None,
            content: String::new(),
            msg_type: MessageType::AgentMsg,
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert!(decoded.context_id.is_none());
        assert!(decoded.project.is_none());
        assert!(decoded.metadata.is_empty());
    }

    // ── AgentId JSON serde ──────────────────────────────────────────────────

    #[test]
    fn agent_id_json_round_trip() {
        let id = AgentId::new("copilot", "ws-7");
        let json = serde_json::to_string(&id).unwrap();
        let decoded: AgentId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, decoded);
    }
}
