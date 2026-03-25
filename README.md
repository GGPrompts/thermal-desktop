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
| **thermal-bar** | Top status bar — CPU/GPU/mem/net metrics (left), hotkey cheat sheet (center), Claude sessions + clock (right) |
| **thermal-audio** | TTS daemon — announces Claude session state changes (idle, tool use, awaiting input, context warnings) |
| **thc daemon** | Session daemon — owns PTY sessions, provides Unix socket API for the TUI and other clients |

### On-Demand
| Component | How to Launch | What It Does |
|-----------|--------------|-------------|
| **thermal-launch** | Super+D | Fuzzy app launcher overlay with thermal components at the top |
| **thermal-monitor** | `thermal-monitor` in kitty | Standalone ratatui TUI showing all Claude sessions with subagent nesting, context %, tools |
| **thermal-conductor** | `thc` or `thc tui` | Tabbed ratatui TUI dashboard — Sessions, Spawn, Profiles, Services tabs |
| **thermal-conductor** | `thermal-conductor window` | GPU-rendered terminal with agent overlays (HUD badge, timeline bar) |
| **thermal-hud** | `thermal-hud` | Layer-shell overlay showing Claude session tabs or voice assistant state |
| **thermal-notify** | Runs as D-Bus service | Notification daemon with thermal-styled popups |
| **thermal-screensaver** | `thermal-screensaver` | Idle-triggered thermal fluid simulation overlay (reaction-diffusion shader) |
| **thermal-wallpaper** | `thermal-wallpaper` | Animated WGSL thermal shader wallpaper — heat field modulated by system metrics |
| **thermal-lock** | Disabled (NVIDIA) | Lock screen with WGSL heatmap shader + PAM auth |

### CLI Tools
```bash
# Interactive TUI dashboard (default when no subcommand given)
thc                            # Launch tabbed TUI (Sessions/Spawn/Profiles/Services)
thc tui                        # Same, explicit subcommand

# Session management
thc daemon                     # Start session daemon
thc spawn                      # Spawn a shell session
thc spawn -n 3                 # Spawn 3 sessions
thc list                       # List sessions
thc status                     # Show Claude state for all sessions
thc kill ID                    # Kill a session

# Audio control
thc audio on                   # Start TTS daemon
thc audio off                  # Stop TTS daemon
thc audio status               # Check if running
thc audio test "hello world"   # Test TTS

# MCP server (used by Claude for desktop control)
thermal-commander              # 20 tools: screenshots, window mgmt, app launch, clipboard
```

### Voice Assistant Pipeline
Push-to-talk voice input with Whisper transcription, routed through Claude Haiku for tool execution:

```
Voice (Super+\) → thermal-voice (Whisper STT) → thermal-dispatcher (Haiku API)
    → tool execution (trust-tier gated) → thermal-audio (TTS response)
```

```bash
# Install dependencies
pip install faster-whisper sounddevice numpy

# Start daemons
python3 scripts/thermal-voice.py --daemon   # STT daemon (voice.sock)
thermal-dispatcher &                         # Command routing (dispatcher.sock)
thermal-audio &                              # TTS playback (audio.sock)

# Toggle recording: Super+Backslash
```

**Trust tiers** (`config/trust-tiers.toml`):
- **AUTO** — safe read-only tools execute immediately (screenshot, clipboard, beads)
- **CONFIRM** — desktop interaction tools require HUD confirmation (click, type, open_app)
- **BLOCK** — destructive tools are rejected with TTS announcement (kill_claude)

### Spawn Profiles
The TUI Spawn page loads profiles from `config/profiles.toml` (or `~/.config/thermal/profiles.toml`):

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
- **Super+T** — thermal-status readout
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
  ├── ClaudeStatePoller (/tmp/claude-code-state/)
  ├── WgpuContext (GPU device factory)
  └── ThermalTextRenderer (glyphon + fonts)

thermal-bar ──────────── layer-shell top bar, 1Hz metrics
thermal-launch ───────── layer-shell overlay launcher
thermal-hud ──────────── layer-shell HUD (agent tabs + voice state)
thermal-lock ─────────── session-lock screen
thermal-notify ───────── D-Bus notification server
thermal-audio ────────── TTS daemon (edge-tts + mpv, Unix socket API)
thermal-voice ────────── push-to-talk STT daemon (cpal + Whisper)
thermal-monitor ──────── standalone ratatui TUI dashboard
thermal-conductor ────── tabbed TUI hub + PTY session daemon + GPU terminal
thermal-commander ────── MCP server (20 desktop control tools)
thermal-dispatcher ───── voice command router (Whisper → Haiku → tool execution)
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
| Subagent state | `/tmp/claude-code-state/{session}.agent.{agent_id}.json` |
| Voice state | `/tmp/thermal-voice-state.json` |
| HUD state | `/tmp/thermal-hud-state.json` |
| Conductor socket | `/run/user/1000/thermal/conductor.sock` |
| Voice socket | `/run/user/1000/thermal/voice.sock` |
| Dispatcher socket | `/run/user/1000/thermal/dispatcher.sock` |
| Audio socket | `/run/user/1000/thermal/audio.sock` |
| TTS cache | `~/.cache/thermal-audio/` |
| Spawn profiles | `config/profiles.toml` or `~/.config/thermal/profiles.toml` |
| Trust tiers | `config/trust-tiers.toml` or `~/.config/thermal/trust-tiers.toml` |
| Hyprland config | `~/.config/hypr/hyprland.conf` |
| Screenshots | `~/Pictures/Screenshots/` |
| Color definitions | `colors/thermal.toml` |
