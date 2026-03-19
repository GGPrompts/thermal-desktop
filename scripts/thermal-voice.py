#!/usr/bin/env python3
"""
thermal-voice: Push-to-talk voice input daemon for Thermal Desktop.

Listens on a Unix socket for start/stop commands, records audio from the
default PipeWire mic, transcribes via faster-whisper, and emits JSON results.

=== Installation ===
    pip install faster-whisper sounddevice numpy

The base.en model (~150 MB) will be downloaded automatically on first run.
If you want to pre-download:
    python3 -c "from faster_whisper import WhisperModel; WhisperModel('base.en')"

=== Usage ===
    python3 thermal-voice.py              # foreground
    python3 thermal-voice.py --daemon     # daemonize (stdout/stderr to log)

Control via Unix socket:
    echo '{"action":"start"}' | socat - UNIX-CONNECT:/run/user/1000/thermal/voice.sock
    echo '{"action":"stop"}'  | socat - UNIX-CONNECT:/run/user/1000/thermal/voice.sock
    echo '{"action":"status"}'| socat - UNIX-CONNECT:/run/user/1000/thermal/voice.sock

Keybind (Hyprland):
    bind = $mod, backslash, exec, ~/projects/thermal-desktop/scripts/thermal-voice-toggle.sh
"""

import argparse
import asyncio
import io
import json
import logging
import os
import signal
import struct
import sys
import tempfile
import time
import wave
from datetime import datetime, timezone
from pathlib import Path

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

SOCKET_DIR = Path(f"/run/user/{os.getuid()}/thermal")
SOCKET_PATH = SOCKET_DIR / "voice.sock"
STATE_FILE = Path("/tmp/thermal-voice-state.json")
LOG_FILE = Path("/tmp/thermal-voice.log")

SAMPLE_RATE = 16000  # 16 kHz mono — what Whisper expects
CHANNELS = 1
WHISPER_MODEL = "base.en"
WHISPER_DEVICE = "auto"  # "cpu", "cuda", or "auto"
WHISPER_COMPUTE = "int8"  # "float16", "int8", "float32"

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

log = logging.getLogger("thermal-voice")


def setup_logging(daemon: bool = False):
    level = logging.INFO
    fmt = "%(asctime)s [%(levelname)s] %(message)s"
    if daemon:
        logging.basicConfig(
            filename=str(LOG_FILE), level=level, format=fmt, force=True
        )
    else:
        logging.basicConfig(level=level, format=fmt, force=True)


# ---------------------------------------------------------------------------
# State file helpers
# ---------------------------------------------------------------------------


def write_state(
    listening: bool,
    last_transcript: str = "",
    confidence: float = 0.0,
    error: str = "",
):
    state = {
        "listening": listening,
        "last_transcript": last_transcript,
        "confidence": round(confidence, 3),
        "error": error,
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "pid": os.getpid(),
    }
    tmp = STATE_FILE.with_suffix(".tmp")
    tmp.write_text(json.dumps(state, indent=2) + "\n")
    tmp.rename(STATE_FILE)


# ---------------------------------------------------------------------------
# Whisper model (lazy-loaded)
# ---------------------------------------------------------------------------


class Transcriber:
    """Lazy-loads the faster-whisper model on first use."""

    def __init__(self):
        self._model = None

    def _ensure_model(self):
        if self._model is not None:
            return
        log.info("Loading faster-whisper model '%s' (first run downloads ~150 MB)...", WHISPER_MODEL)
        from faster_whisper import WhisperModel

        self._model = WhisperModel(
            WHISPER_MODEL,
            device=WHISPER_DEVICE,
            compute_type=WHISPER_COMPUTE,
        )
        log.info("Model loaded.")

    def transcribe(self, audio_bytes: bytes) -> dict:
        """Transcribe raw 16-bit PCM 16 kHz mono audio. Returns {transcript, confidence}."""
        self._ensure_model()
        import numpy as np

        # Convert raw PCM bytes to float32 array [-1, 1]
        samples = np.frombuffer(audio_bytes, dtype=np.int16).astype(np.float32) / 32768.0

        if len(samples) < SAMPLE_RATE * 0.3:
            return {"transcript": "", "confidence": 0.0, "error": "Audio too short (< 0.3s)"}

        segments, info = self._model.transcribe(
            samples,
            beam_size=5,
            language="en",
            vad_filter=True,
            vad_parameters=dict(min_silence_duration_ms=500),
        )

        texts = []
        total_prob = 0.0
        count = 0
        for seg in segments:
            texts.append(seg.text.strip())
            total_prob += seg.avg_log_prob
            count += 1

        transcript = " ".join(texts).strip()
        # Convert avg log prob to a rough 0-1 confidence
        avg_log_prob = total_prob / count if count > 0 else -1.0
        confidence = max(0.0, min(1.0, 1.0 + avg_log_prob))  # log_prob is negative

        return {"transcript": transcript, "confidence": confidence, "error": ""}


