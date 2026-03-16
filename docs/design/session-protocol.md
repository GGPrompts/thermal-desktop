# Session Daemon Protocol — Design Document

**Status:** Draft
**Phase:** 3 (Session Daemon)
**Issue:** therm-5x1d

---

## Overview

Phase 3 introduces a session daemon (`thermal-sessiond`) that owns PTY sessions independently of any window or frontend process. Sessions survive window close, crash, and display reconnection — analogous to a tmux server but purpose-built for the thermal-conductor GPU terminal stack.

This document defines the session state contract, Unix socket wire protocol, D-Bus management interface, and daemon lifecycle. It is the authoritative reference for the three implementation tasks: the daemon itself, the D-Bus API, and frontend reconnection.

---

## 1. Architecture

```
┌─────────────────────────────────────────────────────────┐
│  thermal-sessiond  (system user service, long-lived)     │
│                                                          │
│  SessionRegistry                                         │
│    ├── Session "alpha"  [PtySession + Term + metadata]   │
│    ├── Session "beta"   [PtySession + Term + metadata]   │
│    └── Session "gamma"  [PtySession + Term + metadata]   │
│                                                          │
│  Unix socket:  /run/user/<uid>/thermal/sessiond.sock     │
│  D-Bus object: /org/thermal/conductor  (session bus)     │
└────────────────┬────────────────────────────────────────┘
                 │  Unix socket (screen-update stream)
       ┌─────────┴──────────┐
       │                    │
┌──────┴───────┐    ┌───────┴──────┐
│  Window A    │    │  Window B    │
│  (frontend)  │    │  (frontend)  │
│  attached to │    │  attached to │
│  "alpha"     │    │  "alpha"     │  ← read-only mirror
└──────────────┘    └──────────────┘
```

The daemon is the single PTY owner. Frontends connect over a Unix socket to receive a screen update stream and relay input. The D-Bus interface handles session lifecycle (spawn, list, kill) independently of any attached frontend.

---

## 2. Session State Struct

The daemon tracks one `Session` per PTY. This maps to the current `PtySession` + `Terminal` pair in `crates/thermal-conductor/src/pty.rs` and `terminal.rs`, but owned by the daemon rather than the window.

```rust
pub struct Session {
    // ── Identity ───────────────────────────────────────────────────────────
    /// Stable human-readable name (e.g. "alpha", "beta-2").
    /// Set at spawn time, never changes.
    pub id: SessionId,

    /// Wall-clock time this session was spawned.
    pub created_at: SystemTime,

    // ── PTY ────────────────────────────────────────────────────────────────
    /// Owned PTY master fd + child process management.
    /// Corresponds to the current `PtySession` in pty.rs.
    pub pty: PtySession,

    /// Shell child PID (available from `pty.child_pid()`).
    pub shell_pid: Pid,

    /// Most recently observed working directory of the shell.
    /// Updated by OSC 7 (shell integration) or /proc/<pid>/cwd polling.
    pub cwd: Option<PathBuf>,

    // ── Terminal emulator state ─────────────────────────────────────────────
    /// VT parser + alacritty Term grid, behind FairMutex.
    /// Corresponds to `Terminal` in terminal.rs.
    pub term: Terminal,

    /// Current grid dimensions in cells.
    pub size: TermSize,

    // ── OSC 633 / semantic scrollback ──────────────────────────────────────
    /// Command blocks extracted from OSC 633 shell-integration marks.
    /// Populated by the byte processor (already wired in terminal.rs).
    pub command_tracker: Arc<Mutex<CommandTracker>>,

    // ── Connection tracking ─────────────────────────────────────────────────
    /// Currently attached frontend connections.
    pub frontends: Vec<FrontendHandle>,

    // ── Status ─────────────────────────────────────────────────────────────
    pub state: SessionState,
}

pub enum SessionState {
    /// Shell is running and accepting input.
    Live,
    /// Child process has exited; session is preserved for scrollback.
    /// Attach is still allowed (read-only).
    Exited { code: Option<i32>, at: SystemTime },
    /// Being shut down; no new attaches accepted.
    Dying,
}

/// Grid dimensions.
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
    /// Pixel size of one cell (used for TIOCSWINSZ xpixel/ypixel).
    pub cell_width: u16,
    pub cell_height: u16,
}

/// Opaque stable identifier.
pub type SessionId = Arc<str>;
```

