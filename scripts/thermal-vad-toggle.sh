#!/bin/bash
# Toggle thermal-voice VAD (always-listening) on/off.
# If running in listen mode, stop it. If stopped, start it.

PIDFILE="/run/user/$(id -u)/thermal/voice.pid"

if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
    # Running — stop it
    kill "$(cat "$PIDFILE")"
    echo '{"state":"muted"}' > /tmp/thermal-voice-state.json
else
    # Not running — start in VAD mode
    setsid --fork thermal-voice listen
fi
