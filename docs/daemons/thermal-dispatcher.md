# thermal-dispatcher

AI voice command router with tool-use and trust tiers.

## What it does
Listens on a Unix socket for transcript JSON from thermal-voice. Sends
transcripts to an LLM (Claude CLI primary, Copilot CLI secondary, Ollama
fallback) with a 3-tool schema: `speak` (TTS via thermal-audio), `read`
(screen capture via thermal-commander), and `route` (message bus dispatch).
Trust tiers (AUTO/CONFIRM/BLOCK) gate tool execution. Maintains multi-turn
conversational context (8-turn rolling window, 2min session timeout).

## Socket / Pidfile
- Socket: `/run/user/$UID/thermal/dispatcher.sock`
- Pidfile: `/run/user/$UID/thermal/dispatcher.pid`

## CLI Usage
```
thermal-dispatcher
```
| Env var | Description |
|---------|-------------|
| `THERMAL_DISPATCHER_BACKEND` | Force backend: `claude`, `copilot`, or `ollama` |
| `THERMAL_DISPATCHER_MODEL` | Override model (e.g. `qwen3:8b` for Ollama) |

## Dependencies
- **Needs**: thermal-audio (for `speak`), thermal-messages (for `route`), thermal-commander (for `read`). At least one LLM backend: Claude CLI, Copilot CLI, or Ollama
- **Needed by**: thermal-voice (sends transcripts here in VAD mode)

## Troubleshooting
- If commands are not routed, check that thermal-messages is running
- Backend auto-detection order: Claude CLI > Copilot CLI > Ollama
- Trust tiers configured in `config/trust-tiers.toml`
- Max 10 tool iterations per turn to prevent infinite loops
