# Thermal Desktop

Custom-built Wayland desktop components with a thermal/FLIR infrared aesthetic. Native Rust applications replacing browser-based agent dashboards with GPU-accelerated, purpose-built tools.

## Architecture

Cargo workspace with shared dependencies. All components use `thermal-core` for the color palette and shared rendering utilities. Each crate has its own `CLAUDE.md` with detailed architecture — read those when working on a specific component.

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
- **IPC**: Unix sockets in `/run/user/$UID/thermal/` (conductor, voice, dispatcher, audio, messages)
- **State exchange**: `/tmp/claude-code-state/`, `/tmp/codex-state/`, `/tmp/copilot-state/` JSON files written by hooks/adapters. The conductor daemon owns a single `ClaudeStatePoller` (inotify) and broadcasts changes as semantic events — other components subscribe to the daemon instead of watching files directly. `/tmp/thermal-voice-state.json` for voice state + audio level
- **Observability**: tracing crate with env-filter (`RUST_LOG=debug thc tui 2>thc.log`)

### Color Palette
All colors in `thermal-core/src/palette.rs`. Use `ThermalPalette::*` constants everywhere.

## Development

### Dev Environment
Dual-boot: WSL2 (Windows) for coding, Arch Linux for runtime testing (Wayland, PipeWire, GPU). WSL2 lacks native Wayland — compilation works but GPU windows, HUD, audio, and transparency require Arch. `libssl-dev`/`openssl` needed for thermal-voice/audio crates.

### Build Environment
`CARGO_TARGET_DIR` is set to `~/.cargo-target` (keeps build artifacts outside the project tree). This means:
- `cargo build` outputs go to `~/.cargo-target/debug/`, **not** `target/debug/`
- Stale binaries may exist at `target/debug/` — **do not trust them**
- The authoritative installed binaries live in `~/.cargo/bin/` via `cargo install`

### Installing / Updating Binaries
After making changes, **you must `cargo install --path crates/<name>`** to update the running binary. `cargo build` alone does NOT update `~/.cargo/bin/`. Use `/rebuild` to auto-detect changes, install, and restart daemons.

### Code Generation (ggl)
`thermal-core` uses [ggl](~/projects/ggl) codegen for wire protocol types. Schema: `crates/thermal-core/schemas/thermal-protocol.ggl`, built via `build.rs` → `ggl-build` → generated Rust in `OUT_DIR`. Integration layer at `src/ggl_types.rs`.
- Edit the `.ggl` file to change type definitions, not the generated output
- `ggl_types.rs` is hand-written glue (aliases, Display, Default) — safe to edit

## Task Tracking
Issue tracking via beads (prefix: `therm`).
