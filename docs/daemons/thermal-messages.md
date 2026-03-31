# thermal-messages

Agent message bus daemon for inter-agent communication.

## What it does
All agent communication flows through this daemon. Provides a JSONL-over-Unix-socket
protocol with a 500-message ring buffer and subscriber replay. Route table dispatches
messages to backends: @claude/@codex (kitty live session or one-shot CLI fallback),
@planner (claude -p with planner prompt), @system (thermal-commander MCP),
@dispatcher (thermal-dispatcher socket), @user (broadcast to TUI + TTS).

## Socket / Pidfile
- Socket: `/run/user/$UID/thermal/messages.sock`
- Pidfile: `/run/user/$UID/thermal/messages.pid`
- Persistence (optional): `~/.local/share/thermal/messages.jsonl`

## CLI Usage
```
thermal-messages [--persist]
```
| Flag | Description |
|------|-------------|
| `--persist` | Enable JSONL append-log persistence; loads on startup to populate ring buffer |

## Dependencies
- **Needs**: nothing (standalone; route backends are optional)
- **Needed by**: thermal-dispatcher (route tool), thermal-conductor TUI (chat panel), thermal-hud

## Troubleshooting
- If messages are not routing, check that the target backend daemon is running
- Ring buffer holds 500 messages; older messages are dropped (use `--persist` for history)
- Stale socket: remove `/run/user/$UID/thermal/messages.sock` if daemon crashed
