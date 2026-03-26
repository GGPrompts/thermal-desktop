#!/usr/bin/env bash
#
# copilot-state-tracker.sh — GitHub Copilot CLI hook for thermal state tracking.
#
# Writes Copilot session state to /tmp/copilot-state/<session-id>.json so that
# ClaudeStatePoller (thermal-core) can monitor Copilot sessions alongside
# Claude and Codex sessions.
#
# Hook events:
#   session-start, session-end, pre-tool, post-tool
#
# Copilot hook stdin format:
#   sessionStart: {sessionId, timestamp, cwd, source, initialPrompt}
#   preToolUse:   {sessionId, timestamp, cwd, toolName, toolArgs}
#   postToolUse:  {sessionId, timestamp, cwd, toolName, toolArgs, toolResult}
#   sessionEnd:   {sessionId, timestamp, cwd, reason}
#
# Install in ~/.copilot/config.json:
#   "hooks": {
#     "sessionStart": [{"type":"command","command":"<path>/copilot-state-tracker.sh session-start"}],
#     "preToolUse":   [{"type":"command","command":"<path>/copilot-state-tracker.sh pre-tool"}],
#     "postToolUse":  [{"type":"command","command":"<path>/copilot-state-tracker.sh post-tool"}],
#     "sessionEnd":   [{"type":"command","command":"<path>/copilot-state-tracker.sh session-end"}]
#   }

set -euo pipefail

STATE_DIR="/tmp/copilot-state"
mkdir -p "$STATE_DIR"

# Read stdin (hook data from Copilot).
STDIN_DATA=$(timeout 0.1 cat 2>/dev/null || echo "")

# Copilot uses camelCase field names.
SESSION_ID=$(echo "$STDIN_DATA" | jq -r '.sessionId // ""' 2>/dev/null || echo "")
if [[ -z "$SESSION_ID" ]]; then
    SESSION_ID="copilot-$$"
fi

# Sanitize session ID — only allow safe characters for file paths.
SESSION_ID="${SESSION_ID//[^A-Za-z0-9_-]/}"
if [[ -z "$SESSION_ID" ]]; then
    SESSION_ID="copilot-$$"
fi

STATE_FILE="$STATE_DIR/${SESSION_ID}.json"
TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
HOOK_TYPE="${1:-unknown}"
WORKING_DIR=$(echo "$STDIN_DATA" | jq -r '.cwd // ""' 2>/dev/null || echo "")
[[ -z "$WORKING_DIR" ]] && WORKING_DIR="$PWD"

# --- Model extraction ---
# Copilot doesn't pass model in hook stdin. Read from env or config.
get_copilot_model() {
    # 1. COPILOT_MODEL env var
    if [[ -n "${COPILOT_MODEL:-}" ]]; then
        printf '%s' "$COPILOT_MODEL"
        return
    fi
    # 2. ~/.copilot/config.json
    local config="${COPILOT_HOME:-$HOME/.copilot}/config.json"
    if [[ -f "$config" ]]; then
        local model
        model=$(jq -r '.model // empty' "$config" 2>/dev/null || true)
        if [[ -n "$model" ]]; then
            printf '%s' "$model"
            return
        fi
    fi
    printf 'unknown'
}

# Cache model per session in a sidecar file (model can change mid-session).
MODEL_CACHE="$STATE_DIR/.model.${SESSION_ID}"

read_cached_model() {
    if [[ -f "$MODEL_CACHE" ]]; then
        cat "$MODEL_CACHE" 2>/dev/null || true
    fi
}

cache_model() {
    echo "$1" > "$MODEL_CACHE" 2>/dev/null || true
}

# --- Tool name mapping (Copilot → thermal display names) ---
map_tool_name() {
    local raw="$1"
    case "$raw" in
        shell|bash|exec_command)    printf 'Bash' ;;
        read_file)                  printf 'Read' ;;
        write_file|write)           printf 'Write' ;;
        edit_file|apply_patch)      printf 'Edit' ;;
        search|grep|rg)             printf 'Grep' ;;
        glob|list_dir)              printf 'Glob' ;;
        web_fetch|fetch)            printf 'WebFetch' ;;
        web_search|search_query)    printf 'WebSearch' ;;
        spawn_agent|send_input)     printf 'Agent' ;;
        mcp__*)                     printf 'MCP' ;;
        *)                          printf '%s' "$raw" ;;
    esac
}

