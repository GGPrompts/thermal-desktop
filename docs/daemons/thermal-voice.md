# thermal-voice

Voice input daemon with push-to-talk and always-listening VAD modes.

## What it does
Captures audio via cpal, runs local Whisper STT for transcription, and
dispatches results. In PTT mode, records on toggle and types transcripts
at the cursor via wtype. In VAD (listen) mode, continuously monitors audio,
detects speech via energy-based VAD with optional Silero model, and sends
transcripts to thermal-dispatcher for AI processing. Exports RMS audio
level to a state file for the bar's voice meter.

## Socket / Pidfile
- Socket: `/run/user/$UID/thermal/voice.sock`
- Pidfile: `/run/user/$UID/thermal/voice.pid`
- State file: `/tmp/thermal-voice-state.json` (state + RMS level, ~5Hz)

## CLI Usage
```
thermal-voice [subcommand]
```
| Subcommand | Description |
|------------|-------------|
| *(none)* | Run daemon in push-to-talk mode |
| `listen` | Run daemon in always-listening VAD mode |
| `toggle` | Send start/stop toggle to the running daemon |
| `status` | Print current daemon state and exit |

### Listen mode flags
| Flag | Description |
|------|-------------|
| `--threshold <0.0-1.0>` | Silero VAD speech probability threshold (default: 0.5) |
| `--streaming` | Use WebSocket streaming STT instead of batch |
| `--streaming-url <URL>` | WhisperLiveKit server URL |
| `--no-wake-word` | Disable wake word ("Alfred") detection |

## Dependencies
- **Needs**: whisper-cpp (local STT with CUDA), cpal-compatible audio device, wtype (PTT mode), wl-copy (clipboard)
- **Needed by**: thermal-dispatcher (receives transcripts in VAD mode)

## Troubleshooting
- If STT fails, check whisper-cpp is installed: `which whisper-cpp`
- Model file expected at `~/.local/share/thermal/models/ggml-base.en.bin`
- If no audio is captured, check PipeWire and `cpal` device enumeration
- Hotkeys: Super+\\ or mouse back button for PTT toggle, mouse forward for VAD on/off
