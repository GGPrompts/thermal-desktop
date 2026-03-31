# thermal-terminal

Shared platform-agnostic terminal primitives for desktop (thermal-conductor) and Android (thermobile).

## Modules
- **`pty`** — `PtySession`: fork/exec with `nix::pty::openpty()`, blocking reader thread, tokio mpsc output channel. Core type used by thermal-conductor's daemon and window.
- **`input`** — `KeyCode`/`Modifiers` enums + `encode_key()` to convert keyboard events to xterm escape sequences. Platform-agnostic — downstream crates map their native key events to these types.
- **`osc633`** — Stateful byte-stream parser for VS Code shell integration (OSC 633). Tracks command boundaries (prompt/exec/finish), exit codes, command text. Used by thermal-conductor for semantic scrollback.
- **`terminal`** — `TerminalSize` struct satisfying alacritty_terminal's `Dimensions` trait. Deliberately avoids importing alacritty_terminal to stay lightweight.
- **`state_inference`** — `AgentStateInference`: infers agent session state (idle/processing/tool_use/awaiting_input) from PTY output patterns + OSC 633 command tracker transitions. Writes state files to `/tmp/{claude-code,codex,copilot}-state/` in the format consumed by `ClaudeStatePoller`. Replaces external hook scripts for daemon-mode sessions.

## Feature Flags
- `bell_detection` — Adds `Arc<AtomicBool>` bell flag to the event listener. Used by thermobile to poll bell events from JNI.

## Key Infrastructure
- **AgentStateInference** (`src/state_inference.rs`): Keeps a local `StateFile` struct aligned with `SessionStateV1` to avoid pulling `thermal-core` GPU deps. State files include command telemetry: `last_command`, `last_exit_code`, `last_command_started_at`, `last_command_duration_ms`, `consecutive_failures`.
- **Per-session event log** (`src/event_log.rs`): JSONL event log at `/run/user/$UID/thermal/sessions/<id>.events.jsonl`. Captures lifecycle events (Spawn, StatusChange, CommandStart, CommandFinish, Resize, PtyEof, Bell). Truncate-on-overflow rotation (5000 entries).
- **ExitReason** (`src/pty.rs`): Structured session exit enum (PtyEof/Signal/SpawnFailed/FrontendClose/DaemonShutdown). `has_exited()` still works as fast lock-free check.

## Design Constraints
- No GPU, no Wayland, no desktop dependencies — this crate must compile for Android NDK targets.
- `alacritty_terminal` is a downstream concern (thermal-conductor), not a dependency here.
