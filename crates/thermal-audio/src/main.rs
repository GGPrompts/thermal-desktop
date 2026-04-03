use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc;
use std::time::Instant;
use std::{fs, thread};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use thermal_core::{ClaudeSessionState, ClaudeStatePoller, ClaudeStatus};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{debug, info, warn};

mod capture;
mod daemon_client;
mod streaming;
mod transcript_filter;
mod vad;
mod wakeword;

/// Thermal Audio — unified audio daemon (TTS playback + voice capture).
#[derive(Parser)]
#[command(name = "thermal-audio")]
struct Cli {
    /// Speak the given text and exit (for testing).
    #[arg(long)]
    test: Option<String>,

    #[command(subcommand)]
    command: Option<AudioCommand>,
}

#[derive(Subcommand)]
enum AudioCommand {
    /// Send start/stop toggle to the running daemon's voice capture.
    /// Records voice -> transcribes -> types at cursor via wtype.
    Toggle,
    /// Start/stop recording and send transcript to thermal-dispatcher.
    Dispatch,
    /// Print current voice daemon state and exit.
    VoiceStatus,
    /// Run in always-listening mode with Voice Activity Detection.
    Listen {
        /// Silero VAD speech probability threshold (0.0-1.0, default 0.5).
        #[arg(long, default_value = "0.5")]
        threshold: f32,
        /// Use WebSocket streaming STT instead of batch transcription.
        #[arg(long)]
        streaming: bool,
        /// WebSocket URL of the WhisperLiveKit streaming STT server.
        #[arg(long, default_value = streaming::DEFAULT_STREAMING_URL)]
        streaming_url: String,
        /// Disable wake word detection (revert to pure VAD mode).
        #[arg(long)]
        no_wake_word: bool,
    },
}

// ---------------------------------------------------------------------------
// Voice pool
// ---------------------------------------------------------------------------

const VOICES: [&str; 12] = [
    "en-US-GuyNeural",
    "en-US-JennyNeural",
    "en-GB-SoniaNeural",
    "en-GB-RyanNeural",
    "en-AU-NatashaNeural",
    "en-AU-WilliamNeural",
    "en-CA-ClaraNeural",
    "en-CA-LiamNeural",
    "en-IN-NeerjaNeural",
    "en-IN-PrabhatNeural",
    "en-IE-EmilyNeural",
    "en-IE-ConnorNeural",
];

/// Dedicated assistant voice for socket API requests (distinct from agent pool).
const ASSISTANT_VOICE: &str = "en-US-JennyNeural";

struct VoicePool {
    assignments: HashMap<String, usize>,
    next_index: usize,
}

impl VoicePool {
    fn new() -> Self {
        Self {
            assignments: HashMap::new(),
            next_index: 0,
        }
    }

    fn assign(&mut self, session_id: &str) -> &str {
        let idx = *self
            .assignments
            .entry(session_id.to_string())
            .or_insert_with(|| {
                let i = self.next_index;
                self.next_index = (self.next_index + 1) % VOICES.len();
                i
            });
        VOICES[idx]
    }
}

// ---------------------------------------------------------------------------
// Socket API types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TtsRequest {
    text: String,
    voice: Option<String>,
    #[serde(default = "default_priority")]
    priority: Priority,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Priority {
    Normal,
    High,
}

fn default_priority() -> Priority {
    Priority::Normal
}

#[derive(Serialize)]
struct TtsResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Extended socket protocol — discriminated union with backward compatibility.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum SocketMessage {
    /// TTS request (new-style with explicit action).
    Tts {
        text: String,
        voice: Option<String>,
        #[serde(default = "default_priority")]
        priority: Priority,
    },
    /// Toggle mute on/off.
    ToggleMute,
    /// Set mute state explicitly.
    SetMute { muted: bool },
    /// Set volume (0.0 to 1.0).
    SetVolume { value: f32 },
    /// Get current mute/volume status.
    GetStatus,
    // -- Voice capture commands (merged from thermal-voice) --
    /// Toggle PTT recording.
    VoiceToggle,
    /// Start voice recording (PTT).
    VoiceStart,
    /// Stop voice recording (PTT) and transcribe.
    VoiceStop,
    /// Start recording + dispatch to thermal-dispatcher.
    VoiceDispatch,
    /// Set voice capture mode.
    VoiceSetMode {
        #[serde(default)]
        #[allow(dead_code)]
        mode: String,
    },
    /// Get voice capture status.
    VoiceGetStatus,
}

/// Response for control commands (mute/volume/status).
#[derive(Serialize)]
struct ControlResponse {
    ok: bool,
    muted: bool,
    volume: f32,
}

// ---------------------------------------------------------------------------
// Audio state (mute + volume) — shared across threads
// ---------------------------------------------------------------------------

/// Persistent audio state for mute/volume control.
#[derive(Debug, Clone)]
struct AudioState {
    muted: bool,
    volume: f32,
}

impl Default for AudioState {
    fn default() -> Self {
        Self {
            muted: false,
            volume: 1.0,
        }
    }
}

/// Config file path: ~/.config/thermal/audio.toml
fn audio_config_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".config")
        });
    config_dir.join("thermal").join("audio.toml")
}

/// Load audio state from config file, returning default if missing/invalid.
fn load_audio_state() -> AudioState {
    let path = audio_config_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return AudioState::default(),
    };

    let mut state = AudioState::default();
    for line in content.lines() {
        let line = line.trim();
        if let Some(val) = line
            .strip_prefix("muted")
            .and_then(|s| s.trim_start().strip_prefix('='))
        {
            let val = val.trim();
            if val == "true" {
                state.muted = true;
            } else if val == "false" {
                state.muted = false;
            }
        } else if let Some(val) = line
            .strip_prefix("volume")
            .and_then(|s| s.trim_start().strip_prefix('='))
            && let Ok(v) = val.trim().parse::<f32>()
        {
            state.volume = v.clamp(0.0, 1.0);
        }
    }
    state
}

/// Save audio state to config file.
fn save_audio_state(state: &AudioState) {
    let path = audio_config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let content = format!("muted = {}\nvolume = {:.2}\n", state.muted, state.volume);
    if let Err(e) = fs::write(&path, content) {
        warn!("failed to save audio config to {}: {e}", path.display());
    }
}

// ---------------------------------------------------------------------------
// Audio manager
// ---------------------------------------------------------------------------

/// Debounce window per session.
const DEBOUNCE_MS: u128 = 500;

use std::sync::{Arc, Mutex};

/// Shared handle to the currently-playing mpv process.
/// The main (tokio) thread can kill it for high-priority interrupts,
/// while the audio player thread owns the actual playback loop.
type CurrentChild = Arc<Mutex<Option<std::process::Child>>>;

struct AudioManager {
    cache_dir: PathBuf,
    last_play: HashMap<String, Instant>,
    audio_tx: mpsc::Sender<PathBuf>,
    current_child: CurrentChild,
    /// Resolved absolute path to edge-tts binary, or None if not found.
    edge_tts_bin: Option<PathBuf>,
    /// Shared volume percentage (0-100) readable by the audio player thread.
    volume_pct: Arc<AtomicU8>,
}

impl AudioManager {
    fn new(initial_volume: f32) -> Result<Self> {
        let cache_dir = dirs_cache().join("thermal-audio");
        fs::create_dir_all(&cache_dir)
            .with_context(|| format!("creating cache dir {:?}", cache_dir))?;

        // Resolve edge-tts binary — check PATH first, then common locations
        // so it works even when the daemon is spawned with a minimal PATH.
        let edge_tts_bin = find_edge_tts();
        if let Some(ref bin) = edge_tts_bin {
            info!("edge-tts found at {}", bin.display());
        } else {
            warn!("edge-tts not found — TTS generation will be skipped");
        }

        let volume_pct = Arc::new(AtomicU8::new(
            (initial_volume.clamp(0.0, 1.0) * 100.0) as u8,
        ));
        let current_child: CurrentChild = Arc::new(Mutex::new(None));
        let audio_tx = spawn_audio_thread(Arc::clone(&current_child), Arc::clone(&volume_pct));

        Ok(Self {
            cache_dir,
            last_play: HashMap::new(),
            audio_tx,
            current_child,
            edge_tts_bin,
            volume_pct,
        })
    }

    async fn announce(&mut self, session_id: &str, voice: &str, text: &str) -> Result<()> {
        // Debounce check.
        if let Some(last) = self.last_play.get(session_id)
            && last.elapsed().as_millis() < DEBOUNCE_MS
        {
            return Ok(());
        }
        self.last_play
            .insert(session_id.to_string(), Instant::now());

        let path = self.generate_tts(voice, text).await?;
        if let Err(e) = self.audio_tx.send(path) {
            warn!("audio channel send failed: {e}");
        }
        Ok(())
    }

    /// Speak text via the socket API. Supports high-priority (interrupt).
    async fn speak(&mut self, voice: &str, text: &str, high_priority: bool) -> Result<()> {
        let path = self.generate_tts(voice, text).await?;

        if high_priority {
            // Kill current playback from this thread (not the audio thread).
            self.interrupt_current();
        }

        if let Err(e) = self.audio_tx.send(path) {
            warn!("audio channel send failed: {e}");
        }
        Ok(())
    }

    /// Kill the currently-playing mpv process (if any) to allow immediate playback.
    fn interrupt_current(&self) {
        if let Ok(mut guard) = self.current_child.lock() {
            if let Some(ref mut child) = *guard {
                info!("interrupting current playback (pid {})", child.id());
                let _ = child.kill();
                let _ = child.wait();
            }
            *guard = None;
        }
    }

    async fn generate_tts(&self, voice: &str, text: &str) -> Result<PathBuf> {
        let mut hasher = Md5::new();
        hasher.update(voice.as_bytes());
        hasher.update(b"+20%");
        hasher.update(text.as_bytes());
        let hash = format!("{:x}", hasher.finalize());

        let path = self.cache_dir.join(format!("{hash}.mp3"));

        // Return cached file if it exists.
        if path.exists() {
            return Ok(path);
        }

        let bin = self
            .edge_tts_bin
            .as_ref()
            .context("edge-tts not available")?;

        // Use tokio::process::Command so edge-tts runs without blocking
        // the event loop — socket TTS and other transitions stay responsive.
        let status = tokio::process::Command::new(bin)
            .arg("--voice")
            .arg(voice)
            .arg("--rate")
            .arg("+20%")
            .arg("--text")
            .arg(text)
            .arg("--write-media")
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .context("failed to run edge-tts")?;

        if !status.success() {
            anyhow::bail!("edge-tts exited with status {status}");
        }

        Ok(path)
    }
}

