# Thermal Desktop

Custom-built Wayland desktop components with a thermal/FLIR infrared aesthetic. Native Rust applications replacing browser-based agent dashboards with GPU-accelerated, purpose-built tools.

## Architecture

Cargo workspace with shared dependencies. All components use `thermal-core` for the color palette and shared rendering utilities.

### Disambiguation
- **thermal-terminal vs kitty**: thermal-terminal is our custom Rust crate (`crates/thermal-terminal/`) — PTY management, OSC 633 parsing, state inference engine. kitty is the external terminal emulator used as a backend. When asked to work on "terminal code", default to `crates/thermal-terminal/` unless kitty is explicitly named.

### Core Stack
- **GPU rendering**: wgpu 23 + glyphon 0.7 + cosmic-text 0.12 (glyph atlas)
- **Wayland**: smithay-client-toolkit 0.19 + winit 0.30
- **Terminal emulation**: alacritty_terminal 0.25 (used by thermal-conductor GPU window)
- **Audio**: rodio 0.20 (PipeWire-compatible) + edge-tts CLI for TTS
- **D-Bus**: zbus 5 (async, tokio, 100% Rust)
- **File watching**: notify 7
- **IPC**: Unix sockets in `/run/user/$UID/thermal/` (conductor, voice, dispatcher, audio, messages)
- **Agent message bus**: thermal-messages daemon — JSONL over Unix socket, ring buffer with subscriber replay, route table dispatching to @claude/@codex/@planner/@system/@user/@dispatcher backends
- **State exchange**: `/tmp/claude-code-state/`, `/tmp/codex-state/`, `/tmp/copilot-state/` JSON files read by multiple components. In daemon mode, state files are written natively by the PTY-based state inference engine (thermal-terminal); in kitty mode, legacy hook scripts write them. `/tmp/thermal-voice-state.json` for voice state + audio level
- **Voice pipeline**: cpal + faster-whisper (STT) → thermal-dispatcher (Claude CLI / Copilot CLI / Ollama fallback) → speak/read/route (delegates actions to agents)
- **Voice Activity Detection**: Energy-based VAD with hysteresis (silero-vad-rust planned)
- **LLM dispatch**: Claude CLI primary (`--json-schema` structured output), Copilot CLI secondary, Ollama (qwen3:8b) offline fallback. Backend configurable via `THERMAL_DISPATCHER_BACKEND` env var (claude/copilot/ollama). Model configurable via `THERMAL_DISPATCHER_MODEL` env var
- **Observability**: tracing crate with env-filter for structured logging (`RUST_LOG=debug thc tui 2>thc.log`)

### Current Architecture (thermal-conductor)
thermal-conductor has two primary modes and one optional backend:

1. **TUI hub** (`thc` / `thc tui`): Tabbed ratatui dashboard with 4 tabs — Sessions (3-panel: agent list + live kitty preview + @-mention chat with bus routing), Profiles (Launch/Edit sub-modes for spawning and editing spawn profiles), Services (daemon management with auto-conflict resolution + integrated settings UI for `~/.config/thermal/settings.toml`, 'e' hotkey to edit), Messages (read-only message bus log).
2. **GPU terminal** (`thermal-conductor window`): wgpu-rendered terminal with alacritty_terminal backend. Supports standalone mode (own PTY) or client mode (streams from session daemon). Agent overlay HUD (badge + timeline bar).
3. **Session daemon** (`thc daemon`): Optional background daemon that owns PTY sessions, providing Unix socket API at `/run/user/$UID/thermal/conductor.sock`. Native PTY-based agent state detection via state_inference engine (no hook scripts needed). Not required when kitty is available.

The TUI hub uses a pluggable backend layer to manage terminal sessions:

```
thc tui (ratatui)
    ↕ Backend::Kitty (default)          ↕ Backend::Daemon (fallback)
kitty @ remote control API          thc daemon (Unix socket)
    ↕ kitty windows (PTYs)              ↕ alacritty_terminal PTYs
```

Backend is selected via `--backend=auto|kitty|daemon` (default: `auto`). In `auto` mode, kitty is probed first — checks `KITTY_LISTEN_ON`, then globs `/tmp/kitty-thc-*` for socket discovery (works from outside kitty), then bare `kitty @ ls` as fallback; the daemon is used if kitty remote control is unavailable. Session metadata (worktree paths, profile names, spawn times, display names) is persisted in a sidecar file at `/run/user/$UID/thermal/sessions.json`.

