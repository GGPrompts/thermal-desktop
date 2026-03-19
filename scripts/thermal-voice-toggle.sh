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
DAEMON_SCRIPT="$(dirname "$(realpath "$0")")/thermal-voice.py"
STATE_FILE="/tmp/thermal-voice-state.json"

# --- Helper: send a command to the daemon via python socket ---
send_cmd() {
    python3 -c "
import socket, sys, json
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    sock.connect('$SOCKET')
    sock.sendall(json.dumps({'action': '$1'}).encode())
    sock.shutdown(socket.SHUT_WR)
    resp = sock.recv(4096).decode()
    print(resp)
except ConnectionRefusedError:
    print(json.dumps({'error': 'daemon not responding'}))
    sys.exit(1)
finally:
    sock.close()
"
}

# --- Ensure daemon is running ---
if [ ! -S "$SOCKET" ]; then
    echo "Starting thermal-voice daemon..."
    python3 "$DAEMON_SCRIPT" --daemon
    # Give it a moment to bind the socket
    for i in 1 2 3 4 5; do
        [ -S "$SOCKET" ] && break
        sleep 0.3
    done
    if [ ! -S "$SOCKET" ]; then
        notify-send -a "thermal-voice" -u critical "Voice daemon failed to start" \
            "Check /tmp/thermal-voice.log" 2>/dev/null
        exit 1
    fi
fi

# --- Toggle based on current state ---
if [ -f "$STATE_FILE" ]; then
    LISTENING=$(jq -r '.listening' "$STATE_FILE" 2>/dev/null)
else
    LISTENING="false"
fi

if [ "$LISTENING" = "true" ]; then
    # Stop recording and transcribe
    RESULT=$(send_cmd stop)
    TRANSCRIPT=$(echo "$RESULT" | jq -r '.transcript // empty' 2>/dev/null)
    if [ -n "$TRANSCRIPT" ]; then
        # Copy transcript to clipboard
        echo -n "$TRANSCRIPT" | wl-copy 2>/dev/null
        notify-send -a "thermal-voice" -t 5000 "Transcribed" "$TRANSCRIPT" 2>/dev/null
    else
        ERROR=$(echo "$RESULT" | jq -r '.error // "No speech detected"' 2>/dev/null)
        notify-send -a "thermal-voice" -t 3000 "Voice" "$ERROR" 2>/dev/null
    fi
else
    # Start recording
    send_cmd start >/dev/null
    notify-send -a "thermal-voice" -t 2000 "Listening..." \
        "Press Super+\\ again to stop" 2>/dev/null
fi
