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
- **State exchange**: `/tmp/claude-code-state/`, `/tmp/codex-state/`, `/tmp/copilot-state/` JSON files read by multiple components; `/tmp/thermal-voice-state.json` for voice state + audio level
- **Voice pipeline**: cpal + faster-whisper (STT) → thermal-dispatcher (local Ollama qwen3:8b) → tool execution
- **Voice Activity Detection**: Energy-based VAD with hysteresis (silero-vad-rust planned)
- **LLM dispatch**: Local Ollama (qwen3:8b) — no API key required. Model configurable via `THERMAL_DISPATCHER_MODEL` env var

### Current Architecture (thermal-conductor)
thermal-conductor has two primary modes and one optional backend:

1. **TUI hub** (`thc` / `thc tui`): Tabbed ratatui dashboard with 3 tabs — Sessions (multi-agent session monitor for Claude/Codex/Copilot), Profiles (Launch/Edit sub-modes for spawning and editing spawn profiles), Services (daemon management with auto-conflict resolution for shared-binary services).
2. **GPU terminal** (`thermal-conductor window`): Standalone wgpu-rendered terminal with alacritty_terminal backend and agent overlay HUD (badge + timeline bar).
3. **Session daemon** (`thc daemon`): Optional background daemon that owns PTY sessions, providing Unix socket API at `/run/user/$UID/thermal/conductor.sock`. Not required when kitty is available.

The TUI hub uses a pluggable backend layer to manage terminal sessions:

```
thc tui (ratatui)
    ↕ Backend::Kitty (default)          ↕ Backend::Daemon (fallback)
kitty @ remote control API          thc daemon (Unix socket)
    ↕ kitty windows (PTYs)              ↕ alacritty_terminal PTYs
```

Backend is selected via `--backend=auto|kitty|daemon` (default: `auto`). In `auto` mode, kitty is probed first (`kitty @ ls`); the daemon is used if kitty remote control is unavailable. Session metadata (worktree paths, profile names, spawn times) is persisted in a sidecar file at `/run/user/$UID/thermal/sessions.json`.

### Roadmap: GPU AI Terminal
Evolving toward a fully integrated GPU terminal with native agent orchestration:

- **Phase 1** (done): GPU terminal window rendering a single PTY via alacritty_terminal + wgpu
- **Phase 2**: Multi-pane layout with agent-aware overlays
- **Phase 3** (partial): Session daemon for persistence without tmux/kitty
- **Phase 4**: AI-native features (semantic scrollback, context heatmaps, smart routing)

### Components

| Crate | Status | Description |
|-------|--------|-------------|
| **thermal-core** | Production | Shared palette, GPU context factory, multi-agent StatePoller (Claude/Codex/Copilot), text rendering, PTY session mgmt |
| **thermal-terminal** | Production | Shared terminal primitives — OSC 633 parser, input encoding, PTY session, terminal size (used by thermal-conductor and thermobile) |
| **thermal-conductor** | Production | Tabbed TUI hub (Sessions/Profiles/Services) + GPU terminal window + agent communication graph (F3). Orchestrates kitty windows via `kitty @` API (primary) or optional PTY session daemon (fallback). |
| **thermal-bar** | Production | GPU-rendered Wayland layer-shell status bar (CPU/GPU/mem/net + workspace map + agent sessions + voice level meter) |
| **thermal-lock** | Production | GPU lock screen with WGSL heatmap shader + PAM auth (disabled on NVIDIA due to GPU context clash) |
| **thermal-launch** | Prototype | GPU fuzzy-search app launcher overlay |
| **thermal-notify** | Production | GPU notification daemon implementing org.freedesktop.Notifications via D-Bus |
| **thermal-audio** | Production | TTS daemon — 12-voice pool, per-agent voices, state transition alerts (edge-tts + Unix socket API) |
| **thermal-monitor** | Production | Standalone ratatui TUI dashboard showing all agent sessions (Claude/Codex/Copilot) with color-coded status |
| **thermal-voice** | Production | Voice input daemon — always-listening VAD mode with PTT override, cpal audio capture, RMS level export, local Whisper STT, Unix socket API |
| **thermal-dispatcher** | Production | AI voice command router — receives transcripts from thermal-voice, dispatches via local Ollama (qwen3:8b), trust-tier gated execution, multi-turn conversational context (8-turn rolling window, 2min session timeout) |
| **thermal-commander** | Production | MCP server for Wayland/Hyprland desktop control — screenshots, click, type, window mgmt, system metrics (JSON-RPC 2.0 over stdio) |
| **thermal-face** | Prototype | GPU-rendered SDF avatar with thermal palette — animated face in layer-shell overlay, auto-blink, audio-driven mouth sync (planned) |
| **thermal-hud** | Functional | Layer-shell HUD overlay — Claude session tabs, voice assistant state display |
| **thermal-screensaver** | Functional | Idle-triggered thermal fluid simulation overlay — reaction-diffusion WGSL shader, ext-idle-notify-v1 |
| **thermal-wallpaper** | Functional | Animated WGSL thermal shader wallpaper — simplex-noise heat field modulated by real-time system metrics |