### Scrollback retention

The daemon retains the full `alacritty_terminal::Term` grid (including scrollback) for the lifetime of the session. When a frontend reconnects, it receives a full grid snapshot rather than replaying raw PTY bytes. The daemon does **not** store raw PTY byte history — the Term grid is the canonical representation.

---

## 3. Wire Protocol

### 3.1 Transport

Sessions are accessed over a **Unix domain socket** (SOCK_STREAM). One connection per frontend per session.

Socket path: `/run/user/<uid>/thermal/sessiond.sock`

The socket is created by the daemon at startup and deleted on clean shutdown. On startup, the daemon checks for a stale socket and removes it before binding.

### 3.2 Framing

All messages are length-prefixed binary frames:

```
┌──────────────────────────────────────┐
│  u32 length (little-endian)          │  4 bytes — length of payload
│  u8  message type                    │  1 byte
│  payload bytes                       │  length - 1 bytes
└──────────────────────────────────────┘
```

Payload encoding is **MessagePack** (via the `rmp-serde` crate). MessagePack was chosen over JSON for compactness in high-frequency screen update messages and over custom binary for schema evolution. Both client and daemon must handle unknown fields gracefully (forward compatibility).

### 3.3 Message Types

#### Client → Daemon

| Type byte | Name | Description |
|-----------|------|-------------|
| `0x01` | `Attach` | Attach to a session and begin receiving updates |
| `0x02` | `Detach` | Gracefully detach from a session |
| `0x03` | `Input` | Send bytes to the PTY (keyboard/paste input) |
| `0x04` | `Resize` | Notify the daemon of a window resize |
| `0x05` | `QueryState` | Request a full grid snapshot |
| `0x06` | `SetInputPriority` | Claim or release exclusive write access |

```rust
/// 0x01 — sent as first message after connecting.
struct Attach {
    session_id: String,
    /// Client-advertised initial window size. Daemon uses this if no other
    /// frontend is attached; ignored (but recorded) if a primary is present.
    initial_size: Option<TermSize>,
    /// If true, client requests read-only mode (no input relay).
    read_only: bool,
}

/// 0x03 — relay keyboard or paste input.
struct Input {
    session_id: String,
    bytes: Vec<u8>,
}

/// 0x04 — frontend window resized.
struct Resize {
    session_id: String,
    cols: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
}

/// 0x06 — claim or yield the write seat.
struct SetInputPriority {
    session_id: String,
    /// true = request exclusive input; false = release it.
    exclusive: bool,
}
```

#### Daemon → Client

| Type byte | Name | Description |
|-----------|------|-------------|
| `0x80` | `AttachOk` | Attach accepted; includes full grid snapshot |
| `0x81` | `AttachErr` | Attach rejected (session not found, etc.) |
| `0x82` | `ScreenUpdate` | Incremental dirty-cell update |
| `0x83` | `TitleChange` | Terminal title changed |
| `0x84` | `Bell` | Terminal bell |
| `0x85` | `SessionExited` | Child process exited |
| `0x86` | `Resized` | Daemon applied a resize (echoed to all frontends) |
| `0x87` | `InputPriorityGrant` | Exclusive input granted to this client |
| `0x88` | `InputPriorityRevoke` | Exclusive input revoked (another client claimed it) |

```rust
/// 0x80 — full grid snapshot sent immediately after a successful attach.
/// This is also sent in response to QueryState (0x05).
struct AttachOk {
    session_id: String,
    size: TermSize,
    /// Encoded as a flat list of cells, row-major, top to bottom.
    /// Length must equal size.cols * size.rows.
    cells: Vec<ScreenCell>,
    cursor: CursorState,
    /// OSC 633 command blocks for semantic scrollback overlay.
    command_blocks: Vec<CommandBlock>,
    /// Current window title.
    title: String,
    /// Session wall-clock age in seconds (useful for display).
    age_secs: u64,
}

/// 0x82 — incremental update carrying only cells that changed.
struct ScreenUpdate {
    session_id: String,
    /// Monotonically increasing sequence number. Gaps indicate a missed
    /// update; the client must request a full QueryState to resync.
    seq: u64,
    dirty_cells: Vec<DirtyCell>,
    cursor: CursorState,
}
```

### 3.4 Cell Encoding

