# Thermal Desktop

Custom Wayland desktop environment with a thermal/FLIR infrared aesthetic. GPU-accelerated Rust components replacing browser-based dashboards with purpose-built tools for AI-assisted coding.

## Quick Start

```bash
cargo install --path crates/thermal-conductor  # TUI + daemon + GPU terminal + bar + HUD
cargo install --path crates/thermal-audio       # TTS + voice capture
cargo install --path crates/thermal-dispatcher  # AI command routing

thc                            # TUI dashboard (Super+T)
thc window                     # GPU terminal (Super+Enter)
thc daemon                     # Session daemon (auto-starts via systemd)
```

## Architecture (3 Daemons)

Consolidated from 9 daemons into 3. Bar, HUD, and message bus are now in-process conductor modules.

| Daemon | What It Does | Auto-starts |
|--------|-------------|-------------|
| **thermal-conductor** | Session daemon, TUI, GPU terminal, bar (layer-shell), HUD (layer-shell), message bus | Yes (systemd) |
| **thermal-audio** | TTS playback (edge-tts) + voice capture (VAD, Whisper STT) | Yes (systemd) |
| **thermal-dispatcher** | LLM API routing, trust tiers, adaptive learning | Yes (systemd) |

All three auto-start at login via `thermal.target` (systemd user services).

### On-Demand Components
| Component | How to Launch | What It Does |
|-----------|--------------|-------------|
| **thermal-launch** | Super+D | Fuzzy app launcher overlay |
| **thermal-lock** | Disabled (NVIDIA) | Lock screen with WGSL heatmap shader + PAM auth |
| **thermal-screensaver** | `thermal-screensaver` | Idle-triggered thermal fluid simulation overlay |
| **thermal-wallpaper** | `thermal-wallpaper` | Animated WGSL thermal shader wallpaper |
| **thermal-commander** | MCP server (stdio) | Desktop control tools for Claude |

### CLI Tools
```bash
# TUI dashboard
thc                            # Launch tabbed TUI (Sessions/Profiles/Services)
thc tui                        # Same, explicit subcommand

# GPU terminal
thc window                     # Standalone or daemon client mode

# Session management
thc spawn                      # Spawn a shell session
thc list                       # List sessions
thc kill ID                    # Kill a session
thc send ID "text"             # Send text to a session

# Audio control
thc audio status               # Check audio daemon status
thermal-audio --test "hello"   # Test TTS

# Voice (integrated into thermal-audio)
thc voice toggle               # Toggle push-to-talk
thc voice listen               # Start always-listening VAD mode

# Health check
thc doctor                     # Check all daemons
thc doctor --fix               # Clean stale files + restart dead daemons
```

### Voice Pipeline
Voice input with local Whisper transcription and AI dispatch:

```
Super+\ -> thermal-audio (cpal mic capture -> Whisper STT) -> claude -p (dispatch) -> tool execution
         OR
thc voice listen -> VAD detects speech -> same pipeline
```

### Spawn Profiles
The TUI Profiles tab loads from `config/profiles.toml` (or `~/.config/thermal/profiles.toml`).

## Hotkeys

See [docs/HOTKEYS.md](docs/HOTKEYS.md) for the complete reference.

- **Super+Enter** — GPU terminal (thc window)
- **Super+Shift+Enter** — kitty terminal (fallback)
- **Super+T** — TUI Hub (thc)
- **Super+D** — App launcher
- **Super+\\** — Push-to-talk voice input
- **Super+Q** — Close window
- **Super+B** — btop system monitor
- **Print** — Screenshot region select

## Troubleshooting

### Daemons not running
```bash
# Check status
thc doctor

# Restart via systemd
systemctl --user restart thermal-audio thermal-dispatcher thermal-conductor

# Or restart all
systemctl --user restart thermal.target
```

### thermal-audio not speaking
```bash
thc audio status
thermal-audio --test "testing one two three"

# Test underlying pipeline
edge-tts --text "hello" --write-media /tmp/test.mp3 && mpv --no-video /tmp/test.mp3
```

### thermal-conductor window shows black/purple grid
```bash
# Check daemon socket
ls /run/user/$UID/thermal/conductor.sock

# Launch with stderr visible
thc window 2>&1 | head -50
```

### Stale processes / duplicate instances
```bash
# Full reset
thc doctor --fix

# Or manual
pkill -f 'thc daemon'; pkill thermal-audio; pkill thermal-dispatcher
rm -f /run/user/$UID/thermal/*.pid /run/user/$UID/thermal/*.sock
```

## Crate Map

```
thermal-protocol (wire types, config — lightweight, no GPU)
thermal-runtime  (socket/pid/cleanup helpers — no GPU)
thermal-core     (palette, text, wgpu context — re-exports protocol+runtime)
thermal-terminal (PTY, OSC 633, state inference)

thermal-conductor ── TUI + daemon + GPU terminal + bar + HUD + message bus
thermal-audio ────── TTS playback + voice capture (VAD, Whisper STT)
thermal-dispatcher ─ AI command routing, trust tiers, adaptive learning
thermal-commander ── MCP server (desktop control tools for Claude)
thermal-launch ───── fuzzy app launcher overlay
thermal-lock ─────── lock screen (WGSL shader + PAM)
thermal-screensaver  idle thermal fluid simulation
thermal-wallpaper ── animated thermal shader wallpaper
```

## File Locations

| What | Where |
|------|-------|
| Claude session state | `/tmp/claude-code-state/*.json` |
| Codex session state | `/tmp/codex-state/*.json` |
| Copilot session state | `/tmp/copilot-state/*.json` |
| Voice state | `/tmp/thermal-voice-state.json` |
| Focus state | `/tmp/thermal-focus-state.json` |
| Conductor socket | `/run/user/$UID/thermal/conductor.sock` |
| Dispatcher socket | `/run/user/$UID/thermal/dispatcher.sock` |
| Audio socket | `/run/user/$UID/thermal/audio.sock` |
| TTS cache | `~/.cache/thermal-audio/` |
| Audio settings | `~/.config/thermal/audio.toml` |
| Spawn profiles | `config/profiles.toml` or `~/.config/thermal/profiles.toml` |
| Sessions sidecar | `/run/user/$UID/thermal/sessions.json` |
| Color definitions | `thermal-core/src/palette.rs` |
| Systemd units | `~/.config/systemd/user/thermal-*.service` |

## Hardware

- **Ultrawide**: 3440x1440 @ 100Hz (DP-1, primary)
- **Portrait**: 1920x1080 @ 144Hz (HDMI-A-2, rotated)
- **GPU**: NVIDIA GeForce RTX 3070 (Vulkan backend for wgpu)
