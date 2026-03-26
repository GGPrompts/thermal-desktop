#!/usr/bin/env bash
#
# codex-state-adapter.sh — Mirror live Codex session JSONL into thermal state.
#
# Default mode watches ~/.codex/sessions for rollout-*.jsonl files, parses the
# real Codex event schema, and writes /tmp/codex-state/<session-id>.json files
# that ClaudeStatePoller can consume alongside Claude's state tracker.
#
# Stream mode is still available for ad hoc replay / piping:
#   codex exec --json 2>&1 | ./scripts/codex-state-adapter.sh --stdin
#
# Environment overrides:
#   CODEX_SESSIONS_DIR          Source session tree (default: ~/.codex/sessions)
#   CODEX_STATE_DIR             Output state dir (default: /tmp/codex-state)
#   CODEX_STATE_POLL_INTERVAL   Watch-mode poll interval in seconds (default: 1)
#   CODEX_STATE_STALE_SECS      Remove untouched sessions after N seconds (3600)

set -euo pipefail

STATE_DIR="${CODEX_STATE_DIR:-/tmp/codex-state}"
SESSIONS_DIR="${CODEX_SESSIONS_DIR:-${HOME}/.codex/sessions}"
POLL_INTERVAL="${CODEX_STATE_POLL_INTERVAL:-1}"
STALE_SECS="${CODEX_STATE_STALE_SECS:-3600}"

RUNTIME_BASE="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
RUN_DIR="${RUNTIME_BASE}/thermal"
PID_FILE="${RUN_DIR}/codex-state-adapter.pid"

MODE="auto"
STREAM_SESSION_ID=""
STREAM_WORKDIR="$PWD"

declare -A SOURCE_LINE_COUNT=()
declare -A SOURCE_LAST_TOUCH=()
declare -A SOURCE_SESSION_ID=()
declare -A SOURCE_WORKDIR=()
declare -A SESSION_STATUS=()
declare -A SESSION_TOOL=()
declare -A SESSION_DETAILS=()
declare -A SESSION_CONTEXT=()
declare -A SESSION_LAST_UPDATED=()
declare -A SESSION_WORKDIR=()
declare -A SESSION_STATE_FILE=()
declare -A SESSION_SOURCE_FILE=()
declare -A WRITTEN_STATE_FILES=()

