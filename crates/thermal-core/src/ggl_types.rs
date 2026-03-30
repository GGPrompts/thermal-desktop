//! Generated ggl types with convenience aliases and trait impls.
//!
//! The raw generated types are versioned (`AgentIdV1`, `TaskStateV1`).
//! This module re-exports them under stable names for use across the codebase.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// Pull in the generated code from build.rs.
include!(concat!(env!("OUT_DIR"), "/thermal-protocol.rs"));

// ── Type aliases ─────────────────────────────────────────────────────────────

/// Current version of `AgentId` used throughout the codebase.
pub type AgentId = AgentIdV1;

/// Current version of `TaskState` used throughout the codebase.
pub type TaskState = TaskStateV1;

// ClaudeStatusV1 is generated but not aliased here — ClaudeStatus stays
// hand-written in claude_state.rs because it needs #[serde(rename_all)].

/// Current version of `ToolArgs` used throughout the codebase.
pub type ToolArgs = ToolArgsV1;

/// Current version of `ToolDetails` used throughout the codebase.
pub type ToolDetails = ToolDetailsV1;

// ── Default impls (codegen doesn't emit these) ──────────────────────────────

impl Default for ToolArgs {
    fn default() -> Self {
        Self {
            file_path: None,
            command: None,
            pattern: None,
            description: None,
        }
    }
}

impl Default for ToolDetails {
    fn default() -> Self {
        Self {
            event: None,
            tool: None,
            args: None,
        }
    }
}

// ── AgentId extras ───────────────────────────────────────────────────────────

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
