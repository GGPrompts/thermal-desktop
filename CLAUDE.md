# Thermal Desktop

Custom-built Wayland desktop components with a thermal/FLIR infrared aesthetic. Native Rust applications replacing browser-based agent dashboards with GPU-accelerated, purpose-built tools.

## Architecture

Cargo workspace with shared dependencies. All components use `thermal-core` for the color palette and shared rendering utilities.

### Core Stack
- **GPU rendering**: wgpu 23 + glyphon 0.7 + cosmic-text 0.12 (glyph atlas)
- **Wayland**: smithay-client-toolkit 0.19 + winit 0.30
- **Terminal emulation**: alacritty_terminal 0.25 (in workspace, not yet wired to conductor)
- **Audio**: rodio 0.20 (PipeWire-compatible) + edge-tts CLI for TTS
- **D-Bus**: zbus 5 (async, tokio, 100% Rust)
- **File watching**: notify 7
- **IPC**: kitty remote control (current), D-Bus org.thermal.Conductor (planned)
- **State exchange**: `/tmp/claude-code-state/*.json` files read by multiple components

### Current Architecture (thermal-conductor)
thermal-conductor is currently a **kitty remote control CLI** — it spawns and manages kitty terminal windows running Claude sessions:
```
thermal-conductor CLI (thc)
        ↕ kitty @ spawn/send-text/ls (Unix socket IPC)
kitty terminal (GPU rendering, PTY management)
```

### Roadmap: GPU AI Terminal
The goal is to evolve thermal-conductor into a standalone GPU-rendered terminal purpose-built for AI coding — replacing kitty with a custom terminal that has native agent orchestration, state overlays, and multi-pane management.

- **Phase 1**: GPU terminal window rendering a single PTY via alacritty_terminal + wgpu
- **Phase 2**: Multi-pane layout with agent-aware overlays
- **Phase 3**: Session daemon for persistence without tmux/kitty
- **Phase 4**: AI-native features (semantic scrollback, context heatmaps, smart routing)

### Components

| Crate | Status | Description |
|-------|--------|-------------|
| **thermal-core** | Production | Shared palette, GPU context factory, ClaudeStatePoller, text rendering, PTY session mgmt |
| **thermal-conductor** | Functional | Kitty remote control CLI (`thc spawn/status/send/list/kill/audio`) |
| **thermal-bar** | Production | GPU-rendered Wayland layer-shell status bar (CPU/GPU/mem/net + Claude sessions) |
| **thermal-lock** | Production | GPU lock screen with WGSL heatmap shader + PAM auth (disabled on NVIDIA due to GPU context clash) |
| **thermal-launch** | Prototype | GPU fuzzy-search app launcher overlay |
| **thermal-notify** | Production | GPU notification daemon implementing org.freedesktop.Notifications via D-Bus |
| **thermal-audio** | Production | TTS voice announcements — 12-voice pool, per-agent voices, state transition alerts |
| **thermal-monitor** | Production | Ratatui TUI dashboard showing all Claude sessions with color-coded status |

### Key Shared Infrastructure
- **ClaudeStatePoller** (`thermal-core/src/claude_state.rs`): File-watches `/tmp/claude-code-state/` for Claude session JSON files. Used by thermal-conductor, thermal-bar, thermal-monitor, thermal-audio.
- **ThermalPalette** (`thermal-core/src/palette.rs`): 18 thermal color constants with gradient interpolation. Used everywhere.
- **WgpuContext** (`thermal-core/src/wgpu_ctx.rs`): Shared GPU device/queue factory.
- **ThermalTextRenderer** (`thermal-core/src/text.rs`): glyphon wrapper with cached font system.

## Color Palette
All colors defined in `thermal-core/src/palette.rs`. Use `ThermalPalette::*` constants everywhere.

## Development
```bash
cargo build                           # Build all
cargo run -p thermal-conductor        # Run conductor CLI (requires kitty)
cargo run -p thermal-bar              # Run the status bar
cargo run -p thermal-monitor          # Run TUI dashboard
cargo run -p thermal-audio            # Run TTS daemon
cargo run -p thermal-lock             # Run lock screen (caution: NVIDIA GPU clash)
```

## Known Issues
- **thermal-lock on NVIDIA**: GPU context deadlock when kitty (OpenGL/Vulkan) and thermal-lock (wgpu) compete for GPU. Currently disabled in Hyprland config.
- **thermal-launch**: Functional but fuzzy matching and reticle UI need refinement.

## Task Tracking
Issue tracking via beads (prefix: `therm`). Legacy per-crate `tasks.jsonl` files for historical reference.
