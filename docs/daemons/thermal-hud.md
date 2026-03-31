# thermal-hud

Layer-shell HUD overlay showing agent session tabs and voice state.

## What it does
Renders a transparent overlay via wlr-layer-shell with Claude session tabs
showing display names (opus, sonnet-2) and status labels (ready/active/tool/idle).
Also displays voice assistant state. Mouse clicks select tabs and focus the
corresponding session workspace.

## Socket / Pidfile
- Pidfile: `/run/user/$UID/thermal/hud.pid`
- No socket (renders directly via Wayland)

## CLI Usage
```
thermal-hud
```
No flags.

## Dependencies
- **Needs**: Wayland compositor with wlr-layer-shell, agent state files in `/tmp/claude-code-state/`
- **Needed by**: nothing (standalone visual component)

## Troubleshooting
- If the HUD does not appear, verify wlr-layer-shell support in your compositor
- Agent tabs require running Claude/Codex sessions that write state files
- Voice state display reads `/tmp/thermal-voice-state.json` (written by thermal-voice)