#### Sessions Tab (3-Panel Layout)
The Sessions tab is designed as a command center for a vertical monitor:
- **Top**: Agent session list with model-based display names (opus, sonnet, gpt5.4mini instead of hex IDs), status badges, context %, workspace number, age (time since last state update), command duration (from OSC 633 telemetry). Single-select focuses the agent's kitty window (workspace switch). Multi-select (Space/Ctrl+A) for broadcast.
- **Middle**: Live terminal preview via `kitty @ get-text --extent=screen` of the selected session, refreshed every 500ms. PgUp/PgDn/Home/End to scroll. Mouse scroll when preview is focused.
- **Bottom**: Chat input with @-mention routing (@dispatcher, @claude, @system, etc.) and response display via message bus subscriber. @-mentions take priority over highlighted session routing. Tab-triggered autocomplete popup for live agents. Command history (up/down arrows). Press 's' on a session to save it as a spawn profile.

**Panel focus**: Tri-state focus system (`FocusedPanel` enum: AgentList/Preview/Chat). Tab/Shift+Tab cycles panels, click-to-focus, Esc returns to AgentList. Focused panel has accent-colored border.

### Roadmap: GPU AI Terminal
Evolving toward a fully integrated GPU terminal with native agent orchestration:

- **Phase 1** (done): GPU terminal window rendering a single PTY via alacritty_terminal + wgpu
- **Phase 2**: Multi-pane layout with agent-aware overlays
- **Phase 3** (done): Session daemon streaming to GPU terminal — daemon broadcasts ScreenUpdate diffs, GPU window consumes via `spawn_daemon_reader_task()`
- **Phase 4**: AI-native features (semantic scrollback, context heatmaps, smart routing)

### Components

| Crate | Status | Description |
|-------|--------|-------------|
| **thermal-core** | Production | Shared palette, GPU context factory, multi-agent StatePoller (Claude/Codex/Copilot), text rendering, PTY session mgmt |
| **thermal-terminal** | Production | Shared terminal primitives — OSC 633 parser, input encoding, PTY session (with structured ExitReason), terminal size, native agent state inference from PTY output, per-session JSONL event log (used by thermal-conductor and thermobile) |
| **thermal-conductor** | Production | Tabbed TUI hub (Sessions/Profiles/Services/Messages) + GPU terminal window. Sessions tab: 3-panel layout with agent list, live kitty preview, @-mention chat with bus routing. Named agents (opus, sonnet, gpt5.4mini). Orchestrates kitty windows via `kitty @` API (primary) or optional PTY session daemon (fallback). |
| **thermal-bar** | Production | GPU-rendered Wayland layer-shell status bar (CPU/GPU/mem/net + workspace map + agent sessions + voice level meter). Mouse click support: workspace switch, voice mute toggle, session focus |
| **thermal-lock** | Production | GPU lock screen with WGSL heatmap shader + PAM auth (disabled on NVIDIA due to GPU context clash) |
| **thermal-launch** | Prototype | GPU fuzzy-search app launcher overlay |
| **thermal-notify** | Production | GPU notification daemon implementing org.freedesktop.Notifications via D-Bus |
| **thermal-audio** | Production | TTS daemon — 12-voice pool, per-agent voices, state transition alerts (edge-tts + Unix socket API) |
| **thermal-monitor** | Production | Standalone ratatui TUI dashboard showing all agent sessions (Claude/Codex/Copilot) with color-coded status |
| **thermal-voice** | Production | Voice input daemon — always-listening VAD mode with PTT override, cpal audio capture, RMS level export, local Whisper STT, Unix socket API |
| **thermal-dispatcher** | Production | AI voice command router — receives transcripts from thermal-voice, dispatches via Claude CLI (primary) / Copilot CLI (secondary) / Ollama (fallback) with 3-tool schema (speak/read/route), delegates all actions to agents, multi-turn conversational context (8-turn rolling window, 2min session timeout) |
| **thermal-messages** | Production | Agent message bus daemon — JSONL over Unix socket, ring buffer (500 msgs) with subscriber replay, route table dispatching to @claude/@codex/@planner/@system/@user/@dispatcher backends, optional JSONL persistence, kitty live-session routing with one-shot fallback |
| **thermal-commander** | Production | MCP server for Wayland/Hyprland desktop control — pane capture (kitty @ get-text), click, type, window mgmt, system metrics (JSON-RPC 2.0 over stdio) |
| **thermal-face** | Prototype | GPU-rendered SDF avatar with thermal palette — animated face in layer-shell overlay, auto-blink, audio-driven mouth sync (planned) |
| **thermal-hud** | Production | Layer-shell HUD overlay — Claude session tabs with display names (opus, sonnet-2) and status labels (ready/active/tool/idle), voice assistant state display. Mouse click support: tab selection, session workspace focus |
| **thermal-screensaver** | Functional | Idle-triggered thermal fluid simulation overlay — reaction-diffusion WGSL shader, ext-idle-notify-v1 |
| **thermal-wallpaper** | Functional | Animated WGSL thermal shader wallpaper — simplex-noise heat field modulated by real-time system metrics |

