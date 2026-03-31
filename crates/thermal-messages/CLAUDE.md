# thermal-messages

Agent message bus daemon — JSONL over Unix socket.

## What This Does
Ring buffer (500 msgs) with subscriber replay, route table dispatching to named backends. Optional JSONL persistence (`--persist`).

## Route Table
- `@claude`/`@codex` → kitty live session (via send-text) or one-shot CLI fallback
- `@system` → thermal-commander MCP (trust-tier gated via `config/trust-tiers.toml`)
- `@planner` → claude -p with planner system prompt
- `@dispatcher` → thermal-dispatcher socket
- `@user` → broadcast to TUI subscribers + TTS
