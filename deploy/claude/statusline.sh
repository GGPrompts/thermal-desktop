#!/bin/bash
input=$(cat)

DIR=$(echo "$input" | jq -r '.workspace.current_dir')
PCT=$(echo "$input" | jq -r '.context_window.used_percentage // 0' | cut -d. -f1)

# Shorten home prefix
DIR=$(echo "$DIR" | sed "s|^$HOME|~|")

# Git info
GIT=""
if git -C "$(echo "$input" | jq -r '.workspace.current_dir')" rev-parse --git-dir > /dev/null 2>&1; then
  BRANCH=$(git -C "$(echo "$input" | jq -r '.workspace.current_dir')" branch --show-current 2>/dev/null)
  DIRTY=$(git -C "$(echo "$input" | jq -r '.workspace.current_dir')" status --porcelain 2>/dev/null | head -1)
  if [ -n "$DIRTY" ]; then
    GIT=" \e[33m${BRANCH}*\e[0m"
  else
    GIT=" \e[32m${BRANCH}\e[0m"
  fi
fi

# Context bar with color based on usage
if [ "$PCT" -lt 50 ]; then
  COLOR="\e[32m"
elif [ "$PCT" -lt 80 ]; then
  COLOR="\e[33m"
else
  COLOR="\e[31m"
fi

# Active subagent workers — icon per tool
WORKERS=""
SESSION_ID=$(echo "$input" | jq -r '.session_id // ""')
if [ -n "$SESSION_ID" ]; then
  ACTUAL_DIR=$(echo "$input" | jq -r '.workspace.current_dir')
  for f in /tmp/claude-code-state/${SESSION_ID}.agent.*.json; do
    [ -f "$f" ] || continue
    TOOL=$(jq -r '.current_tool // ""' "$f" 2>/dev/null)
    case "$TOOL" in
      Edit)                  WORKERS="${WORKERS}🤖✏️" ;;
      Read)                  WORKERS="${WORKERS}🤖📖" ;;
      Grep|Glob)             WORKERS="${WORKERS}🤖🔍" ;;
      Bash)                  WORKERS="${WORKERS}🤖💻" ;;
      Write)                 WORKERS="${WORKERS}🤖📝" ;;
      *)                     WORKERS="${WORKERS}🤖⚙️" ;;
    esac
  done
  [ -n "$WORKERS" ] && WORKERS=" \e[35m${WORKERS}\e[0m"
fi

echo -e "\e[36m${DIR}\e[0m${GIT}${WORKERS} ${COLOR}${PCT}%\e[0m"
if [ -n "$SESSION_ID" ]; then
  STATE_DIR="/tmp/claude-code-state"
  STATE_FILE="$STATE_DIR/${SESSION_ID}.json"
  if [ -f "$STATE_FILE" ]; then
    # Merge context_percent into existing state file
    TEMP_FILE="${STATE_FILE}.ctx.$$"
    jq --argjson pct "$PCT" '.context_percent = $pct' "$STATE_FILE" > "$TEMP_FILE" 2>/dev/null && mv -f "$TEMP_FILE" "$STATE_FILE"
  fi
fi
