#!/bin/bash
# Claude Code State Tracker for Thermal OS
# Writes Claude's current state to /tmp/claude-code-state/*.json
# Read by: thermal-bar (ClaudeModule), thc status, thermal-audio, thermal-monitor
#
# Hook events handled:
#   session-start, session-end, user-prompt,
#   pre-tool, post-tool,
#   subagent-start, subagent-stop,
#   stop, notification,
#   worktree-create, worktree-remove

set -euo pipefail

STATE_DIR="/tmp/claude-code-state"
SUBAGENT_DIR="$STATE_DIR/subagents"
mkdir -p "$STATE_DIR" "$SUBAGENT_DIR"

# Read stdin (hook data from Claude).
# Use a generous timeout so large payloads aren't truncated.
STDIN_DATA=$(timeout 1 cat 2>/dev/null || echo "")

# Session identifier — prefer Claude's own session_id from stdin
STDIN_SESSION_ID=$(echo "$STDIN_DATA" | jq -r '.session_id // ""' 2>/dev/null || echo "")
if [[ -n "$STDIN_SESSION_ID" ]]; then
    SESSION_ID="$STDIN_SESSION_ID"
elif [[ -n "${CLAUDE_SESSION_ID:-}" ]]; then
    SESSION_ID="$CLAUDE_SESSION_ID"
elif [[ -n "$PWD" ]]; then
    SESSION_ID=$(echo "$PWD" | md5sum | cut -d' ' -f1 | head -c 12)
else
    SESSION_ID="$$"
fi

# Check if this event is from a subagent (agent_id present in stdin).
# Subagent tool events go to a separate file; parent events go to the main file.
AGENT_ID=$(echo "$STDIN_DATA" | jq -r '.agent_id // ""' 2>/dev/null || echo "")

if [[ -n "$AGENT_ID" ]]; then
    STATE_FILE="$STATE_DIR/${SESSION_ID}.agent.${AGENT_ID}.json"
else
    STATE_FILE="$STATE_DIR/${SESSION_ID}.json"
fi

PARENT_STATE_FILE="$STATE_DIR/${SESSION_ID}.json"
SUBAGENT_COUNT_FILE="$SUBAGENT_DIR/${SESSION_ID}.count"

get_subagent_count() {
    cat "$SUBAGENT_COUNT_FILE" 2>/dev/null || echo "0"
}

increment_subagent_count() {
    (
        flock -x 200
        local count=$(cat "$SUBAGENT_COUNT_FILE" 2>/dev/null || echo "0")
        echo $((count + 1)) > "$SUBAGENT_COUNT_FILE"
    ) 200>"$SUBAGENT_COUNT_FILE.lock"
}

decrement_subagent_count() {
    (
        flock -x 200
        local count=$(cat "$SUBAGENT_COUNT_FILE" 2>/dev/null || echo "0")
        local new_count=$((count - 1))
        [[ $new_count -lt 0 ]] && new_count=0
        echo "$new_count" > "$SUBAGENT_COUNT_FILE"
    ) 200>"$SUBAGENT_COUNT_FILE.lock"
}

TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
HOOK_TYPE="${1:-unknown}"

