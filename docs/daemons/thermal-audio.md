# thermal-audio

Unified audio daemon — TTS playback + voice capture (VAD, PTT, STT).

## What it does
Combined audio daemon handling both TTS playback and voice input capture.
Watches agent state files and announces transitions (e.g. "opus is now active")
via edge-tts. Maintains a 12-voice pool so each agent session gets a distinct
voice. Also exposes a Unix socket API for on-demand TTS from other daemons.

Voice capture: cpal audio capture with Silero ONNX VAD, push-to-talk,
wake word detection ("Alfred"), and local Whisper STT. In listen mode,
continuously monitors audio and dispatches transcripts to thermal-dispatcher.

## Socket / Pidfile
- Audio socket: `/run/user/$UID/thermal/audio.sock` (TTS + voice commands)
- Pidfile: `/run/user/$UID/thermal/audio.pid`
- State file: `/tmp/thermal-voice-state.json` (state + RMS level, ~5Hz)
- Echo suppression: in-process `Arc<AtomicBool>` (replaces former cross-daemon file polling)

## CLI Usage
```
thermal-audio [--test <TEXT>] [subcommand]
```
| Subcommand | Description |
|------------|-------------|
| *(none)* | Run daemon with PTT voice capture |
| `listen` | Run daemon with always-listening VAD mode |
| `toggle` | Send PTT toggle to running daemon |
| `dispatch` | PTT toggle with dispatcher routing |
| `voice-status` | Print voice capture state |

### Listen mode flags
| Flag | Description |
|------|-------------|
| `--threshold <0.0-1.0>` | Silero VAD speech probability threshold (default: 0.5) |
| `--streaming` | Use WebSocket streaming STT instead of batch |
| `--streaming-url <URL>` | WhisperLiveKit server URL |
| `--no-wake-word` | Disable wake word ("Alfred") detection |

## Dependencies
- **Needs**: `edge-tts` CLI for TTS, whisper-cpp for STT, cpal-compatible audio device
- **Needed by**: thermal-dispatcher (receives transcripts in VAD mode, sends TTS via socket)

## Troubleshooting
- If TTS is silent, verify `edge-tts` is installed: `pip install edge-tts`
- If STT fails, check whisper-cpp is installed: `which whisper-cpp`
- Model file expected at `~/.local/share/thermal/models/ggml-base.en.bin`
- Stale socket: if the daemon crashes, remove `/run/user/$UID/thermal/audio.sock`
- Audio conflicts: uses rodio (PipeWire-compatible); check PipeWire is running