usage() {
    cat <<'EOF'
Usage:
  codex-state-adapter.sh [--daemon]
  codex-state-adapter.sh --stdin [--session-id ID] [--working-dir DIR]

Options:
  --daemon               Force watch mode over ~/.codex/sessions.
  --stdin                Force stream mode from stdin.
  --session-id ID        Override the session id in stream mode.
  --working-dir DIR      Override the working dir in stream mode.
  --sessions-dir DIR     Override the watched Codex sessions dir.
  --state-dir DIR        Override the output state dir.
  --poll-interval N      Seconds between scans in watch mode.
  --stale-secs N         Drop untouched sessions after N seconds.
  --help                 Show this message.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --daemon)
            MODE="daemon"
            shift
            ;;
        --stdin)
            MODE="stdin"
            shift
            ;;
        --session-id)
            [[ $# -lt 2 ]] && { echo "--session-id requires an argument" >&2; exit 1; }
            STREAM_SESSION_ID="$2"
            shift 2
            ;;
        --working-dir)
            [[ $# -lt 2 ]] && { echo "--working-dir requires an argument" >&2; exit 1; }
            STREAM_WORKDIR="$2"
            shift 2
            ;;
        --sessions-dir)
            [[ $# -lt 2 ]] && { echo "--sessions-dir requires an argument" >&2; exit 1; }
            SESSIONS_DIR="$2"
            shift 2
            ;;
        --state-dir)
            [[ $# -lt 2 ]] && { echo "--state-dir requires an argument" >&2; exit 1; }
            STATE_DIR="$2"
            shift 2
            ;;
        --poll-interval)
            [[ $# -lt 2 ]] && { echo "--poll-interval requires an argument" >&2; exit 1; }
            POLL_INTERVAL="$2"
            shift 2
            ;;
        --stale-secs)
            [[ $# -lt 2 ]] && { echo "--stale-secs requires an argument" >&2; exit 1; }
            STALE_SECS="$2"
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

if [[ "$MODE" == "auto" ]]; then
    if [[ -p /dev/stdin || -f /dev/stdin ]]; then
        MODE="stdin"
    else
        MODE="daemon"
    fi
fi

sanitize_session_id() {
    local id="$1"
    id="${id//[^A-Za-z0-9_-]/}"
    [[ -z "$id" ]] && id="unknown-$$"
    printf '%s' "$id"
}

STREAM_SESSION_ID="$(sanitize_session_id "$STREAM_SESSION_ID")"
if [[ -z "$STREAM_SESSION_ID" || "$STREAM_SESSION_ID" == "unknown-$$" ]]; then
    STREAM_SESSION_ID="codex-$$"
fi

now_iso() {
    date -u +"%Y-%m-%dT%H:%M:%SZ"
}

now_epoch() {
    date +%s
}

ensure_dirs() {
    mkdir -p "$STATE_DIR"
    mkdir -p "$RUN_DIR"
}

ensure_single_instance() {
    # Use flock for atomic single-instance enforcement (no TOCTOU race).
    exec 9>"$PID_FILE"
    if ! flock -n 9; then
        echo "codex-state-adapter already running" >&2
        exit 0
    fi
    echo "$$" >&9
}

reset_state_dir() {
    find "$STATE_DIR" -maxdepth 1 -type f -name '*.json' -delete 2>/dev/null || true
}

cleanup() {
    for state_file in "${!WRITTEN_STATE_FILES[@]}"; do
        rm -f "$state_file" 2>/dev/null || true
    done
    rm -f "$PID_FILE" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

assign_session_to_source() {
    local source="$1"
    local session_id="$2"
    local old_session="${SOURCE_SESSION_ID[$source]:-}"

    if [[ -n "$old_session" && "$old_session" != "$session_id" ]]; then
        remove_state_for_session "$old_session"
    fi

    SOURCE_SESSION_ID["$source"]="$session_id"
    SESSION_SOURCE_FILE["$session_id"]="$source"
}

state_file_for_session() {
    local session_id="$1"
    printf '%s/%s.json' "$STATE_DIR" "$session_id"
}

remove_state_for_session() {
    local session_id="$1"
    local state_file
    state_file="${SESSION_STATE_FILE[$session_id]:-$(state_file_for_session "$session_id")}"

    rm -f "$state_file" 2>/dev/null || true
    unset WRITTEN_STATE_FILES["$state_file"]
    unset SESSION_STATE_FILE["$session_id"]
    unset SESSION_STATUS["$session_id"]
    unset SESSION_TOOL["$session_id"]
    unset SESSION_DETAILS["$session_id"]
    unset SESSION_CONTEXT["$session_id"]
    unset SESSION_LAST_UPDATED["$session_id"]
    unset SESSION_WORKDIR["$session_id"]
    unset SESSION_SOURCE_FILE["$session_id"]
}

write_state_for_session() {
    local session_id="$1"
    local status="${SESSION_STATUS[$session_id]:-idle}"
    local current_tool="${SESSION_TOOL[$session_id]:-}"
    local working_dir="${SESSION_WORKDIR[$session_id]:-${SOURCE_WORKDIR[${SESSION_SOURCE_FILE[$session_id]:-}]:-}}"
    local last_updated="${SESSION_LAST_UPDATED[$session_id]:-$(now_iso)}"
    local details_json="${SESSION_DETAILS[$session_id]:-null}"
    local context_percent="${SESSION_CONTEXT[$session_id]:-}"
    local state_file temp_file

    state_file="$(state_file_for_session "$session_id")"
    temp_file="${state_file}.tmp.$$"
    SESSION_STATE_FILE["$session_id"]="$state_file"
    WRITTEN_STATE_FILES["$state_file"]=1

    if ! jq -n \
        --arg sid "$session_id" \
        --arg agent_type "codex" \
        --arg status "$status" \
        --arg current_tool "$current_tool" \
        --arg working_dir "$working_dir" \
        --arg last_updated "$last_updated" \
        --arg context_percent "$context_percent" \
        --argjson details "$details_json" \
        --argjson pid "$$" \
        '{
            session_id: $sid,
            agent_type: $agent_type,
            status: $status,
            current_tool: (if $current_tool == "" then null else $current_tool end),
            working_dir: (if $working_dir == "" then null else $working_dir end),
            last_updated: $last_updated,
            details: $details,
            pid: $pid,
            subagent_count: 0,
            context_percent: (if $context_percent == "" then null else ($context_percent | tonumber) end)
        }' > "$temp_file" 2>/dev/null; then
        rm -f "$temp_file"
        return 0
    fi
    mv -f "$temp_file" "$state_file"
}

json_arg_value() {
    local arguments="$1"
    local filter="$2"

    jq -r "((fromjson? // .) | if type == \"object\" then . else {} end) | $filter" <<<"$arguments" 2>/dev/null || true
}

patch_target_from_text() {
    local patch_text="$1"
    local file_path

    file_path="$(sed -nE 's/^\*\*\* (Update|Add|Delete) File: (.+)$/\2/p' <<<"$patch_text" | head -n 1)"
    if [[ -z "$file_path" ]]; then
        file_path="$(sed -nE 's/^\*\*\* Move to: (.+)$/\1/p' <<<"$patch_text" | head -n 1)"
    fi
    printf '%s' "$file_path"
}

map_tool_name() {
    local raw_name="$1"
    case "$raw_name" in
        exec_command|write_stdin|shell|bash)
            printf 'Bash'
            ;;
        read_file)
            printf 'Read'
            ;;
        write_file)
            printf 'Write'
            ;;
        edit_file|apply_patch)
            printf 'Edit'
            ;;
        search|grep|rg|find)
            printf 'Grep'
            ;;
        glob|rg_files|list_dir)
            printf 'Glob'
            ;;
        spawn_agent|send_input|wait_agent|close_agent|resume_agent)
            printf 'Agent'
            ;;
        update_plan|request_user_input|mcp__*)
            printf 'Task'
            ;;
        open|click)
            printf 'WebFetch'
            ;;
        search_query|image_query)
            printf 'WebSearch'
            ;;
        *)
            printf '%s' "$raw_name"
            ;;
    esac
}