```rust
/// A single terminal cell in a full snapshot.
struct ScreenCell {
    /// UTF-8 character. Single space for empty cells.
    ch: char,
    fg: Color,
    bg: Color,
    flags: u16,  // alacritty Flags bitfield (bold, italic, underline, etc.)
}

/// A changed cell in an incremental update.
struct DirtyCell {
    col: u16,
    row: u16,
    cell: ScreenCell,
}

struct CursorState {
    col: u16,
    row: u16,
    /// alacritty CursorStyle integer.
    style: u8,
    /// False when the cursor is hidden (DECTCEM off).
    visible: bool,
}

/// 24-bit RGB color, with a flag for "default" (palette index 0 / bg).
struct Color {
    r: u8,
    g: u8,
    b: u8,
    /// True if this is the terminal default color (transparent/palette).
    is_default: bool,
}
```

---

## 4. Update Granularity

### 4.1 Full snapshot (connect / resync)

On `Attach` or `QueryState`, the daemon sends a complete `AttachOk` containing the entire current grid. This is always correct regardless of when the client connected or how many updates were missed.

### 4.2 Incremental dirty-cell diffs

After a frontend is attached, the daemon sends `ScreenUpdate` messages driven by the `alacritty_terminal::Term` damage tracking API (`TermDamage`). The current `window.rs` already queries `term.damage()` after each render frame; the daemon adopts the same pattern.

Damage accumulation loop (daemon side, per session):

1. The byte processor task (already in `terminal.rs`) sets `pty_dirty = true` after each batch.
2. A per-session update task wakes on `pty_dirty`, locks the Term, calls `term.damage()`, and builds `DirtyCell` list.
3. The update is broadcast to all attached frontends with an incrementing `seq`.
4. `term.reset_damage()` is called after the broadcast.

**Sequence gaps:** If the client detects a gap in `seq` (e.g. seq 42 arrives after seq 40 with no 41), it issues a `QueryState` to resync. This handles any future buffering or partial-send edge cases.

**Burst batching:** The byte processor already batches all pending PTY chunks before processing (see `terminal.rs` `spawn_byte_processor`). The daemon inherits this — a single `ScreenUpdate` covers all cells dirtied by a burst of output, not one message per byte.

---

## 5. Multi-Frontend: Two Windows on the Same Session

Two frontends may attach to the same session simultaneously. The use case is a mirror view (e.g. the status bar showing a thumbnail) or a second developer window.

### 5.1 Input priority

The daemon tracks an optional "primary" frontend per session — the one whose `Resize` events and `Input` messages are honored. Rules:

- The **first** frontend to attach becomes primary by default.
- A frontend may call `SetInputPriority { exclusive: true }` to steal primary status. The daemon sends `InputPriorityRevoke` to the previous primary.
- When the primary detaches, the next attached frontend (by attach time) becomes primary automatically.
- A frontend that is not primary may still send `Input`; the daemon silently drops it unless no primary exists.
- A frontend in `read_only: true` mode never receives primary status, even if it is the only connection.

### 5.2 Resize negotiation

Only the primary frontend's `Resize` messages are applied to the PTY via `TIOCSWINSZ`. All frontends receive the resulting `Resized` echo and must adapt their render viewport. A secondary frontend that is larger than the session size crops or letterboxes; it does not force a resize.

### 5.3 Broadcast

`ScreenUpdate`, `TitleChange`, `Bell`, and `SessionExited` messages are broadcast to all attached frontends simultaneously. The daemon sends them on each frontend's write channel independently; a slow frontend's backpressure does not block others (use a bounded async channel per frontend; if the channel is full, drop the update and force a `QueryState` on the next write attempt).

---

## 6. Reconnection Contract

When a frontend reconnects after a disconnect:

1. Frontend opens a new Unix socket connection.
2. Sends `Attach { session_id, initial_size, read_only }`.
3. Daemon responds with `AttachOk` containing:
   - Full current grid snapshot (all cells, current dimensions).
   - Current cursor position and style.
   - Full `command_blocks` list (OSC 633 semantic scrollback).
   - Current window title.
   - `age_secs` — how long the session has been alive.
4. Daemon assigns input priority per the rules in §5.1.
5. Incremental `ScreenUpdate` messages resume from the current `seq`.

The client does **not** need to remember any prior `seq` across reconnects. The full snapshot in `AttachOk` is always current. The `seq` counter restarts from the daemon's current value after the snapshot; the client treats the snapshot as ground truth and uses subsequent `seq` only for gap detection going forward.

