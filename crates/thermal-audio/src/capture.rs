//! Voice capture module — VAD loop, PTT, wake word, and transcription.
//!
//! Ported from thermal-voice into the unified audio daemon. All voice capture
//! functionality lives here; coordination with playback uses in-process
//! `Arc<AtomicBool>` instead of file-based polling.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tracing::{error, info, warn};

use crate::streaming;
use crate::transcript_filter::{FilterResult, filter_transcript};
use crate::vad::{VadDetector, VadEvent};
use crate::wakeword::WakeWordDetector;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const SAMPLE_RATE: u32 = 16_000;
const CHANNELS: u16 = 1;

/// Default model path under ~/.local/share/thermal/models/
const DEFAULT_MODEL_FILENAME: &str = "ggml-base.en.bin";

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// Path to the whisper.cpp GGML model file.
    pub model_path: Option<String>,
    /// Name of the whisper CLI binary (default: "whisper-cpp").
    pub whisper_command: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            model_path: None,
            whisper_command: "whisper-cpp".to_string(),
        }
    }
}

pub fn load_voice_config() -> VoiceConfig {
    let config_path = super::config_dir().join("voice.toml");
    if config_path.exists() {
        match fs::read_to_string(&config_path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(cfg) => return cfg,
                Err(e) => warn!("failed to parse {}: {e}", config_path.display()),
            },
            Err(e) => warn!("failed to read {}: {e}", config_path.display()),
        }
    }
    VoiceConfig::default()
}

fn dirs_data() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".local/share")
        })
}

fn default_model_path() -> PathBuf {
    dirs_data()
        .join("thermal/models")
        .join(DEFAULT_MODEL_FILENAME)
}

fn resolve_model_path(config: &VoiceConfig) -> PathBuf {
    config
        .model_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(default_model_path)
}

// ---------------------------------------------------------------------------
// Runtime paths
// ---------------------------------------------------------------------------

// voice pidfile is no longer needed — unified daemon uses audio.pid

// Voice state file: producer end of the voice state chain. Written at ~5Hz
// with RMS level + state. Consumers: thermal-bar (voice module), thermal-hud.
const STATE_FILE: &str = "/tmp/thermal-voice-state.json";

// ---------------------------------------------------------------------------
// State file (matches thermal-bar voice module schema)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceState {
    #[default]
    Muted,
    /// Always-listening idle: audio capture is running, VAD is active,
    /// but no speech has been detected yet.
    Monitoring,
    /// Wake word mode: listening for the wake word ("Alfred") before
    /// activating speech capture.
    WakeWord,
    Listening,
    Processing,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceStateFile {
    pub state: VoiceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Current RMS audio level (0.0-1.0), written by VAD loop for visual meters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<f32>,
}

fn write_state(state: VoiceState, label: Option<&str>) {
    write_state_with_level(state, label, None);
}

fn write_state_with_level(state: VoiceState, label: Option<&str>, level: Option<f32>) {
    let file = VoiceStateFile {
        state,
        label: label.map(String::from),
        level,
    };
    let json = match serde_json::to_string_pretty(&file) {
        Ok(j) => j,
        Err(e) => {
            error!("failed to serialize state: {e}");
            return;
        }
    };
    let tmp = format!("{STATE_FILE}.tmp");
    if let Err(e) = fs::write(&tmp, format!("{json}\n")).and_then(|_| fs::rename(&tmp, STATE_FILE))
    {
        error!("failed to write state file: {e}");
    }
}

