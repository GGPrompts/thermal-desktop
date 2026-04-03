/// Agent status module for the bar's right zone.
///
/// Reads agent session state directly from conductor's `SemanticEventBus`
/// (in-process, no D-Bus or file polling needed).
///
/// Display prefixes: "CLU" for Claude, "COX" for Codex, "COP" for Copilot.
use std::sync::Arc;

use thermal_core::{ClaudeSessionState, ClaudeStatus, ThermalPalette};

use crate::bar::layout::{ModuleOutput, Zone};
use crate::protocol::{AgentActivity, AgentRuntime, EventScope};
use crate::semantic_state::SemanticEventBus;

/// Backward-compatible alias.
pub type ClaudeModule = AgentModule;

pub struct AgentModule {
    event_bus: Arc<SemanticEventBus>,
}

impl AgentModule {
    pub fn new(event_bus: Arc<SemanticEventBus>) -> Self {
        Self { event_bus }
    }

    /// Poll agent sessions from the event bus and produce right-zone module outputs.
    pub fn render(&self) -> Vec<ModuleOutput> {
        let sessions = sessions_from_bus(&self.event_bus);

        if sessions.is_empty() {
            return Vec::new();
        }

        // Partition sessions by agent type.
        let mut claude_sessions: Vec<&ClaudeSessionState> = Vec::new();
        let mut codex_sessions: Vec<&ClaudeSessionState> = Vec::new();
        let mut copilot_sessions: Vec<&ClaudeSessionState> = Vec::new();

        for s in &sessions {
            match s.agent_type.as_deref() {
                Some("codex") => codex_sessions.push(s),
                Some("copilot") => copilot_sessions.push(s),
                _ => claude_sessions.push(s),
            }
        }

        let mut outputs = Vec::new();

        if let Some(output) = build_agent_summary("CLU", &claude_sessions) {
            outputs.push(output);
        }
        if let Some(output) = build_agent_summary("COX", &codex_sessions) {
            outputs.push(output);
        }
        if let Some(output) = build_agent_summary("COP", &copilot_sessions) {
            outputs.push(output);
        }

        outputs
    }
}

/// Convert event bus snapshots to session states for display.
fn sessions_from_bus(bus: &SemanticEventBus) -> Vec<ClaudeSessionState> {
    let syncs = bus.snapshot_syncs(&EventScope::All);
    syncs
        .iter()
        .filter(|s| s.snapshot.is_alive)
        .map(|s| snapshot_to_session_state(&s.snapshot))
        .collect()
}

fn snapshot_to_session_state(
    snap: &crate::protocol::SemanticSessionSnapshot,
) -> ClaudeSessionState {
    let status = match snap.agent_activity {
        AgentActivity::Idle | AgentActivity::Exited => ClaudeStatus::Idle,
        AgentActivity::Prompting | AgentActivity::WaitingInput => ClaudeStatus::AwaitingInput,
        AgentActivity::Thinking | AgentActivity::StreamingOutput => ClaudeStatus::Processing,
        AgentActivity::ToolRunning => ClaudeStatus::ToolUse,
    };

    let agent_type = match snap.runtime {
        AgentRuntime::Claude => Some("claude".into()),
        AgentRuntime::Codex => Some("codex".into()),
        AgentRuntime::Copilot => Some("copilot".into()),
        AgentRuntime::Unknown => None,
    };

    ClaudeSessionState {
        session_id: snap.session_id.clone(),
        status,
        current_tool: snap.current_tool.clone(),
        working_dir: snap.cwd.clone(),
        context_percent: snap.context_state.saturation.map(|s| s * 100.0),
        agent_type,
        last_updated: snap.last_activity_at.clone(),
        pid: snap.pid.map(|p| p as i64),
        ..ClaudeSessionState::default()
    }
}

/// Build a single ModuleOutput for a group of sessions with the given prefix.
fn build_agent_summary(prefix: &str, sessions: &[&ClaudeSessionState]) -> Option<ModuleOutput> {
    if sessions.is_empty() {
        return None;
    }

    let total = sessions.len();
    let mut tool_use = 0usize;
    let mut processing = 0usize;
    let mut idle = 0usize;
    let mut awaiting = 0usize;

    for s in sessions {
        match s.status {
            ClaudeStatus::ToolUse => tool_use += 1,
            ClaudeStatus::Processing => processing += 1,
            ClaudeStatus::Idle => idle += 1,
            ClaudeStatus::AwaitingInput => awaiting += 1,
        }
    }

    let color = if tool_use > 0 {
        ThermalPalette::ACCENT_HOT
    } else if processing > 0 {
        ThermalPalette::ACCENT_WARM
    } else if awaiting > 0 {
        ThermalPalette::ACCENT_COOL
    } else {
        ThermalPalette::ACCENT_COLD
    };

    let mut parts: Vec<String> = Vec::new();
    if tool_use > 0 {
        parts.push(format!("{tool_use} tool"));
    }
    if processing > 0 {
        parts.push(format!("{processing} run"));
    }
    if awaiting > 0 {
        parts.push(format!("{awaiting} wait"));
    }
    if idle > 0 {
        parts.push(format!("{idle} idle"));
    }

    let summary = if parts.len() == 1 && total == 1 {
        format!("{prefix} {}", parts[0])
    } else {
        format!("{prefix} {total}: {}", parts.join(", "))
    };

    Some(ModuleOutput::new(Zone::Right, summary, color))
}
