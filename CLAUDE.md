# Thermal Desktop

Custom-built Wayland desktop components with a thermal/FLIR infrared aesthetic. Native Rust applications replacing browser-based agent dashboards with GPU-accelerated, purpose-built tools.

## Architecture

Cargo workspace with shared dependencies. All components use `thermal-core` for the color palette and shared rendering utilities.

### Core Stack
- **GPU rendering**: wgpu 23 + glyphon 0.7 + cosmic-text 0.12 (glyph atlas)
- **Wayland**: smithay-client-toolkit 0.19 + winit 0.30
- **Terminal emulation**: alacritty_terminal 0.25 (used by thermal-conductor GPU window)
- **Audio**: rodio 0.20 (PipeWire-compatible) + edge-tts CLI for TTS
- **D-Bus**: zbus 5 (async, tokio, 100% Rust)
- **File watching**: notify 7
- **IPC**: Unix sockets in `/run/user/$UID/thermal/` (conductor, voice, dispatcher, audio)
- **State exchange**: `/tmp/claude-code-state/*.json` files read by multiple components
- **Voice pipeline**: cpal + faster-whisper (STT) → Anthropic Haiku API (intent) → tool execution

### Current Architecture (thermal-conductor)
thermal-conductor has three modes:

1. **TUI hub** (`thc` / `thc tui`): Tabbed ratatui dashboard with 4 tabs — Sessions (Claude session monitor), Spawn (profile-based session launcher), Profiles (profile editor), Services (daemon management).
2. **Session daemon** (`thc daemon`): Background daemon that owns PTY sessions, providing Unix socket API at `/run/user/$UID/thermal/conductor.sock`.
3. **GPU terminal** (`thermal-conductor window`): Standalone wgpu-rendered terminal with alacritty_terminal backend and agent overlay HUD (badge + timeline bar).

```
thc tui (ratatui)           thermal-conductor window (wgpu)
    ↕ Unix socket IPC            ↕ alacritty_terminal PTY
thc daemon (PTY owner)       standalone PTY (no daemon)
```

### Roadmap: GPU AI Terminal
Evolving toward a fully integrated GPU terminal with native agent orchestration:

- **Phase 1** (done): GPU terminal window rendering a single PTY via alacritty_terminal + wgpu
- **Phase 2**: Multi-pane layout with agent-aware overlays
- **Phase 3** (partial): Session daemon for persistence without tmux/kitty
- **Phase 4**: AI-native features (semantic scrollback, context heatmaps, smart routing)

### Components

| Crate | Status | Description |
|-------|--------|-------------|
| **thermal-core** | Production | Shared palette, GPU context factory, ClaudeStatePoller, text rendering, PTY session mgmt |
| **thermal-conductor** | Production | Tabbed TUI hub (Sessions/Spawn/Profiles/Services) + PTY session daemon + GPU terminal window |
| **thermal-bar** | Production | GPU-rendered Wayland layer-shell status bar (CPU/GPU/mem/net + Claude sessions) |
| **thermal-lock** | Production | GPU lock screen with WGSL heatmap shader + PAM auth (disabled on NVIDIA due to GPU context clash) |
| **thermal-launch** | Prototype | GPU fuzzy-search app launcher overlay |
| **thermal-notify** | Production | GPU notification daemon implementing org.freedesktop.Notifications via D-Bus |
| **thermal-audio** | Production | TTS daemon — 12-voice pool, per-agent voices, state transition alerts (edge-tts + Unix socket API) |
| **thermal-monitor** | Production | Standalone ratatui TUI dashboard showing all Claude sessions with color-coded status |
| **thermal-voice** | Production | Push-to-talk voice input daemon — cpal audio capture, local Whisper STT, Unix socket API |
| **thermal-dispatcher** | Production | AI voice command router — receives transcripts from thermal-voice, calls Haiku API with tool-use, trust-tier gated execution |
| **thermal-commander** | Production | MCP server for Wayland/Hyprland desktop control — screenshots, click, type, window mgmt (JSON-RPC 2.0 over stdio) |
| **thermal-hud** | Functional | Layer-shell HUD overlay — Claude session tabs, voice assistant state display |
| **thermal-screensaver** | Functional | Idle-triggered thermal fluid simulation overlay — reaction-diffusion WGSL shader, ext-idle-notify-v1 |
| **thermal-wallpaper** | Functional | Animated WGSL thermal shader wallpaper — simplex-noise heat field modulated by real-time system metrics |

### Key Shared Infrastructure
- **ClaudeStatePoller** (`thermal-core/src/claude_state.rs`): File-watches `/tmp/claude-code-state/` for Claude session JSON files. Used by thermal-conductor, thermal-bar, thermal-monitor, thermal-audio.
- **ThermalPalette** (`thermal-core/src/palette.rs`): 18 thermal color constants with gradient interpolation. Used everywhere.
- **WgpuContext** (`thermal-core/src/wgpu_ctx.rs`): Shared GPU device/queue factory (queries surface capabilities for format selection).
- **ThermalTextRenderer** (`thermal-core/src/text.rs`): glyphon wrapper with cached font system.
- **Pidfile guards**: Daemons (thermal-voice, thermal-dispatcher) use pidfiles in `/run/user/$UID/thermal/` for single-instance enforcement.
- **Spawn profiles** (`config/profiles.toml` or `~/.config/thermal/profiles.toml`): Project definitions loaded by the TUI Spawn and Profiles tabs.
- **Trust tiers** (`config/trust-tiers.toml`): AUTO/CONFIRM/BLOCK classification for voice-triggered tool execution.

## Color Palette
All colors defined in `thermal-core/src/palette.rs`. Use `ThermalPalette::*` constants everywhere.

## Development
```bash
cargo build                           # Build all
cargo run -p thermal-conductor        # Run TUI hub (thc)
cargo run -p thermal-conductor -- window  # Run GPU terminal window
cargo run -p thermal-conductor -- daemon  # Run session daemon
cargo run -p thermal-bar              # Run the status bar
cargo run -p thermal-monitor          # Run standalone TUI dashboard
cargo run -p thermal-audio            # Run TTS daemon
cargo run -p thermal-voice            # Run voice input daemon
cargo run -p thermal-dispatcher       # Run voice command router
cargo run -p thermal-commander        # Run MCP server (stdio)
cargo run -p thermal-hud              # Run layer-shell HUD overlay
cargo run -p thermal-launch           # Run app launcher
cargo run -p thermal-screensaver      # Run screensaver (idle-triggered)
cargo run -p thermal-wallpaper        # Run animated wallpaper
cargo run -p thermal-lock             # Run lock screen (caution: NVIDIA GPU clash)
```

## Known Issues
- **thermal-lock on NVIDIA**: GPU context clash when kitty (OpenGL/Vulkan) and thermal-lock (wgpu) compete for GPU. Surface format fix applied (queries capabilities instead of hardcoding Bgra8UnormSrgb), but still disabled in Hyprland config pending further testing.
- **thermal-launch**: Functional but fuzzy matching and reticle UI need refinement.
- **thermal-conductor window + daemon**: Screen streaming from daemon to GPU window not yet implemented — GPU window runs in standalone mode only.

## Task Tracking
Issue tracking via beads (prefix: `therm`). Legacy per-crate `tasks.jsonl` files for historical reference.
