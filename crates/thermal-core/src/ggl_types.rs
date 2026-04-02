//! Generated ggl types with convenience aliases and trait impls.
//!
//! The raw generated types are versioned (`AgentIdV1`, `TaskStateV1`).
//! This module re-exports them under stable names for use across the codebase.
//!
//! NOTE: The `include!()` below pulls in Rust code generated at build time by
//! `ggl-build` (see `build.rs`). rust-analyzer will only resolve these types if
//! build scripts are enabled (`rust-analyzer.cargo.buildScripts.enable: true`).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::palette::Color;

// Pull in the generated code from build.rs.
include!(concat!(env!("OUT_DIR"), "/thermal-protocol.rs"));

// ── Type aliases ─────────────────────────────────────────────────────────────

/// Current version of `AgentId` used throughout the codebase.
pub type AgentId = AgentIdV1;

/// Current version of `TaskState` used throughout the codebase.
pub type TaskState = TaskStateV1;

/// Current version of `ClaudeStatus` used throughout the codebase.
pub type ClaudeStatus = ClaudeStatusV1;

/// Current version of `SessionState` used throughout the codebase.
pub type SessionState = SessionStateV1;

/// Current version of `ToolArgs` used throughout the codebase.
pub type ToolArgs = ToolArgsV1;

/// Current version of `ToolDetails` used throughout the codebase.
pub type ToolDetails = ToolDetailsV1;

/// Current version of `Layout` used throughout the codebase.
pub type Layout = LayoutV1;

/// Current version of `AgentState` used throughout the codebase.
pub type AgentState = AgentStateV1;

/// Current version of `ConductorConfig` used throughout the codebase.
pub type ConductorConfig = ConductorConfigV1;

/// Current version of `PaneInfo` used throughout the codebase.
pub type PaneInfo = PaneInfoV1;

// ── Default impls for types with non-Option fields ─────────────────────────

impl Default for ClaudeStatus {
    fn default() -> Self {
        ClaudeStatusV1::Idle
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            parent_session_id: None,
            agent_id: None,
            agent_type: None,
            model: None,
            status: ClaudeStatusV1::Idle,
            current_tool: None,
            subagent_count: Some(0),
            context_percent: None,
            working_dir: None,
            last_updated: None,
            details: None,
            hook_type: None,
            tmux_pane: None,
            pid: None,
            workspace: None,
            source: None,
            last_command: None,
            last_exit_code: None,
            last_command_started_at: None,
            last_command_duration_ms: None,
            consecutive_failures: None,
        }
    }
}

impl Default for ConductorConfig {
    fn default() -> Self {
        Self {
            tmux_session: "thermal-conductor".to_string(),
            max_panes: 16,
            capture_fps: 30,
            layout: LayoutV1::Grid,
            audio_enabled: true,
            dbus_enabled: true,
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

// ── AgentState extras ────────────────────────────────────────────────────────

impl AgentState {
    /// Returns the thermal-palette `Color` that represents this state.
    pub fn color(self) -> Color {
        match self {
            AgentState::Idle => Color::ACCENT_COOL,
            AgentState::Running => Color::WARM,
            AgentState::Thinking => Color::HOT,
            AgentState::Warning => Color::HOTTER,
            AgentState::Error => Color::SEARING,
            AgentState::Complete => Color::WHITE_HOT,
        }
    }

    /// Short uppercase label suitable for HUD readouts.
    pub fn label(self) -> &'static str {
        match self {
            AgentState::Idle => "IDLE",
            AgentState::Running => "RUNNING",
            AgentState::Thinking => "THINKING",
            AgentState::Warning => "WARNING",
            AgentState::Error => "ERROR",
            AgentState::Complete => "COMPLETE",
        }
    }

    /// Single-character icon for compact status indicators.
    pub fn icon(self) -> &'static str {
        match self {
            AgentState::Idle => "○",
            AgentState::Running => "◉",
            AgentState::Thinking => "◎",
            AgentState::Warning => "▲",
            AgentState::Error => "✗",
            AgentState::Complete => "✓",
        }
    }
}

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
