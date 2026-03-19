# Thermal Desktop

Custom Wayland desktop environment with a thermal/FLIR infrared aesthetic. GPU-accelerated Rust components replacing browser-based dashboards with purpose-built tools for AI-assisted coding.

## Quick Start

```bash
cargo build                    # Build everything
thermal-bar &                  # Status bar (auto-starts at login)
thermal-audio &                # TTS announcements (auto-starts at login)
thermal-launch                 # App launcher (Super+D)
thermal-monitor                # TUI dashboard (run in kitty)
thermal-conductor window       # GPU terminal (standalone mode)
```

## Components

### Always Running (autostart)
These launch automatically at login via Hyprland `exec-once`:

| Component | What It Does |
|-----------|-------------|
| **thermal-bar** | Top status bar — CPU/GPU/mem/net metrics (left), hotkey cheat sheet (center), Claude sessions + clock (right) |
| **thermal-audio** | TTS daemon — announces Claude session state changes (idle, tool use, awaiting input, context warnings) |

### On-Demand
| Component | How to Launch | What It Does |
|-----------|--------------|-------------|
| **thermal-launch** | Super+D | Fuzzy app launcher overlay with thermal components at the top |
| **thermal-monitor** | `thermal-monitor` in kitty, or Super+T | Ratatui TUI showing all Claude sessions with status, context %, tools |
| **thermal-conductor** | `thermal-conductor window` | GPU-rendered terminal with agent overlays (HUD badge, timeline bar) |
| **thermal-hud** | `thermal-hud` | Layer-shell overlay showing Claude session tabs or voice assistant state |
| **thermal-notify** | Runs as D-Bus service | Notification daemon with thermal-styled popups |
| **thermal-lock** | Disabled (NVIDIA) | Lock screen with WGSL heatmap shader + PAM auth |

### CLI Tools
```bash
# Session management
thermal-conductor daemon       # Start session daemon
thermal-conductor spawn        # Spawn a shell session
thermal-conductor spawn -n 3   # Spawn 3 sessions
thermal-conductor list         # List sessions
thermal-conductor status       # Show Claude state for all sessions
thermal-conductor kill ID      # Kill a session

# Audio control
thermal-conductor audio on     # Start TTS daemon
thermal-conductor audio off    # Stop TTS daemon
thermal-conductor audio status # Check if running
thermal-conductor audio test "hello world"  # Test TTS

# MCP server (used by Claude for desktop control)
thermal-commander              # 20 tools: screenshots, window mgmt, app launch, clipboard
```

### Voice Assistant (WIP)
Push-to-talk voice input with Whisper transcription:

```bash
# Install dependencies
pip install faster-whisper sounddevice numpy

# Start daemon
python3 scripts/thermal-voice.py --daemon

# Toggle recording: Super+Backslash
# Transcript is copied to clipboard on stop
```

## Hotkeys

See [docs/HOTKEYS.md](docs/HOTKEYS.md) for the complete reference.

Key ones:
- **Super+D** — App launcher (thermal components listed first)
- **Super+Enter** — New kitty terminal
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
thermal-conductor audio status

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

thermal-bar ──────── layer-shell top bar, 1Hz metrics
thermal-launch ───── layer-shell overlay launcher
thermal-hud ──────── layer-shell HUD (agent tabs + voice)
thermal-lock ─────── session-lock screen
thermal-notify ───── D-Bus notification server
thermal-audio ────── TTS daemon (edge-tts + mpv)
thermal-monitor ──── ratatui TUI dashboard
thermal-conductor ── PTY session daemon + GPU terminal
thermal-commander ── MCP server (20 desktop control tools)
```

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
| Voice state | `/tmp/thermal-voice-state.json` |
| Conductor socket | `/run/user/1000/thermal/conductor.sock` |
| Voice socket | `/run/user/1000/thermal/voice.sock` |
| TTS cache | `~/.cache/thermal-audio/` |
| Hyprland config | `~/.config/hypr/hyprland.conf` |
| Screenshots | `~/Pictures/Screenshots/` |
| Color definitions | `colors/thermal.toml` |