tool_details_json() {
    local raw_name="$1"
    local display_name="$2"
    local arguments="$3"
    local file_path=""
    local command=""
    local pattern=""
    local description=""

    case "$raw_name" in
        exec_command)
            command="$(json_arg_value "$arguments" '.cmd // empty')"
            description="$(json_arg_value "$arguments" '.justification // empty')"
            ;;
        write_stdin)
            command="$(json_arg_value "$arguments" '.chars // empty')"
            ;;
        read_file)
            file_path="$(json_arg_value "$arguments" '.path // .file_path // empty')"
            ;;
        write_file)
            file_path="$(json_arg_value "$arguments" '.path // .file_path // empty')"
            ;;
        edit_file)
            file_path="$(json_arg_value "$arguments" '.path // .file_path // empty')"
            description="$(json_arg_value "$arguments" '.instruction // .description // empty')"
            ;;
        apply_patch)
            # arguments may be raw patch text or a JSON object wrapping it.
            local patch_text
            patch_text="$(json_arg_value "$arguments" '.patch // .content // empty')"
            if [[ -z "$patch_text" ]]; then
                patch_text="$arguments"
            fi
            file_path="$(patch_target_from_text "$patch_text")"
            description="applying patch"
            ;;
        search|grep|rg|find|search_query|image_query)
            pattern="$(json_arg_value "$arguments" '.pattern // .query // .q // empty')"
            ;;
        open|click)
            description="$(json_arg_value "$arguments" '.ref_id // .url // empty')"
            ;;
        spawn_agent|send_input)
            description="$(json_arg_value "$arguments" '.message // .id // empty')"
            ;;
        wait_agent|close_agent|resume_agent)
            description="$(json_arg_value "$arguments" '.id // empty')"
            ;;
        update_plan)
            description="$(json_arg_value "$arguments" '.explanation // .plan[0].step // empty')"
            ;;
        mcp__beads__show|mcp__beads__claim|mcp__beads__close|mcp__beads__update)
            description="$(json_arg_value "$arguments" '.issue_id // empty')"
            ;;
        mcp__beads__create)
            description="$(json_arg_value "$arguments" '.title // empty')"
            ;;
        mcp__beads__context)
            description="$(json_arg_value "$arguments" '.action // empty')"
            ;;
        mcp__beads__list|mcp__beads__ready|mcp__beads__blocked|mcp__beads__projects|mcp__beads__stats)
            description="$(json_arg_value "$arguments" '.query // .prefix // .labels // empty')"
            ;;
    esac

    jq -n \
        --arg tool "$display_name" \
        --arg file_path "$file_path" \
        --arg command "$command" \
        --arg pattern "$pattern" \
        --arg description "$description" \
        '{
            event: "tool_start",
            tool: $tool
        }
        + (if ($file_path != "" or $command != "" or $pattern != "" or $description != "") then {
            args: {
                file_path: (if $file_path == "" then null else $file_path end),
                command: (if $command == "" then null else $command end),
                pattern: (if $pattern == "" then null else $pattern end),
                description: (if $description == "" then null else $description end)
            }
        } else {} end)
        | if .args? then .args |= with_entries(select(.value != null)) else . end'
}

