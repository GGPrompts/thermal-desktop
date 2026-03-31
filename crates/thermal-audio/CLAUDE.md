# thermal-audio

TTS daemon — 12-voice pool, per-agent voices, state transition alerts.

## What This Does
edge-tts based text-to-speech via Unix socket API at `/run/user/$UID/thermal/audio.sock`. Audio suppressed during voice input to avoid feedback loops. rodio 0.20 for PipeWire-compatible playback.