### Stale session detection

If the client requests a `session_id` that does not exist, the daemon returns `AttachErr { reason: "not_found" }`. The client should fall back to D-Bus `List` to discover live sessions and offer to spawn a new one.

---

## 7. D-Bus Interface: `org.thermal.Conductor`

The D-Bus interface handles session lifecycle. It is implemented by `thermal-sessiond` on the session bus and used by `thc` CLI commands and other thermal components (thermal-bar, thermal-monitor).

This extends the existing stub in `crates/thermal-bar/src/dbus.rs`, which already defines a `ConductorProxy` with `panes()` and `get_agent_state()`. The Phase 3 interface is a superset.

### 7.1 Interface definition (zbus IDL style)

```
interface org.thermal.Conductor {

    // ── Session lifecycle ─────────────────────────────────────────────────

    /// Spawn a new session. Returns the new session ID.
    /// shell: shell binary path, or "" to use $SHELL.
    /// cwd: initial working directory, or "" for $HOME.
    /// name: desired human-readable name, or "" for auto-assignment.
    Spawn(shell: s, cwd: s, name: s) -> (session_id: s)

    /// Kill a session (sends SIGHUP to the PTY child).
    Kill(session_id: s)

    /// List all live sessions.
    List() -> (sessions: a{s(sstu)})
    //  Map<session_id, (state_str, cwd, shell_pid, age_secs)>

    // ── Per-session queries ───────────────────────────────────────────────

    /// Get the agent state string for a session ("idle", "running", etc.).
    /// Reads from /tmp/claude-code-state/ via ClaudeStatePoller — the same
    /// mechanism thermal-bar already uses.
    GetAgentState(session_id: s) -> (state: s)

    /// Get the socket path the frontend should connect to for this session.
    /// Always "/run/user/<uid>/thermal/sessiond.sock" — included for forward
    /// compatibility (e.g. if sessions are ever proxied over TCP).
    GetSocketPath(session_id: s) -> (path: s)

    // ── Properties ───────────────────────────────────────────────────────

    /// All active pane IDs (backward-compatible with the existing ConductorProxy).
    @property
    Panes -> as

    // ── Signals ──────────────────────────────────────────────────────────

    /// Emitted when a session is spawned.
    signal SessionSpawned(session_id: s, name: s)

    /// Emitted when a session's state changes (idle → running → idle, etc.).
    signal SessionStateChanged(session_id: s, new_state: s)

    /// Emitted when a session exits (child process ended).
    signal SessionExited(session_id: s, exit_code: i)
}
```

D-Bus object path: `/org/thermal/conductor`
Service name: `org.thermal.Conductor`

### 7.2 Backward compatibility

The existing `ConductorProxy` in thermal-bar uses `panes()` (property) and `get_agent_state(pane_id)`. Phase 3 must keep these working. Session IDs serve as pane IDs — the mapping is 1:1.

### 7.3 zbus implementation note

Implement using `zbus::interface` macro (async, tokio). The `thermal-sessiond` binary owns the name `org.thermal.Conductor` on the session bus. When the daemon is not running, thermal-bar's `ConductorClient` already handles the `None` connection case gracefully (returns empty results).

---

## 8. Daemon Lifecycle

### 8.1 systemd user service

File: `~/.config/systemd/user/thermal-sessiond.service`

```ini
[Unit]
Description=Thermal session daemon
PartOf=graphical-session.target
After=graphical-session.target

[Service]
ExecStart=%h/.cargo/bin/thermal-sessiond
Restart=on-failure
RestartSec=2s
# Propagate the Wayland environment so child shells inherit WAYLAND_DISPLAY etc.
Environment="PATH=/usr/bin:/bin:%h/.cargo/bin"

[Install]
WantedBy=graphical-session.target
```

Enable with: `systemctl --user enable --now thermal-sessiond`

### 8.2 Auto-start via D-Bus activation (optional)

Add a D-Bus service file so the daemon starts on demand when any client calls `org.thermal.Conductor`:

File: `~/.local/share/dbus-1/services/org.thermal.Conductor.service`

```ini
[D-BUS Service]
Name=org.thermal.Conductor
Exec=/home/builder/.cargo/bin/thermal-sessiond
```

This allows `thc spawn` to work even if the daemon is not yet running, at the cost of a ~100ms startup delay on first call.

### 8.3 Startup sequence

