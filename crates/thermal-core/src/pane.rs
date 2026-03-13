use serde::{Deserialize, Serialize};

use crate::state::AgentState;

/// Snapshot of a single tmux/PTY pane's state and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Stable identifier, e.g. `"pane-0"` or tmux `"%0"`.
    pub id: String,

    /// Human-readable display title shown in the HUD header.
    pub title: String,

    /// Current operational state of the agent running in this pane.
    pub state: AgentState,

    /// The command currently running inside the pane (argv[0] or full cmdline).
    pub command: String,

    /// The most recent line of output captured from the pane.
    pub last_output_line: String,

    /// Total number of output lines captured since the pane was created.
    pub output_lines: usize,

    /// Unix timestamp (seconds) when this pane was first created.
    pub created_at: u64,

    /// Unix timestamp (seconds) of the most recent activity in this pane.
    pub updated_at: u64,
}
