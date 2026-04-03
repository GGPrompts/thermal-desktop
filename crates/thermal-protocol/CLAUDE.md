# thermal-protocol

Lightweight wire protocol types for the thermal desktop suite. Zero GPU/async dependencies — designed for any crate that needs protocol types without pulling in wgpu/glyphon.

## What This Contains
- **ggl-generated types**: `AgentId`, `TaskState`, `ClaudeStatus`, `SessionState`, `AgentState`, `ConductorConfig`, etc.
- **Message bus types**: `Message`, `MessageType` for the internal message routing system
- **Config/state types**: Re-exported config and state schemas

## Code Generation
Uses [ggl](~/projects/ggl) codegen. Schema at `schemas/thermal-protocol.ggl`, built via `build.rs` → `ggl-build` → generated Rust in `OUT_DIR`. Integration layer at `src/ggl_types.rs` (hand-written aliases, Display, Default).

## Dependencies
Intentionally minimal: `serde`, `serde_json`, `ggl-build` (build only). No GPU, no async, no system deps.

## Who Depends on This
- `thermal-core` (re-exports everything for backward compat)
- `thermal-terminal` (uses session state types without GPU deps)
- Any new crate that needs protocol types should depend here, not on thermal-core
