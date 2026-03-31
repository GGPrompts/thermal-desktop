# thermal-conductor (daemon mode)

Optional PTY session daemon for managing terminal sessions without kitty.

## What it does
Owns PTY sessions via alacritty_terminal, exposing a Unix socket API for the
TUI hub and GPU terminal window. Provides native agent state detection through
the state inference engine (no hook scripts needed). Broadcasts ScreenUpdate
diffs so GPU terminal clients can render remotely.

## Socket / Pidfile
- Socket: `/run/user/$UID/thermal/conductor.sock`
- No pidfile (detected via `pgrep -f "thc daemon"`)

## CLI Usage
```
thc daemon
```
No additional flags. The TUI hub connects automatically when `--backend=daemon`
or `--backend=auto` (fallback when kitty is unavailable).

## Dependencies
- **Needs**: nothing (self-contained PTY management)
- **Needed by**: `thc tui` (when using daemon backend), `thc window` (client mode)

## Troubleshooting
- Stale socket: if the daemon crashes, `conductor.sock` may linger causing `thc window` to enter client mode against a dead socket. Fix: `rm /run/user/$UID/thermal/conductor.sock`
- State files are written to `/tmp/claude-code-state/` by the state inference engine
- Session event logs are stored at `/run/user/$UID/thermal/sessions/<id>.events.jsonl`
