# Thermal Desktop

Custom-built Wayland desktop components with a thermal/FLIR infrared aesthetic. Native Rust applications replacing browser-based agent dashboards with GPU-accelerated, purpose-built tools.

## Architecture

This is a Cargo workspace with shared dependencies. All components use `thermal-core` for the color palette and shared rendering utilities.

### Core Stack
- **GPU rendering**: wgpu + glyphon + cosmic-text (glyph atlas)
- **Wayland**: smithay-client-toolkit + winit
- **Terminal backend**: tmux (Phase 1) → alacritty_terminal + nix PTY (Phase 2)
- **Audio**: rodio (PipeWire-compatible)
- **D-Bus**: zbus (async, 100% Rust)
- **File watching**: notify crate
- **IPC**: D-Bus via org.thermal.Conductor + thermal-ctl CLI

### Hybrid Architecture (thermal-conductor)
Phase 1 uses tmux as the terminal backend — thermal-conductor is a GPU-rendered tmux frontend:
```
thermal-conductor (wgpu renderer + state tracker)
        ↕ tmux capture-pane / send-keys
tmux server (PTY management, scrollback, session persistence)
```
This gets a working product fast. Phase 2 replaces tmux with direct PTY management. Phase 3 adds a session daemon for persistence without tmux.

### Components
- **thermal-conductor**: The centerpiece — native GPU-rendered agent dashboard (tmux frontend → native PTY)
- **thermal-bar**: Wayland layer-shell status bar with real system data
- **thermal-launch**: Fuzzy-search app launcher overlay
- **thermal-notify**: D-Bus notification daemon
- **thermal-lock**: GPU-rendered lock screen
- **thermal-core**: Shared palette, types, and rendering utilities

## Color Palette
All colors defined in `thermal-core/src/palette.rs`. Use `ThermalPalette::*` constants everywhere.

## Development
```bash
cargo build                           # Build all
cargo run -p thermal-conductor        # Run the agent dashboard
cargo run -p thermal-bar              # Run the status bar
```

## Task Tracking
Each crate has its own `tasks.jsonl` for granular progress tracking.
