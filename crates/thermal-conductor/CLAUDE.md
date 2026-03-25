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

## Dependencies
- `thermal-core` for `ClaudeStatePoller` (Sessions tab) and shared palette
- kitty with `allow_remote_control socket-only` (or `thc daemon` as fallback)
