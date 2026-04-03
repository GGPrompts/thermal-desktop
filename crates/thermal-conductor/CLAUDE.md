# thermal-conductor

Tabbed ratatui TUI hub for orchestrating Claude agent **therminals**. Uses kitty's remote control API as the primary session backend, with the optional PTY session daemon as a fallback.

## What This Does
Spawns, tracks, and manages terminal sessions inside kitty via `kitty @` remote control. Session metadata (worktree paths, profile names, spawn timestamps) is persisted in a JSON sidecar at `/run/user/$UID/thermal/sessions.json`. A daemon-based backend is available as a fallback when kitty is not running with remote control enabled.

## Usage
```bash
thc                                    # Launch the TUI hub (auto-detects backend)
thc tui                                # Same as above
thc --backend=kitty                    # Force kitty backend (error if unavailable)
thc --backend=daemon                   # Force daemon backend
thc --backend=auto                     # Try kitty first, then daemon (default)
thc daemon                             # Start the optional PTY session daemon
thc window                             # Launch the standalone GPU terminal window
thc doctor                             # Check health of all thermal daemons
thc doctor --fix                       # Clean stale files + restart dead core daemons
```

## Architecture
```
thc tui (ratatui)
    ↕ Backend::Kitty (default)          ↕ Backend::Daemon (fallback)
kitty @ remote control API          thc daemon (Unix socket / MessagePack)
    ↕ kitty windows (PTYs)              ↕ alacritty_terminal PTYs
```

Backend detection order for `--backend=auto`:
1. Probe `kitty @ ls` — if it succeeds, use the kitty backend.
2. Try connecting to the daemon socket — use daemon backend if available.
3. Error with instructions if neither is reachable.

## Key Files
- `src/main.rs` — clap CLI, subcommand dispatch, `--backend` flag parsing
- `src/backend.rs` — `BackendPreference` enum, `detect_backend()` auto-detection logic
- `src/kitty.rs` — `KittyController`: async `kitty @` interface (spawn, list, close, send, focus) + sidecar read/write
- `src/client.rs` — `DaemonClient`: Unix socket communication with the optional daemon
- `src/daemon.rs` — Optional PTY session daemon implementation
- `src/protocol.rs` — Wire protocol types (Request/Response, MessagePack framing)

## kitty Requirements
kitty must be started with remote control enabled via Unix socket:
```
allow_remote_control socket-only
listen_on unix:/tmp/kitty-thc
```
Full thermal-themed kitty.conf lives in `thermal-os-dotfiles/config/kitty/kitty.conf`.

## TUI Sessions Tab (3-Panel Layout)
Designed as a command center for a vertical monitor:
- **Top**: Agent session list with model-based display names (opus, sonnet, gpt5.4mini), status badges, context %, workspace number, age, command duration (OSC 633). Single-select focuses kitty window. Multi-select (Space/Ctrl+A) for broadcast.
- **Middle**: Live terminal preview via daemon broadcast subscription (`PreviewSubscriber` + `Attach` protocol). PgUp/PgDn/Home/End to scroll.
- **Bottom**: Chat input with @-mention routing and response display via message bus. Tab-triggered autocomplete. Command history (up/down). Press 's' to save session as spawn profile.

**Panel focus**: Tri-state (`FocusedPanel` enum: AgentList/Preview/Chat). Tab/Shift+Tab cycles, click-to-focus, Esc returns to AgentList.

## TUI Patterns
- `handle_key()` returns `KeyResult` (not bool) — use `KeyResult::CLEAR` when spawning an external process that swaps the alternate screen (e.g. editor). The main loop calls `terminal.clear()` to force a full redraw.
- `TuiScreenGuard` (RAII) manages raw mode + alternate screen lifecycle — don't manually call enable/disable raw mode in the main TUI loop.
- `EditorSuspendGuard` in `settings.rs` handles editor launch/return — use `open_in_editor()` rather than spawning editors directly.
- TUI logging goes to file only (`/run/user/$UID/thermal/conductor-tui.log`), never stderr.

## GPU Terminal Window
`thermal-conductor window` — wgpu-rendered terminal with alacritty_terminal backend. Supports standalone mode (own PTY) or client mode (streams from `thc daemon` via `spawn_daemon_reader_task()`). Agent overlay HUD is decorative. Wayland input handlers (`input_handlers.rs`) clean up keyboard repeat/modifier state on focus loss and mouse button state on pointer leave — maintain this pattern when adding new input handling.

## Roadmap: GPU AI Terminal
- Phase 1 (done): GPU terminal rendering single PTY
- Phase 2: Multi-pane layout with agent-aware overlays
- Phase 3 (done): Session daemon streaming to GPU terminal
- Phase 4: AI-native features (semantic scrollback, context heatmaps)

## State File Watcher
The daemon owns a **single `ClaudeStatePoller`** (inotify) that watches `/tmp/{claude-code,codex,copilot}-state/` and imports external sessions into the semantic event bus. This replaces the old pattern where every consumer ran its own poller. External sessions get the same granular events (activity, tool start/stop, context threshold) as daemon-owned PTY sessions. Sessions are tagged `backend: "external"` vs `"daemon"`.

## Health Checks
`thc doctor` checks PID liveness and socket connectivity for all thermal daemons. `thc doctor --fix` cleans stale PID/socket files and restarts core service daemons.

## Known Issues
- **Stale socket hazard**: if `conductor.sock` lingers after daemon crash, `thc window` enters client mode against dead socket — use `thc doctor --fix` or `rm /run/user/$UID/thermal/conductor.sock`

## Dependencies
- `thermal-core` for `ClaudeStatePoller` (daemon-owned, single instance) and shared palette
- kitty with `allow_remote_control socket-only` (or `thc daemon` as fallback)
