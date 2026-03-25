#!/usr/bin/env bash
#
# thermal-voice-toggle.sh — Toggle push-to-talk via thermal-voice daemon.
# Designed for a Hyprland keybind (Super+Backslash).
#
# If not listening → sends "start" (begin recording)
# If listening     → sends "stop"  (stop + transcribe)
#
# Also starts the daemon if it is not running.
#

SOCKET="/run/user/${UID}/thermal/voice.sock"
STATE_FILE="/tmp/thermal-voice-state.json"

# --- Ensure daemon is running ---
if [ ! -S "$SOCKET" ]; then
    echo "Starting thermal-voice daemon..."
    thermal-voice &
    disown
    # Give it a moment to bind the socket
    for i in 1 2 3 4 5; do
        [ -S "$SOCKET" ] && break
        sleep 0.3
    done
    if [ ! -S "$SOCKET" ]; then
        notify-send -a "thermal-voice" -u critical "Voice daemon failed to start" \
            "Check journalctl or daemon logs" 2>/dev/null
        exit 1
    fi
fi

# --- Toggle using the native Rust subcommand ---
RESULT=$(thermal-voice toggle 2>&1)
EXIT_CODE=$?

if [ $EXIT_CODE -ne 0 ]; then
    notify-send -a "thermal-voice" -t 3000 "Voice Error" "$RESULT" 2>/dev/null
    exit 1
fi

# Check what state we're now in to decide notification
if [ -f "$STATE_FILE" ]; then
    STATE=$(jq -r '.state // "muted"' "$STATE_FILE" 2>/dev/null)
else
    STATE="muted"
fi

LABEL=""
if [ -f "$STATE_FILE" ]; then
    LABEL=$(jq -r '.label // ""' "$STATE_FILE" 2>/dev/null)
fi

case "$STATE" in
    listening)
        notify-send -a "thermal-voice" -t 2000 "Listening..." \
            "Press Super+\\ again to stop" 2>/dev/null
        ;;
    processing)
        if [ "$LABEL" = "dispatching" ]; then
            notify-send -a "thermal-voice" -t 3000 "Dispatching..." \
                "Sending to Claude" 2>/dev/null
        else
            notify-send -a "thermal-voice" -t 2000 "Processing..." \
                "Transcribing audio" 2>/dev/null
        fi
        ;;
    muted)
        # toggle just finished a stop — output is the transcript or error
        if [ -n "$RESULT" ]; then
            notify-send -a "thermal-voice" -t 5000 "Transcribed" "$RESULT" 2>/dev/null
        fi
        ;;
esac
