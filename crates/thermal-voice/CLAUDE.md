# thermal-voice

Voice input daemon — always-listening VAD mode with PTT override.

## What This Does
cpal audio capture with energy-based VAD (hysteresis). RMS level export to `/tmp/thermal-voice-state.json` (~5Hz). Local Whisper STT via whisper-cpp (CUDA). Unix socket API at `/run/user/$UID/thermal/voice.sock`.

## Modes
- **VAD mode** (`thermal-voice listen`): Speech auto-detected → transcript sent to dispatcher socket
- **PTT mode** (`thermal-voice` / `thermal-voice toggle`): Manual start/stop → wtype at cursor + clipboard + dispatcher

## Voice State File
Written to `/tmp/thermal-voice-state.json` with `state` (muted/monitoring/listening/processing), optional `label`, and `level` (RMS energy 0.0–1.0). Read by thermal-bar for the voice level meter.

## Hotkeys
| Input | Action | Binding |
|-------|--------|---------|
| Super+\\ | PTT toggle | `thermal-voice toggle` |
| Mouse back (thumb) | PTT toggle | mouse:275 |
| Mouse forward | VAD on/off | `thermal-vad-toggle.sh` (mouse:276) |

## Dependencies
- **whisper-cpp**: Local STT with CUDA. Install via `thermal-os-dotfiles/bin/install-whisper-cpp`.
- **wtype**: Wayland text input for PTT dictation mode.
- **wl-copy**: Clipboard for PTT transcripts.
- Pidfile guard in `/run/user/$UID/thermal/` for single-instance enforcement.