fn read_state_file() -> Option<VoiceStateFile> {
    fs::read_to_string(STATE_FILE)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

// ---------------------------------------------------------------------------
// Socket command protocol (voice commands via audio.sock or voice.sock)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct VoiceSocketResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl VoiceSocketResponse {
    pub fn ok(status: &str) -> Self {
        Self {
            status: Some(status.to_string()),
            state: None,
            transcript: None,
            error: None,
        }
    }

    pub fn with_transcript(transcript: String) -> Self {
        Self {
            status: Some("transcribed".to_string()),
            state: None,
            transcript: Some(transcript),
            error: None,
        }
    }

    pub fn error(msg: &str) -> Self {
        Self {
            status: Some("error".to_string()),
            state: None,
            transcript: None,
            error: Some(msg.to_string()),
        }
    }

    pub fn state_response(state: VoiceState) -> Self {
        let s = match state {
            VoiceState::Muted => "muted",
            VoiceState::Monitoring => "monitoring",
            VoiceState::WakeWord => "wake_word",
            VoiceState::Listening => "listening",
            VoiceState::Processing => "processing",
        };
        Self {
            status: None,
            state: Some(s.to_string()),
            transcript: None,
            error: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Audio recorder (cpal) — for PTT mode
// ---------------------------------------------------------------------------

struct Recorder {
    samples: Arc<Mutex<Vec<i16>>>,
    stream: Option<cpal::Stream>,
    native_rate: Option<u32>,
    native_channels: Option<u16>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            samples: Arc::new(Mutex::new(Vec::new())),
            stream: None,
            native_rate: None,
            native_channels: None,
        }
    }

    fn start(&mut self) -> Result<()> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no default audio input device found")?;

        info!("recording from: {}", device.name().unwrap_or_default());

        let default_config = device
            .default_input_config()
            .context("failed to get default input config")?;
        let native_rate = default_config.sample_rate().0;
        let native_channels = default_config.channels();
        info!("native format: {native_rate}Hz, {native_channels}ch");

        let config = cpal::StreamConfig {
            channels: native_channels,
            sample_rate: cpal::SampleRate(native_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        self.native_rate = Some(native_rate);
        self.native_channels = Some(native_channels);

        let samples = Arc::clone(&self.samples);
        samples.lock().unwrap().clear();

        let err_fn = |e: cpal::StreamError| {
            error!("audio stream error: {e}");
        };

        let channels = native_channels;
        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut buf = samples.lock().unwrap();
                for chunk in data.chunks(channels as usize) {
                    let mono: f32 = chunk.iter().sum::<f32>() / channels as f32;
                    let clamped = mono.clamp(-1.0, 1.0);
                    buf.push((clamped * 32767.0) as i16);
                }
            },
            err_fn,
            None,
        )?;

        stream.play()?;
        self.stream = Some(stream);
        info!("recording started");
        Ok(())
    }

    fn stop(&mut self) -> Vec<i16> {
        self.stream.take();
        let raw_samples = self.samples.lock().unwrap().clone();
        let native_rate = self.native_rate.unwrap_or(SAMPLE_RATE);

        let samples = if native_rate != SAMPLE_RATE {
            let ratio = SAMPLE_RATE as f64 / native_rate as f64;
            let new_len = (raw_samples.len() as f64 * ratio) as usize;
            let mut resampled = Vec::with_capacity(new_len);
            for i in 0..new_len {
                let src_idx = i as f64 / ratio;
                let idx = src_idx as usize;
                let frac = src_idx - idx as f64;
                let s0 = raw_samples[idx.min(raw_samples.len() - 1)] as f64;
                let s1 = raw_samples[(idx + 1).min(raw_samples.len() - 1)] as f64;
                resampled.push((s0 + frac * (s1 - s0)) as i16);
            }
            info!(
                "resampled {native_rate}Hz -> {SAMPLE_RATE}Hz ({} -> {} samples)",
                raw_samples.len(),
                resampled.len()
            );
            resampled
        } else {
            raw_samples
        };

        let duration_secs = samples.len() as f64 / SAMPLE_RATE as f64;
        info!(
            "recording stopped: {duration_secs:.1}s captured ({} samples)",
            samples.len()
        );
        samples
    }

    fn is_recording(&self) -> bool {
        self.stream.is_some()
    }
}

// ---------------------------------------------------------------------------
// Whisper transcription (shells out to whisper-cpp or whisper CLI)
// ---------------------------------------------------------------------------

fn write_wav(samples: &[i16], path: &Path) -> Result<()> {
    let mut file = fs::File::create(path)?;
    let data_len = (samples.len() * 2) as u32;
    let file_len = 36 + data_len;

    // WAV header
    file.write_all(b"RIFF")?;
    file.write_all(&file_len.to_le_bytes())?;
    file.write_all(b"WAVE")?;

    // fmt chunk
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&CHANNELS.to_le_bytes())?;
    file.write_all(&SAMPLE_RATE.to_le_bytes())?;
    let byte_rate = SAMPLE_RATE * CHANNELS as u32 * 2;
    file.write_all(&byte_rate.to_le_bytes())?;
    let block_align = CHANNELS * 2;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;

    // data chunk
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;
    for &s in samples {
        file.write_all(&s.to_le_bytes())?;
    }

    Ok(())
}

fn transcribe(samples: &[i16], config: &VoiceConfig) -> Result<String> {
    let model_path = resolve_model_path(config);

    let tmp_dir = std::env::temp_dir();
    let wav_path = tmp_dir.join("thermal-voice-recording.wav");
    write_wav(samples, &wav_path).context("writing WAV file")?;

    let commands_to_try: Vec<(&str, Vec<String>)> = vec![
        (
            &config.whisper_command,
            vec![
                "-m".to_string(),
                model_path.display().to_string(),
                "-f".to_string(),
                wav_path.display().to_string(),
                "--no-timestamps".to_string(),
                "-l".to_string(),
                "en".to_string(),
            ],
        ),
        (
            "whisper",
            vec![
                wav_path.display().to_string(),
                "--model".to_string(),
                "base.en".to_string(),
                "--language".to_string(),
                "en".to_string(),
                "--output_format".to_string(),
                "txt".to_string(),
                "--output_dir".to_string(),
                tmp_dir.display().to_string(),
            ],
        ),
    ];

    for (cmd, args) in &commands_to_try {
        info!("trying transcription with: {cmd}");
        match std::process::Command::new(cmd)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout).trim().to_string();

                if text.is_empty() {
                    let txt_path = wav_path.with_extension("txt");
                    if txt_path.exists() {
                        let file_text = fs::read_to_string(&txt_path)
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        let _ = fs::remove_file(&txt_path);
                        if !file_text.is_empty() {
                            info!("transcription complete: {} chars", file_text.len());
                            let _ = fs::remove_file(&wav_path);
                            return Ok(file_text);
                        }
                    }
                }

                if !text.is_empty() {
                    info!("transcription complete: {} chars", text.len());
                    let _ = fs::remove_file(&wav_path);
                    return Ok(text);
                }

                warn!("{cmd} produced empty output");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!("{cmd} failed: {stderr}");
            }
            Err(e) => {
                warn!("{cmd} not found or failed to execute: {e}");
            }
        }
    }

    let _ = fs::remove_file(&wav_path);
    anyhow::bail!(
        "no whisper CLI available. Install whisper-cpp or whisper, or set whisper_command in {}",
        super::config_dir().join("voice.toml").display()
    )
}

