# thermal-conductor

CLI tool for orchestrating Claude agent **therminals** via kitty remote control.

## What This Does
Spawns, tracks, polls, and sends to kitty terminal windows running Claude sessions. Hyprland auto-tiles the spawned windows. No terminal rendering, no tmux — just `kitty @` commands wrapped in a clean CLI.

## Usage
```bash
thc spawn                          # Spawn 1 therminal
thc spawn -n 4 -p ~/projects/foo   # Spawn 4 in a project dir
thc status                         # Show all therminals with Claude state
thc list                           # Compact window list
thc list --json                    # Raw kitty JSON (verbose)
thc send <window-id> "fix the bug" # Send text to a therminal
thc kill <window-id>               # Close a therminal
```

## Architecture
```
thc (CLI tool)
  ↕ kitty @ launch / get-text / send-text / ls / close-window
kitty windows (GPU terminal rendering)
  ↕ Hyprland (auto-tiling window management)
```

## Key Files
- `src/main.rs` — clap CLI, subcommand dispatch
- `src/kitty.rs` — KittyController wrapping `kitty @` commands

## Dependencies
- `kitty` with `allow_remote_control yes` and `listen_on unix:/tmp/kitty-{kitty_pid}`
- `thermal-core` for ClaudeStatePoller (status command)