1. Parse config (socket path, default shell, scrollback limit).
2. Remove stale socket file if present.
3. Bind Unix socket at `/run/user/<uid>/thermal/sessiond.sock`.
4. Register D-Bus name `org.thermal.Conductor`.
5. Restore sessions from state file (see §8.5) if present.
6. Enter tokio event loop: accept connections, handle D-Bus calls.

### 8.4 Graceful shutdown

On `SIGTERM`:

1. Stop accepting new connections.
2. Broadcast `SessionExited` (with no exit code) to all frontends, then close their sockets.
3. Write session state snapshot to disk (see §8.5).
4. Send SIGHUP to all PTY children (same behavior as the current `PtySession::drop`).
5. Release D-Bus name.
6. Remove socket file.
7. Exit.

SIGKILL is unrecoverable; sessions are lost. PTY children will receive SIGHUP from the kernel when the master fd closes.

### 8.5 State persistence

The daemon writes a snapshot to `/run/user/<uid>/thermal/sessiond-state.json` on clean shutdown. On restart, it attempts to re-adopt any PTY children that are still alive by re-opening `/proc/<pid>/fd/<master_fd_num>` (Linux-specific). If re-adoption fails (child exited, fd gone), the session is dropped.

Persisted state per session (JSON):

```json
{
  "id": "alpha",
  "created_at": 1710000000,
  "shell_pid": 12345,
  "master_fd_path": "/proc/12345/fd/5",
  "size": { "cols": 220, "rows": 50, "cell_width": 8, "cell_height": 16 },
  "title": "~/projects/thermal-desktop — zsh"
}
```

The Term grid is **not** persisted; on re-adoption the daemon sends a `\x1b[2J\x1b[H` (clear + home) to the PTY to trigger a redraw by the running shell/app, then lets normal output rebuild the grid.

---

## 9. Design Decisions and Rationale

### Why Unix socket + MessagePack, not D-Bus for screen updates?

D-Bus is well-suited for infrequent method calls and signals (session lifecycle, state queries). It is not designed for high-frequency streaming — screen updates during TUI app output can arrive at 60+ Hz with thousands of dirty cells. A raw Unix socket with a compact binary encoding avoids D-Bus's per-message overhead and GLib event loop assumptions.

### Why not replay raw PTY bytes on reconnect?

Replaying raw bytes requires storing an unbounded history and re-running the VTE parser on reconnect. The Term grid is already the fully-parsed, canonical representation. Snapshotting it is O(cols × rows) and produces a deterministic result regardless of how many bytes were consumed to reach that state.

### Why alacritty_terminal damage tracking instead of full snapshots on every update?

The Term grid for a 220×50 terminal is 11,000 cells. At 60 Hz that is 660,000 cells/sec over the socket per attached frontend. Dirty-cell diffs typically cover <5% of cells during normal interactive use, reducing bandwidth by 20x.

### Why is the daemon a separate binary, not a library linked into the window?

The core requirement of Phase 3 is session survival across window close. The only way to survive window close is to not be in the same process as the window. A separate binary + IPC is the minimal correct architecture — the same conclusion tmux, zellij, and kitty (in persistent mode) all reached.

### Input priority design

"First attacher is primary" with an explicit steal mechanism is borrowed from tmux's session model. It is simple to reason about and avoids deadlocks. The read-only flag allows monitoring tools (thermal-bar thumbnails, thermal-monitor) to attach without accidentally stealing input focus.

---

## 10. Reference: Related Art

| System | PTY ownership | Protocol | Reconnect | Notes |
|--------|--------------|----------|-----------|-------|
| **tmux** | server process | custom binary over Unix socket | full grid redraw via `\033[H` + output replay | client sends `%begin`/`%end`-delimited control messages |
| **zellij** | server process | custom msgpack over Unix socket | full layout + cell snapshot | has first-class multi-user; input routed by focused pane |
| **kitty** | window process | `kitty @` remote control JSON | none (window close kills session) | Phase 2 reference for our current thc CLI |
| **screen** | daemon process | custom pty-replay over socket | raw byte replay from internal ring buffer | legacy; byte replay is the approach we explicitly avoid |

The thermal-sessiond design is closest to zellij: daemon-owned PTYs, MessagePack frames, cell-snapshot reconnect. It omits zellij's layout engine (each session is a single PTY) and multi-user access control (single-user desktop daemon).