### Key Shared Infrastructure
- **ClaudeStatePoller** (`thermal-core/src/claude_state.rs`): File-watches `/tmp/claude-code-state/`, `/tmp/codex-state/`, and `/tmp/copilot-state/` for agent session JSON files. Infers `agent_type` from directory name. `model_display_name()` maps model IDs to short names (opus, sonnet, haiku, gpt5.4mini, etc.). Used by thermal-conductor, thermal-bar, thermal-monitor, thermal-audio.
- **ThermalPalette** (`thermal-core/src/palette.rs`): 18 thermal color constants with gradient interpolation. Used everywhere.
- **WgpuContext** (`thermal-core/src/wgpu_ctx.rs`): Shared GPU device/queue factory (queries surface capabilities for format selection).
- **ThermalTextRenderer** (`thermal-core/src/text.rs`): glyphon wrapper with cached font system.
- **Voice state** (`/tmp/thermal-voice-state.json`): Written by thermal-voice with `state` (muted/monitoring/listening/processing), optional `label`, and `level` (RMS energy 0.0–1.0, updated ~5Hz). Read by thermal-bar for the voice level meter.
- **Pidfile guards**: Daemons (thermal-voice, thermal-dispatcher) use pidfiles in `/run/user/$UID/thermal/` for single-instance enforcement.
- **Spawn profiles** (`config/profiles.toml` or `~/.config/thermal/profiles.toml`): Project definitions loaded by the TUI Profiles tab (Launch/Edit sub-modes). Sessions can be saved as profiles via 's' hotkey.
- **Trust tiers** (`config/trust-tiers.toml`): AUTO/CONFIRM/BLOCK classification for voice-triggered tool execution. Used by thermal-messages routing (dispatcher delegates all actions to agents via route).
- **Display name registry** (`sessions.json` sidecar): Maps session_id → display_name (opus, sonnet-2, gpt5.4mini). Dedup numbering for multiple sessions with same model. Used by TUI, HUD, and message bus for @-mention routing.
- **AgentStateInference** (`thermal-terminal/src/state_inference.rs`): Native PTY-based agent state detection. Combines OSC 633 CommandTracker transitions with output heuristics (spinners, tool blocks, prompts) to infer agent status. Writes state files atomically to `/tmp/*-state/` directories. State files include command telemetry: `last_command`, `last_exit_code`, `last_command_started_at`, `last_command_duration_ms`, `consecutive_failures`. Used by thermal-conductor daemon mode; replaces hook scripts.
- **Per-session event log** (`thermal-terminal/src/event_log.rs`): JSONL event log per daemon session at `/run/user/$UID/thermal/sessions/<id>.events.jsonl`. Captures semantic lifecycle events (Spawn, StatusChange, CommandStart, CommandFinish, Resize, PtyEof, Bell) with ISO 8601 timestamps. Truncate-on-overflow rotation (default 5000 entries). Written by AgentStateInference + daemon; cleaned up on session removal.
- **ExitReason** (`thermal-terminal/src/pty.rs`): Structured session exit enum (PtyEof/Signal/SpawnFailed/FrontendClose/DaemonShutdown) replacing bare `has_exited()` boolean. Propagated through the daemon protocol's `SessionExited` response. `has_exited()` still works as a fast lock-free check.
- **Settings** (`~/.config/thermal/settings.toml`): Unified per-component config file. Read by the TUI Services tab (inline summary + 'e' hotkey to edit in `$EDITOR`). Auto-created with documented defaults on first access.

## Color Palette
All colors defined in `thermal-core/src/palette.rs`. Use `ThermalPalette::*` constants everywhere.

## Development

### Build Environment
`CARGO_TARGET_DIR` is set to `~/.cargo-target` (keeps build artifacts outside the project tree for Android NDK cross-compilation support). This means:
- `cargo build` outputs go to `~/.cargo-target/debug/`, **not** `target/debug/`
- Stale binaries may exist at `target/debug/` or `target/release/` from before this was set — **do not trust them**
- The authoritative installed binaries live in `~/.cargo/bin/` via `cargo install`

