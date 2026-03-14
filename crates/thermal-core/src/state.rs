use serde::{Deserialize, Serialize};

use crate::palette::Color;

/// The operational state of a single agent pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentState {
    /// Waiting for input — cold blue.
    Idle,
    /// Active output in progress — warm green.
    Running,
    /// Producing / reasoning — yellow.
    Thinking,
    /// Warning condition — orange.
    Warning,
    /// Failed — searing red.
    Error,
    /// Just finished successfully — white-hot flash.
    Complete,
}

impl AgentState {
    /// Returns the thermal-palette `Color` that represents this state.
    pub fn color(self) -> Color {
        match self {
            AgentState::Idle => Color::ACCENT_COOL,   // #3b82f6 — cool blue
            AgentState::Running => Color::WARM,       // #22c55e — warm green
            AgentState::Thinking => Color::HOT,       // #eab308 — yellow
            AgentState::Warning => Color::HOTTER,     // #f97316 — orange
            AgentState::Error => Color::SEARING,      // #ef4444 — red
            AgentState::Complete => Color::WHITE_HOT, // #fef3c7 — white-hot
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