case "$HOOK_TYPE" in
    session-start)
        STATUS="idle"
        CURRENT_TOOL=""
        DETAILS='{"event":"session_started"}'
        echo "0" > "$SUBAGENT_COUNT_FILE"
        # Cleanup stale state files older than 1 hour
        find "$STATE_DIR" -name "*.json" -mmin +60 -delete 2>/dev/null &
        ;;

    session-end)
        # Clean up this session's state file
        rm -f "$STATE_FILE" "$SUBAGENT_COUNT_FILE" "$SUBAGENT_COUNT_FILE.lock" 2>/dev/null
        exit 0
        ;;

    user-prompt)
        STATUS="processing"
        CURRENT_TOOL=""
        DETAILS='{"event":"user_prompt_submitted"}'
        ;;

    pre-tool)
        STATUS="tool_use"
        CURRENT_TOOL=$(echo "$STDIN_DATA" | jq -r '.tool_name // .tool // .name // "unknown"' 2>/dev/null || echo "unknown")
        TOOL_ARGS_STR=$(echo "$STDIN_DATA" | jq -c '.tool_input // .input // .parameters // {}' 2>/dev/null || echo '{}')
        DETAILS=$(jq -n --arg tool "$CURRENT_TOOL" --arg args "$TOOL_ARGS_STR" '{event:"tool_starting",tool:$tool,args:($args|fromjson)}' 2>/dev/null || echo '{"event":"tool_starting"}')
        ;;

    post-tool)
        STATUS="processing"
        CURRENT_TOOL=$(echo "$STDIN_DATA" | jq -r '.tool_name // .tool // .name // "unknown"' 2>/dev/null || echo "unknown")
        DETAILS='{"event":"tool_completed"}'
        ;;

    subagent-start)
        increment_subagent_count
        SA_AGENT_ID=$(echo "$STDIN_DATA" | jq -r '.agent_id // "unknown"' 2>/dev/null || echo "unknown")
        SA_AGENT_TYPE=$(echo "$STDIN_DATA" | jq -r '.agent_type // "unknown"' 2>/dev/null || echo "unknown")
        SUBAGENT_COUNT=$(get_subagent_count)

        # Write parent state update (subagent count changed).
        STATE_FILE="$PARENT_STATE_FILE"
        STATUS="processing"
        CURRENT_TOOL=""
        DETAILS=$(jq -n \
            --arg id "$SA_AGENT_ID" \
            --arg type "$SA_AGENT_TYPE" \
            --arg count "$SUBAGENT_COUNT" \
            '{event:"subagent_started",agent_id:$id,agent_type:$type,active_subagents:($count|tonumber)}')

        # Also create per-subagent state file so swarm watcher + audio can track it.
        SA_STATE_FILE="$STATE_DIR/${SESSION_ID}.agent.${SA_AGENT_ID}.json"
        SA_JSON=$(jq -n \
            --arg sid "${SESSION_ID}.agent.${SA_AGENT_ID}" \
            --arg parent "$SESSION_ID" \
            --arg agent_id "$SA_AGENT_ID" \
            --arg agent_type "$SA_AGENT_TYPE" \
            --arg cwd "$PWD" \
            --arg ts "$TIMESTAMP" \
            --arg source "hook" \
            '{session_id:$sid,parent_session_id:$parent,agent_id:$agent_id,agent_type:$agent_type,status:"processing",working_dir:$cwd,last_updated:$ts,source:$source}')
        SA_TEMP="${SA_STATE_FILE}.tmp.$$"
        echo "$SA_JSON" > "$SA_TEMP" && mv -f "$SA_TEMP" "$SA_STATE_FILE"
        ;;

    subagent-stop)
        decrement_subagent_count
        SUBAGENT_COUNT=$(get_subagent_count)
        SA_AGENT_ID=$(echo "$STDIN_DATA" | jq -r '.agent_id // "unknown"' 2>/dev/null || echo "unknown")
        SA_AGENT_TYPE=$(echo "$STDIN_DATA" | jq -r '.agent_type // "unknown"' 2>/dev/null || echo "unknown")

        # Remove per-subagent state file (swarm watcher detects removal).
        rm -f "$STATE_DIR/${SESSION_ID}.agent.${SA_AGENT_ID}.json" 2>/dev/null

        # Write parent state update.
        STATE_FILE="$PARENT_STATE_FILE"
        CURRENT_TOOL=""
        if [[ "$SUBAGENT_COUNT" -eq 0 ]]; then
            STATUS="awaiting_input"
            DETAILS=$(jq -n \
                --arg id "$SA_AGENT_ID" \
                --arg type "$SA_AGENT_TYPE" \
                '{event:"subagent_stopped",agent_id:$id,agent_type:$type,remaining_subagents:0,all_complete:true}')
        else
            STATUS="processing"
            DETAILS=$(jq -n \
                --arg id "$SA_AGENT_ID" \
                --arg type "$SA_AGENT_TYPE" \
                --arg count "$SUBAGENT_COUNT" \
                '{event:"subagent_stopped",agent_id:$id,agent_type:$type,remaining_subagents:($count|tonumber)}')
        fi
        ;;

    worktree-create)
        # Track worktree creation (gg-execute spawns worktrees)
        WORKTREE_PATH=$(echo "$STDIN_DATA" | jq -r '.worktree_path // ""' 2>/dev/null || echo "")
        if [[ -f "$STATE_FILE" ]]; then
            STATUS=$(jq -r '.status // "processing"' "$STATE_FILE") || STATUS="processing"
            CURRENT_TOOL=$(jq -r '.current_tool // ""' "$STATE_FILE") || CURRENT_TOOL=""
        else
            STATUS="processing"
            CURRENT_TOOL=""
        fi
        DETAILS=$(jq -n --arg path "$WORKTREE_PATH" '{event:"worktree_created",path:$path}')
        ;;

    worktree-remove)
        WORKTREE_PATH=$(echo "$STDIN_DATA" | jq -r '.worktree_path // ""' 2>/dev/null || echo "")
        if [[ -f "$STATE_FILE" ]]; then
            STATUS=$(jq -r '.status // "processing"' "$STATE_FILE") || STATUS="processing"
            CURRENT_TOOL=$(jq -r '.current_tool // ""' "$STATE_FILE") || CURRENT_TOOL=""
        else
            STATUS="processing"
            CURRENT_TOOL=""
        fi
        DETAILS=$(jq -n --arg path "$WORKTREE_PATH" '{event:"worktree_removed",path:$path}')
        ;;

    stop)
        CURRENT_TOOL=""
        # Subagent stop events are handled by subagent-stop hook — don't
        # change status here or audio will announce a false "exited".
        if [[ -n "$AGENT_ID" ]]; then
            STATUS="processing"
            DETAILS='{"event":"claude_stopped","is_subagent":true}'
        elif [[ "$(get_subagent_count)" -gt 0 ]]; then
            # Keep parent at "processing" while subagents are active to avoid
            # false "exited" / "needs input" announcements.
            STATUS="processing"
            DETAILS='{"event":"claude_stopped","waiting_for_subagents":true}'
        else
            STATUS="awaiting_input"
            DETAILS='{"event":"claude_stopped","waiting_for_user":true}'
        fi
        ;;

    notification)
        NOTIF_TYPE=$(echo "$STDIN_DATA" | jq -r '.notification_type // "unknown"' 2>/dev/null || echo "unknown")
        case "$NOTIF_TYPE" in
            idle_prompt|awaiting-input)
                CURRENT_TOOL=""
                if [[ "$(get_subagent_count)" -gt 0 ]]; then
                    STATUS="processing"
                    DETAILS='{"event":"awaiting_input","waiting_for_subagents":true}'
                else
                    STATUS="awaiting_input"
                    DETAILS='{"event":"awaiting_input"}'
                fi
                ;;
            *)
                if [[ -f "$STATE_FILE" ]]; then
                    STATUS=$(jq -r '.status // "idle"' "$STATE_FILE") || STATUS="idle"
                    CURRENT_TOOL=$(jq -r '.current_tool // ""' "$STATE_FILE") || CURRENT_TOOL=""
                else
                    STATUS="idle"
                    CURRENT_TOOL=""
                fi
                DETAILS=$(jq -n --arg type "$NOTIF_TYPE" '{event:"notification",type:$type}')
                ;;
        esac
        ;;

    *)
        if [[ -f "$STATE_FILE" ]]; then
            STATUS=$(jq -r '.status // "idle"' "$STATE_FILE") || STATUS="idle"
            CURRENT_TOOL=$(jq -r '.current_tool // ""' "$STATE_FILE") || CURRENT_TOOL=""
        else
            STATUS="idle"
            CURRENT_TOOL=""
        fi
        DETAILS=$(jq -n --arg hook "$HOOK_TYPE" '{event:"unknown_hook",hook:$hook}')
        ;;
esac

SUBAGENT_COUNT=$(get_subagent_count)

# Use composite session_id for subagent events so the conductor
# doesn't collapse them into the parent session.
if [[ -n "$AGENT_ID" ]]; then
    EFFECTIVE_SESSION_ID="${SESSION_ID}.agent.${AGENT_ID}"
    PARENT_FIELD="\"parent_session_id\": \"$SESSION_ID\","
else
    EFFECTIVE_SESSION_ID="$SESSION_ID"
    PARENT_FIELD=""
fi

STATE_JSON=$(cat <<EOF
{
  "session_id": "$EFFECTIVE_SESSION_ID",
  ${PARENT_FIELD}
  "status": "$STATUS",
  "current_tool": "$CURRENT_TOOL",
  "subagent_count": $SUBAGENT_COUNT,
  "working_dir": "$PWD",
  "last_updated": "$TIMESTAMP",
  "pid": $$,
  "hook_type": "$HOOK_TYPE",
  "details": $DETAILS
}
EOF
)

# Atomic write
TEMP_FILE="${STATE_FILE}.tmp.$$"
echo "$STATE_JSON" > "$TEMP_FILE" && mv -f "$TEMP_FILE" "$STATE_FILE"

exit 0
