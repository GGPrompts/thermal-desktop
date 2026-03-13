# thermal-conductor

The centerpiece — a native Wayland GPU-rendered agent dashboard.

## What This Does
A wall of terminal panes rendered with wgpu. Each pane runs a Claude agent session. Thermal state indicators show agent status. Click-to-focus sidebar. Native PipeWire audio cues. Git diff awareness.

## Architecture: Hybrid tmux + GPU Rendering

### Phase 1: tmux backend (v0.1 — get something working fast)
thermal-conductor acts as a **GPU-rendered tmux frontend**. tmux handles all the hard stuff (PTY management, scrollback, session persistence, input routing). thermal-conductor handles rendering and state tracking.

```
┌─────────────────────────────────────────────────┐
│  thermal-conductor (Wayland/wgpu)               │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐       │
│  │  Pane 0  │ │  Pane 1  │ │  Pane 2  │  ...  │
│  │ (GPU)    │ │ (GPU)    │ │ (GPU)    │       │
│  └──────────┘ └──────────┘ └──────────┘       │
│       ↕              ↕            ↕             │
│  tmux capture-pane   tmux capture  tmux capture │
│       ↕              ↕            ↕             │
│  ┌──────────────────────────────────────┐      │
│  │  tmux server (manages PTYs)          │      │
│  │  session: "thermal-conductor"        │      │
│  │  window 0: pane 0, 1, 2, ...        │      │
│  └──────────────────────────────────────┘      │
└─────────────────────────────────────────────────┘
```

**How it works:**
- On startup, create a tmux session `thermal-conductor` with N panes
- Poll `tmux capture-pane -t %N -p -e` (with ANSI escapes) at ~60fps per active pane
- Parse ANSI output and render with wgpu + glyphon
- Send user input via `tmux send-keys -t %N`
- Scrollback: `tmux capture-pane -t %N -p -S -1000`
- Resize: `tmux resize-pane -t %N -x W -y H`

**What you get immediately:**
- Session persistence (tmux sessions survive conductor crashes/restarts)
- All tmux features work (send-keys, capture-pane, scripting)
- Existing tmux muscle memory and scripts still work
- Can fall back to raw `tmux attach` if conductor has issues
- Focus on the GPU rendering + thermal aesthetic, not PTY plumbing

**What you defer:**
- Direct PTY management (Phase 2)
- Sub-millisecond latency (tmux IPC adds ~1-2ms, acceptable for v0.1)

### Phase 2: Direct PTY management (v0.2+ — replace tmux internals)
Gradually replace tmux with direct PTY management via nix crate:
- Spawn PTYs directly with `nix::pty::openpty()`
- epoll multiplexing for I/O
- alacritty_terminal for ANSI parsing + Grid state
- Keep tmux-compatible D-Bus API so scripts don't break

### Phase 3: Full native (v1.0)
- Custom scrollback buffer
- Session daemon (PTY server survives UI restarts, like WezTerm's mux)
- Native input handling
- Zero external dependencies for core functionality

## D-Bus API: org.thermal.Conductor

The control interface — replaces `tmux send-keys` and friends:

```
Interface: org.thermal.Conductor
Path: /org/thermal/conductor

Methods:
  CreatePane(command: str) → pane_id: str
  DestroyPane(pane_id: str)
  SendKeys(pane_id: str, keys: str)
  GetPaneContent(pane_id: str, lines: i32) → content: str
  FocusPane(pane_id: str)
  SetLayout(layout: str)  # "grid", "sidebar", "stack"
  GetAgentState(pane_id: str) → state: str

Signals:
  PaneCreated(pane_id: str)
  PaneDestroyed(pane_id: str)
  AgentStateChanged(pane_id: str, old_state: str, new_state: str)
  OutputReceived(pane_id: str, lines: i32)

Properties:
  Panes: array of pane_id
  ActivePane: str
  Layout: str
```

**CLI wrapper for scripting:**
```bash
# These feel like tmux commands but go through D-Bus
thermal-ctl create "claude --print-me"
thermal-ctl send pane-0 "build the thing"
thermal-ctl focus pane-2
thermal-ctl layout sidebar
thermal-ctl state pane-0  # → "running"
```

## Agent State Detection

How thermal-conductor knows what state an agent is in:

1. **Output pattern matching** — watch PTY output for patterns:
   - `$` or `❯` prompt visible → idle (blue)
   - Streaming output → running (green)
   - `error` / `Error` / exit code != 0 → error (red)
   - No output for 30s+ → waiting (yellow)

2. **Claude hooks integration** — watch hook output files:
   - `~/.claude/hooks/` state files
   - notify crate watches for changes
   - Direct state updates without guessing

3. **D-Bus signals** — external tools can push state:
   - `thermal-ctl set-state pane-0 running`

## Agent State Colors
- Blue (#1e3a8a): idle/waiting for input
- Green (#22c55e): running/active output
- Yellow (#eab308): producing results / thinking
- Orange (#f97316): warning
- Red (#ef4444): error/failed
- White-hot (#fef3c7): completing successfully (flash)

## Key References
- **tmux source** — `cmd-capture-pane.c`, `tty.c` for understanding tmux's internals
- **Alacritty** — alacritty_terminal crate for Grid<Cell> terminal state (Phase 2)
- **WezTerm mux** — architecture for PTY server + GUI client separation (Phase 3)
- **Rio terminal** — wgpu + cosmic-text rendering pipeline
- **Zed GPUI terminal** — embedding alacritty_terminal in GPU app

## Development
```bash
cargo run -p thermal-conductor
```