session_id_for_source() {
    local source="$1"
    printf '%s' "${SOURCE_SESSION_ID[$source]:-}"
}

touch_source() {
    local source="$1"
    SOURCE_LAST_TOUCH["$source"]="$(stat -c %Y "$source" 2>/dev/null || now_epoch)"
}

process_line() {
    local source="$1"
    local line="$2"
    local timestamp top_type payload_type session_id display_tool details_json context_percent

    [[ -z "$line" ]] && return 0

    top_type="$(jq -r '.type // empty' <<<"$line" 2>/dev/null || true)"
    [[ -z "$top_type" ]] && return 0

    timestamp="$(jq -r '.timestamp // empty' <<<"$line" 2>/dev/null || true)"
    if [[ -z "$timestamp" ]]; then
        timestamp="$(now_iso)"
    fi

    case "$top_type" in
        session_meta)
            session_id="$(jq -r '.payload.id // empty' <<<"$line" 2>/dev/null || true)"
            session_id="$(sanitize_session_id "${session_id:-$STREAM_SESSION_ID}")"
            assign_session_to_source "$source" "$session_id"

            SOURCE_WORKDIR["$source"]="$(jq -r '.payload.cwd // empty' <<<"$line" 2>/dev/null || true)"
            SESSION_WORKDIR["$session_id"]="${SOURCE_WORKDIR[$source]}"
            SESSION_STATUS["$session_id"]="${SESSION_STATUS[$session_id]:-idle}"
            unset SESSION_CONTEXT["$session_id"]
            SESSION_LAST_UPDATED["$session_id"]="$timestamp"
            write_state_for_session "$session_id"
            ;;
        event_msg)
            payload_type="$(jq -r '.payload.type // empty' <<<"$line" 2>/dev/null || true)"
            session_id="$(session_id_for_source "$source")"
            [[ -z "$session_id" ]] && session_id="$STREAM_SESSION_ID"
            session_id="$(sanitize_session_id "$session_id")"
            assign_session_to_source "$source" "$session_id"

            case "$payload_type" in
                task_started|user_message)
                    SESSION_STATUS["$session_id"]="processing"
                    SESSION_TOOL["$session_id"]=""
                    SESSION_DETAILS["$session_id"]="null"
                    # Clear stale context until Codex emits a fresh token_count
                    # for the current turn. Otherwise old high-water values can
                    # trigger warnings immediately at turn start.
                    unset SESSION_CONTEXT["$session_id"]
                    ;;
                task_complete)
                    SESSION_STATUS["$session_id"]="idle"
                    SESSION_TOOL["$session_id"]=""
                    SESSION_DETAILS["$session_id"]="null"
                    ;;
                agent_message)
                    SESSION_STATUS["$session_id"]="processing"
                    SESSION_TOOL["$session_id"]=""
                    SESSION_DETAILS["$session_id"]="null"
                    ;;
                token_count)
                    context_percent="$(
                        jq -r '
                            (.payload.info.model_context_window // .payload.model_context_window // 0) as $window
                            | (
                                (.payload.info.last_token_usage.input_tokens // .payload.last_token_usage.input_tokens // .payload.input_tokens // 0)
                                + (.payload.info.last_token_usage.cached_input_tokens // .payload.last_token_usage.cached_input_tokens // .payload.cached_input_tokens // 0)
                            ) as $tokens
                            | if $window > 0 and $tokens > 0 then (($tokens / $window) * 100) else empty end
                        ' <<<"$line" 2>/dev/null || true
                    )"
                    if [[ -n "$context_percent" ]] && jq -en --arg pct "$context_percent" '$pct | tonumber | . >= 0 and . <= 100' >/dev/null; then
                        SESSION_CONTEXT["$session_id"]="$context_percent"
                    else
                        unset SESSION_CONTEXT["$session_id"]
                    fi
                    ;;
                *)
                    return 0
                    ;;
            esac

            SESSION_LAST_UPDATED["$session_id"]="$timestamp"
            write_state_for_session "$session_id"
            ;;
        response_item)
            payload_type="$(jq -r '.payload.type // empty' <<<"$line" 2>/dev/null || true)"
            session_id="$(session_id_for_source "$source")"
            [[ -z "$session_id" ]] && session_id="$STREAM_SESSION_ID"
            session_id="$(sanitize_session_id "$session_id")"
            assign_session_to_source "$source" "$session_id"

            case "$payload_type" in
                function_call)
                    display_tool="$(map_tool_name "$(jq -r '.payload.name // "tool"' <<<"$line" 2>/dev/null || true)")"
                    details_json="$(
                        tool_details_json \
                            "$(jq -r '.payload.name // "tool"' <<<"$line" 2>/dev/null || true)" \
                            "$display_tool" \
                            "$(jq -r '.payload.arguments // ""' <<<"$line" 2>/dev/null || true)"
                    )"
                    SESSION_STATUS["$session_id"]="tool_use"
                    SESSION_TOOL["$session_id"]="$display_tool"
                    SESSION_DETAILS["$session_id"]="$details_json"
                    ;;
                function_call_output|reasoning|message)
                    SESSION_STATUS["$session_id"]="processing"
                    SESSION_TOOL["$session_id"]=""
                    SESSION_DETAILS["$session_id"]="null"
                    ;;
                *)
                    return 0
                    ;;
            esac

            SESSION_LAST_UPDATED["$session_id"]="$timestamp"
            write_state_for_session "$session_id"
            ;;
    esac
}

