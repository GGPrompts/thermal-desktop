# thermal-core

Shared library for the thermal desktop suite.

## What This Provides
- **ThermalPalette** (`src/palette.rs`): 18 thermal color constants with gradient interpolation. Used everywhere.
- **WgpuContext** (`src/wgpu_ctx.rs`): Shared GPU device/queue factory (queries surface capabilities for format selection).
- **ThermalTextRenderer** (`src/text.rs`): glyphon wrapper with cached font system.
- **ClaudeStatePoller** (`src/claude_state.rs`): File-watches `/tmp/claude-code-state/`, `/tmp/codex-state/`, `/tmp/copilot-state/` for agent session JSON files. Infers `agent_type` from directory. `model_display_name()` maps model IDs to short names (opus, sonnet, haiku, gpt5.4mini). Used by thermal-conductor, thermal-bar, thermal-monitor, thermal-audio.
- **ggl codegen types** (`src/ggl_types.rs`): Wire protocol types generated from `schemas/thermal-protocol.ggl`. `ClaudeStatus` and `SessionState` (aliased as `ClaudeSessionState`) are ggl-generated.

## Display Name Registry
`sessions.json` sidecar maps session_id to display_name (opus, sonnet-2, gpt5.4mini). Dedup numbering for multiple sessions with same model. Used by TUI, HUD, and message bus for @-mention routing.
