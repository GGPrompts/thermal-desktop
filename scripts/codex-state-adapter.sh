#!/usr/bin/env bash
#
# codex-state-adapter.sh — Bridge Codex JSONL events to thermal state files.
#
# Reads Codex CLI JSONL output (from stdin or by tailing a log) and writes
# compatible state JSON files to /tmp/codex-state/ so that ClaudeStatePoller
# (thermal-core) can monitor Codex sessions alongside Claude sessions.
#
# Usage:
#   codex exec --json 2>&1 | ./codex-state-adapter.sh [session-id]
#   codex exec --json 2>&1 | ./codex-state-adapter.sh [session-id] [working-dir]
#
# If session-id is omitted, uses "codex-$$" (adapter PID).
# If working-dir is omitted, uses $PWD.
#
# Expected Codex JSONL event format (best-effort — based on public docs):
#
#   {"type": "message.created", "message": {"id": "msg_...", "role": "assistant", ...}}
#   {"type": "response.created", ...}
#   {"type": "response.in_progress", ...}
#   {"type": "response.output_item.added", "item": {"type": "function_call", "name": "shell", ...}}
#   {"type": "response.output_item.done", "item": {"type": "function_call", ...}}
#   {"type": "response.completed", ...}
#   {"type": "response.failed", ...}
#
# Mapped statuses:
#   response.in_progress / message.created  → processing
#   response.output_item.added (function_call) → tool_use
#   response.output_item.done  → processing (back from tool)
#   response.completed / response.failed    → idle
#   (no events for 30s)                     → idle (timeout)
#
# Output: Writes /tmp/codex-state/<session-id>.json in the same format as
# Claude Code state files, with agent_type="codex".
#
# Dependencies: bash, jq
# ---------------------------------------------------------------------------

set -euo pipefail

STATE_DIR="/tmp/codex-state"
SESSION_ID="${1:-codex-$$}"
WORKING_DIR="${2:-$PWD}"
STATE_FILE="${STATE_DIR}/${SESSION_ID}.json"

mkdir -p "$STATE_DIR"

# Write a state file given status and optional tool info.
write_state() {
    local status="$1"
    local current_tool="${2:-}"
    local tool_detail="${3:-}"
    local now
    now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

    local tool_json="null"
    if [[ -n "$current_tool" ]]; then
        tool_json="\"$current_tool\""
    fi

    local details_json="null"
    if [[ -n "$tool_detail" ]]; then
        details_json=$(jq -n \
            --arg event "tool_start" \
            --arg tool "$current_tool" \
            --arg desc "$tool_detail" \
            '{event: $event, tool: $tool, args: {description: $desc}}')
    fi

    jq -n \
        --arg sid "$SESSION_ID" \
        --arg status "$status" \
        --arg agent_type "codex" \
        --arg working_dir "$WORKING_DIR" \
        --arg last_updated "$now" \
        --argjson current_tool "$tool_json" \
        --argjson details "$details_json" \
        --argjson pid "$$" \
        '{
            session_id: $sid,
            agent_type: $agent_type,
            status: $status,
            current_tool: $current_tool,
            working_dir: $working_dir,
            last_updated: $last_updated,
            details: $details,
            pid: $pid,
            subagent_count: 0,
            context_percent: null
        }' > "$STATE_FILE"
}

# Clean up state file on exit.
cleanup() {
    rm -f "$STATE_FILE"
}
trap cleanup EXIT INT TERM

# Initial state: idle.
write_state "idle"

# Read JSONL from stdin line by line.
while IFS= read -r line; do
    # Skip empty lines.
    [[ -z "$line" ]] && continue

    # Parse the event type.
    event_type=$(echo "$line" | jq -r '.type // empty' 2>/dev/null) || continue
    [[ -z "$event_type" ]] && continue

    case "$event_type" in
        message.created|response.created|response.in_progress)
            write_state "processing"
            ;;
        response.output_item.added)
            # Check if this is a function/tool call.
            item_type=$(echo "$line" | jq -r '.item.type // empty' 2>/dev/null)
            if [[ "$item_type" == "function_call" ]]; then
                tool_name=$(echo "$line" | jq -r '.item.name // "tool"' 2>/dev/null)
                # Map common Codex tool names to display names.
                case "$tool_name" in
                    shell|bash)     display_tool="Bash" ;;
                    read_file)      display_tool="Read" ;;
                    write_file)     display_tool="Write" ;;
                    edit_file)      display_tool="Edit" ;;
                    search|grep)    display_tool="Grep" ;;
                    *)              display_tool="$tool_name" ;;
                esac
                write_state "tool_use" "$display_tool" "$tool_name"
            else
                write_state "processing"
            fi
            ;;
        response.output_item.done)
            # Tool finished — back to processing.
            write_state "processing"
            ;;
        response.completed|response.done)
            write_state "idle"
            ;;
        response.failed|response.cancelled|error)
            write_state "idle"
            ;;
        *)
            # Unknown event type — ignore, keep current state.
            ;;
    esac
done

# If stdin closes, clean up.
write_state "idle"