### Key Shared Infrastructure
- **ClaudeStatePoller** (`thermal-core/src/claude_state.rs`): File-watches `/tmp/claude-code-state/`, `/tmp/codex-state/`, and `/tmp/copilot-state/` for agent session JSON files. Infers `agent_type` from directory name. Used by thermal-conductor, thermal-bar, thermal-monitor, thermal-audio.
- **ThermalPalette** (`thermal-core/src/palette.rs`): 18 thermal color constants with gradient interpolation. Used everywhere.
- **WgpuContext** (`thermal-core/src/wgpu_ctx.rs`): Shared GPU device/queue factory (queries surface capabilities for format selection).
- **ThermalTextRenderer** (`thermal-core/src/text.rs`): glyphon wrapper with cached font system.
- **Voice state** (`/tmp/thermal-voice-state.json`): Written by thermal-voice with `state` (muted/monitoring/listening/processing), optional `label`, and `level` (RMS energy 0.0–1.0, updated ~5Hz). Read by thermal-bar for the voice level meter.
- **Pidfile guards**: Daemons (thermal-voice, thermal-dispatcher) use pidfiles in `/run/user/$UID/thermal/` for single-instance enforcement.
- **Spawn profiles** (`config/profiles.toml` or `~/.config/thermal/profiles.toml`): Project definitions loaded by the TUI Profiles tab (Launch/Edit sub-modes).
- **Trust tiers** (`config/trust-tiers.toml`): AUTO/CONFIRM/BLOCK classification for voice-triggered tool execution.

## Color Palette
All colors defined in `thermal-core/src/palette.rs`. Use `ThermalPalette::*` constants everywhere.

## Development

### Build Environment
`CARGO_TARGET_DIR` is set to `/tmp/cargo-target` (for Android NDK cross-compilation support). This means:
- `cargo build` outputs go to `/tmp/cargo-target/debug/`, **not** `target/debug/`
- Stale binaries may exist at `target/debug/` or `target/release/` from before this was set — **do not trust them**
- `/usr/local/bin/thermal-*` are symlinks to `target/release/` — these are **dead links**, ignore them
- The authoritative installed binaries live in `~/.cargo/bin/` via `cargo install`

### Installing / Updating Binaries
After making changes to a crate, **you must `cargo install`** to update the running binary:
```bash
cargo install --path crates/thermal-audio    # Installs to ~/.cargo/bin/thermal-audio
cargo install --path crates/thermal-bar      # etc.
```
`cargo build` alone does NOT update `~/.cargo/bin/`. Running daemons use the `~/.cargo/bin/` copies (resolved via PATH), so forgetting `cargo install` means your changes won't take effect at runtime.

### Running from Source
```bash
cargo run -p thermal-conductor        # Run TUI hub (thc)
cargo run -p thermal-conductor -- window  # Run GPU terminal window
cargo run -p thermal-conductor -- daemon  # Run session daemon
cargo run -p thermal-bar              # Run the status bar
cargo run -p thermal-monitor          # Run standalone TUI dashboard
cargo run -p thermal-audio            # Run TTS daemon
cargo run -p thermal-voice            # Run voice input daemon (push-to-talk)
cargo run -p thermal-voice -- listen  # Run voice input daemon (VAD always-listening + PTT override)
cargo run -p thermal-dispatcher       # Run voice command router
cargo run -p thermal-commander        # Run MCP server (stdio)
cargo run -p thermal-hud              # Run layer-shell HUD overlay
cargo run -p thermal-launch           # Run app launcher
cargo run -p thermal-screensaver      # Run screensaver (idle-triggered)
cargo run -p thermal-wallpaper        # Run animated wallpaper
cargo run -p thermal-face             # Run SDF face avatar overlay
cargo run -p thermal-lock             # Run lock screen (caution: NVIDIA GPU clash)
```

## Known Issues
- **thermal-lock on NVIDIA**: GPU context clash when kitty (OpenGL/Vulkan) and thermal-lock (wgpu) compete for GPU. Surface format fix applied (queries capabilities instead of hardcoding Bgra8UnormSrgb), but still disabled in Hyprland config pending further testing.
- **thermal-launch**: Functional but fuzzy matching and reticle UI need refinement.
- **thermal-conductor GPU window**: Runs in standalone mode only (no connection to kitty or daemon backends); agent overlay HUD is decorative until backend streaming is implemented.
- **NVIDIA DPMS resume** (therm-uqay): After 1-2hr AFK, terminals could become unresponsive. Mitigated: hypridle now uses brightness 0 instead of DPMS off, `NVD_BACKEND=direct` added, and thermal-wallpaper/bar/screensaver have non-fatal `conn.flush()` + screensaver has 5min watchdog for keyboard grab release.

## kitty Configuration Requirements
The default kitty backend requires kitty to be started with remote control enabled via a Unix socket. The full thermal-themed `kitty.conf` lives in `thermal-os-dotfiles/config/kitty/kitty.conf` and includes the thermal color scheme (mapped from `palette.rs`), tab bar styling, and remote control setup.

Critical settings for thc integration:
```
allow_remote_control socket-only
listen_on unix:/tmp/kitty-thc
```

To apply: symlink or copy to `~/.config/kitty/kitty.conf`. Without these settings, `thc` falls back to `--backend=daemon` automatically.

## Task Tracking
Issue tracking via beads (prefix: `therm`). Legacy per-crate `tasks.jsonl` files for historical reference.
