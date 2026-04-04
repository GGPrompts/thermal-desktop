# thermal-runtime

Runtime path, socket, and pidfile helpers for thermal daemons. No GPU dependencies.

## What This Contains
- `runtime_dir()` — XDG runtime dir for thermal sockets/pidfiles
- `socket_path(name)` / `pidfile_path(name)` — canonical path construction
- `cleanup_stale_socket()` — remove sockets from dead daemons
- `validate_pidfile()` / `write_pidfile()` — PID liveness checking
- `enforce_single_instance()` — pidfile-based single-instance guard (legacy)
- `acquire_instance_lock()` — flock-based single-instance guard (preferred, atomic, no TOCTOU race)
- `try_connect_or_cleanup()` — connect to socket, clean up if stale

## Dependencies
Intentionally minimal: `nix` (Unix APIs), `tracing`. No GPU, no async runtime required.

## Who Depends on This
- `thermal-core` (re-exports for backward compat)
- Daemon crates that need socket/pidfile management