# --- Build tool details JSON ---
build_details() {
    local tool_name="$1"
    local tool_args="$2"
    local display_name
    display_name="$(map_tool_name "$tool_name")"

    local file_path="" command="" pattern="" description=""
    case "$tool_name" in
        shell|bash|exec_command)
            command=$(echo "$tool_args" | jq -r '.cmd // .command // empty' 2>/dev/null || true) ;;
        read_file)
            file_path=$(echo "$tool_args" | jq -r '.path // .file_path // empty' 2>/dev/null || true) ;;
        write_file|write)
            file_path=$(echo "$tool_args" | jq -r '.path // .file_path // empty' 2>/dev/null || true) ;;
        edit_file|apply_patch)
            file_path=$(echo "$tool_args" | jq -r '.path // .file_path // empty' 2>/dev/null || true)
            description=$(echo "$tool_args" | jq -r '.instruction // .description // empty' 2>/dev/null || true) ;;
        search|grep|rg|search_query)
            pattern=$(echo "$tool_args" | jq -r '.pattern // .query // empty' 2>/dev/null || true) ;;
    esac

    jq -n \
        --arg tool "$display_name" \
        --arg file_path "$file_path" \
        --arg command "$command" \
        --arg pattern "$pattern" \
        --arg description "$description" \
        '{
            event: "tool_start",
            tool: $tool,
            args: (
                {}
                + (if $file_path != "" then {file_path: $file_path} else {} end)
                + (if $command != "" then {command: $command} else {} end)
                + (if $pattern != "" then {pattern: $pattern} else {} end)
                + (if $description != "" then {description: $description} else {} end)
            )
        }
        | if (.args | length) == 0 then del(.args) else . end'
}

# --- Write state file ---
write_state() {
    local status="$1"
    local current_tool="${2:-}"
    local details_json="${3:-null}"
    local model
    model="$(read_cached_model)"
    [[ -z "$model" ]] && model="$(get_copilot_model)" && cache_model "$model"

    local temp_file="${STATE_FILE}.tmp.$$"
    if ! jq -n \
        --arg session_id "$SESSION_ID" \
        --arg agent_type "copilot" \
        --arg model "$model" \
        --arg status "$status" \
        --arg current_tool "$current_tool" \
        --arg working_dir "$WORKING_DIR" \
        --arg last_updated "$TIMESTAMP" \
        --argjson details "$details_json" \
        --argjson pid $$ \
        '{
            session_id: $session_id,
            agent_type: $agent_type,
            model: $model,
            status: $status,
            current_tool: (if $current_tool == "" then null else $current_tool end),
            working_dir: $working_dir,
            last_updated: $last_updated,
            details: $details,
            pid: $pid,
            subagent_count: 0,
            context_percent: null
        }' > "$temp_file" 2>/dev/null; then
        rm -f "$temp_file"
        return 0
    fi
    mv -f "$temp_file" "$STATE_FILE"
}

# --- Event handling ---
case "$HOOK_TYPE" in
    session-start)
        # Cache model at session start.
        cache_model "$(get_copilot_model)"
        write_state "idle" "" '{"event":"session_started"}'
        # Cleanup stale state files older than 1 hour.
        find "$STATE_DIR" -name "*.json" -mmin +60 -delete 2>/dev/null &
        ;;

    session-end)
        # Copilot fires sessionEnd after each turn (not just process exit).
        # Write idle state instead of deleting so the session stays visible.
        write_state "idle" "" '{"event":"session_ended"}'
        ;;

    user-prompt)
        write_state "processing" "" '{"event":"user_prompt_submitted"}'
        ;;

    pre-tool)
        TOOL_NAME=$(echo "$STDIN_DATA" | jq -r '.toolName // "unknown"' 2>/dev/null || echo "unknown")
        TOOL_ARGS=$(echo "$STDIN_DATA" | jq -c '.toolArgs // {}' 2>/dev/null || echo '{}')
        DISPLAY_TOOL="$(map_tool_name "$TOOL_NAME")"
        DETAILS="$(build_details "$TOOL_NAME" "$TOOL_ARGS")"
        write_state "tool_use" "$DISPLAY_TOOL" "$DETAILS"
        ;;

    post-tool)
        write_state "processing" "" '{"event":"tool_completed"}'
        ;;

    stop)
        write_state "awaiting_input" "" '{"event":"copilot_stopped","waiting_for_user":true}'
        ;;

    *)
        write_state "processing" "" "$(jq -n --arg hook "$HOOK_TYPE" '{event:"unknown_hook",hook:$hook}')"
        ;;
esac

exit 0
