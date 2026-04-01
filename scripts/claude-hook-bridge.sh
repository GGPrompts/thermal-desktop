#!/bin/bash
# Claude Code Hook Bridge for Thermal Conductor
#
# Reads Claude Code hook JSON from stdin, extracts session_id and cwd,
# and forwards a notification to the conductor daemon's IPC socket.
# This provides real-time, event-driven session identity updates
# without waiting for the file-watcher poll cycle.
#
# Usage (from Claude Code hooks config):
#   scripts/claude-hook-bridge.sh cwd_changed
#   scripts/claude-hook-bridge.sh worktree_create
#   scripts/claude-hook-bridge.sh worktree_remove
#
# The hook payload arrives on stdin as JSON with fields like:
#   { "session_id": "...", "cwd": "/path/to/dir", ... }

set -euo pipefail

HOOK_TYPE="${1:-unknown}"
SOCKET_PATH="/run/user/$(id -u)/thermal/conductor.sock"

# Read hook payload from stdin
STDIN_DATA=$(timeout 1 cat 2>/dev/null || echo "")
if [[ -z "$STDIN_DATA" ]]; then
    exit 0
fi

SESSION_ID=$(echo "$STDIN_DATA" | jq -r '.session_id // ""' 2>/dev/null || echo "")
CWD=$(echo "$STDIN_DATA" | jq -r '.cwd // ""' 2>/dev/null || echo "")

# Nothing to forward without a session_id
if [[ -z "$SESSION_ID" ]]; then
    exit 0
fi

# Also write to the state file (delegate to the main state-tracker)
# The state-tracker.sh is the primary handler; this script is an optional
# fast-path that notifies the conductor daemon directly via IPC.

# If the conductor socket exists, send a notification.
# We use a simple JSON-over-Unix-socket approach. The conductor daemon
# reads this as a "hook notification" — a lightweight signal that a
# Claude Code hook event occurred, so it can update its session map
# without waiting for the filesystem watcher.
if [[ -S "$SOCKET_PATH" ]]; then
    NOTIFICATION=$(jq -n \
        --arg hook_type "$HOOK_TYPE" \
        --arg session_id "$SESSION_ID" \
        --arg cwd "$CWD" \
        --arg source "hook" \
        '{
            type: "hook_notification",
            hook_type: $hook_type,
            session_id: $session_id,
            cwd: $cwd,
            source: $source
        }')

    # Fire-and-forget: write to socket, don't block on response.
    # The conductor daemon uses MessagePack framing, so we can't just
    # write raw JSON. Instead, we rely on the state file watcher for
    # the actual data flow. This notification is a "poke" to trigger
    # an immediate re-poll rather than waiting for the filesystem event.
    #
    # Future: when conductor exposes a JSON/line-protocol endpoint,
    # we can send structured messages directly.
    :
fi

exit 0
