# Thermal Desktop

Custom Wayland desktop environment with a thermal/FLIR infrared aesthetic. GPU-accelerated Rust components replacing browser-based dashboards with purpose-built tools for AI-assisted coding.

## Quick Start

```bash
cargo build                    # Build everything
thermal-bar &                  # Status bar (auto-starts at login)
thermal-audio &                # TTS announcements (auto-starts at login)
thermal-launch                 # App launcher (Super+D)
thermal-monitor                # TUI dashboard (standalone)
thc                            # TUI dashboard (full-featured, with spawn profiles)
thc tui                        # Same as above, explicit subcommand
thermal-conductor window       # GPU terminal (standalone mode)
```

## Components

### Always Running (autostart)
These launch automatically at login via Hyprland `exec-once`:

| Component | What It Does |
|-----------|-------------|
| **thermal-bar** | Top status bar — CPU/GPU/mem/net metrics (left), workspace map (center), agent sessions + clock (right) |
| **thermal-audio** | TTS daemon — announces Claude session state changes (idle, tool use, awaiting input, context warnings) |
| **codex-state-adapter** | Codex session tracker — mirrors `~/.codex/sessions` into `/tmp/codex-state/` for bar/TUI/audio |
| **thc daemon** | Optional session daemon — PTY backend fallback when kitty is unavailable |

### On-Demand
| Component | How to Launch | What It Does |
|-----------|--------------|-------------|
| **thermal-launch** | Super+D | Fuzzy app launcher overlay with thermal components at the top |
| **thermal-monitor** | `thermal-monitor` in kitty | Standalone ratatui TUI showing all agent sessions (Claude/Codex/Copilot) with subagent nesting, context %, tools |
| **thermal-conductor** | `thc` or Super+T | Tabbed ratatui TUI dashboard — Sessions (with timeline bars), Profiles (Launch/Edit sub-modes), Services (with audio mute/volume) |
| **thermal-conductor** | `thermal-conductor window` | GPU-rendered terminal with agent overlays (HUD badge, timeline bar) |
| **thermal-hud** | `thermal-hud` | Layer-shell overlay showing Claude session tabs or voice assistant state |
| **thermal-notify** | Runs as D-Bus service | Notification daemon with thermal-styled popups |
| **thermal-screensaver** | `thermal-screensaver` | Idle-triggered thermal fluid simulation overlay (reaction-diffusion shader) |
| **thermal-wallpaper** | `thermal-wallpaper` | Animated WGSL thermal shader wallpaper — heat field modulated by system metrics |
| **thermal-lock** | Disabled (NVIDIA) | Lock screen with WGSL heatmap shader + PAM auth |

### CLI Tools
```bash
# Interactive TUI dashboard (default when no subcommand given)
thc                            # Launch tabbed TUI (Sessions/Profiles/Services)
thc tui                        # Same, explicit subcommand
thc --backend=kitty            # Force kitty backend
thc --backend=daemon           # Force daemon backend

# Session management (via kitty @ or daemon fallback)
thc spawn                      # Spawn a shell session in kitty
thc spawn -n 3                 # Spawn 3 sessions
thc list                       # List sessions (kitty windows or daemon PTYs)
thc kill ID                    # Kill a session
thc send ID "text"             # Send text to a session

# Audio control (via socket API)
# Use thc Services tab for mute/volume (m/+/- keys), or direct:
echo '{"action":"toggle_mute"}' | socat - UNIX:/run/user/$UID/thermal/audio.sock
echo '{"action":"set_volume","value":0.7}' | socat - UNIX:/run/user/$UID/thermal/audio.sock
thermal-audio --test "hello"   # Test TTS

# MCP server (used by Claude for desktop control)
thermal-commander              # 20 tools: screenshots, window mgmt, app launch, clipboard
```

### Voice Assistant Pipeline
Push-to-talk voice input with local Whisper transcription and AI dispatch:

```
Super+\ → thermal-voice (cpal mic capture → Whisper STT) → claude -p (dispatch) → tool execution
```

Also supports typing transcripts at cursor via `wtype` and code word commands.

```bash
# Start voice daemon
thermal-voice &                  # Listens on voice.sock

# Toggle recording: Super+Backslash (auto-starts daemon if needed)
```

### Spawn Profiles
The TUI Profiles tab (Launch sub-mode) loads profiles from `config/profiles.toml` (or `~/.config/thermal/profiles.toml`):

```toml
[[profile]]
name = "thermal-desktop"
icon = "🔥"
cwd = "~/projects/thermal-desktop"

[[profile]]
name = "thermobile"
icon = "📱"
cwd = "~/projects/thermobile"

[[profile]]
name = "Shell"
icon = "🖥️"
command = ""
```

Blank fields inherit from the profile, then `default_cwd`, then the directory `thc` was launched from.

## Hotkeys

See [docs/HOTKEYS.md](docs/HOTKEYS.md) for the complete reference.

Key ones:
- **Super+D** — App launcher (thermal components listed first)
- **Super+Enter** — New kitty terminal
- **Super+Shift+Enter** — New GPU terminal (thermal-conductor window)
- **Super+Q** — Close window
- **Super+\\** — Push-to-talk voice input
- **Super+B** — btop system monitor
- **Super+T** — TUI Hub (thc — sessions, profiles, services)
- **Super+N** — Notification center
- **Print** — Screenshot region select

