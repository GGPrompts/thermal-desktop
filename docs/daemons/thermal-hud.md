# thermal-hud (built into thermal-conductor)

> **Note**: thermal-hud is no longer a standalone daemon. It runs as a managed layer-shell surface inside thermal-conductor. This doc describes the HUD's behavior.

Layer-shell HUD overlay showing agent session tabs and voice state.

## What it does
Renders a transparent overlay via wlr-layer-shell with Claude session tabs
showing display names (opus, sonnet-2) and status labels (ready/active/tool/idle).
Also displays voice assistant state. Mouse clicks select tabs and focus the
corresponding session workspace.

## How it runs
The HUD surface is spawned automatically when thermal-conductor starts in daemon mode.
It shares the conductor's wgpu device/queue and reads session state directly from
the SemanticEventBus (in-process, no socket needed).

## Dependencies
- **Needs**: Wayland compositor with wlr-layer-shell, agent state files in `/tmp/claude-code-state/`
- Voice state display reads `/tmp/thermal-voice-state.json` (written by thermal-audio)