### Code Generation (ggl)
`thermal-core` uses [ggl](~/projects/ggl) codegen for wire protocol types. Schema source is `crates/thermal-core/schemas/thermal-protocol.ggl`, built via `build.rs` → `ggl-build` → generated Rust in `OUT_DIR`. Integration layer at `src/ggl_types.rs` (type aliases + manual trait impls).
- **Edit the `.ggl` file** to change type definitions, not the generated output
- **`ggl_types.rs`** is hand-written glue (aliases, Display, Default) — safe to edit
- `ClaudeStatus` and `SessionState` (aliased as `ClaudeSessionState`) are ggl-generated with per-type overrides in `build.rs` for serde attributes
- `thermal-terminal/src/state_inference.rs` keeps a local `StateFile` struct aligned with `SessionStateV1` to avoid pulling `thermal-core` GPU deps

### Installing / Updating Binaries
After making changes to a crate, **you must `cargo install`** to update the running binary:
```bash
cargo install --path crates/thermal-audio    # Installs to ~/.cargo/bin/thermal-audio
cargo install --path crates/thermal-bar      # etc.
```
`cargo build` alone does NOT update `~/.cargo/bin/`. Running daemons use the `~/.cargo/bin/` copies (resolved via PATH), so forgetting `cargo install` means your changes won't take effect at runtime.

Use `/rebuild` to automatically detect changed crates, install binaries, and restart running daemons.

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
cargo run -p thermal-messages          # Run agent message bus daemon
cargo run -p thermal-messages -- --persist  # Run with JSONL persistence
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
- **thermal-conductor GPU window**: Supports standalone mode and daemon client mode (`SessionMode::Client` streams from `thc daemon`). Agent overlay HUD is decorative. Daemon streaming is implemented but needs end-to-end testing. **Stale socket hazard**: if `conductor.sock` lingers after a daemon crash/kill, `thc window` may enter client mode against a dead socket — clean up with `rm /run/user/$UID/thermal/conductor.sock`. The `/rebuild` skill should also clean stale sockets.
- **NVIDIA DPMS resume** (therm-uqay): After 1-2hr AFK, terminals could become unresponsive. Mitigated: hypridle now uses brightness 0 instead of DPMS off, `NVD_BACKEND=direct` added, and thermal-wallpaper/bar/screensaver have non-fatal `conn.flush()` + screensaver has 5min watchdog for keyboard grab release.

## Voice Pipeline

### Architecture
```
thermal-voice (cpal + whisper-cpp STT)
    ├─ VAD mode: speech → dispatcher socket → LLM → tool execution
    └─ PTT mode: speech → wtype at cursor + clipboard + dispatcher
thermal-dispatcher (Claude CLI primary → Copilot CLI → Ollama fallback, 3 tools)
    ├─ speak(text) → thermal-audio socket → TTS
    ├─ read() → thermal-commander capture_pane → LLM summarizes
    └─ route(to, msg) → thermal-messages bus → @claude/@codex/@planner/@system
thermal-messages (agent message bus)
    ├─ @claude/@codex → kitty live session (via send-text) or one-shot CLI fallback
    ├─ @system → thermal-commander MCP (trust-tier gated)
    ├─ @planner → claude -p with planner system prompt
    ├─ @dispatcher → thermal-dispatcher socket
    └─ @user → broadcast to TUI subscribers + TTS
thermal-audio (TTS responses, suppressed during voice input)
```

### Hotkeys & Mouse Buttons
| Input | Action | Binding |
|-------|--------|---------|
| **Super+\\** | PTT toggle (start/stop recording) | `thermal-voice toggle` |
| **Mouse back (thumb)** | PTT toggle | `thermal-voice toggle` (mouse:275) |
| **Mouse forward** | VAD on/off (always-listening) | `thermal-vad-toggle.sh` (mouse:276) |

### Dependencies
- **whisper-cpp**: Local STT with CUDA. Install via `thermal-os-dotfiles/bin/install-whisper-cpp`.
- **Ollama**: Local LLM server at localhost:11434, offline fallback. Model: `qwen3:8b` (configurable via `THERMAL_DISPATCHER_MODEL`). Backend selectable via `THERMAL_DISPATCHER_BACKEND` (claude/copilot/ollama; default: auto-detect in priority order).
- **wtype**: Wayland text input for PTT dictation mode.
- **wl-copy**: Clipboard for PTT transcripts.

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
