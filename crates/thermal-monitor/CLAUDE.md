# thermal-monitor

Standalone ratatui TUI dashboard showing all agent sessions (Claude/Codex/Copilot) with color-coded status.

## What This Does
Displays a live table of active agent sessions. Simpler alternative to thermal-conductor for monitoring only. Currently reads from `ClaudeStatePoller` directly — should be migrated to subscribe to the conductor daemon's event bus (the canonical source for session state).