// ---------------------------------------------------------------------------
// Dispatch helpers
// ---------------------------------------------------------------------------

/// Send a transcript to thermal-dispatcher via its Unix socket for command execution.
async fn dispatch_to_dispatcher(transcript: String) {
    let sock_path = thermal_core::runtime::socket_path("dispatcher");
    match tokio::net::UnixStream::connect(&sock_path).await {
        Ok(stream) => {
            let (reader, mut writer) = tokio::io::split(stream);
            let msg = serde_json::json!({"transcript": transcript});
            let payload = format!("{}\n", msg);
            if let Err(e) = writer.write_all(payload.as_bytes()).await {
                error!("failed to write to dispatcher socket: {e}");
                return;
            }
            let _ = writer.shutdown().await;
            let mut response = String::new();
            let mut buf_reader = tokio::io::BufReader::new(reader);
            match tokio::time::timeout(
                std::time::Duration::from_secs(120),
                buf_reader.read_line(&mut response),
            )
            .await
            {
                Ok(Ok(_)) => info!("dispatcher response: {}", response.trim()),
                Ok(Err(e)) => warn!("failed to read dispatcher response: {e}"),
                Err(_) => {
                    warn!("dispatcher response timed out after 120s, falling back to claude -p");
                    dispatch_to_claude(&transcript).await;
                }
            }
        }
        Err(e) => {
            warn!(
                "cannot connect to dispatcher socket at {}: {e}",
                sock_path.display()
            );
            warn!("is thermal-dispatcher running? falling back to claude -p");
            dispatch_to_claude(&transcript).await;
        }
    }
}

/// Run `claude -p "{transcript}"`, send response to TTS, update state.
async fn dispatch_to_claude(transcript: &str) {
    write_state(VoiceState::Processing, Some("dispatching"));

    info!("dispatching transcript to claude -p");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("claude")
            .arg("-p")
            .arg(transcript)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            let response = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if response.is_empty() {
                info!("claude -p returned empty response");
            } else {
                info!("claude -p response: {} chars", response.len());
                send_to_audio_socket(&response).await;
            }
        }
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!(
                "claude -p failed (exit {}): {}",
                output.status,
                stderr.chars().take(200).collect::<String>()
            );
        }
        Ok(Err(e)) => {
            error!("failed to execute claude: {e}");
        }
        Err(_) => {
            error!("claude -p timed out after 30s");
        }
    }

    write_state(VoiceState::Muted, None);
}

/// Send text to ourselves (audio.sock) for TTS playback.
async fn send_to_audio_socket(text: &str) {
    let audio_sock = thermal_core::runtime::socket_path("audio");

    match tokio::net::UnixStream::connect(&audio_sock).await {
        Ok(stream) => {
            let msg = serde_json::json!({
                "action": "tts",
                "text": text,
            });
            let (_, mut writer) = stream.into_split();
            let payload = serde_json::to_string(&msg).unwrap_or_default() + "\n";
            if let Err(e) = writer.write_all(payload.as_bytes()).await {
                warn!("failed to write to audio socket: {e}");
            } else {
                info!("sent TTS to audio socket: {} chars", text.len());
            }
        }
        Err(e) => {
            warn!(
                "audio socket not available at {}: {e}",
                audio_sock.display()
            );
        }
    }
}

fn copy_to_clipboard(text: &str) {
    match std::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
        Err(e) => warn!("wl-copy failed (clipboard not set): {e}"),
    }
}

/// Type text at the current cursor position using wtype (Wayland).
fn type_at_cursor(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    match std::process::Command::new("wtype")
        .arg("-d")
        .arg("1")
        .arg("--")
        .arg(text)
        .status()
    {
        Ok(status) if status.success() => {
            info!("typed text at cursor via wtype ({} chars)", text.len());
            true
        }
        Ok(status) => {
            warn!("wtype exited with {status}, falling back to clipboard");
            copy_to_clipboard(text);
            false
        }
        Err(e) => {
            warn!("wtype not available ({e}), falling back to clipboard");
            copy_to_clipboard(text);
            false
        }
    }
}

/// Send a special key or key-combo via wtype.
fn wtype_key(key: &str, modifier: Option<&str>) {
    let mut cmd = std::process::Command::new("wtype");
    if let Some(m) = modifier {
        cmd.arg("-M").arg(m).arg("-k").arg(key);
    } else {
        cmd.arg("-k").arg(key);
    }
    match cmd.status() {
        Ok(s) if s.success() => info!("wtype key: {key}"),
        Ok(s) => warn!("wtype -k {key} exited with {s}"),
        Err(e) => warn!("wtype -k {key} failed: {e}"),
    }
}

/// A code word detected at the end of a transcript.
#[derive(Clone, Copy)]
enum CodeWord {
    Submit,
    SelectAll,
    Undo,
    NewLine,
    Tab,
}

