#!/bin/bash
# Toggle thermal-voice VAD (always-listening) on/off.
# If daemon is running, kill it and write muted state.
# If not running, start in VAD listen mode.
#
# Uses kill/start because the daemon has no mute socket command yet.
# The Services tab in thc handles stale pidfile cleanup on restart.

PIDFILE="/run/user/$(id -u)/thermal/voice.pid"
STATEFILE="/tmp/thermal-voice-state.json"

if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
    # Running — stop it gracefully
    kill "$(cat "$PIDFILE")"
    # Wait briefly for process to exit and clean up its own pidfile
    sleep 0.3
    # Clean up if it didn't (prevents stale pidfile blocking restart)
    if [ -f "$PIDFILE" ] && ! kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
        rm -f "$PIDFILE"
    fi
    echo '{"state":"muted"}' > "$STATEFILE"
else
    # Not running — clean up stale pidfile if present, then start
    if [ -f "$PIDFILE" ]; then
        rm -f "$PIDFILE"
    fi
    setsid --fork thermal-voice listen
fi
