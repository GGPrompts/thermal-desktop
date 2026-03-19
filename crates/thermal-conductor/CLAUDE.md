# thermal-conductor

CLI tool for orchestrating Claude agent **therminals** via the session daemon.

## What This Does
Spawns, tracks, polls, and sends to terminal sessions managed by the thermal-conductor session daemon. The daemon owns PTY sessions; the CLI communicates over a Unix socket.

## Usage
```bash
thc daemon                             # Start the session daemon (run first)
thc spawn                              # Spawn 1 therminal session
thc spawn -n 4 -p ~/projects/foo       # Spawn 4 in a project dir
thc status                             # Show all therminals with Claude state
thc list                               # Compact session list
thc list --json                        # Raw JSON session data
thc send <session-id> "fix the bug"    # Send text to a session
thc kill <session-id>                  # Kill a session
thc window                             # Launch the GPU terminal window
```

## Architecture
```
thc (CLI tool)
  ↕ Unix socket (MessagePack protocol)
thermal-conductor daemon (PTY ownership, session management)
  ↕ alacritty_terminal (terminal emulation)
PTY child processes (shells, Claude sessions)
```

## Key Files
- `src/main.rs` — clap CLI, subcommand dispatch via DaemonClient
- `src/client.rs` — DaemonClient for Unix socket communication with daemon
- `src/daemon.rs` — Session daemon implementation
- `src/protocol.rs` — Wire protocol types (Request/Response, MessagePack framing)
- `src/kitty.rs` — DEPRECATED: KittyController (no longer used by CLI)

## Dependencies
- Session daemon must be running (`thc daemon`)
- `thermal-core` for ClaudeStatePoller (status command)