fn parse_code_word(transcript: &str) -> (String, Option<CodeWord>) {
    let trimmed = transcript.trim();
    if trimmed.is_empty() {
        return (String::new(), None);
    }

    let lower = trimmed.to_lowercase();
    let matchable = lower.trim_end_matches(|c: char| c.is_ascii_punctuation());

    let two_word_codes: &[(&[&str], CodeWord)] = &[
        (&["select all"], CodeWord::SelectAll),
        (&["new line", "newline"], CodeWord::NewLine),
    ];
    for (phrases, code) in two_word_codes {
        for phrase in *phrases {
            if matchable.ends_with(phrase) {
                let cleaned = strip_trailing_phrase(trimmed, phrase);
                return (cleaned, Some(*code));
            }
        }
    }

    let single_word_codes: &[(&[&str], CodeWord)] = &[
        (
            &["send", "submit", "enter", "send it", "submit it"],
            CodeWord::Submit,
        ),
        (&["undo", "undo that"], CodeWord::Undo),
        (&["tab"], CodeWord::Tab),
    ];
    for (phrases, code) in single_word_codes {
        for phrase in *phrases {
            if matchable.ends_with(phrase) {
                let cleaned = strip_trailing_phrase(trimmed, phrase);
                return (cleaned, Some(*code));
            }
        }
    }

    (trimmed.to_string(), None)
}

fn strip_trailing_phrase(text: &str, phrase: &str) -> String {
    let lower = text.to_lowercase();
    let trimmed_lower = lower.trim_end_matches(|c: char| c.is_ascii_punctuation());
    if let Some(pos) = trimmed_lower.rfind(phrase) {
        if pos + phrase.len() == trimmed_lower.len() {
            let prefix = &text[..pos];
            return prefix
                .trim_end()
                .trim_end_matches([',', '.', '-'])
                .trim_end()
                .to_string();
        }
    }
    text.to_string()
}

fn execute_code_word(code: &CodeWord) {
    match code {
        CodeWord::Submit => wtype_key("Return", None),
        CodeWord::SelectAll => wtype_key("a", Some("ctrl")),
        CodeWord::Undo => wtype_key("z", Some("ctrl")),
        CodeWord::NewLine => wtype_key("Return", None),
        CodeWord::Tab => wtype_key("Tab", None),
    }
}

// ---------------------------------------------------------------------------
// Daemon command (sent from socket handler to main capture loop)
// ---------------------------------------------------------------------------

pub struct VoiceDaemonCommand {
    pub action: String,
    pub reply: oneshot::Sender<VoiceSocketResponse>,
}

// ---------------------------------------------------------------------------
// PTT handlers
// ---------------------------------------------------------------------------

fn handle_start(recorder: &mut Recorder) -> VoiceSocketResponse {
    if recorder.is_recording() {
        return VoiceSocketResponse::ok("already_recording");
    }

    match recorder.start() {
        Ok(()) => {
            write_state(VoiceState::Listening, None);
            VoiceSocketResponse::ok("recording")
        }
        Err(e) => {
            error!("failed to start recording: {e}");
            write_state(VoiceState::Muted, None);
            VoiceSocketResponse::error(&format!("failed to start recording: {e}"))
        }
    }
}

async fn handle_stop(recorder: &mut Recorder, config: &VoiceConfig) -> VoiceSocketResponse {
    if !recorder.is_recording() {
        return VoiceSocketResponse::ok("not_recording");
    }

    let samples = recorder.stop();
    write_state(VoiceState::Processing, Some("transcribing"));

    let min_samples = (SAMPLE_RATE as f64 * 0.3) as usize;
    if samples.len() < min_samples {
        write_state(VoiceState::Muted, None);
        return VoiceSocketResponse::error("audio too short (< 0.3s)");
    }

    let config_cmd = config.whisper_command.clone();
    let config_model = config.model_path.clone();
    let transcript = tokio::task::spawn_blocking(move || {
        let cfg = VoiceConfig {
            model_path: config_model,
            whisper_command: config_cmd,
        };
        transcribe(&samples, &cfg)
    })
    .await;

    match transcript {
        Ok(Ok(text)) => {
            info!("transcript: {text}");
            let (cleaned_text, code_word) = parse_code_word(&text);
            type_at_cursor(&cleaned_text);
            copy_to_clipboard(&text);

            if let Some(ref code) = code_word {
                execute_code_word(code);
            }

            write_state(VoiceState::Muted, None);
            VoiceSocketResponse::with_transcript(text)
        }
        Ok(Err(e)) => {
            write_state(VoiceState::Muted, None);
            error!("transcription failed: {e}");
            VoiceSocketResponse::error(&format!("transcription failed: {e}"))
        }
        Err(e) => {
            write_state(VoiceState::Muted, None);
            error!("transcription task panicked: {e}");
            VoiceSocketResponse::error("transcription task panicked")
        }
    }
}

async fn handle_dispatch(recorder: &mut Recorder, config: &VoiceConfig) -> VoiceSocketResponse {
    if !recorder.is_recording() {
        return VoiceSocketResponse::ok("not_recording");
    }

    let samples = recorder.stop();
    write_state(VoiceState::Processing, Some("transcribing"));

    let min_samples = (SAMPLE_RATE as f64 * 0.3) as usize;
    if samples.len() < min_samples {
        write_state(VoiceState::Muted, None);
        return VoiceSocketResponse::error("audio too short (< 0.3s)");
    }

    let config_cmd = config.whisper_command.clone();
    let config_model = config.model_path.clone();
    let transcript = tokio::task::spawn_blocking(move || {
        let cfg = VoiceConfig {
            model_path: config_model,
            whisper_command: config_cmd,
        };
        transcribe(&samples, &cfg)
    })
    .await;

    match transcript {
        Ok(Ok(text)) => {
            info!("dispatcher transcript: {text}");
            write_state(VoiceState::Processing, Some("dispatching"));

            let text_clone = text.clone();
            tokio::spawn(async move {
                dispatch_to_dispatcher(text_clone).await;
            });

            write_state(VoiceState::Muted, None);
            VoiceSocketResponse::with_transcript(text)
        }
        Ok(Err(e)) => {
            write_state(VoiceState::Muted, None);
            error!("transcription failed: {e}");
            VoiceSocketResponse::error(&format!("transcription failed: {e}"))
        }
        Err(e) => {
            write_state(VoiceState::Muted, None);
            error!("transcription task panicked: {e}");
            VoiceSocketResponse::error("transcription task panicked")
        }
    }
}