/// Spawn a dedicated audio thread that plays files via mpv.
/// The thread registers each spawned mpv process in `current_child` so that
/// external callers (e.g. high-priority interrupt) can kill it.
fn spawn_audio_thread(
    current_child: CurrentChild,
    volume_pct: Arc<AtomicU8>,
) -> mpsc::Sender<PathBuf> {
    let (tx, rx) = mpsc::channel::<PathBuf>();

    thread::Builder::new()
        .name("thermal-audio-player".into())
        .spawn(move || {
            for path in rx {
                let vol = volume_pct.load(Ordering::Relaxed);
                if let Err(e) = play_file(&path, &current_child, vol) {
                    warn!("audio play error: {e}");
                }
            }
        })
        .expect("failed to spawn audio thread");

    tx
}

/// Play an audio file via mpv, registering the child process in the shared
/// mutex so it can be killed externally for interrupt support.
/// `volume` is 0-100, passed to mpv's `--volume` flag.
fn play_file(path: &Path, current_child: &CurrentChild, volume: u8) -> Result<()> {
    let child = std::process::Command::new("mpv")
        .arg("--no-video")
        .arg("--really-quiet")
        .arg(format!("--volume={volume}"))
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn mpv")?;

    // Register child so it can be killed on interrupt.
    {
        let mut guard = current_child.lock().unwrap_or_else(|p| p.into_inner());
        *guard = Some(child);
    }

    // Poll for completion (non-blocking) so that an external kill is
    // reflected promptly rather than blocking forever on wait().
    loop {
        // Take the child out briefly to check status.
        let mut guard = current_child.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_mut() {
            Some(c) => {
                match c.try_wait() {
                    Ok(Some(status)) => {
                        *guard = None;
                        drop(guard);
                        if !status.success() {
                            // Exit code 4 = killed (expected on interrupt).
                            if status.code() != Some(4) {
                                warn!("mpv exited with status {status} for {}", path.display());
                            }
                        }
                        return Ok(());
                    }
                    Ok(None) => {
                        // Still running, keep polling.
                        drop(guard);
                        thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(e) => {
                        *guard = None;
                        drop(guard);
                        anyhow::bail!("failed to wait on mpv: {e}");
                    }
                }
            }
            None => {
                // Child was taken and killed by interrupt handler.
                drop(guard);
                return Ok(());
            }
        }
    }
}

/// Resolve the edge-tts binary — check PATH first, then well-known locations.
/// Returns the absolute path if found, None otherwise.
fn find_edge_tts() -> Option<PathBuf> {
    // Try PATH first (works when launched from a full shell).
    if let Ok(output) = std::process::Command::new("edge-tts")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        if output.success() {
            return Some(PathBuf::from("edge-tts"));
        }
    }

    // Check common locations when PATH is minimal (e.g. spawned from TUI).
    let candidates = [
        dirs_home().join(".local/bin/edge-tts"),
        PathBuf::from("/usr/local/bin/edge-tts"),
        PathBuf::from("/usr/bin/edge-tts"),
    ];
    for path in &candidates {
        if path.is_file() {
            return Some(path.clone());
        }
    }
    None
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/home/builder"))
}

/// Returns `~/.config/thermal` (XDG_CONFIG_HOME/thermal if set).
/// Used by sub-modules (wakeword, capture) via `super::config_dir()`.
fn config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".config")
        });
    base.join("thermal")
}

/// Returns `~/.cache` (or XDG_CACHE_HOME if set).
fn dirs_cache() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache");
    }
    PathBuf::from("/tmp")
}

// ---------------------------------------------------------------------------
// Unix socket listener
// ---------------------------------------------------------------------------

/// Determine the socket path. Uses $XDG_RUNTIME_DIR/thermal/audio.sock,
/// falling back to /run/user/<uid>/thermal/audio.sock.
fn socket_path() -> PathBuf {
    thermal_core::runtime::socket_path("audio")
}

// ---------------------------------------------------------------------------
// Voice state check — suppress TTS while mic is active
// ---------------------------------------------------------------------------

/// Returns true if voice capture is actively listening/processing,
/// so we can suppress TTS announcements that would interfere with
/// recording or be picked up by the microphone.
fn is_voice_active() -> bool {
    capture::is_voice_active_from_state()
}

// ---------------------------------------------------------------------------
// Suppressed announcement queue
// ---------------------------------------------------------------------------

/// Max entries in the suppressed-announcement queue.
const SUPPRESSED_QUEUE_MAX: usize = 8;

/// An announcement that was suppressed during voice activity.
#[derive(Debug, Clone)]
struct SuppressedAnnouncement {
    session_id: String,
    voice: String,
    text: String,
}

/// Bounded queue that holds announcements suppressed while voice is active.
/// Keeps only the latest announcement per session (coalesces duplicates).
struct SuppressedQueue {
    entries: VecDeque<SuppressedAnnouncement>,
    was_voice_active: bool,
}

impl SuppressedQueue {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            was_voice_active: false,
        }
    }

    /// Push an announcement, coalescing by session_id (keeps latest).
    fn push(&mut self, session_id: String, voice: String, text: String) {
        // Remove any existing entry for this session.
        self.entries.retain(|e| e.session_id != session_id);
        // Evict oldest if at capacity.
        if self.entries.len() >= SUPPRESSED_QUEUE_MAX {
            self.entries.pop_front();
        }
        self.entries.push_back(SuppressedAnnouncement {
            session_id,
            voice,
            text,
        });
    }

    /// Check for voice active→inactive transition and drain the queue.
    /// Returns the queued announcements to replay, or empty vec.
    fn check_and_drain(&mut self) -> Vec<SuppressedAnnouncement> {
        let currently_active = is_voice_active();
        let was_active = self.was_voice_active;
        self.was_voice_active = currently_active;

        if was_active && !currently_active && !self.entries.is_empty() {
            info!(
                "voice went inactive — replaying {} suppressed announcement(s)",
                self.entries.len()
            );
            self.entries.drain(..).collect()
        } else if !currently_active {
            // Voice not active and wasn't before — clear any stale entries
            // (shouldn't happen, but defensive).
            self.entries.clear();
            Vec::new()
        } else {
            Vec::new()
        }
    }

    /// Update voice-active tracking without draining (used when we already
    /// checked voice state inline).
    fn track_voice_state(&mut self, currently_active: bool) {
        self.was_voice_active = currently_active;
    }
}

// ---------------------------------------------------------------------------
// State transition announcements
// ---------------------------------------------------------------------------

fn transition_text(
    session_label: &str,
    prev: &ClaudeStatus,
    curr: &ClaudeStatus,
    session: &ClaudeSessionState,
) -> Option<String> {
    match (prev, curr) {
        (ClaudeStatus::Idle, ClaudeStatus::Processing) => {
            Some(format!("{session_label} started working"))
        }
        (ClaudeStatus::Idle, ClaudeStatus::ToolUse)
        | (ClaudeStatus::Processing, ClaudeStatus::ToolUse)
        | (ClaudeStatus::ToolUse, ClaudeStatus::ToolUse) => {
            let tool = session.current_tool.as_deref().unwrap_or("a tool");
            let detail = tool_detail(tool, session);
            if let Some(d) = detail {
                Some(format!("{session_label}: {d}"))
            } else {
                Some(format!("{session_label} using {tool}"))
            }
        }
        (_, ClaudeStatus::AwaitingInput) => Some(format!("{session_label} needs input")),
        (ClaudeStatus::Processing, ClaudeStatus::Idle)
        | (ClaudeStatus::ToolUse, ClaudeStatus::Idle) => Some(format!("{session_label} finished")),
        _ => None,
    }
}

/// Extract a human-friendly description of what a tool is doing.
fn tool_detail(tool: &str, session: &ClaudeSessionState) -> Option<String> {
    let args = session.details.as_ref()?.args.as_ref()?;

    match tool {
        "Read" => {
            let filename = basename(args.file_path.as_deref()?);
            Some(format!("reading {filename}"))
        }
        "Write" => {
            let filename = basename(args.file_path.as_deref()?);
            Some(format!("writing {filename}"))
        }
        "Edit" => {
            let filename = basename(args.file_path.as_deref()?);
            Some(format!("editing {filename}"))
        }
        "Bash" => {
            let desc = args.description.as_deref().or_else(|| {
                args.command
                    .as_deref()
                    .map(|c| if c.len() > 40 { &c[..40] } else { c })
            });
            desc.map(|d| format!("running {d}"))
        }
        "Glob" | "Grep" => {
            let pat = args.pattern.as_deref()?;
            Some(format!("searching {pat}"))
        }
        "Agent" | "Task" => {
            let desc = args.description.as_deref()?;
            Some(format!("agent: {desc}"))
        }
        "WebFetch" | "WebSearch" => Some("web search".to_string()),
        _ => None,
    }
}

