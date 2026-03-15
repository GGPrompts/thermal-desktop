/// Claude agent status module for thermal-bar.
///
/// Monitors active Claude Code sessions via `ClaudeStatePoller` and
/// displays an aggregate status summary in the bar's right zone.
/// Shows nothing when no sessions are active to avoid clutter.
use thermal_core::{ClaudeStatePoller, ClaudeStatus, ThermalPalette};

use crate::layout::{ModuleOutput, Zone};

pub struct ClaudeModule {
    poller: ClaudeStatePoller,
}

impl ClaudeModule {
    pub fn new() -> Self {
        // ClaudeStatePoller::new() creates /tmp/claude-code-state/ if needed
        // and sets up a filesystem watcher. If it fails (e.g. no inotify),
        // we still want the bar to run, so fall back gracefully.
        let poller = ClaudeStatePoller::new().expect("failed to create ClaudeStatePoller");
        Self { poller }
    }

    /// Poll Claude sessions and produce right-zone module outputs.
    ///
    /// Returns an empty vec if no sessions are active.
    pub fn render(&mut self) -> Vec<ModuleOutput> {
        let sessions = self.poller.poll();

        if sessions.is_empty() {
            return Vec::new();
        }

        let total = sessions.len();
        let mut tool_use = 0usize;
        let mut processing = 0usize;
        let mut idle = 0usize;
        let mut awaiting = 0usize;

        for s in &sessions {
            match s.status {
                ClaudeStatus::ToolUse => tool_use += 1,
                ClaudeStatus::Processing => processing += 1,
                ClaudeStatus::Idle => idle += 1,
                ClaudeStatus::AwaitingInput => awaiting += 1,
            }
        }

        // Pick the dominant color based on hottest active state.
        let color = if tool_use > 0 {
            ThermalPalette::ACCENT_HOT
        } else if processing > 0 {
            ThermalPalette::ACCENT_WARM
        } else if awaiting > 0 {
            ThermalPalette::ACCENT_COOL
        } else {
            ThermalPalette::ACCENT_COLD
        };

        // Build a compact summary string.
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
            // Single session — just show the status directly.
            format!("CLU {}", parts[0])
        } else {
            format!("CLU {total}: {}", parts.join(", "))
        };

        vec![ModuleOutput::new(Zone::Right, summary, color)]
    }
}