# ---------------------------------------------------------------------------
# Audio recorder
# ---------------------------------------------------------------------------


class Recorder:
    """Records audio from default PipeWire/ALSA mic via sounddevice."""

    def __init__(self):
        self._chunks: list[bytes] = []
        self._stream = None
        self.recording = False

    def start(self):
        import sounddevice as sd

        self._chunks.clear()
        self.recording = True

        def callback(indata, frames, time_info, status):
            if status:
                log.warning("Audio status: %s", status)
            # indata is float32, convert to int16
            import numpy as np

            pcm = (indata[:, 0] * 32767).astype(np.int16)
            self._chunks.append(pcm.tobytes())

        self._stream = sd.InputStream(
            samplerate=SAMPLE_RATE,
            channels=CHANNELS,
            dtype="float32",
            blocksize=1024,
            callback=callback,
        )
        self._stream.start()
        log.info("Recording started.")

    def stop(self) -> bytes:
        """Stop recording, return raw 16-bit PCM bytes."""
        if self._stream is not None:
            self._stream.stop()
            self._stream.close()
            self._stream = None
        self.recording = False
        audio = b"".join(self._chunks)
        self._chunks.clear()
        duration = len(audio) / (SAMPLE_RATE * 2)  # 2 bytes per sample
        log.info("Recording stopped. %.1f seconds captured.", duration)
        return audio


# ---------------------------------------------------------------------------
# Socket server
# ---------------------------------------------------------------------------