// ---------------------------------------------------------------------------
// Resampling helpers
// ---------------------------------------------------------------------------

fn resample_f32_to_i16(samples: &[f32], src_rate: u32, dst_rate: u32) -> Vec<i16> {
    if samples.is_empty() {
        return Vec::new();
    }

    if src_rate == dst_rate {
        return samples
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();
    }

    let ratio = dst_rate as f64 / src_rate as f64;
    let new_len = (samples.len() as f64 * ratio) as usize;
    let mut resampled = Vec::with_capacity(new_len);
    for i in 0..new_len {
        let src_idx = i as f64 / ratio;
        let idx = src_idx as usize;
        let frac = src_idx - idx as f64;
        let s0 = samples[idx.min(samples.len() - 1)] as f64;
        let s1 = samples[(idx + 1).min(samples.len() - 1)] as f64;
        let val = s0 + frac * (s1 - s0);
        resampled.push((val.clamp(-1.0, 1.0) * 32767.0) as i16);
    }
    resampled
}

fn resample_f32_for_streaming(samples: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }

    if src_rate == dst_rate {
        return samples.to_vec();
    }

    let ratio = dst_rate as f64 / src_rate as f64;
    let new_len = (samples.len() as f64 * ratio) as usize;
    let mut resampled = Vec::with_capacity(new_len);
    for i in 0..new_len {
        let src_idx = i as f64 / ratio;
        let idx = src_idx as usize;
        let frac = src_idx - idx as f64;
        let s0 = samples[idx.min(samples.len() - 1)] as f64;
        let s1 = samples[(idx + 1).min(samples.len() - 1)] as f64;
        let val = (s0 + frac * (s1 - s0)) as f32;
        resampled.push(val.clamp(-1.0, 1.0));
    }
    resampled
}

// ---------------------------------------------------------------------------
// Echo suppression — in-process flag replaces file-based polling
// ---------------------------------------------------------------------------

/// Check if playback is active via the shared in-process flag.
/// This replaces the old file-based `/tmp/thermal-audio-state.json` polling.
fn is_playback_active(playback_active: &Arc<AtomicBool>) -> bool {
    playback_active.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// PTT-only daemon mode
// ---------------------------------------------------------------------------

/// Run the push-to-talk daemon (no VAD).
pub async fn run_ptt_daemon(
    voice_cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<VoiceDaemonCommand>,
) -> Result<()> {
    let config = load_voice_config();

    let model_path = resolve_model_path(&config);
    if !model_path.exists() {
        warn!("Whisper model not found at {}", model_path.display());
    }

    write_state(VoiceState::Muted, None);

    let mut recorder = Recorder::new();
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            Some(daemon_cmd) = voice_cmd_rx.recv() => {
                let response = match daemon_cmd.action.as_str() {
                    "start" => handle_start(&mut recorder),
                    "stop" => handle_stop(&mut recorder, &config).await,
                    "toggle" => {
                        if recorder.is_recording() {
                            handle_stop(&mut recorder, &config).await
                        } else {
                            handle_start(&mut recorder)
                        }
                    }
                    "dispatch" => handle_dispatch(&mut recorder, &config).await,
                    "status" => {
                        let state = if recorder.is_recording() {
                            VoiceState::Listening
                        } else {
                            VoiceState::Muted
                        };
                        VoiceSocketResponse::state_response(state)
                    }
                    other => VoiceSocketResponse::error(&format!("unknown action: {other}")),
                };
                let _ = daemon_cmd.reply.send(response);
            }
            _ = &mut shutdown => {
                info!("shutting down PTT...");
                break;
            }
        }
    }

    if recorder.is_recording() {
        recorder.stop();
    }
    write_state(VoiceState::Muted, None);
    Ok(())
}

// ---------------------------------------------------------------------------
// VAD listen daemon mode
// ---------------------------------------------------------------------------

/// Size of each VAD analysis chunk in milliseconds.
const VAD_CHUNK_MS: u32 = 50;

/// Maximum speech duration in seconds before forced reset.
const MAX_SPEECH_SECS: u32 = 30;

