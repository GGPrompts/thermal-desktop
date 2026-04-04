# thermal-conductor

Process manager and orchestration hub for AI agent sessions. The conductor daemon owns agent PTYs and provides session persistence, the message bus, and semantic event streaming. The TUI is the control plane; `thc window` (GPU terminal) is the primary display for agent sessions.

## What This Does
Manages AI agent sessions (Claude, Codex, Copilot CLI) as daemon-owned processes with full lifecycle control — spawn, attach/detach, persist, observe, and orchestrate. The daemon owns PTYs so sessions survive window closes (like tmux, but purpose-built for agents). The GPU terminal (`thc window`) attaches to daemon sessions for rendering. kitty is used as a stable fallback for non-agent terminals and during GPU terminal development.

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

### Target architecture
```
thc tui (ratatui control plane)
    ↕ thc daemon (Unix socket / MessagePack)
    ↕ owns agent PTYs, message bus, semantic events
    ↕
thc window (GPU terminal)              kitty (non-agent terminals)
    ↕ attaches to daemon sessions           ↕ plain PTYs
```

### Current state (transitional)
Agent sessions currently spawn via kitty (`kitty @ launch`). The daemon exists and handles PTY sessions, but the TUI doesn't yet spawn through it — that wiring is tracked in therm-uayi. kitty is used as the stable development environment while the GPU terminal reaches full parity.

Backend detection order for `--backend=auto`:
1. Probe `kitty @ ls` — if it succeeds, use the kitty backend.
2. Try connecting to the daemon socket — use daemon backend if available.
3. Error with instructions if neither is reachable.

## Key Files
- `src/main.rs` — clap CLI, subcommand dispatch, `--backend` flag parsing
- `src/doctor.rs` — `thc doctor` health checker and `thc smoke` pipeline
- `src/config.rs` — `thc config` command: display effective settings with source annotations
- `src/backend.rs` — `BackendPreference` enum, `detect_backend()` auto-detection logic
- `src/kitty.rs` — `KittyController`: async `kitty @` interface (spawn, list, close, send, focus) + sidecar read/write
- `src/client.rs` — `DaemonClient`: Unix socket communication with the optional daemon
- `src/daemon/mod.rs` — Optional PTY session daemon (module root, re-exports `run_daemon`)
- `src/daemon/entry.rs` — `run_daemon` / `run_daemon_on` entry points, state file watcher
- `src/daemon/session.rs` — `Daemon` impl: spawn/list sessions, handle requests, persist state
- `src/daemon/client_handler.rs` — Per-connection handler: reads requests, streams responses
- `src/daemon/helpers.rs` — Cell/color conversion, grid snapshots, name generation
- `src/daemon/tests.rs` — Integration tests for the session daemon
- `src/protocol.rs` — Wire protocol types (Request/Response, MessagePack framing)

## kitty Requirements
kitty must be started with remote control enabled via Unix socket:
```
allow_remote_control socket-only
listen_on unix:/tmp/kitty-thc
```
Full thermal-themed kitty.conf lives in `deploy/kitty/kitty.conf`.

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
`thermal-conductor window` — wgpu-rendered terminal with alacritty_terminal backend. Currently runs in standalone mode (own PTY). Client mode (attach to daemon session) is scaffolded but not yet wired — tracked in therm-uayi. Wayland input handlers (`input_handlers.rs`) clean up keyboard repeat/modifier state on focus loss and mouse button state on pointer leave — maintain this pattern when adding new input handling.

## Roadmap: GPU AI Terminal
- Phase 1 (done): GPU terminal rendering single PTY — near parity with kitty
- Phase 2: Wire daemon → GPU terminal for agent sessions (therm-uayi) — spawn agents through daemon, attach/detach via `thc window`
- Phase 3: Message bus in daemon — agents coordinate without UI attached
- Phase 4: Multi-pane layout, semantic features, context heatmaps
- End goal: GPU terminal replaces kitty entirely for agent sessions; kitty only for dev shells during transition

## State File Watcher
The daemon owns a **single `ClaudeStatePoller`** (inotify) that watches `/tmp/{claude-code,codex,copilot}-state/` and imports external sessions into the semantic event bus. This replaces the old pattern where every consumer ran its own poller. External sessions get the same granular events (activity, tool start/stop, context threshold) as daemon-owned PTY sessions. Sessions are tagged `backend: "external"` vs `"daemon"`.

## Daemon Lifecycle
Shared kill/restart/counting logic lives in `src/daemon_lifecycle.rs`. Both `thc doctor` (main.rs) and the TUI Services page (tui/services.rs) delegate to this module instead of duplicating pgrep/pkill logic. Key functions:
- `count_instances()` / `list_pids()` — pgrep-based instance detection
- `kill_duplicates()` / `kill_all()` / `force_kill_all()` — unified kill with SIGTERM→SIGKILL escalation
- `is_stale_binary()` — compares /proc/PID/exe mtime against on-disk binary
- `cleanup_artifacts()` — removes socket + pidfile for a daemon
- `start_direct()` — setsid fallback when systemd units are not enabled
- `is_systemd_managed()` — cached check for whether a daemon's systemd unit is enabled
- `start_daemon()` / `stop_daemon()` / `restart_daemon()` — unified lifecycle ops (systemctl when managed, direct fallback otherwise)

The conductor daemon uses a flock-based single-instance guard (`conductor.lock` in the runtime dir) — atomic, no TOCTOU race, auto-released on crash. Pidfiles are still written for diagnostics / `thc doctor`.

## Health Checks
`thc doctor` checks PID liveness, socket connectivity, instance count, and binary staleness for all thermal daemons. `thc doctor --fix` cleans stale PID/socket files, kills duplicates, and restarts dead core daemons. Stale binary warnings appear when the on-disk binary is newer than the running process.

Integration tests in `tests/daemon_lifecycle.rs` cover: pidfile lifecycle, stale binary detection, socket lifecycle, single-instance guard.

## Known Issues
- **Stale socket hazard**: if `conductor.sock` lingers after daemon crash, `thc window` enters client mode against dead socket — use `thc doctor --fix` or `rm /run/user/$UID/thermal/conductor.sock`

## Dependencies
- `thermal-core` for `ClaudeStatePoller` (daemon-owned, single instance) and shared palette
- kitty with `allow_remote_control socket-only` (used for non-agent terminals and during GPU terminal development)