class VoiceDaemon:
    def __init__(self):
        self.recorder = Recorder()
        self.transcriber = Transcriber()
        self._transcribing = False

    async def handle_client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        try:
            data = await asyncio.wait_for(reader.read(4096), timeout=5.0)
            if not data:
                writer.close()
                return

            text = data.decode("utf-8", errors="replace").strip()
            # Support bare commands or JSON
            try:
                msg = json.loads(text)
                action = msg.get("action", "").lower()
            except json.JSONDecodeError:
                action = text.lower()

            response = await self.handle_action(action)
            writer.write(json.dumps(response).encode("utf-8") + b"\n")
            await writer.drain()
        except asyncio.TimeoutError:
            log.warning("Client timed out.")
        except Exception as e:
            log.error("Client handler error: %s", e)
            try:
                writer.write(json.dumps({"error": str(e)}).encode("utf-8") + b"\n")
                await writer.drain()
            except Exception:
                pass
        finally:
            try:
                writer.close()
                await writer.wait_closed()
            except Exception:
                pass

    async def handle_action(self, action: str) -> dict:
        if action == "start":
            return await self.do_start()
        elif action == "stop":
            return await self.do_stop()
        elif action == "toggle":
            if self.recorder.recording:
                return await self.do_stop()
            else:
                return await self.do_start()
        elif action == "status":
            return self.do_status()
        else:
            return {"error": f"Unknown action: {action}"}

    async def do_start(self) -> dict:
        if self.recorder.recording:
            return {"status": "already_recording"}
        if self._transcribing:
            return {"status": "busy", "error": "Transcription in progress"}

        try:
            self.recorder.start()
            write_state(listening=True)
            return {"status": "recording"}
        except Exception as e:
            log.error("Failed to start recording: %s", e)
            write_state(listening=False, error=str(e))
            return {"status": "error", "error": str(e)}

    async def do_stop(self) -> dict:
        if not self.recorder.recording:
            return {"status": "not_recording"}

        self._transcribing = True
        try:
            audio = self.recorder.stop()
            write_state(listening=False)

            if not audio:
                self._transcribing = False
                return {"transcript": "", "confidence": 0.0, "error": "No audio captured"}

            # Run transcription in thread pool to avoid blocking the event loop
            loop = asyncio.get_event_loop()
            result = await loop.run_in_executor(None, self.transcriber.transcribe, audio)

            write_state(
                listening=False,
                last_transcript=result.get("transcript", ""),
                confidence=result.get("confidence", 0.0),
                error=result.get("error", ""),
            )

            log.info("Transcript: %s (confidence: %.2f)", result.get("transcript", ""), result.get("confidence", 0.0))
            return result
        except Exception as e:
            log.error("Transcription failed: %s", e)
            write_state(listening=False, error=str(e))
            return {"transcript": "", "confidence": 0.0, "error": str(e)}
        finally:
            self._transcribing = False

    def do_status(self) -> dict:
        return {
            "recording": self.recorder.recording,
            "transcribing": self._transcribing,
            "pid": os.getpid(),
        }

    async def run(self):
        # Ensure socket directory exists
        SOCKET_DIR.mkdir(parents=True, exist_ok=True)

        # Remove stale socket
        if SOCKET_PATH.exists():
            SOCKET_PATH.unlink()

        # Write initial state
        write_state(listening=False)

        server = await asyncio.start_unix_server(self.handle_client, path=str(SOCKET_PATH))
        os.chmod(str(SOCKET_PATH), 0o600)

        log.info("thermal-voice daemon listening on %s", SOCKET_PATH)
        log.info("Model: %s | Sample rate: %d Hz | Device: %s", WHISPER_MODEL, SAMPLE_RATE, WHISPER_DEVICE)

        # Handle signals for clean shutdown
        loop = asyncio.get_event_loop()
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, lambda s=sig: asyncio.ensure_future(self.shutdown(server, s)))

        async with server:
            await server.serve_forever()

    async def shutdown(self, server, sig):
        log.info("Received signal %s, shutting down...", sig.name)
        if self.recorder.recording:
            self.recorder.stop()
        write_state(listening=False)
        server.close()
        await server.wait_closed()

        # Clean up socket
        if SOCKET_PATH.exists():
            SOCKET_PATH.unlink()

        # Stop the event loop
        asyncio.get_event_loop().stop()


# ---------------------------------------------------------------------------
# Entrypoint
# ---------------------------------------------------------------------------


def main():
    parser = argparse.ArgumentParser(description="thermal-voice: push-to-talk STT daemon")
    parser.add_argument("--daemon", action="store_true", help="Run as background daemon")
    parser.add_argument("--model", default=WHISPER_MODEL, help=f"Whisper model (default: {WHISPER_MODEL})")
    parser.add_argument("--device", default=WHISPER_DEVICE, help=f"Compute device (default: {WHISPER_DEVICE})")
    args = parser.parse_args()

    global WHISPER_MODEL, WHISPER_DEVICE
    WHISPER_MODEL = args.model
    WHISPER_DEVICE = args.device

    if args.daemon:
        # Fork to background
        pid = os.fork()
        if pid > 0:
            print(f"thermal-voice daemon started (pid {pid})")
            print(f"  Socket: {SOCKET_PATH}")
            print(f"  Log:    {LOG_FILE}")
            print(f"  State:  {STATE_FILE}")
            sys.exit(0)
        # Child: detach
        os.setsid()
        setup_logging(daemon=True)
    else:
        setup_logging(daemon=False)

    # Dependency check (fail early with helpful message)
    missing = []
    try:
        import faster_whisper  # noqa: F401
    except ImportError:
        missing.append("faster-whisper")
    try:
        import sounddevice  # noqa: F401
    except ImportError:
        missing.append("sounddevice")
    try:
        import numpy  # noqa: F401
    except ImportError:
        missing.append("numpy")

    if missing:
        log.error("Missing Python packages: %s", ", ".join(missing))
        log.error("Install with: pip install %s", " ".join(missing))
        sys.exit(1)

    daemon = VoiceDaemon()
    asyncio.run(daemon.run())


if __name__ == "__main__":
    main()