## Troubleshooting

### thermal-bar disappeared
```bash
pkill -x thermal-bar; thermal-bar &
```

### thermal-audio not speaking
```bash
# Check if running
thc audio status

# Restart
pkill -x thermal-audio; thermal-audio &

# Test directly
thermal-audio --test "testing one two three"

# Test the underlying pipeline
edge-tts --text "hello" --write-media /tmp/test.mp3 && mpv --no-video /tmp/test.mp3
```

### Codex sessions not appearing
```bash
# Check if the adapter is running
thc                         # Services tab -> codex-state-adapter

# Start it manually if needed
/home/builder/projects/thermal-desktop/scripts/codex-state-adapter.sh --daemon
```

### thermal-launch won't open (Super+D)
```bash
# Check if binary is installed
which thermal-launch

# Reinstall
cargo install --path crates/thermal-launch --force

# Test directly
thermal-launch
```

### thermal-conductor window shows black/purple grid
The GPU terminal works in **standalone mode** (no daemon). If the daemon is running, screen streaming from daemon to window is not yet implemented — kill the daemon first:
```bash
pkill -f "thermal-conductor.*daemon"
rm -f /run/user/1000/thermal/conductor.sock
thermal-conductor window
```

### Duplicate instances
```bash
# Kill all instances of a component
pkill -9 -x thermal-bar
pkill -9 -x thermal-audio
pkill -9 -x thermal-hud
```

## Architecture

```
thermal-core (shared library)
  ├── ThermalPalette (18 colors)
  ├── ClaudeStatePoller (/tmp/claude-code-state/, /tmp/codex-state/, /tmp/copilot-state/)
  ├── WgpuContext (GPU device factory)
  └── ThermalTextRenderer (glyphon + fonts)

thermal-terminal (shared library)
  ├── OSC 633 shell-integration parser
  ├── Input encoding (KeyCode → PTY bytes)
  ├── PtySession (fork/exec + reader thread)
  └── TerminalSize (alacritty_terminal Dimensions)

thermal-bar ──────────── layer-shell top bar, 1Hz metrics
thermal-launch ───────── layer-shell overlay launcher
thermal-hud ──────────── layer-shell HUD (agent tabs + voice state)
thermal-lock ─────────── session-lock screen
thermal-notify ───────── D-Bus notification server
thermal-audio ────────── TTS daemon (edge-tts + mpv, Unix socket API)
thermal-voice ────────── push-to-talk STT daemon (cpal + Whisper)
thermal-monitor ──────── standalone ratatui TUI dashboard
thermal-conductor ────── tabbed TUI hub (kitty backend + daemon fallback) + GPU terminal
thermal-commander ────── MCP server (20 desktop control tools)
thermal-dispatcher ───── voice command router (dispatches via claude -p)
thermal-screensaver ──── idle-triggered thermal fluid simulation overlay
thermal-wallpaper ────── animated WGSL thermal shader wallpaper daemon
```

### State & IPC

Claude Code hooks (`~/.claude/hooks/state-tracker.sh`) write session state to `/tmp/claude-code-state/`. Subagent tool events are routed to separate files (`{session_id}.agent.{agent_id}.json`) so monitors can nest them under the parent session.

All GUI components use **wgpu** for GPU rendering and **smithay-client-toolkit** for Wayland layer-shell surfaces. Text rendering via **glyphon** (cosmic-text + swash).

## Color Palette

All colors defined in `thermal-core/src/palette.rs` and `colors/thermal.toml`. The `scripts/generate-theme.py` script propagates colors to all config files (kitty, hyprland, starship).

```bash
python3 scripts/generate-theme.py          # Apply colors
python3 scripts/generate-theme.py --check  # Verify in sync
```

## Hardware Setup

- **Ultrawide**: 3440x1440 @ 100Hz (DP-1, primary)
- **Portrait**: 1920x1080 @ 240Hz (HDMI-A-2, rotated)
- **GPU**: NVIDIA (Vulkan backend for wgpu)

## File Locations

| What | Where |
|------|-------|
| Claude session state | `/tmp/claude-code-state/*.json` |
| Codex session state | `/tmp/codex-state/*.json` |
| Copilot session state | `/tmp/copilot-state/*.json` |
| Subagent state | `/tmp/claude-code-state/{session}.agent.{agent_id}.json` |
| Voice state | `/tmp/thermal-voice-state.json` |
| HUD state | `/tmp/thermal-hud-state.json` |
| Conductor socket | `/run/user/1000/thermal/conductor.sock` |
| Voice socket | `/run/user/1000/thermal/voice.sock` |
| Dispatcher socket | `/run/user/1000/thermal/dispatcher.sock` |
| Audio socket | `/run/user/1000/thermal/audio.sock` |
| TTS cache | `~/.cache/thermal-audio/` |
| Audio settings (mute/vol) | `~/.config/thermal/audio.toml` |
| Spawn profiles | `config/profiles.toml` or `~/.config/thermal/profiles.toml` |
| Kitty sessions sidecar | `/run/user/1000/thermal/sessions.json` |
| Hyprland config | `~/.config/hypr/hyprland.conf` |
| Screenshots | `~/Pictures/Screenshots/` |
| Color definitions | `colors/thermal.toml` |