/// Get the filename from a path.
fn basename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// Derive a short human-readable label from a session.
/// Prefers the project folder name from working_dir, falls back to short session ID.
/// Prefixes with agent type for non-Claude sessions to distinguish in TTS.
fn session_label(session: &ClaudeSessionState) -> String {
    let prefix = match session.agent_type.as_deref() {
        Some("codex") => "Codex ",
        Some("copilot") => "Copilot ",
        _ => "",
    };

    // Try to extract project folder name from working_dir
    if let Some(ref dir) = session.working_dir
        && let Some(name) = std::path::Path::new(dir).file_name()
        && let Some(s) = name.to_str()
        && !s.is_empty()
    {
        return format!("{prefix}{s}");
    }
    if !session.session_id.is_empty() {
        let short = if session.session_id.len() > 8 {
            &session.session_id[..8]
        } else {
            &session.session_id
        };
        return format!("{prefix}{short}");
    }
    format!("{prefix}session")
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Voice subcommand client (sends to running daemon's voice.sock)
// ---------------------------------------------------------------------------

async fn send_voice_command(action: &str) -> Result<capture::VoiceSocketResponse> {
    let sock = thermal_core::runtime::socket_path("voice");
    let stream = tokio::net::UnixStream::connect(&sock)
        .await
        .with_context(|| format!("connecting to voice socket at {}", sock.display()))?;

    let (reader, mut writer) = stream.into_split();

    let cmd = capture::VoiceSocketCommand {
        action: action.to_string(),
    };
    let mut msg = serde_json::to_string(&cmd)?;
    msg.push('\n');
    writer.write_all(msg.as_bytes()).await?;
    writer.shutdown().await?;

    let mut buf_reader = BufReader::new(reader);
    let mut response_line = String::new();
    buf_reader.read_line(&mut response_line).await?;

    let resp: capture::VoiceSocketResponse = serde_json::from_str(response_line.trim())?;
    Ok(resp)
}

async fn run_voice_toggle() -> Result<()> {
    let status_resp = send_voice_command("status").await?;
    let current_state = status_resp
        .state
        .as_deref()
        .unwrap_or("muted");

    match current_state {
        "muted" | "monitoring" | "wake_word" => {
            let resp = send_voice_command("start").await?;
            if let Some(err) = resp.error {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
            println!("listening...");
        }
        "listening" => {
            let resp = send_voice_command("stop").await?;
            if let Some(transcript) = &resp.transcript {
                println!("{transcript}");
            } else if let Some(err) = &resp.error {
                eprintln!("error: {err}");
            }
        }
        "processing" => {
            eprintln!("currently processing, please wait...");
        }
        _ => {
            eprintln!("unknown voice state: {current_state}");
        }
    }

    Ok(())
}

async fn run_voice_dispatch() -> Result<()> {
    let status_resp = send_voice_command("status").await?;
    let current_state = status_resp
        .state
        .as_deref()
        .unwrap_or("muted");

    match current_state {
        "muted" | "monitoring" | "wake_word" => {
            let resp = send_voice_command("start").await?;
            if let Some(err) = resp.error {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
            println!("listening (dispatch mode)...");
        }
        "listening" => {
            let resp = send_voice_command("dispatch").await?;
            if let Some(transcript) = &resp.transcript {
                println!("dispatched: {transcript}");
            } else if let Some(err) = &resp.error {
                eprintln!("error: {err}");
            }
        }
        "processing" => {
            eprintln!("currently processing, please wait...");
        }
        _ => {}
    }

    Ok(())
}

async fn run_voice_status() -> Result<()> {
    match send_voice_command("status").await {
        Ok(resp) => {
            if let Some(state) = &resp.state {
                println!("voice state: {state}");
            }
        }
        Err(e) => {
            println!("voice capture not running: {e}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async_main())
}

async fn async_main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "thermal_audio=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    // Handle voice subcommands (client-side, talk to running daemon).
    match &cli.command {
        Some(AudioCommand::Toggle) => return run_voice_toggle().await,
        Some(AudioCommand::Dispatch) => return run_voice_dispatch().await,
        Some(AudioCommand::VoiceStatus) => return run_voice_status().await,
        _ => {} // Daemon mode or Listen — continue below
    }

    // Load persisted audio state (mute/volume).
    let audio_state = Arc::new(Mutex::new(load_audio_state()));
    {
        let st = audio_state.lock().unwrap_or_else(|p| p.into_inner());
        info!(
            "loaded audio state: muted={}, volume={:.2}",
            st.muted, st.volume
        );
    }

    let initial_volume = audio_state.lock().unwrap_or_else(|p| p.into_inner()).volume;
    let mut audio = AudioManager::new(initial_volume)?;
    let mut voices = VoicePool::new();

    // --test mode: speak text and exit.
    if let Some(text) = &cli.test {
        let voice = VOICES[0];
        info!("test mode: voice={voice}, text={text:?}");
        match audio.generate_tts(voice, text).await {
            Ok(path) => {
                if let Err(e) = audio.audio_tx.send(path) {
                    warn!("audio send failed: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            }
            Err(e) => {
                warn!("TTS generation failed: {e}");
            }
        }
        return Ok(());
    }

    // Daemon mode — single-instance guard via pidfile.
    thermal_core::runtime::ensure_runtime_dir()?;
    let pidfile = thermal_core::runtime::pidfile_path("audio");
    thermal_core::runtime::enforce_single_instance("thermal-audio");
    thermal_core::runtime::write_pidfile("thermal-audio", &pidfile)
        .with_context(|| format!("writing pidfile {:?}", pidfile))?;

    info!("thermal-audio daemon starting (pid {})", std::process::id());

    // Shared playback-active flag for echo suppression (replaces file polling).
    let playback_active = Arc::new(AtomicBool::new(false));

    // Set up the Unix socket listener.
    let sock_path = socket_path();
    thermal_core::runtime::cleanup_stale_socket("thermal-audio", &sock_path);
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding socket {:?}", sock_path))?;
    info!("socket API listening on {}", sock_path.display());

    let (sock_tx, sock_rx) = tokio::sync::mpsc::unbounded_channel::<TtsRequest>();

    // Voice capture command channel.
    let (voice_cmd_tx, mut voice_cmd_rx) =
        tokio::sync::mpsc::unbounded_channel::<capture::VoiceDaemonCommand>();

    // Spawn voice.sock listener (accepts commands from `thermal-audio toggle` etc.)
    let _voice_sock = capture::spawn_voice_socket_listener(voice_cmd_tx.clone()).await?;

    // Spawn the audio.sock listener task.
    let socket_audio_state = Arc::clone(&audio_state);
    let socket_volume_pct = Arc::clone(&audio.volume_pct);
    let socket_voice_cmd_tx = voice_cmd_tx.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let tx = sock_tx.clone();
                    let state = Arc::clone(&socket_audio_state);
                    let vol_pct = Arc::clone(&socket_volume_pct);
                    let voice_tx = socket_voice_cmd_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_socket_connection(stream, tx, state, vol_pct, voice_tx).await {
                            warn!("socket connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    warn!("socket accept error: {e}");
                }
            }
        }
    });

    // Try connecting to the conductor daemon for event-driven mode.
    let daemon_stream = match daemon_client::connect_and_subscribe().await {
        Ok(Some(stream)) => {
            info!("connected to conductor daemon — using event-driven mode");
            Some(stream)
        }
        Ok(None) => {
            warn!("conductor daemon unavailable — falling back to file-polling mode");
            None
        }
        Err(e) => {
            warn!("daemon connection error: {e} — falling back to file-polling mode");
            None
        }
    };

    // Voice capture runs on the main task (cpal::Stream is not Send).
    // TTS playback also runs on the main task. Both are async and use
    // tokio::select! internally, so we spawn_local the TTS loop and run
    // voice capture directly.
    let playback_active_clone = Arc::clone(&playback_active);

    let tts_handle = tokio::task::spawn_local(async move {
        let result = match daemon_stream {
            Some(stream) => {
                run_daemon_event_loop(stream, &mut audio, &mut voices, audio_state, sock_rx).await
            }
            None => run_poll_loop(&mut audio, &mut voices, audio_state, sock_rx).await,
        };
        if let Err(e) = result {
            warn!("TTS loop error: {e}");
        }
    });

    // Run voice capture on this task (main thread, owns cpal::Stream).
    match &cli.command {
        Some(AudioCommand::Listen {
            threshold,
            streaming,
            streaming_url,
            no_wake_word,
        }) => {
            if let Err(e) = capture::run_listen_daemon(
                *threshold,
                *streaming,
                streaming_url,
                !*no_wake_word,
                playback_active_clone,
                &mut voice_cmd_rx,
            )
            .await
            {
                warn!("voice capture (listen) error: {e}");
            }
        }
        _ => {
            if let Err(e) = capture::run_ptt_daemon(&mut voice_cmd_rx).await {
                warn!("voice capture (PTT) error: {e}");
            }
        }
    }

    // If capture exits, wait for TTS to finish too
    let _ = tts_handle.await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Event-driven mode (daemon semantic events)
// ---------------------------------------------------------------------------

/// Run the main loop using daemon semantic events for state change announcements.
async fn run_daemon_event_loop(
    mut stream: daemon_client::DaemonEventStream,
    audio: &mut AudioManager,
    voices: &mut VoicePool,
    audio_state: Arc<Mutex<AudioState>>,
    mut sock_rx: tokio::sync::mpsc::UnboundedReceiver<TtsRequest>,
) -> Result<()> {
    use daemon_client::*;

    let mut suppressed_queue = SuppressedQueue::new();

    // Track per-session display names and previous activity for announcements.
    let mut session_names: HashMap<String, String> = HashMap::new();
    let mut session_activities: HashMap<String, AgentActivity> = HashMap::new();
    // Track context thresholds to avoid repeat alerts (keyed by session_id).
    let mut prev_context_level: HashMap<String, ContextThreshold> = HashMap::new();

    let mut voice_check = tokio::time::interval(std::time::Duration::from_millis(500));
    voice_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;

            // Socket TTS requests (direct API calls) — highest priority.
            Some(req) = sock_rx.recv() => {
                let voice = req.voice.as_deref().unwrap_or(ASSISTANT_VOICE);
                let high_priority = req.priority == Priority::High;
                info!("socket TTS: voice={voice}, priority={:?}, text={:?}", req.priority, req.text);
                let is_muted = audio_state.lock().unwrap_or_else(|p| p.into_inner()).muted;
                if !is_muted {
                    if let Err(e) = audio.speak(voice, &req.text, high_priority).await {
                        warn!("socket TTS failed: {e}");
                    }
                } else {
                    info!("socket TTS skipped (muted)");
                }
            }

            // Replay suppressed announcements when voice goes inactive.
            _ = voice_check.tick() => {
                let replays = suppressed_queue.check_and_drain();
                if !replays.is_empty() {
                    let is_muted = audio_state.lock().unwrap_or_else(|p| p.into_inner()).muted;
                    if !is_muted {
                        for entry in replays {
                            info!("replaying suppressed: [{}] {}", entry.session_id, entry.text);
                            if let Err(e) = audio.announce(&entry.session_id, &entry.voice, &entry.text).await {
                                warn!("replay announce failed: {e}");
                            }
                        }
                    }
                }
            }

            // Daemon event stream.
            msg = stream.next_message() => {
                match msg {
                    Ok(Some(DaemonMessage::Snapshot(sync))) => {
                        let snap = &sync.snapshot;
                        let label = daemon_session_label(snap);
                        debug!("snapshot: {} ({}) activity={:?}", snap.session_id, label, snap.agent_activity);
                        session_names.insert(snap.session_id.clone(), label);
                        session_activities.insert(snap.session_id.clone(), snap.agent_activity.clone());
                    }
                    Ok(Some(DaemonMessage::Events(batch))) => {
                        for event in batch.events {
                            let sid = &event.session_id;
                            let label = session_names.get(sid).cloned()
                                .unwrap_or_else(|| short_id(sid));

                            let text = match &event.kind {
                                SemanticEventKind::SessionSpawned { display_name, .. } => {
                                    let name = display_name.as_deref().unwrap_or(&label);
                                    session_names.insert(sid.clone(), name.to_string());
                                    Some(format!("{name} started"))
                                }
                                SemanticEventKind::SessionExited { .. } => {
                                    session_activities.remove(sid);
                                    Some(format!("{label} exited"))
                                }
                                SemanticEventKind::AgentActivityChanged { activity, previous } => {
                                    let prev = previous.as_ref()
                                        .or_else(|| session_activities.get(sid))
                                        .cloned()
                                        .unwrap_or(AgentActivity::Idle);
                                    session_activities.insert(sid.clone(), activity.clone());
                                    daemon_activity_text(&label, &prev, activity)
                                }
                                SemanticEventKind::ToolStarted { tool_name } => {
                                    session_activities.insert(sid.clone(), AgentActivity::ToolRunning);
                                    Some(format!("{label} using {tool_name}"))
                                }
                                SemanticEventKind::ContextThresholdCrossed { level, saturation } => {
                                    // Only announce if this is a new/higher threshold.
                                    let dominated = match (&prev_context_level.get(sid), level) {
                                        (Some(ContextThreshold::Critical), _) => true,
                                        (Some(ContextThreshold::Warning), ContextThreshold::Warning) => true,
                                        _ => false,
                                    };
                                    if dominated {
                                        None
                                    } else {
                                        prev_context_level.insert(sid.clone(), level.clone());
                                        let pct = saturation.map(|s| (s * 100.0) as u32).unwrap_or(0);
                                        let urgency = match level {
                                            ContextThreshold::Critical => "Alert",
                                            ContextThreshold::Warning => "Warning",
                                        };
                                        Some(format!("{urgency}, {label} at {pct}% context"))
                                    }
                                }
                                _ => None,
                            };

                            if let Some(text) = text {
                                info!("[{sid}] daemon event: {text}");
                                let is_muted = audio_state.lock().unwrap_or_else(|p| p.into_inner()).muted;
                                let voice_active = is_voice_active();
                                suppressed_queue.track_voice_state(voice_active);
                                if !is_muted && !voice_active {
                                    let voice = voices.assign(sid);
                                    if let Err(e) = audio.announce(sid, voice, &text).await {
                                        warn!("announce failed: {e}");
                                    }
                                } else if voice_active && !is_muted {
                                    let voice = voices.assign(sid).to_string();
                                    info!("queued suppressed announcement (voice active): {text}");
                                    suppressed_queue.push(sid.to_string(), voice, text);
                                }
                            }
                        }
                    }
                    Ok(Some(DaemonMessage::SessionExited { id, .. })) => {
                        let label = session_names.remove(&id).unwrap_or_else(|| short_id(&id));
                        session_activities.remove(&id);
                        prev_context_level.remove(&id);
                        let text = format!("{label} exited");
                        info!("[{id}] daemon event: {text}");
                        let is_muted = audio_state.lock().unwrap_or_else(|p| p.into_inner()).muted;
                        let voice_active = is_voice_active();
                        suppressed_queue.track_voice_state(voice_active);
                        if !is_muted && !voice_active {
                            let voice = voices.assign(&id);
                            if let Err(e) = audio.announce(&id, voice, &text).await {
                                warn!("announce failed: {e}");
                            }
                        } else if voice_active && !is_muted {
                            let voice = voices.assign(&id).to_string();
                            info!("queued suppressed announcement (voice active): {text}");
                            suppressed_queue.push(id.clone(), voice, text);
                        }
                    }
                    Ok(None) => {
                        warn!("daemon disconnected — will not reconnect (restart thermal-audio to retry)");
                        return Ok(());
                    }
                    Err(e) => {
                        warn!("daemon stream error: {e} — continuing");
                    }
                }
            }
        }
    }
}

/// Map daemon AgentActivity transitions to TTS text, analogous to `transition_text`.
fn daemon_activity_text(
    label: &str,
    prev: &daemon_client::AgentActivity,
    curr: &daemon_client::AgentActivity,
) -> Option<String> {
    use daemon_client::AgentActivity::*;
    match (prev, curr) {
        (Idle, Thinking) | (Idle, StreamingOutput) | (WaitingInput, Thinking) => {
            Some(format!("{label} started working"))
        }
        (_, ToolRunning) => {
            // ToolStarted events provide tool_name; this is the fallback.
            Some(format!("{label} running a tool"))
        }
        (_, WaitingInput) | (_, Prompting) => Some(format!("{label} needs input")),
        (Thinking, Idle) | (ToolRunning, Idle) | (StreamingOutput, Idle) => {
            Some(format!("{label} finished"))
        }
        (_, Exited) => Some(format!("{label} exited")),
        _ => None,
    }
}

/// Derive a label from a daemon session snapshot.
fn daemon_session_label(snap: &daemon_client::SemanticSessionSnapshot) -> String {
    if let Some(ref name) = snap.display_name {
        if !name.is_empty() {
            return name.clone();
        }
    }
    if let Some(ref cwd) = snap.cwd {
        if let Some(name) = std::path::Path::new(cwd)
            .file_name()
            .and_then(|n| n.to_str())
        {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    short_id(&snap.session_id)
}

/// Short session ID for labels.
fn short_id(id: &str) -> String {
    if id.len() > 8 {
        id[..8].to_string()
    } else {
        id.to_string()
    }
}

// ---------------------------------------------------------------------------
// Poll-based fallback mode (ClaudeStatePoller)
// ---------------------------------------------------------------------------

/// Run the main loop using file-polling for state change announcements.
/// This is the compatibility fallback when the conductor daemon is unavailable.
/// Sessions here are file-derived (no `source` tagging). The daemon event
/// loop (`run_daemon_event_loop`) is preferred when the daemon is running.
async fn run_poll_loop(
    audio: &mut AudioManager,
    voices: &mut VoicePool,
    audio_state: Arc<Mutex<AudioState>>,
    mut sock_rx: tokio::sync::mpsc::UnboundedReceiver<TtsRequest>,
) -> Result<()> {
    let mut poller = ClaudeStatePoller::new().context("creating state poller")?;
    let mut prev_states: HashMap<String, (ClaudeStatus, Option<String>)> = HashMap::new();
    let mut prev_context_alert: HashMap<String, u32> = HashMap::new();
    let mut suppressed_queue = SuppressedQueue::new();

    // Seed initial states without announcing.
    for session in poller.poll() {
        if !session.session_id.is_empty() {
            prev_states.insert(
                session.session_id.clone(),
                (session.status.clone(), session.current_tool.clone()),
            );
            if let Some(pct) = session.context_percent {
                let threshold = context_threshold(pct as u32);
                prev_context_alert.insert(session.session_id.clone(), threshold);
            }
        }
    }

    let mut poll_interval = tokio::time::interval(std::time::Duration::from_millis(500));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;

            Some(req) = sock_rx.recv() => {
                let voice = req.voice.as_deref().unwrap_or(ASSISTANT_VOICE);
                let high_priority = req.priority == Priority::High;
                info!("socket TTS: voice={voice}, priority={:?}, text={:?}", req.priority, req.text);
                let is_muted = audio_state.lock().unwrap_or_else(|p| p.into_inner()).muted;
                if !is_muted {
                    if let Err(e) = audio.speak(voice, &req.text, high_priority).await {
                        warn!("socket TTS failed: {e}");
                    }
                } else {
                    info!("socket TTS skipped (muted)");
                }
            }

            _ = poll_interval.tick() => {
                // Replay suppressed announcements on voice active→inactive transition.
                let replays = suppressed_queue.check_and_drain();
                if !replays.is_empty() {
                    let is_muted = audio_state.lock().unwrap_or_else(|p| p.into_inner()).muted;
                    if !is_muted {
                        for entry in replays {
                            info!("replaying suppressed: [{}] {}", entry.session_id, entry.text);
                            if let Err(e) = audio.announce(&entry.session_id, &entry.voice, &entry.text).await {
                                warn!("replay announce failed: {e}");
                            }
                        }
                    }
                }

                let sessions = poller.poll();

                for session in &sessions {
                    if session.session_id.is_empty() {
                        continue;
                    }

                    let label = session_label(session);

                    // State transition announcements
                    let prev = prev_states
                        .get(&session.session_id)
                        .cloned()
                        .unwrap_or((ClaudeStatus::Idle, None));

                    let curr_tool = session.current_tool.clone();
                    let changed = prev.0 != session.status
                        || (session.status == ClaudeStatus::ToolUse && prev.1 != curr_tool);

                    if changed {
                        if let Some(text) = transition_text(&label, &prev.0, &session.status, session) {
                            info!("[{}] {} -> {:?}: {text}", session.session_id, format!("{prev:?}"), session.status);
                            let is_muted = audio_state.lock().unwrap_or_else(|p| p.into_inner()).muted;
                            let voice_active = is_voice_active();
                            suppressed_queue.track_voice_state(voice_active);
                            if !is_muted && !voice_active {
                                let voice = voices.assign(&session.session_id);
                                if let Err(e) = audio.announce(&session.session_id, voice, &text).await {
                                    warn!("announce failed: {e}");
                                }
                            } else if voice_active && !is_muted {
                                let voice = voices.assign(&session.session_id).to_string();
                                info!("queued suppressed announcement (voice active): {text}");
                                suppressed_queue.push(session.session_id.clone(), voice, text);
                            }
                        }
                        prev_states.insert(session.session_id.clone(), (session.status.clone(), curr_tool.clone()));
                    }

                    // Context % alerts at 50%, 75%, 90%
                    if let Some(pct) = session.context_percent {
                        let pct = pct as u32;
                        let threshold = context_threshold(pct);
                        let prev_threshold = prev_context_alert.get(&session.session_id).copied().unwrap_or(0);

                        if threshold > prev_threshold {
                            let urgency = if pct >= 90 { "Alert" } else { "Warning" };
                            let text = format!("{urgency}, {label} at {pct}% context");
                            info!("[{}] context alert: {text}", session.session_id);
                            let is_muted = audio_state.lock().unwrap_or_else(|p| p.into_inner()).muted;
                            let voice_active = is_voice_active();
                            suppressed_queue.track_voice_state(voice_active);
                            if !is_muted && !voice_active {
                                let voice = voices.assign(&session.session_id);
                                if let Err(e) = audio.announce(&format!("{}-ctx", session.session_id), voice, &text).await {
                                    warn!("context announce failed: {e}");
                                }
                            } else if voice_active && !is_muted {
                                let voice = voices.assign(&session.session_id).to_string();
                                info!("queued suppressed context alert (voice active): {text}");
                                suppressed_queue.push(format!("{}-ctx", session.session_id), voice, text);
                            }
                            prev_context_alert.insert(session.session_id.clone(), threshold);
                        }
                    }
                }

                // Clean up sessions that are no longer present.
                let active_ids: Vec<String> = sessions
                    .iter()
                    .filter(|s| !s.session_id.is_empty())
                    .map(|s| s.session_id.clone())
                    .collect();
                prev_states.retain(|id, _| active_ids.contains(id));
                prev_context_alert.retain(|id, _| active_ids.contains(id));
            }
        }
    }
}

/// Handle a single socket connection: read one JSON line, parse, forward/handle, respond.
///
/// Control messages (mute/volume/status) are handled directly here.
/// TTS messages are forwarded to the main loop via the channel.
async fn handle_socket_connection(
    stream: tokio::net::UnixStream,
    tx: tokio::sync::mpsc::UnboundedSender<TtsRequest>,
    audio_state: Arc<Mutex<AudioState>>,
    volume_pct: Arc<AtomicU8>,
    voice_cmd_tx: tokio::sync::mpsc::UnboundedSender<capture::VoiceDaemonCommand>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    // Read one line (newline-delimited JSON).
    buf_reader
        .read_line(&mut line)
        .await
        .context("reading from socket")?;

    let line = line.trim();
    if line.is_empty() {
        let resp = serde_json::to_string(&TtsResponse {
            ok: false,
            error: Some("empty request".to_string()),
        })?;
        writer.write_all(resp.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        return Ok(());
    }

    // Try parsing as the new SocketMessage format first, then fall back to
    // the legacy TtsRequest format (no "action" field) for backward compat.
    let message = match serde_json::from_str::<SocketMessage>(line) {
        Ok(msg) => msg,
        Err(_) => {
            // Backward compatibility: try parsing as legacy TtsRequest.
            match serde_json::from_str::<TtsRequest>(line) {
                Ok(req) => SocketMessage::Tts {
                    text: req.text,
                    voice: req.voice,
                    priority: req.priority,
                },
                Err(e) => {
                    let resp = serde_json::to_string(&TtsResponse {
                        ok: false,
                        error: Some(format!("invalid JSON: {e}")),
                    })?;
                    writer.write_all(resp.as_bytes()).await?;
                    writer.write_all(b"\n").await?;
                    return Ok(());
                }
            }
        }
    };

    match message {
        SocketMessage::Tts {
            text,
            voice,
            priority,
        } => {
            if text.is_empty() {
                let resp = serde_json::to_string(&TtsResponse {
                    ok: false,
                    error: Some("text field is empty".to_string()),
                })?;
                writer.write_all(resp.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                return Ok(());
            }

            tx.send(TtsRequest {
                text,
                voice,
                priority,
            })
            .context("forwarding TTS request to main loop")?;

            let resp = serde_json::to_string(&TtsResponse {
                ok: true,
                error: None,
            })?;
            writer.write_all(resp.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        SocketMessage::ToggleMute => {
            let state = {
                let mut st = audio_state.lock().unwrap_or_else(|p| p.into_inner());
                st.muted = !st.muted;
                info!("toggle_mute: muted={}", st.muted);
                st.clone()
            };
            save_audio_state(&state);
            let resp = serde_json::to_string(&ControlResponse {
                ok: true,
                muted: state.muted,
                volume: state.volume,
            })?;
            writer.write_all(resp.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        SocketMessage::SetMute { muted } => {
            let state = {
                let mut st = audio_state.lock().unwrap_or_else(|p| p.into_inner());
                st.muted = muted;
                info!("set_mute: muted={muted}");
                st.clone()
            };
            save_audio_state(&state);
            let resp = serde_json::to_string(&ControlResponse {
                ok: true,
                muted: state.muted,
                volume: state.volume,
            })?;
            writer.write_all(resp.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        SocketMessage::SetVolume { value } => {
            let clamped = value.clamp(0.0, 1.0);
            let state = {
                let mut st = audio_state.lock().unwrap_or_else(|p| p.into_inner());
                st.volume = clamped;
                info!("set_volume: volume={clamped:.2}");
                st.clone()
            };
            // Update the atomic volume for the audio player thread.
            volume_pct.store((clamped * 100.0) as u8, Ordering::Relaxed);
            save_audio_state(&state);
            let resp = serde_json::to_string(&ControlResponse {
                ok: true,
                muted: state.muted,
                volume: state.volume,
            })?;
            writer.write_all(resp.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        SocketMessage::GetStatus => {
            let state = audio_state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            let resp = serde_json::to_string(&ControlResponse {
                ok: true,
                muted: state.muted,
                volume: state.volume,
            })?;
            writer.write_all(resp.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        // Voice capture commands — forwarded to the capture module.
        SocketMessage::VoiceToggle
        | SocketMessage::VoiceStart
        | SocketMessage::VoiceStop
        | SocketMessage::VoiceDispatch
        | SocketMessage::VoiceGetStatus
        | SocketMessage::VoiceSetMode { .. } => {
            let action = match &message {
                SocketMessage::VoiceToggle => "start", // toggle logic is client-side
                SocketMessage::VoiceStart => "start",
                SocketMessage::VoiceStop => "stop",
                SocketMessage::VoiceDispatch => "dispatch",
                SocketMessage::VoiceGetStatus => "status",
                SocketMessage::VoiceSetMode { .. } => "status", // placeholder
                _ => unreachable!(),
            };
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = voice_cmd_tx.send(capture::VoiceDaemonCommand {
                action: action.to_string(),
                reply: reply_tx,
            });
            let voice_resp = reply_rx
                .await
                .unwrap_or_else(|_| capture::VoiceSocketResponse::error("voice capture not running"));
            let resp = serde_json::to_string(&voice_resp)?;
            writer.write_all(resp.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
    }

    Ok(())
}

/// Map a context percentage to the highest alert threshold crossed.
fn context_threshold(pct: u32) -> u32 {
    if pct >= 90 {
        90
    } else if pct >= 75 {
        75
    } else if pct >= 50 {
        50
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use thermal_core::{ClaudeSessionState, ClaudeStatus, ToolArgs, ToolDetails};

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Build a minimal ClaudeSessionState for use in tests.
    fn make_session(id: &str) -> ClaudeSessionState {
        ClaudeSessionState {
            session_id: id.to_string(),
            ..Default::default()
        }
    }

    /// Build a session with a specific working_dir.
    fn make_session_with_dir(id: &str, dir: &str) -> ClaudeSessionState {
        ClaudeSessionState {
            session_id: id.to_string(),
            working_dir: Some(dir.to_string()),
            ..Default::default()
        }
    }

    /// Build a session with a current_tool and optional ToolDetails/ToolArgs.
    fn make_tool_session(id: &str, tool: &str, args: ToolArgs) -> ClaudeSessionState {
        ClaudeSessionState {
            session_id: id.to_string(),
            current_tool: Some(tool.to_string()),
            details: Some(ToolDetails {
                event: None,
                tool: Some(tool.to_string()),
                args: Some(args),
            }),
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // VoicePool: round-robin assignment
    // -----------------------------------------------------------------------

    #[test]
    fn voice_pool_round_robin_first_n_sessions() {
        let mut pool = VoicePool::new();
        // First VOICES.len() unique sessions should get distinct voices in order.
        let voices_assigned: Vec<String> = (0..VOICES.len())
            .map(|i| pool.assign(&format!("session-{i}")).to_string())
            .collect();

        // Each assigned voice must equal the corresponding entry in VOICES.
        for (i, v) in voices_assigned.iter().enumerate() {
            assert_eq!(v.as_str(), VOICES[i], "slot {i} should map to VOICES[{i}]");
        }
    }

    #[test]
    fn voice_pool_wraps_around_after_exhaustion() {
        let mut pool = VoicePool::new();
        // Consume all 12 voices.
        for i in 0..VOICES.len() {
            pool.assign(&format!("session-{i}"));
        }
        // The (n+1)th new session should wrap to VOICES[0].
        let wrapped = pool.assign("new-session").to_string();
        assert_eq!(wrapped, VOICES[0]);
    }

    #[test]
    fn voice_pool_consistent_mapping_same_session() {
        let mut pool = VoicePool::new();
        let first = pool.assign("my-session").to_string();
        let second = pool.assign("my-session").to_string();
        let third = pool.assign("my-session").to_string();
        assert_eq!(first, second, "same session must return same voice");
        assert_eq!(second, third, "same session must always return same voice");
    }

    #[test]
    fn voice_pool_different_sessions_get_different_voices() {
        let mut pool = VoicePool::new();
        let v0 = pool.assign("alpha").to_string();
        let v1 = pool.assign("beta").to_string();
        // They should be consecutive entries in VOICES.
        assert_eq!(v0, VOICES[0]);
        assert_eq!(v1, VOICES[1]);
        assert_ne!(v0, v1);
    }

    #[test]
    fn voice_pool_interleaved_lookups_stable() {
        let mut pool = VoicePool::new();
        // Assign three distinct sessions.
        let a1 = pool.assign("a").to_string();
        let b1 = pool.assign("b").to_string();
        let c1 = pool.assign("c").to_string();
        // Look them up again in different order.
        let c2 = pool.assign("c").to_string();
        let a2 = pool.assign("a").to_string();
        let b2 = pool.assign("b").to_string();
        assert_eq!(a1, a2);
        assert_eq!(b1, b2);
        assert_eq!(c1, c2);
    }

    #[test]
    fn voice_pool_covers_full_voice_list() {
        // After assigning VOICES.len() unique sessions, every VOICES entry
        // should have been used exactly once.
        let mut pool = VoicePool::new();
        let mut used: Vec<String> = (0..VOICES.len())
            .map(|i| pool.assign(&format!("s-{i}")).to_string())
            .collect();
        used.sort_unstable();
        let mut expected: Vec<String> = VOICES.iter().map(|s| s.to_string()).collect();
        expected.sort_unstable();
        assert_eq!(used, expected);
    }

    // -----------------------------------------------------------------------
    // Cache key generation (MD5 of voice + "+20%" + text)
    // -----------------------------------------------------------------------

    /// Compute the expected MD5 hex string for a given (voice, text) pair.
    fn expected_hash(voice: &str, text: &str) -> String {
        let mut hasher = md5::Md5::new();
        md5::Digest::update(&mut hasher, voice.as_bytes());
        md5::Digest::update(&mut hasher, b"+20%");
        md5::Digest::update(&mut hasher, text.as_bytes());
        format!("{:x}", md5::Digest::finalize(hasher))
    }

    #[test]
    fn cache_key_includes_voice_and_text() {
        // Different voices → different hashes even with the same text.
        let h1 = expected_hash("en-US-GuyNeural", "hello");
        let h2 = expected_hash("en-GB-SoniaNeural", "hello");
        assert_ne!(h1, h2);
    }

    #[test]
    fn cache_key_includes_text() {
        // Same voice, different text → different hash.
        let h1 = expected_hash("en-US-GuyNeural", "hello");
        let h2 = expected_hash("en-US-GuyNeural", "world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn cache_key_is_deterministic() {
        // Same inputs always produce the same hash.
        let h1 = expected_hash("en-US-JennyNeural", "test phrase");
        let h2 = expected_hash("en-US-JennyNeural", "test phrase");
        assert_eq!(h1, h2);
    }

    #[test]
    fn cache_key_rate_is_embedded() {
        // A hypothetical hash without the "+20%" separator would differ.
        let with_rate = expected_hash("en-US-GuyNeural", "hi");

        let mut hasher = md5::Md5::new();
        md5::Digest::update(&mut hasher, b"en-US-GuyNeural");
        // No "+20%" separator.
        md5::Digest::update(&mut hasher, b"hi");
        let without_rate = format!("{:x}", md5::Digest::finalize(hasher));

        assert_ne!(
            with_rate, without_rate,
            "rate separator must be part of the key"
        );
    }

    #[test]
    fn cache_key_is_lowercase_hex() {
        let h = expected_hash("en-US-GuyNeural", "something");
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "hash must be lowercase hex: {h}"
        );
    }

    #[test]
    fn cache_key_is_32_chars() {
        // MD5 produces 128 bits = 32 hex characters.
        let h = expected_hash("en-US-GuyNeural", "text");
        assert_eq!(h.len(), 32, "MD5 hex must be 32 chars");
    }

    // -----------------------------------------------------------------------
    // State transition announcement text
    // -----------------------------------------------------------------------

    #[test]
    fn transition_idle_to_processing() {
        let session = make_session("s1");
        let text = transition_text(
            "myproject",
            &ClaudeStatus::Idle,
            &ClaudeStatus::Processing,
            &session,
        );
        assert_eq!(text, Some("myproject started working".to_string()));
    }

    #[test]
    fn transition_processing_to_tool_use_no_detail() {
        let mut session = make_session("s1");
        session.current_tool = Some("Read".to_string());
        // No ToolDetails / ToolArgs → falls back to "using <tool>".
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        );
        assert_eq!(text, Some("proj using Read".to_string()));
    }

    #[test]
    fn transition_processing_to_tool_use_with_file_path() {
        let args = ToolArgs {
            file_path: Some("/home/user/project/main.rs".to_string()),
            ..Default::default()
        };
        let session = make_tool_session("s1", "Read", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        );
        assert_eq!(text, Some("proj: reading main.rs".to_string()));
    }

    #[test]
    fn transition_tool_use_to_tool_use_write() {
        let args = ToolArgs {
            file_path: Some("/some/path/foo.txt".to_string()),
            ..Default::default()
        };
        let session = make_tool_session("s1", "Write", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::ToolUse,
            &ClaudeStatus::ToolUse,
            &session,
        );
        assert_eq!(text, Some("proj: writing foo.txt".to_string()));
    }

    #[test]
    fn transition_tool_use_to_tool_use_edit() {
        let args = ToolArgs {
            file_path: Some("/path/to/lib.rs".to_string()),
            ..Default::default()
        };
        let session = make_tool_session("s1", "Edit", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::ToolUse,
            &ClaudeStatus::ToolUse,
            &session,
        );
        assert_eq!(text, Some("proj: editing lib.rs".to_string()));
    }

    #[test]
    fn transition_tool_use_bash_with_description() {
        let args = ToolArgs {
            description: Some("Build all crates".to_string()),
            command: Some("cargo build".to_string()),
            ..Default::default()
        };
        let session = make_tool_session("s1", "Bash", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        );
        // description takes priority over command
        assert_eq!(text, Some("proj: running Build all crates".to_string()));
    }

    #[test]
    fn transition_tool_use_bash_falls_back_to_command() {
        let args = ToolArgs {
            command: Some("cargo test".to_string()),
            ..Default::default()
        };
        let session = make_tool_session("s1", "Bash", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        );
        assert_eq!(text, Some("proj: running cargo test".to_string()));
    }

    #[test]
    fn transition_tool_use_bash_truncates_long_command() {
        // Build a command longer than 40 chars using two distinct halves so we
        // can assert that only the first 40 chars appear in the output.
        let first_half = "a".repeat(40);
        let second_half = "b".repeat(20);
        let long_cmd = format!("{first_half}{second_half}");
        let args = ToolArgs {
            command: Some(long_cmd.clone()),
            ..Default::default()
        };
        let session = make_tool_session("s1", "Bash", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        )
        .unwrap();
        // The first 40 chars must appear in the output.
        assert!(
            text.contains(&long_cmd[..40]),
            "truncated prefix should be present"
        );
        // The second half (which was cut off) must not appear.
        assert!(
            !text.contains(&second_half),
            "tail beyond 40 chars must be absent"
        );
    }

    #[test]
    fn transition_tool_use_glob_with_pattern() {
        let args = ToolArgs {
            pattern: Some("**/*.rs".to_string()),
            ..Default::default()
        };
        let session = make_tool_session("s1", "Glob", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        );
        assert_eq!(text, Some("proj: searching **/*.rs".to_string()));
    }

    #[test]
    fn transition_tool_use_grep_with_pattern() {
        let args = ToolArgs {
            pattern: Some("fn main".to_string()),
            ..Default::default()
        };
        let session = make_tool_session("s1", "Grep", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        );
        assert_eq!(text, Some("proj: searching fn main".to_string()));
    }

    #[test]
    fn transition_tool_use_agent_with_description() {
        let args = ToolArgs {
            description: Some("analyse logs".to_string()),
            ..Default::default()
        };
        let session = make_tool_session("s1", "Agent", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        );
        assert_eq!(text, Some("proj: agent: analyse logs".to_string()));
    }

    #[test]
    fn transition_tool_use_task_with_description() {
        let args = ToolArgs {
            description: Some("run suite".to_string()),
            ..Default::default()
        };
        let session = make_tool_session("s1", "Task", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        );
        assert_eq!(text, Some("proj: agent: run suite".to_string()));
    }

    #[test]
    fn transition_tool_use_web_fetch() {
        let args = ToolArgs::default();
        let session = make_tool_session("s1", "WebFetch", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        );
        assert_eq!(text, Some("proj: web search".to_string()));
    }

    #[test]
    fn transition_tool_use_web_search() {
        let args = ToolArgs::default();
        let session = make_tool_session("s1", "WebSearch", args);
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        );
        assert_eq!(text, Some("proj: web search".to_string()));
    }

    #[test]
    fn transition_tool_use_unknown_tool_no_detail() {
        let mut session = make_session("s1");
        session.current_tool = Some("UnknownTool".to_string());
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::ToolUse,
            &session,
        );
        // Unknown tool, no detail → fallback "using <tool>"
        assert_eq!(text, Some("proj using UnknownTool".to_string()));
    }

    #[test]
    fn transition_any_to_awaiting_input() {
        let session = make_session("s1");
        for prev in [
            ClaudeStatus::Idle,
            ClaudeStatus::Processing,
            ClaudeStatus::ToolUse,
            ClaudeStatus::AwaitingInput,
        ] {
            let text = transition_text("proj", &prev, &ClaudeStatus::AwaitingInput, &session);
            assert_eq!(
                text,
                Some("proj needs input".to_string()),
                "failed for prev={prev:?}"
            );
        }
    }

    #[test]
    fn transition_processing_to_idle_finished() {
        let session = make_session("s1");
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::Idle,
            &session,
        );
        assert_eq!(text, Some("proj finished".to_string()));
    }

    #[test]
    fn transition_tool_use_to_idle_finished() {
        let session = make_session("s1");
        let text = transition_text(
            "proj",
            &ClaudeStatus::ToolUse,
            &ClaudeStatus::Idle,
            &session,
        );
        assert_eq!(text, Some("proj finished".to_string()));
    }

    #[test]
    fn transition_idle_to_idle_produces_none() {
        let session = make_session("s1");
        let text = transition_text("proj", &ClaudeStatus::Idle, &ClaudeStatus::Idle, &session);
        assert_eq!(text, None);
    }

    #[test]
    fn transition_processing_to_processing_produces_none() {
        let session = make_session("s1");
        let text = transition_text(
            "proj",
            &ClaudeStatus::Processing,
            &ClaudeStatus::Processing,
            &session,
        );
        assert_eq!(text, None);
    }

    // -----------------------------------------------------------------------
    // Context % alert thresholds
    // -----------------------------------------------------------------------

    #[test]
    fn threshold_below_50_returns_zero() {
        for pct in [0, 1, 25, 49] {
            assert_eq!(context_threshold(pct), 0, "pct={pct}");
        }
    }

    #[test]
    fn threshold_exactly_50() {
        assert_eq!(context_threshold(50), 50);
    }

    #[test]
    fn threshold_between_50_and_75() {
        for pct in [51, 60, 74] {
            assert_eq!(context_threshold(pct), 50, "pct={pct}");
        }
    }

    #[test]
    fn threshold_exactly_75() {
        assert_eq!(context_threshold(75), 75);
    }

    #[test]
    fn threshold_between_75_and_90() {
        for pct in [76, 80, 89] {
            assert_eq!(context_threshold(pct), 75, "pct={pct}");
        }
    }

    #[test]
    fn threshold_exactly_90() {
        assert_eq!(context_threshold(90), 90);
    }

    #[test]
    fn threshold_above_90() {
        for pct in [91, 95, 99, 100] {
            assert_eq!(context_threshold(pct), 90, "pct={pct}");
        }
    }

    #[test]
    fn threshold_at_boundaries_are_monotone() {
        // Verify the three real boundaries in order.
        assert!(context_threshold(49) < context_threshold(50));
        assert!(context_threshold(74) < context_threshold(75));
        assert!(context_threshold(89) < context_threshold(90));
    }

    // -----------------------------------------------------------------------
    // Debounce logic
    // -----------------------------------------------------------------------

    /// A minimal stand-in that exercises the debounce HashMap without a real
    /// AudioManager (which requires edge-tts / mpv / filesystem).
    struct DebounceTracker {
        last_play: HashMap<String, Instant>,
    }

    impl DebounceTracker {
        fn new() -> Self {
            Self {
                last_play: HashMap::new(),
            }
        }

        /// Returns true if the announce should proceed (not debounced).
        fn should_announce(&mut self, session_id: &str) -> bool {
            if let Some(last) = self.last_play.get(session_id)
                && last.elapsed().as_millis() < DEBOUNCE_MS
            {
                return false;
            }
            self.last_play
                .insert(session_id.to_string(), Instant::now());
            true
        }
    }

    #[test]
    fn debounce_first_call_always_passes() {
        let mut dt = DebounceTracker::new();
        assert!(dt.should_announce("session-a"));
    }

    #[test]
    fn debounce_immediate_second_call_blocked() {
        let mut dt = DebounceTracker::new();
        dt.should_announce("session-a");
        // Immediately after: should be debounced.
        assert!(!dt.should_announce("session-a"));
    }

    #[test]
    fn debounce_different_sessions_independent() {
        let mut dt = DebounceTracker::new();
        dt.should_announce("session-a");
        // session-b has not been seen → should pass.
        assert!(dt.should_announce("session-b"));
    }

    #[test]
    fn debounce_passes_after_window_expires() {
        let mut dt = DebounceTracker::new();
        // Inject a last_play timestamp that is older than DEBOUNCE_MS.
        let old = Instant::now() - Duration::from_millis(DEBOUNCE_MS as u64 + 10);
        dt.last_play.insert("session-a".to_string(), old);
        assert!(
            dt.should_announce("session-a"),
            "should pass after debounce window"
        );
    }

    #[test]
    fn debounce_blocked_just_inside_window() {
        let mut dt = DebounceTracker::new();
        // Inject a timestamp just inside the debounce window (1 ms ago).
        let recent = Instant::now() - Duration::from_millis(1);
        dt.last_play.insert("session-a".to_string(), recent);
        assert!(
            !dt.should_announce("session-a"),
            "should be debounced within window"
        );
    }

    #[test]
    fn debounce_constant_is_500ms() {
        assert_eq!(DEBOUNCE_MS, 500, "debounce window must be 500 ms");
    }

    // -----------------------------------------------------------------------
    // Socket message JSON parsing (TtsRequest — legacy format)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_minimal_tts_request() {
        let json = r#"{"text": "hello world"}"#;
        let req: TtsRequest = serde_json::from_str(json).expect("should parse");
        assert_eq!(req.text, "hello world");
        assert_eq!(req.voice, None);
        assert_eq!(req.priority, Priority::Normal);
    }

    #[test]
    fn parse_tts_request_with_voice() {
        let json = r#"{"text": "hi", "voice": "en-GB-SoniaNeural"}"#;
        let req: TtsRequest = serde_json::from_str(json).expect("should parse");
        assert_eq!(req.voice.as_deref(), Some("en-GB-SoniaNeural"));
    }

    #[test]
    fn parse_tts_request_with_normal_priority() {
        let json = r#"{"text": "hi", "priority": "normal"}"#;
        let req: TtsRequest = serde_json::from_str(json).expect("should parse");
        assert_eq!(req.priority, Priority::Normal);
    }

    #[test]
    fn parse_tts_request_with_high_priority() {
        let json = r#"{"text": "urgent", "priority": "high"}"#;
        let req: TtsRequest = serde_json::from_str(json).expect("should parse");
        assert_eq!(req.priority, Priority::High);
    }

    #[test]
    fn parse_tts_request_all_fields() {
        let json =
            r#"{"text": "full request", "voice": "en-AU-WilliamNeural", "priority": "high"}"#;
        let req: TtsRequest = serde_json::from_str(json).expect("should parse");
        assert_eq!(req.text, "full request");
        assert_eq!(req.voice.as_deref(), Some("en-AU-WilliamNeural"));
        assert_eq!(req.priority, Priority::High);
    }

    #[test]
    fn parse_tts_request_missing_text_fails() {
        // "text" is not optional in TtsRequest.
        let json = r#"{"voice": "en-US-GuyNeural"}"#;
        let result: Result<TtsRequest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "missing text field should fail");
    }

    #[test]
    fn parse_tts_request_invalid_priority_fails() {
        let json = r#"{"text": "hi", "priority": "urgent"}"#;
        let result: Result<TtsRequest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "invalid priority variant should fail");
    }

    #[test]
    fn parse_tts_request_malformed_json_fails() {
        let result: Result<TtsRequest, _> = serde_json::from_str("{bad json}");
        assert!(result.is_err());
    }

    #[test]
    fn default_priority_is_normal() {
        assert_eq!(default_priority(), Priority::Normal);
    }

    // -----------------------------------------------------------------------
    // Priority handling
    // -----------------------------------------------------------------------

    #[test]
    fn high_priority_differs_from_normal() {
        assert_ne!(Priority::High, Priority::Normal);
    }

    #[test]
    fn priority_high_is_high() {
        let json = r#"{"text": "t", "priority": "high"}"#;
        let req: TtsRequest = serde_json::from_str(json).unwrap();
        assert!(req.priority == Priority::High);
    }

    #[test]
    fn priority_normal_is_not_high() {
        let json = r#"{"text": "t", "priority": "normal"}"#;
        let req: TtsRequest = serde_json::from_str(json).unwrap();
        assert!(req.priority != Priority::High);
    }

    #[test]
    fn priority_default_omitted_is_normal() {
        // When "priority" key is absent the default kicks in.
        let json = r#"{"text": "t"}"#;
        let req: TtsRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.priority, Priority::Normal);
    }

    // -----------------------------------------------------------------------
    // session_label helper
    // -----------------------------------------------------------------------

    #[test]
    fn session_label_uses_working_dir_basename() {
        let session = make_session_with_dir("s1", "/home/user/my-project");
        assert_eq!(session_label(&session), "my-project");
    }

    #[test]
    fn session_label_falls_back_to_short_session_id() {
        let mut session = make_session("abcdefgh12345678");
        session.working_dir = None;
        // First 8 chars of session_id.
        assert_eq!(session_label(&session), "abcdefgh");
    }

    #[test]
    fn session_label_short_session_id_not_truncated() {
        let mut session = make_session("short");
        session.working_dir = None;
        assert_eq!(session_label(&session), "short");
    }

    #[test]
    fn session_label_empty_session_id_returns_session() {
        let session = make_session("");
        assert_eq!(session_label(&session), "session");
    }

    #[test]
    fn session_label_root_path_falls_back_to_id() {
        // Path "/" has no file_name(); should fall back to session_id.
        let session = make_session_with_dir("root-sess", "/");
        let label = session_label(&session);
        // file_name() on "/" is None → falls back to session_id truncated to 8
        assert_eq!(label, "root-ses");
    }

    // -----------------------------------------------------------------------
    // basename helper
    // -----------------------------------------------------------------------

    #[test]
    fn basename_returns_filename() {
        assert_eq!(basename("/home/user/project/main.rs"), "main.rs");
    }

    #[test]
    fn basename_no_directory() {
        assert_eq!(basename("file.txt"), "file.txt");
    }

    #[test]
    fn basename_empty_string_returns_empty() {
        assert_eq!(basename(""), "");
    }

    #[test]
    fn basename_trailing_slash_returns_last_component() {
        // On Linux, Path::new("/some/dir/").file_name() strips the trailing slash
        // and returns "dir", so basename reflects that behaviour.
        let result = basename("/some/dir/");
        assert_eq!(result, "dir");
    }

    // -----------------------------------------------------------------------
    // dirs_cache helper
    // -----------------------------------------------------------------------

    #[test]
    fn dirs_cache_uses_xdg_cache_home_when_set() {
        // Temporarily set XDG_CACHE_HOME and verify the function honours it.
        // Safety: single-threaded test binary; no concurrent env reads.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", "/tmp/test-cache-xdg");
        }
        let result = dirs_cache();
        unsafe {
            std::env::remove_var("XDG_CACHE_HOME");
        }
        assert_eq!(result, std::path::PathBuf::from("/tmp/test-cache-xdg"));
    }

    #[test]
    fn dirs_cache_falls_back_to_home_dot_cache() {
        // Remove XDG_CACHE_HOME, set HOME, and verify fallback.
        // Safety: single-threaded test binary; no concurrent env reads.
        unsafe {
            std::env::remove_var("XDG_CACHE_HOME");
            std::env::set_var("HOME", "/tmp/fakehome");
        }
        let result = dirs_cache();
        unsafe {
            std::env::remove_var("HOME");
        }
        assert_eq!(result, std::path::PathBuf::from("/tmp/fakehome/.cache"));
    }

    // -----------------------------------------------------------------------
    // socket_path helper
    // -----------------------------------------------------------------------

    #[test]
    fn socket_path_uses_xdg_runtime_dir() {
        // Safety: single-threaded test binary; no concurrent env reads.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/9999");
        }
        let p = socket_path();
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        assert_eq!(
            p,
            std::path::PathBuf::from("/run/user/9999/thermal/audio.sock")
        );
    }

    #[test]
    fn socket_path_fallback_without_xdg_runtime_dir() {
        // Safety: single-threaded test binary; no concurrent env reads.
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let p = socket_path();
        assert_eq!(
            p,
            std::path::PathBuf::from("/run/user/1000/thermal/audio.sock")
        );
    }

    // -----------------------------------------------------------------------
    // tool_detail helper
    // -----------------------------------------------------------------------

    #[test]
    fn tool_detail_read_with_file_path() {
        let args = ToolArgs {
            file_path: Some("/some/path/foo.rs".to_string()),
            ..Default::default()
        };
        let session = make_tool_session("s", "Read", args);
        let detail = tool_detail("Read", &session);
        assert_eq!(detail, Some("reading foo.rs".to_string()));
    }

    #[test]
    fn tool_detail_read_without_file_path_returns_none() {
        let args = ToolArgs::default();
        let session = make_tool_session("s", "Read", args);
        let detail = tool_detail("Read", &session);
        assert_eq!(detail, None);
    }

    #[test]
    fn tool_detail_bash_prefers_description_over_command() {
        let args = ToolArgs {
            description: Some("run tests".to_string()),
            command: Some("cargo test".to_string()),
            ..Default::default()
        };
        let session = make_tool_session("s", "Bash", args);
        let detail = tool_detail("Bash", &session);
        assert_eq!(detail, Some("running run tests".to_string()));
    }

    #[test]
    fn tool_detail_unknown_tool_returns_none() {
        let args = ToolArgs::default();
        let session = make_tool_session("s", "FancyNewTool", args);
        let detail = tool_detail("FancyNewTool", &session);
        assert_eq!(detail, None);
    }

    #[test]
    fn tool_detail_no_details_on_session_returns_none() {
        let session = make_session("s");
        let detail = tool_detail("Read", &session);
        assert_eq!(detail, None);
    }

    // -----------------------------------------------------------------------
    // SocketMessage parsing (new protocol)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_socket_message_tts() {
        let json = r#"{"action": "tts", "text": "hello", "voice": "en-US-GuyNeural"}"#;
        let msg: SocketMessage = serde_json::from_str(json).expect("should parse");
        match msg {
            SocketMessage::Tts {
                text,
                voice,
                priority,
            } => {
                assert_eq!(text, "hello");
                assert_eq!(voice.as_deref(), Some("en-US-GuyNeural"));
                assert_eq!(priority, Priority::Normal);
            }
            _ => panic!("expected Tts variant"),
        }
    }

    #[test]
    fn parse_socket_message_tts_with_priority() {
        let json = r#"{"action": "tts", "text": "urgent", "priority": "high"}"#;
        let msg: SocketMessage = serde_json::from_str(json).expect("should parse");
        match msg {
            SocketMessage::Tts { priority, .. } => {
                assert_eq!(priority, Priority::High);
            }
            _ => panic!("expected Tts variant"),
        }
    }

    #[test]
    fn parse_socket_message_toggle_mute() {
        let json = r#"{"action": "toggle_mute"}"#;
        let msg: SocketMessage = serde_json::from_str(json).expect("should parse");
        assert!(matches!(msg, SocketMessage::ToggleMute));
    }

    #[test]
    fn parse_socket_message_set_mute_true() {
        let json = r#"{"action": "set_mute", "muted": true}"#;
        let msg: SocketMessage = serde_json::from_str(json).expect("should parse");
        match msg {
            SocketMessage::SetMute { muted } => assert!(muted),
            _ => panic!("expected SetMute variant"),
        }
    }

    #[test]
    fn parse_socket_message_set_mute_false() {
        let json = r#"{"action": "set_mute", "muted": false}"#;
        let msg: SocketMessage = serde_json::from_str(json).expect("should parse");
        match msg {
            SocketMessage::SetMute { muted } => assert!(!muted),
            _ => panic!("expected SetMute variant"),
        }
    }

    #[test]
    fn parse_socket_message_set_volume() {
        let json = r#"{"action": "set_volume", "value": 0.5}"#;
        let msg: SocketMessage = serde_json::from_str(json).expect("should parse");
        match msg {
            SocketMessage::SetVolume { value } => {
                assert!((value - 0.5).abs() < f32::EPSILON);
            }
            _ => panic!("expected SetVolume variant"),
        }
    }

    #[test]
    fn parse_socket_message_get_status() {
        let json = r#"{"action": "get_status"}"#;
        let msg: SocketMessage = serde_json::from_str(json).expect("should parse");
        assert!(matches!(msg, SocketMessage::GetStatus));
    }

    #[test]
    fn parse_legacy_tts_request_without_action() {
        // Legacy format: no "action" field. Should fail as SocketMessage but
        // succeed as TtsRequest for backward compat.
        let json = r#"{"text": "hello legacy"}"#;
        // SocketMessage parse will fail (no "action" tag)...
        assert!(serde_json::from_str::<SocketMessage>(json).is_err());
        // ...but TtsRequest parse succeeds (backward compat path).
        let req: TtsRequest = serde_json::from_str(json).expect("legacy should parse");
        assert_eq!(req.text, "hello legacy");
    }

    // -----------------------------------------------------------------------
    // AudioState defaults and persistence format
    // -----------------------------------------------------------------------

    #[test]
    fn audio_state_default_is_unmuted_full_volume() {
        let state = AudioState::default();
        assert!(!state.muted);
        assert!((state.volume - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn control_response_serialization() {
        let resp = ControlResponse {
            ok: true,
            muted: false,
            volume: 0.8,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"ok\":true"));
        assert!(json.contains("\"muted\":false"));
        assert!(json.contains("\"volume\":0.8"));
    }

    #[test]
    fn control_response_muted_serialization() {
        let resp = ControlResponse {
            ok: true,
            muted: true,
            volume: 0.5,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"muted\":true"));
        assert!(json.contains("\"volume\":0.5"));
    }

    // -----------------------------------------------------------------------
    // Daemon event-driven mode: activity text mapping
    // -----------------------------------------------------------------------

    #[test]
    fn daemon_activity_idle_to_thinking_announces_started() {
        use daemon_client::AgentActivity::*;
        let text = daemon_activity_text("opus", &Idle, &Thinking);
        assert_eq!(text, Some("opus started working".to_string()));
    }

    #[test]
    fn daemon_activity_waiting_to_thinking_announces_started() {
        use daemon_client::AgentActivity::*;
        let text = daemon_activity_text("opus", &WaitingInput, &Thinking);
        assert_eq!(text, Some("opus started working".to_string()));
    }

    #[test]
    fn daemon_activity_any_to_tool_running() {
        use daemon_client::AgentActivity::*;
        let text = daemon_activity_text("sonnet", &Thinking, &ToolRunning);
        assert_eq!(text, Some("sonnet running a tool".to_string()));
    }

    #[test]
    fn daemon_activity_any_to_waiting_input() {
        use daemon_client::AgentActivity::*;
        let text = daemon_activity_text("opus", &Thinking, &WaitingInput);
        assert_eq!(text, Some("opus needs input".to_string()));
    }

    #[test]
    fn daemon_activity_thinking_to_idle_announces_finished() {
        use daemon_client::AgentActivity::*;
        let text = daemon_activity_text("opus", &Thinking, &Idle);
        assert_eq!(text, Some("opus finished".to_string()));
    }

    #[test]
    fn daemon_activity_tool_to_idle_announces_finished() {
        use daemon_client::AgentActivity::*;
        let text = daemon_activity_text("opus", &ToolRunning, &Idle);
        assert_eq!(text, Some("opus finished".to_string()));
    }

    #[test]
    fn daemon_activity_idle_to_idle_is_none() {
        use daemon_client::AgentActivity::*;
        let text = daemon_activity_text("opus", &Idle, &Idle);
        assert!(text.is_none());
    }

    #[test]
    fn daemon_activity_any_to_exited() {
        use daemon_client::AgentActivity::*;
        let text = daemon_activity_text("opus", &Thinking, &Exited);
        assert_eq!(text, Some("opus exited".to_string()));
    }

    // -----------------------------------------------------------------------
    // Daemon session label derivation
    // -----------------------------------------------------------------------

    #[test]
    fn daemon_label_prefers_display_name() {
        let snap = daemon_client::SemanticSessionSnapshot {
            session_id: "abc123".into(),
            backend: String::new(),
            runtime: daemon_client::AgentRuntime::Claude,
            display_name: Some("opus".into()),
            title: None,
            cwd: Some("/home/user/projects/thermal-desktop".into()),
            workspace_root: None,
            pid: None,
            started_at: None,
            last_activity_at: None,
            exit_code: None,
            is_alive: true,
            agent_activity: daemon_client::AgentActivity::Idle,
            current_tool: None,
            context_state: daemon_client::ContextState::default(),
        };
        assert_eq!(daemon_session_label(&snap), "opus");
    }

    #[test]
    fn daemon_label_falls_back_to_cwd_basename() {
        let snap = daemon_client::SemanticSessionSnapshot {
            session_id: "abc123".into(),
            backend: String::new(),
            runtime: daemon_client::AgentRuntime::Claude,
            display_name: None,
            title: None,
            cwd: Some("/home/user/projects/thermal-desktop".into()),
            workspace_root: None,
            pid: None,
            started_at: None,
            last_activity_at: None,
            exit_code: None,
            is_alive: true,
            agent_activity: daemon_client::AgentActivity::Idle,
            current_tool: None,
            context_state: daemon_client::ContextState::default(),
        };
        assert_eq!(daemon_session_label(&snap), "thermal-desktop");
    }

    #[test]
    fn daemon_label_falls_back_to_short_id() {
        let snap = daemon_client::SemanticSessionSnapshot {
            session_id: "abcdef1234567890".into(),
            backend: String::new(),
            runtime: daemon_client::AgentRuntime::Claude,
            display_name: None,
            title: None,
            cwd: None,
            workspace_root: None,
            pid: None,
            started_at: None,
            last_activity_at: None,
            exit_code: None,
            is_alive: true,
            agent_activity: daemon_client::AgentActivity::Idle,
            current_tool: None,
            context_state: daemon_client::ContextState::default(),
        };
        assert_eq!(daemon_session_label(&snap), "abcdef12");
    }

    #[test]
    fn short_id_truncates_long_ids() {
        assert_eq!(short_id("abcdef1234567890"), "abcdef12");
    }

    #[test]
    fn short_id_preserves_short_ids() {
        assert_eq!(short_id("abc"), "abc");
    }

    // -----------------------------------------------------------------------
    // SuppressedQueue tests
    // -----------------------------------------------------------------------

    #[test]
    fn suppressed_queue_push_and_coalesce() {
        let mut q = SuppressedQueue::new();
        q.push("s1".into(), "voice1".into(), "first".into());
        q.push("s1".into(), "voice1".into(), "second".into());
        assert_eq!(q.entries.len(), 1);
        assert_eq!(q.entries[0].text, "second");
    }

    #[test]
    fn suppressed_queue_bounded_at_max() {
        let mut q = SuppressedQueue::new();
        for i in 0..SUPPRESSED_QUEUE_MAX + 3 {
            q.push(format!("s{i}"), "v".into(), format!("text{i}"));
        }
        assert_eq!(q.entries.len(), SUPPRESSED_QUEUE_MAX);
        // Oldest entries should have been evicted.
        assert_eq!(q.entries[0].session_id, "s3");
    }

    #[test]
    fn suppressed_queue_multiple_sessions() {
        let mut q = SuppressedQueue::new();
        q.push("s1".into(), "v1".into(), "a".into());
        q.push("s2".into(), "v2".into(), "b".into());
        q.push("s1".into(), "v1".into(), "c".into());
        assert_eq!(q.entries.len(), 2);
        // s1 should have the latest text.
        let s1 = q.entries.iter().find(|e| e.session_id == "s1").unwrap();
        assert_eq!(s1.text, "c");
    }

    #[test]
    fn suppressed_queue_drain_on_transition() {
        let mut q = SuppressedQueue::new();
        q.was_voice_active = true;
        q.push("s1".into(), "v".into(), "hello".into());
        // Simulate voice going inactive — check_and_drain reads is_voice_active()
        // which returns false in test env (no state file).
        let drained = q.check_and_drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].text, "hello");
        assert!(q.entries.is_empty());
    }

    #[test]
    fn suppressed_queue_no_drain_when_voice_still_inactive() {
        let mut q = SuppressedQueue::new();
        q.was_voice_active = false;
        q.push("s1".into(), "v".into(), "hello".into());
        // Voice was inactive and still is — should clear stale entries.
        let drained = q.check_and_drain();
        assert!(drained.is_empty());
    }
}
