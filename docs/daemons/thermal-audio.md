# thermal-audio

TTS voice announcements for agent session state changes.

## What it does
Watches agent state files and announces transitions (e.g. "opus is now active")
via edge-tts. Maintains a 12-voice pool so each agent session gets a distinct
voice. Also exposes a Unix socket API for on-demand TTS from other daemons
(thermal-dispatcher uses this for spoken responses).

## Socket / Pidfile
- Socket: `/run/user/$UID/thermal/audio.sock`
- Pidfile: `/run/user/$UID/thermal/audio.pid`

## CLI Usage
```
thermal-audio [--test <TEXT>]
```
| Flag | Description |
|------|-------------|
| `--test <TEXT>` | Speak the given text and exit (for testing) |

## Dependencies
- **Needs**: `edge-tts` CLI (Python package) for speech synthesis
- **Needed by**: thermal-dispatcher (sends `speak` tool calls via socket)

## Troubleshooting
- If TTS is silent, verify `edge-tts` is installed: `pip install edge-tts`
- Stale socket: if the daemon crashes, remove `/run/user/$UID/thermal/audio.sock`
- Audio conflicts: uses rodio (PipeWire-compatible); check PipeWire is running