/// Run the always-listening VAD daemon.
///
/// `playback_active` is a shared flag set by the playback side when TTS is
/// speaking — used for echo suppression instead of polling a state file.
pub async fn run_listen_daemon(
    threshold: f32,
    use_streaming: bool,
    streaming_url: &str,
    use_wake_word: bool,
    playback_active: Arc<AtomicBool>,
    voice_cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<VoiceDaemonCommand>,
) -> Result<()> {
    let threshold = if threshold <= 0.0 || threshold > 1.0 {
        warn!("VAD threshold {threshold} out of range, using Silero default 0.5");
        0.5
    } else {
        threshold
    };

    let config = load_voice_config();

    if use_streaming {
        info!("streaming STT mode enabled — will connect to {streaming_url} on speech detection");
    } else {
        let model_path = resolve_model_path(&config);
        if !model_path.exists() {
            warn!("Whisper model not found at {}", model_path.display());
        }
    }

    // Initialize wake word detector if enabled
    let mut wake_word_detector: Option<WakeWordDetector> = if use_wake_word {
        match WakeWordDetector::new(SAMPLE_RATE) {
            Ok(det) => {
                if det.is_loaded() {
                    info!("wake word detection enabled (say 'Alfred' to activate)");
                    Some(det)
                } else {
                    warn!("wake word model not loaded — falling back to pure VAD mode");
                    None
                }
            }
            Err(e) => {
                error!("failed to create wake word detector: {e}");
                None
            }
        }
    } else {
        info!("wake word detection disabled");
        None
    };

    let initial_state = if wake_word_detector.is_some() {
        VoiceState::WakeWord
    } else {
        VoiceState::Monitoring
    };
    write_state(initial_state, None);

    // Set up continuous audio capture
    let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(64);

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no default audio input device found")?;
    info!("recording from: {}", device.name().unwrap_or_default());

    let default_config = device
        .default_input_config()
        .context("failed to get default input config")?;
    let native_rate = default_config.sample_rate().0;
    let native_channels = default_config.channels();
    info!("native format: {native_rate}Hz, {native_channels}ch (listen mode)");

    let stream_config = cpal::StreamConfig {
        channels: native_channels,
        sample_rate: cpal::SampleRate(native_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let chunk_size = (native_rate * VAD_CHUNK_MS / 1000) as usize;
    let accumulator: Arc<Mutex<Vec<f32>>> =
        Arc::new(Mutex::new(Vec::with_capacity(chunk_size * 2)));
    let acc_clone = Arc::clone(&accumulator);
    let channels = native_channels;

    let err_fn = |e: cpal::StreamError| {
        error!("audio stream error: {e}");
    };

    let audio_stream = device.build_input_stream(
        &stream_config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let Ok(mut acc) = acc_clone.lock() else {
                return;
            };
            for chunk in data.chunks(channels as usize) {
                let mono: f32 = chunk.iter().sum::<f32>() / channels as f32;
                acc.push(mono);
            }
            while acc.len() >= chunk_size {
                let chunk_data: Vec<f32> = acc.drain(..chunk_size).collect();
                let _ = audio_tx.try_send(chunk_data);
            }
        },
        err_fn,
        None,
    )?;

    audio_stream.play()?;
    info!("continuous audio capture started (VAD mode)");

    // Main loop
    let mut vad = VadDetector::new(threshold, native_rate)
        .ok_or_else(|| anyhow::anyhow!("failed to initialize Silero VAD model"))?;
    let mut speech_buffer: Vec<f32> = Vec::new();
    let mut ptt_recorder = Recorder::new();
    let mut ptt_active = false;
    let mut streaming_transcriber: Option<streaming::StreamingTranscriber> = None;
    let streaming_url_owned = streaming_url.to_owned();
    let mut level_tick: u32 = 0;
    let mut current_voice_state = initial_state;
    let mut echo_check_tick: u32 = 0;
    let mut tts_is_speaking = false;

    let ww_frame_size = wake_word_detector
        .as_ref()
        .map(|d| d.samples_per_frame())
        .unwrap_or(0);
    let mut ww_buffer: Vec<f32> = Vec::with_capacity(ww_frame_size * 2);
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            Some(chunk) = audio_rx.recv() => {
                if ptt_active {
                    continue;
                }

                // Echo suppression: use in-process flag instead of file polling
                echo_check_tick += 1;
                if echo_check_tick >= 20 {
                    echo_check_tick = 0;
                    tts_is_speaking = is_playback_active(&playback_active);
                }
                if tts_is_speaking {
                    continue;
                }

                let rms = crate::vad::rms_energy(&chunk);
                level_tick += 1;
                if level_tick >= 4 {
                    level_tick = 0;
                    let level = (rms.min(1.0) * 1000.0).round() / 1000.0;
                    write_state_with_level(current_voice_state, None, Some(level));
                }

                // Wake word gate
                static WW_FRAMES_FED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if current_voice_state == VoiceState::WakeWord {
                    if let Some(ref mut ww_det) = wake_word_detector {
                        let resampled = resample_f32_for_streaming(
                            &chunk, native_rate, SAMPLE_RATE,
                        );
                        ww_buffer.extend_from_slice(&resampled);

                        while ww_buffer.len() >= ww_frame_size && ww_frame_size > 0 {
                            let frame: Vec<f32> =
                                ww_buffer.drain(..ww_frame_size).collect();
                            let count = WW_FRAMES_FED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if count % 100 == 0 {
                                info!("wake word: fed {count} frames (frame_size={ww_frame_size}, rms={:.4})", crate::vad::rms_energy(&frame));
                            }
                            if ww_det.process_samples(&frame).is_some() {
                                info!("wake word detected! transitioning to VAD listening");
                                current_voice_state = VoiceState::Monitoring;
                                write_state(VoiceState::Monitoring, Some("wake word heard"));
                                vad.reset();
                                ww_buffer.clear();
                                break;
                            }
                        }

                        if ww_buffer.len() > ww_frame_size * 4 {
                            let drain_to = ww_buffer.len() - ww_frame_size * 2;
                            ww_buffer.drain(..drain_to);
                        }
                    }
                    if current_voice_state == VoiceState::WakeWord {
                        continue;
                    }
                }

                let event = vad.process_chunk(&chunk);
                match event {
                    VadEvent::Silence => {}
                    VadEvent::SpeechStart => {
                        info!("VAD: speech detected");
                        current_voice_state = VoiceState::Listening;
                        write_state(VoiceState::Listening, Some("vad"));
                        speech_buffer.clear();
                        speech_buffer.extend_from_slice(&chunk);

                        if use_streaming {
                            match streaming::StreamingTranscriber::new(&streaming_url_owned).await {
                                Ok(mut st) => {
                                    let resampled = resample_f32_for_streaming(
                                        &chunk, native_rate, SAMPLE_RATE,
                                    );
                                    if let Err(e) = st.send_audio(&resampled, SAMPLE_RATE).await {
                                        warn!("failed to send initial audio to streaming STT: {e}");
                                    }
                                    streaming_transcriber = Some(st);
                                }
                                Err(e) => {
                                    error!("failed to connect to streaming STT: {e}");
                                }
                            }
                        }
                    }
                    VadEvent::SpeechContinue => {
                        let max_samples = (native_rate * MAX_SPEECH_SECS) as usize;
                        if speech_buffer.len() >= max_samples {
                            warn!("VAD: speech buffer exceeded {MAX_SPEECH_SECS}s cap, resetting");
                            if let Some(mut st) = streaming_transcriber.take() {
                                let _ = st.close().await;
                            }
                            speech_buffer.clear();
                            vad.reset();
                            current_voice_state = if wake_word_detector.is_some() {
                                VoiceState::WakeWord
                            } else {
                                VoiceState::Monitoring
                            };
                            write_state(current_voice_state, None);
                        } else {
                            speech_buffer.extend_from_slice(&chunk);
                            if let Some(ref mut st) = streaming_transcriber {
                                let resampled = resample_f32_for_streaming(
                                    &chunk, native_rate, SAMPLE_RATE,
                                );
                                if let Err(e) = st.send_audio(&resampled, SAMPLE_RATE).await {
                                    warn!("streaming STT send error: {e}");
                                    streaming_transcriber = None;
                                }
                            }
                        }
                    }
                    VadEvent::SpeechEnd => {
                        info!(
                            "VAD: speech ended ({:.1}s buffered)",
                            speech_buffer.len() as f64 / native_rate as f64
                        );
                        write_state(VoiceState::Processing, Some("transcribing"));

                        let transcript_result: Result<String, anyhow::Error> =
                            if let Some(mut st) = streaming_transcriber.take() {
                                info!("streaming STT: closing connection and collecting finals");
                                if let Err(e) = st.close().await {
                                    warn!("streaming STT close error: {e}");
                                }
                                let text = streaming::drain_final_transcripts(&mut st).await;
                                if text.is_empty() {
                                    Err(anyhow::anyhow!("streaming STT returned empty transcript"))
                                } else {
                                    Ok(text)
                                }
                            } else {
                                let samples_i16 = resample_f32_to_i16(
                                    &speech_buffer,
                                    native_rate,
                                    SAMPLE_RATE,
                                );

                                let min_samples = (SAMPLE_RATE as f64 * 0.3) as usize;
                                if samples_i16.len() < min_samples {
                                    info!("VAD: audio too short (< 0.3s), ignoring");
                                    current_voice_state = if wake_word_detector.is_some() {
                                        VoiceState::WakeWord
                                    } else {
                                        VoiceState::Monitoring
                                    };
                                    write_state(current_voice_state, None);
                                    speech_buffer.clear();
                                    vad.reset();
                                    continue;
                                }

                                let config_cmd = config.whisper_command.clone();
                                let config_model = config.model_path.clone();
                                match tokio::task::spawn_blocking(move || {
                                    let cfg = VoiceConfig {
                                        model_path: config_model,
                                        whisper_command: config_cmd,
                                    };
                                    transcribe(&samples_i16, &cfg)
                                })
                                .await
                                {
                                    Ok(Ok(text)) => Ok(text),
                                    Ok(Err(e)) => Err(e),
                                    Err(e) => Err(anyhow::anyhow!("transcription task panicked: {e}")),
                                }
                            };

                        match transcript_result {
                            Ok(text) => {
                                info!("VAD transcript: {text}");
                                match filter_transcript(&text) {
                                    FilterResult::Accept(cleaned) => {
                                        let lower = cleaned.to_lowercase();
                                        if lower.trim() == "abort"
                                            || lower.trim().ends_with("abort")
                                        {
                                            info!("abort keyword detected — discarding transcript");
                                        } else {
                                            tokio::spawn(dispatch_to_dispatcher(cleaned));
                                        }
                                    }
                                    FilterResult::Reject(reason) => {
                                        info!("transcript filtered out: {reason}");
                                    }
                                }
                            }
                            Err(e) => {
                                error!("VAD transcription failed: {e}");
                            }
                        }

                        current_voice_state = if wake_word_detector.is_some() {
                            VoiceState::WakeWord
                        } else {
                            VoiceState::Monitoring
                        };
                        write_state(current_voice_state, None);
                        speech_buffer.clear();
                        vad.reset();
                        if let Some(ref mut ww_det) = wake_word_detector {
                            ww_det.reset();
                            ww_buffer.clear();
                        }
                    }
                }
            }

            // Socket commands (push-to-talk override + status queries)
            Some(daemon_cmd) = voice_cmd_rx.recv() => {
                let response = match daemon_cmd.action.as_str() {
                    "start" => {
                        if ptt_active {
                            VoiceSocketResponse::ok("already_recording")
                        } else {
                            ptt_active = true;
                            vad.reset();
                            speech_buffer.clear();
                            handle_start(&mut ptt_recorder)
                        }
                    }
                    "stop" => {
                        if ptt_active {
                            let resp = handle_stop(&mut ptt_recorder, &config).await;
                            ptt_active = false;
                            current_voice_state = if wake_word_detector.is_some() {
                                VoiceState::WakeWord
                            } else {
                                VoiceState::Monitoring
                            };
                            write_state(current_voice_state, None);
                            if let Some(ref mut ww_det) = wake_word_detector {
                                ww_det.reset();
                                ww_buffer.clear();
                            }
                            resp
                        } else {
                            VoiceSocketResponse::ok("not_recording")
                        }
                    }
                    "toggle" => {
                        if ptt_active {
                            // Currently recording — stop
                            let resp = handle_stop(&mut ptt_recorder, &config).await;
                            ptt_active = false;
                            current_voice_state = if wake_word_detector.is_some() {
                                VoiceState::WakeWord
                            } else {
                                VoiceState::Monitoring
                            };
                            write_state(current_voice_state, None);
                            if let Some(ref mut ww_det) = wake_word_detector {
                                ww_det.reset();
                                ww_buffer.clear();
                            }
                            resp
                        } else {
                            // Not recording — start
                            ptt_active = true;
                            vad.reset();
                            speech_buffer.clear();
                            handle_start(&mut ptt_recorder)
                        }
                    }
                    "dispatch" => {
                        if ptt_active {
                            let resp = handle_dispatch(&mut ptt_recorder, &config).await;
                            ptt_active = false;
                            current_voice_state = if wake_word_detector.is_some() {
                                VoiceState::WakeWord
                            } else {
                                VoiceState::Monitoring
                            };
                            write_state(current_voice_state, None);
                            if let Some(ref mut ww_det) = wake_word_detector {
                                ww_det.reset();
                                ww_buffer.clear();
                            }
                            resp
                        } else {
                            VoiceSocketResponse::ok("not_recording")
                        }
                    }
                    "status" => {
                        if ptt_active {
                            VoiceSocketResponse::state_response(VoiceState::Listening)
                        } else if vad.is_in_speech() {
                            VoiceSocketResponse::state_response(VoiceState::Listening)
                        } else {
                            VoiceSocketResponse::state_response(current_voice_state)
                        }
                    }
                    other => VoiceSocketResponse::error(&format!("unknown action: {other}")),
                };
                let _ = daemon_cmd.reply.send(response);
            }

            _ = &mut shutdown => {
                info!("shutting down listen daemon...");
                break;
            }
        }
    }

    // Cleanup
    drop(audio_stream);
    if let Some(mut st) = streaming_transcriber.take() {
        let _ = st.close().await;
    }
    if ptt_recorder.is_recording() {
        ptt_recorder.stop();
    }
    write_state(VoiceState::Muted, None);
    info!("voice capture stopped");
    Ok(())
}

