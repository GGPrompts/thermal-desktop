# Thermal Desktop

Custom-built Wayland desktop components with a thermal/FLIR infrared aesthetic. Native Rust applications replacing browser-based agent dashboards with GPU-accelerated, purpose-built tools.

## Architecture

This is a Cargo workspace with shared dependencies. All components use `thermal-core` for the color palette and shared rendering utilities.

### Core Stack
- **Terminal emulation**: alacritty_terminal + vte
- **PTY management**: nix crate (direct epoll, custom multiplexing)
- **GPU rendering**: wgpu + glyphon + cosmic-text (glyph atlas)
- **Wayland**: smithay-client-toolkit + winit
- **Audio**: rodio (PipeWire-compatible)
- **D-Bus**: zbus (async, 100% Rust)
- **File watching**: notify crate
- **IPC**: D-Bus via org.thermal.Conductor

### Components
- **thermal-conductor**: The centerpiece — native GPU-rendered agent dashboard with terminal pane wall
- **thermal-bar**: Wayland layer-shell status bar with real system data
- **thermal-launch**: Fuzzy-search app launcher overlay
- **thermal-notify**: D-Bus notification daemon
- **thermal-lock**: GPU-rendered lock screen
- **thermal-core**: Shared palette, types, and rendering utilities

### Thread Architecture (thermal-conductor)
```
PTY Manager Thread → epoll multiplexes N pseudo-terminals
Parser Thread      → vte parses ANSI → updates per-pane Grid state
Render Thread      → wgpu single pass, N viewports, glyph atlas
Main Thread        → event loop, user input, agent lifecycle
```

## Color Palette
All colors defined in `thermal-core/src/palette.rs`. Use `ThermalPalette::*` constants everywhere.

## Development
```bash
cargo build                           # Build all
cargo run -p thermal-conductor        # Run the agent dashboard
cargo run -p thermal-bar              # Run the status bar
```

## Task Tracking
Each crate has its own `tasks.json` for granular progress tracking.
