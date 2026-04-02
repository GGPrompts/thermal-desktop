# thermal-core

Shared library for the thermal desktop suite.

## What This Provides
- **ThermalPalette** (`src/palette.rs`): 21 thermal color constants (including STATUS_OK/WARN/ERROR) with gradient interpolation. `Color::contrast_ratio()` computes WCAG 2.1 ratios — all foreground colors are tested against `BG` in unit tests. Never hardcode RGB values in consumers; add new colors here.
- **WgpuContext** (`src/wgpu_ctx.rs`): Shared GPU device/queue factory (queries surface capabilities for format selection).
- **ThermalTextRenderer** (`src/text.rs`): glyphon wrapper with cached font system.
- **ClaudeStatePoller** (`src/claude_state.rs`): File-watches `/tmp/claude-code-state/`, `/tmp/codex-state/`, `/tmp/copilot-state/` for agent session JSON files. Infers `agent_type` from directory. `model_display_name()` maps model IDs to short names (opus, sonnet, haiku, gpt5.4mini). **Owned by the conductor daemon** — a single poller instance broadcasts state changes as semantic events to all subscribers. Other components (bar, audio, HUD, monitor) should subscribe to the daemon event bus rather than creating their own poller.
- **ggl codegen types** (`src/ggl_types.rs`): Wire protocol types generated from `schemas/thermal-protocol.ggl`. `ClaudeStatus` and `SessionState` (aliased as `ClaudeSessionState`) are ggl-generated.

## Display Name Registry
`sessions.json` sidecar maps session_id to display_name (opus, sonnet-2, gpt5.4mini). Dedup numbering for multiple sessions with same model. Used by TUI, HUD, and message bus for @-mention routing.
