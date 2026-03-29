# thermal-terminal

Shared platform-agnostic terminal primitives for desktop (thermal-conductor) and Android (thermobile).

## Modules
- **`pty`** — `PtySession`: fork/exec with `nix::pty::openpty()`, blocking reader thread, tokio mpsc output channel. Core type used by thermal-conductor's daemon and window.
- **`input`** — `KeyCode`/`Modifiers` enums + `encode_key()` to convert keyboard events to xterm escape sequences. Platform-agnostic — downstream crates map their native key events to these types.
- **`osc633`** — Stateful byte-stream parser for VS Code shell integration (OSC 633). Tracks command boundaries (prompt/exec/finish), exit codes, command text. Used by thermal-conductor for semantic scrollback.
- **`terminal`** — `TerminalSize` struct satisfying alacritty_terminal's `Dimensions` trait. Deliberately avoids importing alacritty_terminal to stay lightweight.

## Feature Flags
- `bell_detection` — Adds `Arc<AtomicBool>` bell flag to the event listener. Used by thermobile to poll bell events from JNI.

## Design Constraints
- No GPU, no Wayland, no desktop dependencies — this crate must compile for Android NDK targets.
- `alacritty_terminal` is a downstream concern (thermal-conductor), not a dependency here.
