# thermal-dispatcher

AI voice command router — receives transcripts from thermal-voice and dispatches to LLM backends.

## What This Does
Listens on Unix socket at `/run/user/$UID/thermal/dispatcher.sock`. Receives voice transcripts, dispatches to LLM with 3-tool schema, delegates all actions to agents. Multi-turn conversational context (8-turn rolling window, 2min session timeout).

## LLM Backends
Priority order (auto-detect): Claude CLI → Copilot CLI → Ollama fallback.
- `THERMAL_DISPATCHER_BACKEND` env var: `claude`/`copilot`/`ollama`
- `THERMAL_DISPATCHER_MODEL` env var: model override (default: backend-specific)
- Copilot CLI: gpt4.1/gpt5-mini via $10/mo GitHub Copilot plan
- Ollama: `qwen3:8b` at localhost:11434

## Tool Schema
- `speak(text)` → thermal-audio socket → TTS
- `read()` → thermal-commander capture_pane → LLM summarizes
- `route(to, msg)` → thermal-messages bus → @claude/@codex/@planner/@system

## Dependencies
- Pidfile guard in `/run/user/$UID/thermal/` for single-instance enforcement.
