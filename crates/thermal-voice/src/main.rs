use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::oneshot;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Thermal Voice — push-to-talk voice input daemon with local Whisper STT.
#[derive(Parser)]
#[command(name = "thermal-voice")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Send start/stop toggle to the running daemon (for Hyprland keybind).
    Toggle,
    /// Print current daemon state and exit.
    Status,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const SAMPLE_RATE: u32 = 16_000;
const CHANNELS: u16 = 1;

/// Default model path under ~/.local/share/thermal/models/
const DEFAULT_MODEL_FILENAME: &str = "ggml-base.en.bin";

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Config {
    /// Path to the whisper.cpp GGML model file.
    model_path: Option<String>,
    /// Name of the whisper CLI binary (default: "whisper-cpp").
    whisper_command: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model_path: None,
            whisper_command: "whisper-cpp".to_string(),
        }
    }
}

fn load_config() -> Config {
    let config_path = config_dir().join("voice.toml");
    if config_path.exists() {
        match fs::read_to_string(&config_path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(cfg) => return cfg,
                Err(e) => warn!("failed to parse {}: {e}", config_path.display()),
            },
            Err(e) => warn!("failed to read {}: {e}", config_path.display()),
        }
    }
    Config::default()
}

fn config_dir() -> PathBuf {
    dirs_config().join("thermal")
}

fn dirs_config() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".config")
        })
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
    dirs_data().join("thermal/models").join(DEFAULT_MODEL_FILENAME)
}

fn resolve_model_path(config: &Config) -> PathBuf {
    config
        .model_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(default_model_path)
}

// ---------------------------------------------------------------------------
// Runtime paths
// ---------------------------------------------------------------------------

fn runtime_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()),
    )
    .join("thermal")
}

fn pidfile_path() -> PathBuf {
    runtime_dir().join("voice.pid")
}

fn socket_path() -> PathBuf {
    runtime_dir().join("voice.sock")
}

const STATE_FILE: &str = "/tmp/thermal-voice-state.json";

