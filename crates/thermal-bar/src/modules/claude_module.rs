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

// ---------------------------------------------------------------------------
// Pure helper — testable without ClaudeStatePoller
// ---------------------------------------------------------------------------

/// Compute the summary text and color from a slice of `ClaudeStatus` values.
///
/// This mirrors the logic in `ClaudeModule::render()` but accepts a pre-built
/// slice so unit tests can drive it without touching the filesystem.
pub(crate) fn build_claude_summary(
    statuses: &[ClaudeStatus],
) -> Option<(String, [f32; 4])> {
    if statuses.is_empty() {
        return None;
    }

    let total = statuses.len();
    let mut tool_use = 0usize;
    let mut processing = 0usize;
    let mut idle = 0usize;
    let mut awaiting = 0usize;

    for s in statuses {
        match s {
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
    if tool_use > 0   { parts.push(format!("{tool_use} tool")); }
    if processing > 0 { parts.push(format!("{processing} run")); }
    if awaiting > 0   { parts.push(format!("{awaiting} wait")); }
    if idle > 0       { parts.push(format!("{idle} idle")); }

    let summary = if parts.len() == 1 && total == 1 {
        format!("CLU {}", parts[0])
    } else {
        format!("CLU {total}: {}", parts.join(", "))
    };

    Some((summary, color))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // build_claude_summary — empty input
    // -----------------------------------------------------------------------

    #[test]
    fn summary_empty_returns_none() {
        assert!(build_claude_summary(&[]).is_none());
    }

    // -----------------------------------------------------------------------
    // Single session, each status
    // -----------------------------------------------------------------------

    #[test]
    fn summary_single_tool_use() {
        let (text, color) = build_claude_summary(&[ClaudeStatus::ToolUse]).unwrap();
        assert_eq!(text, "CLU 1 tool");
        assert_eq!(color, ThermalPalette::ACCENT_HOT);
    }

    #[test]
    fn summary_single_processing() {
        let (text, color) = build_claude_summary(&[ClaudeStatus::Processing]).unwrap();
        assert_eq!(text, "CLU 1 run");
        assert_eq!(color, ThermalPalette::ACCENT_WARM);
    }

    #[test]
    fn summary_single_idle() {
        let (text, color) = build_claude_summary(&[ClaudeStatus::Idle]).unwrap();
        assert_eq!(text, "CLU 1 idle");
        assert_eq!(color, ThermalPalette::ACCENT_COLD);
    }

    #[test]
    fn summary_single_awaiting_input() {
        let (text, color) = build_claude_summary(&[ClaudeStatus::AwaitingInput]).unwrap();
        assert_eq!(text, "CLU 1 wait");
        assert_eq!(color, ThermalPalette::ACCENT_COOL);
    }

    // -----------------------------------------------------------------------
    // Multiple sessions — aggregate formatting
    // -----------------------------------------------------------------------

    #[test]
    fn summary_multiple_sessions_shows_total() {
        let statuses = [ClaudeStatus::Idle, ClaudeStatus::Idle, ClaudeStatus::Idle];
        let (text, _) = build_claude_summary(&statuses).unwrap();
        assert!(text.starts_with("CLU 3:"), "expected 'CLU 3:' prefix, got '{text}'");
    }

    #[test]
    fn summary_two_sessions_one_each() {
        let statuses = [ClaudeStatus::ToolUse, ClaudeStatus::Idle];
        let (text, color) = build_claude_summary(&statuses).unwrap();
        // tool_use > 0 → color should be ACCENT_HOT
        assert_eq!(color, ThermalPalette::ACCENT_HOT);
        assert!(text.contains("tool"), "should mention tool: '{text}'");
        assert!(text.contains("idle"), "should mention idle: '{text}'");
    }

    #[test]
    fn summary_all_statuses_present() {
        let statuses = [
            ClaudeStatus::ToolUse,
            ClaudeStatus::Processing,
            ClaudeStatus::Idle,
            ClaudeStatus::AwaitingInput,
        ];
        let (text, color) = build_claude_summary(&statuses).unwrap();
        assert_eq!(color, ThermalPalette::ACCENT_HOT, "tool_use should dominate");
        assert!(text.contains("tool"),  "text='{text}'");
        assert!(text.contains("run"),   "text='{text}'");
        assert!(text.contains("idle"),  "text='{text}'");
        assert!(text.contains("wait"),  "text='{text}'");
    }

    // -----------------------------------------------------------------------
    // Color priority: ToolUse > Processing > AwaitingInput > Idle
    // -----------------------------------------------------------------------

    #[test]
    fn color_priority_tool_use_beats_processing() {
        let statuses = [ClaudeStatus::ToolUse, ClaudeStatus::Processing];
        let (_, color) = build_claude_summary(&statuses).unwrap();
        assert_eq!(color, ThermalPalette::ACCENT_HOT);
    }

    #[test]
    fn color_priority_processing_beats_awaiting() {
        let statuses = [ClaudeStatus::Processing, ClaudeStatus::AwaitingInput];
        let (_, color) = build_claude_summary(&statuses).unwrap();
        assert_eq!(color, ThermalPalette::ACCENT_WARM);
    }

    #[test]
    fn color_priority_awaiting_beats_idle() {
        let statuses = [ClaudeStatus::AwaitingInput, ClaudeStatus::Idle];
        let (_, color) = build_claude_summary(&statuses).unwrap();
        assert_eq!(color, ThermalPalette::ACCENT_COOL);
    }

    #[test]
    fn color_priority_all_idle_is_cold() {
        let statuses = [ClaudeStatus::Idle, ClaudeStatus::Idle];
        let (_, color) = build_claude_summary(&statuses).unwrap();
        assert_eq!(color, ThermalPalette::ACCENT_COLD);
    }

    // -----------------------------------------------------------------------
    // Summary text — prefix and count
    // -----------------------------------------------------------------------

    #[test]
    fn summary_text_always_starts_with_clu() {
        for statuses in [
            vec![ClaudeStatus::Idle],
            vec![ClaudeStatus::ToolUse, ClaudeStatus::Processing],
            vec![ClaudeStatus::Idle, ClaudeStatus::Idle, ClaudeStatus::Idle],
        ] {
            let (text, _) = build_claude_summary(&statuses).unwrap();
            assert!(text.starts_with("CLU "), "text='{text}' should start with 'CLU '");
        }
    }

    #[test]
    fn summary_single_session_no_total_count_in_prefix() {
        // Single session → "CLU <status>", not "CLU 1: <status>"
        let (text, _) = build_claude_summary(&[ClaudeStatus::Idle]).unwrap();
        assert!(!text.contains(':'), "single session should not have ':' in '{text}'");
    }

    #[test]
    fn summary_multiple_sessions_has_colon_separator() {
        let statuses = [ClaudeStatus::Idle, ClaudeStatus::ToolUse];
        let (text, _) = build_claude_summary(&statuses).unwrap();
        assert!(text.contains(':'), "multiple sessions should have ':' in '{text}'");
    }

    #[test]
    fn summary_counts_are_correct_with_duplicates() {
        // 3 tool-use sessions
        let statuses = [ClaudeStatus::ToolUse, ClaudeStatus::ToolUse, ClaudeStatus::ToolUse];
        let (text, _) = build_claude_summary(&statuses).unwrap();
        assert!(text.contains("3 tool"), "expected '3 tool' in '{text}'");
    }
}
