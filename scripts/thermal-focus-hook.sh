#!/usr/bin/env bash
# thermal-focus-hook.sh — Claude Code pre_tool hook for focus-aware autonomy.
#
# Reads /tmp/thermal-focus-state.json written by thermal-conductor's GPU
# window. When the user has been away from the terminal for >30 seconds,
# the hook outputs nothing (allowing higher autonomy / auto-accept).
# When focused, it also outputs nothing (normal confirmation behavior).
#
# This hook is a no-op by design — it exists as infrastructure for CC's
# KAIROS mode. Future versions may output JSON directives to adjust
# autonomy tiers based on focus duration.
#
# Setup (in .claude/settings.json):
#   {
#     "hooks": {
#       "pre_tool_use": [
#         {
#           "command": "/path/to/thermal-desktop/scripts/thermal-focus-hook.sh",
#           "timeout": 1000
#         }
#       ]
#     }
#   }
#
# Environment:
#   THERMAL_FOCUS_AWAY_THRESHOLD — seconds before "away" (default: 30)

set -euo pipefail

FOCUS_STATE_FILE="/tmp/thermal-focus-state.json"
AWAY_THRESHOLD="${THERMAL_FOCUS_AWAY_THRESHOLD:-30}"

# If no state file, do nothing (conductor not running or no GPU window).
if [[ ! -f "$FOCUS_STATE_FILE" ]]; then
    exit 0
fi

# Read focus state. Use python3 or jq for JSON parsing — prefer jq.
if command -v jq &>/dev/null; then
    focused=$(jq -r '.focused' "$FOCUS_STATE_FILE" 2>/dev/null || echo "null")
    away_seconds=$(jq -r '.away_seconds' "$FOCUS_STATE_FILE" 2>/dev/null || echo "0")
else
    # Fallback: simple grep-based extraction (fragile but works for this schema).
    focused=$(grep -o '"focused": *[a-z]*' "$FOCUS_STATE_FILE" | head -1 | grep -o '[a-z]*$')
    away_seconds=$(grep -o '"away_seconds": *[0-9]*' "$FOCUS_STATE_FILE" | grep -o '[0-9]*$')
fi

# Normalize
away_seconds="${away_seconds:-0}"

# Decision logic:
# - Focused: normal behavior (output nothing)
# - Unfocused < threshold: normal behavior (output nothing)
# - Unfocused >= threshold: elevated autonomy (output nothing for now)
#
# In all cases we exit 0. The hook's value is as infrastructure:
# future KAIROS integration will read the state and emit directives.
# For now, the state file at /tmp/thermal-focus-state.json is the
# integration surface that other tools can consume.

exit 0
