# thermal-dispatcher

AI voice command router — receives transcripts from thermal-audio (unified audio daemon) and dispatches to LLM backends.

## What This Does
Listens on Unix socket at `/run/user/$UID/thermal/dispatcher.sock`. Receives voice transcripts, dispatches to LLM with 3-tool schema, delegates all actions to agents. Multi-turn conversational context (8-turn rolling window, 2min session timeout). Adaptive trust tier learning tracks user confirmations and suggests promotions.

## LLM Backends
Priority order (auto-detect): Claude CLI → Copilot CLI → Ollama fallback.
- `THERMAL_DISPATCHER_BACKEND` env var: `claude`/`copilot`/`ollama`
- `THERMAL_DISPATCHER_MODEL` env var: model override (default: backend-specific)
- Copilot CLI: gpt4.1/gpt5-mini via $10/mo GitHub Copilot plan
- Ollama: `qwen3:8b` at localhost:11434

## Tool Schema
- `speak(text)` → thermal-audio socket → TTS
- `read()` → thermal-commander capture_pane → LLM summarizes
- `route(to, msg)` → conductor MessageForward protocol → internal message bus

## Adaptive Trust Tiers
- Confirmation history persisted at `~/.config/thermal/confirmation_history.toml`
- 3 consecutive approvals → suggest CONFIRM→AUTO promotion
- Deny-list blocks destructive ops from auto-promotion
- `--no-learning` to disable, `--show-promotions` to view pending

## Dependencies
- Pidfile guard in `/run/user/$UID/thermal/` for single-instance enforcement.