// ---------------------------------------------------------------------------
// State file (matches thermal-bar voice module schema)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceState {
    Muted,
    Listening,
    Processing,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VoiceStateFile {
    pub state: VoiceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn write_state(state: VoiceState, label: Option<&str>) {
    let file = VoiceStateFile {
        state,
        label: label.map(String::from),
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
// Socket command protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct SocketCommand {
    pub action: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SocketResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SocketResponse {
    fn ok(status: &str) -> Self {
        Self {
            status: Some(status.to_string()),
            state: None,
            transcript: None,
            error: None,
        }
    }

    fn with_transcript(transcript: String) -> Self {
        Self {
            status: Some("transcribed".to_string()),
            state: None,
            transcript: Some(transcript),
            error: None,
        }
    }

    fn error(msg: &str) -> Self {
        Self {
            status: Some("error".to_string()),
            state: None,
            transcript: None,
            error: Some(msg.to_string()),
        }
    }

    fn state_response(state: VoiceState) -> Self {
        let s = match state {
            VoiceState::Muted => "muted",
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
// Audio recorder (cpal)
// ---------------------------------------------------------------------------

struct Recorder {
    samples: Arc<Mutex<Vec<i16>>>,
    stream: Option<cpal::Stream>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            samples: Arc::new(Mutex::new(Vec::new())),
            stream: None,
        }
    }

    fn start(&mut self) -> Result<()> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no default audio input device found")?;

        info!("recording from: {}", device.name().unwrap_or_default());

        let config = cpal::StreamConfig {
            channels: CHANNELS,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let samples = Arc::clone(&self.samples);
        samples.lock().unwrap().clear();

        let err_fn = |e: cpal::StreamError| {
            error!("audio stream error: {e}");
        };

        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut buf = samples.lock().unwrap();
                for &sample in data {
                    // Convert f32 [-1.0, 1.0] to i16
                    let clamped = sample.clamp(-1.0, 1.0);
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
        // Drop the stream to stop recording
        self.stream.take();
        let samples = self.samples.lock().unwrap().clone();
        let duration_secs = samples.len() as f64 / SAMPLE_RATE as f64;
        info!("recording stopped: {duration_secs:.1}s captured ({} samples)", samples.len());
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
    file.write_all(&16u32.to_le_bytes())?; // chunk size
    file.write_all(&1u16.to_le_bytes())?; // PCM format
    file.write_all(&(CHANNELS as u16).to_le_bytes())?;
    file.write_all(&SAMPLE_RATE.to_le_bytes())?;
    let byte_rate = SAMPLE_RATE * CHANNELS as u32 * 2;
    file.write_all(&byte_rate.to_le_bytes())?;
    let block_align = CHANNELS * 2;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?; // bits per sample

    // data chunk
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;
    for &s in samples {
        file.write_all(&s.to_le_bytes())?;
    }

    Ok(())
}

fn transcribe(samples: &[i16], config: &Config) -> Result<String> {
    let model_path = resolve_model_path(config);

    // Write samples to a temporary WAV file
    let tmp_dir = std::env::temp_dir();
    let wav_path = tmp_dir.join("thermal-voice-recording.wav");
    write_wav(samples, &wav_path).context("writing WAV file")?;

    // Try whisper-cpp CLI first, then whisper CLI
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

                // whisper CLI writes to a .txt file instead of stdout
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
        config_dir().join("voice.toml").display()
    )
}

// ---------------------------------------------------------------------------
// Claude dispatch (shell out to `claude -p`)
// ---------------------------------------------------------------------------

/// Spawn `claude -p` with the transcript and optionally send the response
/// to thermal-audio for TTS. Runs as a background task — does not block
/// the socket response so the caller gets the transcript immediately.
fn spawn_claude_dispatch(transcript: String) {
    tokio::spawn(async move {
        dispatch_to_claude(&transcript).await;
    });
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
                send_to_audio(&response).await;
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

/// Send text to thermal-audio for TTS playback via Unix socket.
async fn send_to_audio(text: &str) {
    let audio_sock = runtime_dir().join("audio.sock");

    match tokio::net::UnixStream::connect(&audio_sock).await {
        Ok(stream) => {
            let msg = serde_json::json!({
                "action": "speak",
                "text": text,
            });
            let (_, mut writer) = stream.into_split();
            let payload = serde_json::to_string(&msg).unwrap_or_default() + "\n";
            if let Err(e) = writer.write_all(payload.as_bytes()).await {
                warn!("failed to write to audio socket: {e}");
            } else {
                info!("sent TTS to thermal-audio: {} chars", text.len());
            }
        }
        Err(e) => {
            warn!(
                "thermal-audio not available at {}: {e}",
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

// ---------------------------------------------------------------------------
// Single-instance guard (pidfile)
// ---------------------------------------------------------------------------

fn check_daemon_running() -> Option<u32> {
    let pidfile = pidfile_path();
    if pidfile.exists() {
        if let Ok(contents) = fs::read_to_string(&pidfile) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                if Path::new(&format!("/proc/{pid}")).exists() {
                    return Some(pid);
                }
            }
        }
        // Stale pidfile
        let _ = fs::remove_file(&pidfile);
    }
    None
}

fn write_pidfile() -> Result<()> {
    let run_dir = runtime_dir();
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("creating runtime dir {:?}", run_dir))?;
    let pidfile = pidfile_path();
    fs::write(&pidfile, std::process::id().to_string())
        .with_context(|| format!("writing pidfile {:?}", pidfile))?;
    Ok(())
}

fn cleanup_pidfile() {
    let _ = fs::remove_file(pidfile_path());
}

// ---------------------------------------------------------------------------
// Socket client (for toggle/status subcommands)
// ---------------------------------------------------------------------------

async fn send_to_daemon(action: &str) -> Result<SocketResponse> {
    let sock = socket_path();
    let stream = tokio::net::UnixStream::connect(&sock)
        .await
        .with_context(|| format!("connecting to daemon socket at {}", sock.display()))?;

    let (reader, mut writer) = stream.into_split();

    let cmd = SocketCommand {
        action: action.to_string(),
    };
    let mut msg = serde_json::to_string(&cmd)?;
    msg.push('\n');
    writer.write_all(msg.as_bytes()).await?;
    writer.shutdown().await?;

    let mut buf_reader = BufReader::new(reader);
    let mut response_line = String::new();
    buf_reader.read_line(&mut response_line).await?;

    let resp: SocketResponse = serde_json::from_str(response_line.trim())?;
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

/// A command sent from socket handler tasks to the main loop which owns the Recorder.
struct DaemonCommand {
    action: String,
    reply: oneshot::Sender<SocketResponse>,
}

async fn run_daemon() -> Result<()> {
    let config = load_config();

    // Check model availability
    let model_path = resolve_model_path(&config);
    if !model_path.exists() {
        warn!(
            "Whisper model not found at {}",
            model_path.display()
        );
        warn!(
            "Download it: curl -L -o {} https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            model_path.display(),
            DEFAULT_MODEL_FILENAME
        );
        warn!("Or set model_path in {}", config_dir().join("voice.toml").display());
        warn!("The daemon will start but transcription will fail until a model or CLI is available.");
    }

    // Single-instance guard
    if let Some(pid) = check_daemon_running() {
        eprintln!("thermal-voice already running (pid {pid}). Exiting.");
        std::process::exit(0);
    }

    write_pidfile()?;

    // Write initial state
    write_state(VoiceState::Muted, None);

    // Set up socket
    let sock_path = socket_path();
    if sock_path.exists() {
        fs::remove_file(&sock_path)
            .with_context(|| format!("removing stale socket {:?}", sock_path))?;
    }
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding socket {:?}", sock_path))?;
    info!("thermal-voice daemon listening on {}", sock_path.display());

    // Channel: socket tasks send commands here, main loop owns the Recorder.
    let (cmd_tx, mut cmd_rx) =
        tokio::sync::mpsc::unbounded_channel::<DaemonCommand>();

    // Socket acceptor task
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let tx = cmd_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, tx).await {
                            warn!("connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    warn!("socket accept error: {e}");
                }
            }
        }
    });

    // Main loop — owns the Recorder (not Send, stays on this task).
    let mut recorder = Recorder::new();
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            Some(daemon_cmd) = cmd_rx.recv() => {
                let response = match daemon_cmd.action.as_str() {
                    "start" => handle_start(&mut recorder),
                    "stop" => handle_stop(&mut recorder, &config).await,
                    "status" => handle_status(&recorder),
                    other => SocketResponse::error(&format!("unknown action: {other}")),
                };
                let _ = daemon_cmd.reply.send(response);
            }
            _ = &mut shutdown => {
                info!("shutting down...");
                break;
            }
        }
    }

    // Cleanup
    if recorder.is_recording() {
        recorder.stop();
    }
    write_state(VoiceState::Muted, None);
    let _ = fs::remove_file(&sock_path);
    cleanup_pidfile();
    info!("thermal-voice daemon stopped");
    Ok(())
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<DaemonCommand>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    buf_reader
        .read_line(&mut line)
        .await
        .context("reading from socket")?;

    if line.trim().is_empty() {
        return Ok(());
    }

    let cmd: SocketCommand = serde_json::from_str(line.trim())
        .with_context(|| format!("parsing command: {}", line.trim()))?;

    let (reply_tx, reply_rx) = oneshot::channel();
    cmd_tx.send(DaemonCommand {
        action: cmd.action,
        reply: reply_tx,
    })?;

    let response = reply_rx.await.unwrap_or_else(|_| {
        SocketResponse::error("daemon dropped the request")
    });

    let mut resp_json = serde_json::to_string(&response)?;
    resp_json.push('\n');
    writer.write_all(resp_json.as_bytes()).await?;
    writer.shutdown().await?;

    Ok(())
}

fn handle_start(recorder: &mut Recorder) -> SocketResponse {
    if recorder.is_recording() {
        return SocketResponse::ok("already_recording");
    }

    match recorder.start() {
        Ok(()) => {
            write_state(VoiceState::Listening, None);
            SocketResponse::ok("recording")
        }
        Err(e) => {
            error!("failed to start recording: {e}");
            write_state(VoiceState::Muted, None);
            SocketResponse::error(&format!("failed to start recording: {e}"))
        }
    }
}

async fn handle_stop(
    recorder: &mut Recorder,
    config: &Config,
) -> SocketResponse {
    if !recorder.is_recording() {
        return SocketResponse::ok("not_recording");
    }

    let samples = recorder.stop();
    write_state(VoiceState::Processing, Some("transcribing"));

    let min_samples = (SAMPLE_RATE as f64 * 0.3) as usize;
    if samples.len() < min_samples {
        write_state(VoiceState::Muted, None);
        return SocketResponse::error("audio too short (< 0.3s)");
    }

    // Run transcription in a blocking thread
    let config_cmd = config.whisper_command.clone();
    let config_model = config.model_path.clone();
    let transcript = tokio::task::spawn_blocking(move || {
        let cfg = Config {
            model_path: config_model,
            whisper_command: config_cmd,
        };
        transcribe(&samples, &cfg)
    })
    .await;

    match transcript {
        Ok(Ok(text)) => {
            copy_to_clipboard(&text);
            info!("transcript: {text}");

            // Fire-and-forget: dispatch to claude -p in the background.
            // The transcript is already on the clipboard and returned to
            // the caller; the claude call updates state and sends TTS
            // independently.
            spawn_claude_dispatch(text.clone());

            SocketResponse::with_transcript(text)
        }
        Ok(Err(e)) => {
            write_state(VoiceState::Muted, None);
            error!("transcription failed: {e}");
            SocketResponse::error(&format!("transcription failed: {e}"))
        }
        Err(e) => {
            write_state(VoiceState::Muted, None);
            error!("transcription task panicked: {e}");
            SocketResponse::error("transcription task panicked")
        }
    }
}

fn handle_status(recorder: &Recorder) -> SocketResponse {
    let state = if recorder.is_recording() {
        VoiceState::Listening
    } else {
        VoiceState::Muted
    };
    SocketResponse::state_response(state)
}

// ---------------------------------------------------------------------------
// Toggle subcommand
// ---------------------------------------------------------------------------

async fn run_toggle() -> Result<()> {
    // Check if daemon is running
    if check_daemon_running().is_none() {
        // Try to read socket anyway (maybe pidfile was cleaned but daemon lives)
        if !socket_path().exists() {
            eprintln!("thermal-voice daemon is not running.");
            eprintln!("Start it with: thermal-voice");
            std::process::exit(1);
        }
    }

    // Read current state
    let current_state = read_state_file()
        .map(|f| f.state)
        .unwrap_or(VoiceState::Muted);

    match current_state {
        VoiceState::Muted => {
            let resp = send_to_daemon("start").await?;
            if let Some(err) = resp.error {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
            println!("listening...");
        }
        VoiceState::Listening => {
            let resp = send_to_daemon("stop").await?;
            if let Some(transcript) = &resp.transcript {
                println!("{transcript}");
            } else if let Some(err) = &resp.error {
                eprintln!("error: {err}");
            }
        }
        VoiceState::Processing => {
            eprintln!("currently processing, please wait...");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Status subcommand
// ---------------------------------------------------------------------------

async fn run_status() -> Result<()> {
    if let Some(pid) = check_daemon_running() {
        println!("thermal-voice daemon running (pid {pid})");
    } else {
        println!("thermal-voice daemon not running");
    }

    if let Some(state) = read_state_file() {
        let s = serde_json::to_string_pretty(&state)?;
        println!("{s}");
    } else {
        println!("no state file");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match cli.command {
        None => run_daemon().await,
        Some(Command::Toggle) => run_toggle().await,
        Some(Command::Status) => run_status().await,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- State file serialization --

    #[test]
    fn state_serialization_muted() {
        let state = VoiceStateFile {
            state: VoiceState::Muted,
            label: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"muted\""));
        assert!(!json.contains("label"));
    }

    #[test]
    fn state_serialization_listening() {
        let state = VoiceStateFile {
            state: VoiceState::Listening,
            label: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"listening\""));
    }

    #[test]
    fn state_serialization_processing() {
        let state = VoiceStateFile {
            state: VoiceState::Processing,
            label: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"processing\""));
    }

    #[test]
    fn state_serialization_with_label() {
        let state = VoiceStateFile {
            state: VoiceState::Listening,
            label: Some("whisper".to_string()),
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"whisper\""));
    }

    #[test]
    fn state_deserialization_roundtrip() {
        for s in [VoiceState::Muted, VoiceState::Listening, VoiceState::Processing] {
            let original = VoiceStateFile {
                state: s,
                label: Some("test".to_string()),
            };
            let json = serde_json::to_string(&original).unwrap();
            let parsed: VoiceStateFile = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.state, original.state);
            assert_eq!(parsed.label, original.label);
        }
    }

    #[test]
    fn state_deserialization_from_bar_format() {
        // The format thermal-bar expects
        let json = r#"{"state": "muted"}"#;
        let parsed: VoiceStateFile = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.state, VoiceState::Muted);
    }

    // -- Command parsing --

    #[test]
    fn parse_socket_command_start() {
        let json = r#"{"action": "start"}"#;
        let cmd: SocketCommand = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.action, "start");
    }

    #[test]
    fn parse_socket_command_stop() {
        let json = r#"{"action": "stop"}"#;
        let cmd: SocketCommand = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.action, "stop");
    }

    #[test]
    fn parse_socket_command_status() {
        let json = r#"{"action": "status"}"#;
        let cmd: SocketCommand = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.action, "status");
    }

    // -- Response serialization --

    #[test]
    fn response_ok_serialization() {
        let resp = SocketResponse::ok("recording");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"recording\""));
        assert!(!json.contains("error"));
        assert!(!json.contains("transcript"));
    }

    #[test]
    fn response_transcript_serialization() {
        let resp = SocketResponse::with_transcript("hello world".to_string());
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("hello world"));
        assert!(json.contains("\"transcribed\""));
    }

    #[test]
    fn response_error_serialization() {
        let resp = SocketResponse::error("something broke");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("something broke"));
        assert!(json.contains("\"error\""));
    }

    #[test]
    fn response_state_serialization() {
        let resp = SocketResponse::state_response(VoiceState::Listening);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"listening\""));
    }

    // -- Config --

    #[test]
    fn default_config() {
        let cfg = Config::default();
        assert!(cfg.model_path.is_none());
        assert_eq!(cfg.whisper_command, "whisper-cpp");
    }

    #[test]
    fn config_deserialization() {
        let toml = r#"
            model_path = "/opt/models/ggml-large.bin"
            whisper_command = "my-whisper"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.model_path.as_deref(), Some("/opt/models/ggml-large.bin"));
        assert_eq!(cfg.whisper_command, "my-whisper");
    }

    #[test]
    fn config_deserialization_partial() {
        let toml = r#"
            model_path = "/my/model.bin"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.model_path.as_deref(), Some("/my/model.bin"));
        assert_eq!(cfg.whisper_command, "whisper-cpp"); // default
    }

    #[test]
    fn config_deserialization_empty() {
        let toml = "";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.model_path.is_none());
    }

    // -- WAV writing --

    #[test]
    fn write_wav_produces_valid_header() {
        let samples = vec![0i16; 16000]; // 1 second of silence
        let tmp = std::env::temp_dir().join("thermal-voice-test.wav");
        write_wav(&samples, &tmp).unwrap();

        let data = fs::read(&tmp).unwrap();
        let _ = fs::remove_file(&tmp);

        // Check RIFF header
        assert_eq!(&data[0..4], b"RIFF");
        assert_eq!(&data[8..12], b"WAVE");
        assert_eq!(&data[12..16], b"fmt ");
        assert_eq!(&data[36..40], b"data");

        // Check data size
        let data_size = u32::from_le_bytes([data[40], data[41], data[42], data[43]]);
        assert_eq!(data_size, 32000); // 16000 samples * 2 bytes
    }

    // -- Path helpers --

    #[test]
    fn runtime_dir_ends_with_thermal() {
        let dir = runtime_dir();
        assert!(dir.ends_with("thermal"));
    }

    #[test]
    fn socket_path_ends_with_voice_sock() {
        let path = socket_path();
        assert_eq!(path.file_name().unwrap(), "voice.sock");
    }

    #[test]
    fn pidfile_path_ends_with_voice_pid() {
        let path = pidfile_path();
        assert_eq!(path.file_name().unwrap(), "voice.pid");
    }

    #[test]
    fn default_model_path_contains_ggml() {
        let path = default_model_path();
        assert!(path.to_str().unwrap().contains("ggml-base.en.bin"));
    }
}