process_source_file() {
    local source="$1"
    local previous_count current_count

    current_count="$(awk 'END{print NR}' "$source" 2>/dev/null || echo 0)"
    previous_count="${SOURCE_LINE_COUNT[$source]:-0}"

    if [[ "$current_count" -lt "$previous_count" ]]; then
        previous_count=0
    fi

    if [[ "$current_count" -gt "$previous_count" ]]; then
        while IFS= read -r line; do
            process_line "$source" "$line"
        done < <(sed -n "$((previous_count + 1)),$((current_count))p" "$source" 2>/dev/null || true)
    fi

    SOURCE_LINE_COUNT["$source"]="$current_count"
    touch_source "$source"
}

prune_stale_sources() {
    local now="$1"
    local source session_id last_touch
    local -a stale_sources=()

    for source in "${!SOURCE_LINE_COUNT[@]}"; do
        if [[ -e "$source" ]]; then
            touch_source "$source"
        fi

        last_touch="${SOURCE_LAST_TOUCH[$source]:-0}"
        if [[ ! -e "$source" || $((now - last_touch)) -gt "$STALE_SECS" ]]; then
            stale_sources+=("$source")
        fi
    done

    for source in "${stale_sources[@]}"; do
        session_id="${SOURCE_SESSION_ID[$source]:-}"
        if [[ -n "$session_id" ]]; then
            remove_state_for_session "$session_id"
        fi
        unset SOURCE_LINE_COUNT["$source"]
        unset SOURCE_LAST_TOUCH["$source"]
        unset SOURCE_SESSION_ID["$source"]
        unset SOURCE_WORKDIR["$source"]
    done
}

watch_mode() {
    local source now

    ensure_dirs
    ensure_single_instance
    reset_state_dir

    while true; do
        while IFS= read -r -d '' source; do
            [[ -z "$source" ]] && continue
            process_source_file "$source"
        done < <(find "$SESSIONS_DIR" -type f -name 'rollout-*.jsonl' -print0 2>/dev/null | sort -z)

        now="$(now_epoch)"
        prune_stale_sources "$now"
        sleep "$POLL_INTERVAL"
    done
}

stdin_mode() {
    local source="stdin://session"

    ensure_dirs
    assign_session_to_source "$source" "$STREAM_SESSION_ID"
    SOURCE_WORKDIR["$source"]="$STREAM_WORKDIR"
    SESSION_WORKDIR["$STREAM_SESSION_ID"]="$STREAM_WORKDIR"
    SESSION_STATUS["$STREAM_SESSION_ID"]="idle"
    write_state_for_session "$STREAM_SESSION_ID"

    while IFS= read -r line; do
        process_line "$source" "$line"
    done
}

case "$MODE" in
    daemon)
        watch_mode
        ;;
    stdin)
        stdin_mode
        ;;
    *)
        echo "Unsupported mode: $MODE" >&2
        exit 1
        ;;
esac