/// Returns true if voice capture is actively listening/processing.
/// Used by the playback side to check if it should suppress TTS.
pub fn is_voice_active_from_state() -> bool {
    match read_state_file() {
        Some(sf) => matches!(
            sf.state,
            VoiceState::Listening | VoiceState::Processing | VoiceState::Monitoring
        ),
        None => false,
    }
}


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_serialization_muted() {
        let state = VoiceStateFile {
            state: VoiceState::Muted,
            label: None,
            level: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"muted\""));
    }

    #[test]
    fn state_serialization_listening() {
        let state = VoiceStateFile {
            state: VoiceState::Listening,
            label: None,
            level: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"listening\""));
    }

    #[test]
    fn state_deserialization_roundtrip() {
        for s in [
            VoiceState::Muted,
            VoiceState::Monitoring,
            VoiceState::WakeWord,
            VoiceState::Listening,
            VoiceState::Processing,
        ] {
            let original = VoiceStateFile {
                state: s,
                label: Some("test".to_string()),
                level: None,
            };
            let json = serde_json::to_string(&original).unwrap();
            let parsed: VoiceStateFile = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.state, original.state);
            assert_eq!(parsed.label, original.label);
        }
    }

    #[test]
    fn voice_response_ok() {
        let resp = VoiceSocketResponse::ok("recording");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"recording\""));
    }

    #[test]
    fn voice_response_state() {
        let resp = VoiceSocketResponse::state_response(VoiceState::Listening);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"listening\""));
    }

    #[test]
    fn resample_same_rate() {
        let samples = vec![0.5f32; 100];
        let result = resample_f32_to_i16(&samples, 16000, 16000);
        assert_eq!(result.len(), 100);
        assert!((result[0] - 16383).abs() <= 1);
    }

    #[test]
    fn resample_downsample() {
        let samples = vec![0.25f32; 4800];
        let result = resample_f32_to_i16(&samples, 48000, 16000);
        assert!((result.len() as i64 - 1600).abs() <= 1);
    }

    #[test]
    fn resample_empty() {
        let result = resample_f32_to_i16(&[], 48000, 16000);
        assert!(result.is_empty());
    }

    #[test]
    fn playback_active_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!is_playback_active(&flag));
        flag.store(true, Ordering::Relaxed);
        assert!(is_playback_active(&flag));
    }
}
