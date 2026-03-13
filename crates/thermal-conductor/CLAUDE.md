# thermal-conductor

The centerpiece — a native Wayland GPU-rendered agent dashboard.

## What This Does
A wall of terminal panes rendered with wgpu. Each pane runs a Claude agent PTY session. Thermal state indicators show agent status. Click-to-focus sidebar. Native PipeWire audio cues. Git diff awareness.

## Architecture
- PTY Manager: spawns and manages N pseudo-terminals via nix crate
- ANSI Parser: vte crate parses terminal output into Grid state
- Renderer: wgpu + glyphon renders all panes in a single GPU pass with glyph atlas
- State Tracker: watches Claude hook outputs via notify crate
- Audio: rodio plays notification sounds on state transitions
- D-Bus: zbus exposes org.thermal.Conductor service

## Key References
- Alacritty terminal emulation: alacritty_terminal crate (Grid<Cell> for terminal state)
- WezTerm mux layer: architecture for managing multiple PTY sessions
- Rio terminal: wgpu-based rendering with cosmic-text
- Zed's GPUI terminal: embedding alacritty_terminal in a GPU-rendered app

## Agent State Colors
- Blue (#1e3a8a): idle/waiting
- Green (#22c55e): running/active
- Yellow (#eab308): output/producing results
- Orange (#f97316): warning
- Red (#ef4444): error/failed

## Development
```bash
cargo run -p thermal-conductor
```
