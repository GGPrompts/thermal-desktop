# Thermal Desktop

Custom-built Wayland desktop components with a thermal/FLIR infrared aesthetic. Native Rust applications replacing browser-based agent dashboards with GPU-accelerated, purpose-built tools.

## Architecture

Cargo workspace with shared dependencies. Three core layers split from the original `thermal-core`:
- **thermal-protocol**: Wire protocol types (ggl-generated), message types, config — lightweight, no GPU deps
- **thermal-runtime**: Socket/pid/cleanup helpers — no GPU deps
- **thermal-core**: Color palette, text rendering, wgpu context — GPU-heavy, re-exports protocol+runtime for compat

Three daemons (consolidated from 9):
- **thermal-conductor**: Terminal hub, TUI, GPU window, bar + HUD (managed layer-shell surfaces), message bus, session management
- **thermal-audio**: Unified TTS playback + voice capture (VAD, Whisper STT), replaces former thermal-voice
- **thermal-dispatcher**: LLM API routing, trust tiers, adaptive learning

Each crate has its own `CLAUDE.md` with detailed architecture — read those when working on a specific component.

### Disambiguation
- **thermal-terminal vs kitty**: thermal-terminal is our custom Rust crate (`crates/thermal-terminal/`) — PTY management, OSC 633 parsing, state inference engine. kitty is an external terminal emulator used as a backend. When asked to work on "terminal code", default to `crates/thermal-terminal/` unless kitty is explicitly named.
- **GPU terminal (`thc window`) vs kitty**: `thc window` is the custom wgpu-rendered terminal using `alacritty_terminal` (a Rust library, not the alacritty app) for terminal emulation. kitty is the stable fallback. The user's primary terminal on Arch is `thc window` (Super+Enter), with kitty as fallback (Super+Shift+Enter). GPU terminal bugs are high priority since they affect daily workflow.
- **alacritty_terminal**: A Rust terminal emulation *library* embedded in `thc window` — not the alacritty terminal application. Handles escape sequence parsing and grid buffer management. The GPU renderer (`grid_renderer.rs`, `color_mapping.rs`) draws the grid with wgpu.

### Core Stack
- **GPU rendering**: wgpu 23 + glyphon 0.7 + cosmic-text 0.12 (glyph atlas)
- **Wayland**: smithay-client-toolkit 0.19 + winit 0.30
- **Terminal emulation**: alacritty_terminal 0.25 (thermal-conductor GPU window)
- **Audio**: rodio 0.20 (PipeWire-compatible) + edge-tts CLI for TTS
- **D-Bus**: zbus 5 (async, tokio, 100% Rust)
- **File watching**: notify 7
- **IPC**: Unix sockets in `/run/user/$UID/thermal/` (conductor, dispatcher, audio). Bar, HUD, and message bus are now in-process conductor modules — no separate sockets. Conductor↔GPU window uses MessagePack framing with protocol version handshake.
- **State exchange**: `/tmp/claude-code-state/`, `/tmp/codex-state/`, `/tmp/copilot-state/` JSON files written by hooks/adapters. The conductor daemon owns a single `ClaudeStatePoller` (inotify) and broadcasts changes as semantic events — other components subscribe to the daemon instead of watching files directly. `/tmp/thermal-voice-state.json` for voice state (written by thermal-audio). `/tmp/thermal-focus-state.json` for terminal focus state (written by conductor GPU window).
- **Observability**: tracing crate with env-filter (`RUST_LOG=debug thc tui 2>thc.log`)

### Color Palette
All colors in `thermal-core/src/palette.rs`. Use `ThermalPalette::*` constants everywhere. Never hardcode `Color::Rgb(...)` in TUI code — use palette constants (includes `STATUS_OK`, `STATUS_WARN`, `STATUS_ERROR`). Palette colors are WCAG-tested against `Color::BG` via `Color::contrast_ratio()` in unit tests.

## Development

### Build Environment
`CARGO_TARGET_DIR` is set to `~/.cargo-target` (keeps build artifacts outside the project tree). This means:
- `cargo build` outputs go to `~/.cargo-target/debug/`, **not** `target/debug/`
- Stale binaries may exist at `target/debug/` — **do not trust them**
- The authoritative installed binaries live in `~/.cargo/bin/` via `cargo install`

### Installing / Updating Binaries
After making changes, **you must `cargo install --path crates/<name>`** to update the running binary. `cargo build` alone does NOT update `~/.cargo/bin/`. Use `/rebuild` to auto-detect changes, install, and restart daemons.

### Code Generation (ggl)
`thermal-protocol` uses [ggl](~/projects/ggl) codegen for wire protocol types. Schema: `crates/thermal-protocol/schemas/thermal-protocol.ggl`, built via `build.rs` → `ggl-build` → generated Rust in `OUT_DIR`. Integration layer at `src/ggl_types.rs`. `thermal-core` re-exports these types for backward compat.
- Edit the `.ggl` file to change type definitions, not the generated output
- `ggl_types.rs` is hand-written glue (aliases, Display, Default) — safe to edit

## Task Tracking
Issue tracking via beads (prefix: `therm`).
