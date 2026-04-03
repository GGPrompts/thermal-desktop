# thermal-audio

Unified audio daemon — TTS playback + voice capture (VAD, PTT, STT).

## What This Does
Combined audio daemon handling both playback (TTS) and capture (voice input). Replaces the former separate thermal-voice daemon.

**Playback**: edge-tts based text-to-speech via Unix socket API at `/run/user/$UID/thermal/audio.sock`. 12-voice pool, per-agent voices, state transition alerts. rodio 0.20 for PipeWire-compatible playback.

**Capture**: cpal audio capture with Silero ONNX VAD. Push-to-talk and always-listening modes. Wake word detection ("Alfred") via rustpotter. Local Whisper STT (batch) or WebSocket streaming STT. Voice commands via `/run/user/$UID/thermal/voice.sock`.

## Modules
- `main.rs` — TTS daemon, socket API, session announcements
- `capture.rs` — Voice capture (VAD loop, PTT, transcription, dispatch)
- `vad.rs` — Silero ONNX VAD engine
- `streaming.rs` — WebSocket STT client
- `transcript_filter.rs` — Whisper hallucination filtering
- `wakeword.rs` — Rustpotter wake word detection
- `daemon_client.rs` — Conductor event subscription

## Echo Suppression
Playback and capture coordinate in-process via `Arc<AtomicBool>` — no file polling needed.
