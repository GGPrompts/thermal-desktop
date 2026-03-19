use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;
use std::{fs, thread};

use anyhow::{Context, Result};
use clap::Parser;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use thermal_core::{ClaudeSessionState, ClaudeStatePoller, ClaudeStatus};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{info, warn};

/// Thermal Audio — TTS voice announcements for Claude session state changes.
#[derive(Parser)]
#[command(name = "thermal-audio")]
struct Cli {
    /// Speak the given text and exit (for testing).
    #[arg(long)]
    test: Option<String>,
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
    edge_tts_available: bool,
}

impl AudioManager {
    fn new() -> Result<Self> {
        let cache_dir = dirs_cache().join("thermal-audio");
        fs::create_dir_all(&cache_dir)
            .with_context(|| format!("creating cache dir {:?}", cache_dir))?;

        // Check if edge-tts is available.
        let edge_tts_available = std::process::Command::new("edge-tts")
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());

        if !edge_tts_available {
            warn!("edge-tts not found on PATH — TTS generation will be skipped");
        }

        let current_child: CurrentChild = Arc::new(Mutex::new(None));
        let audio_tx = spawn_audio_thread(Arc::clone(&current_child));

        Ok(Self {
            cache_dir,
            last_play: HashMap::new(),
            audio_tx,
            current_child,
            edge_tts_available,
        })
    }

    fn announce(&mut self, session_id: &str, voice: &str, text: &str) -> Result<()> {
        // Debounce check.
        if let Some(last) = self.last_play.get(session_id) {
            if last.elapsed().as_millis() < DEBOUNCE_MS {
                return Ok(());
            }
        }
        self.last_play
            .insert(session_id.to_string(), Instant::now());

        let path = self.generate_tts(voice, text)?;
        if let Err(e) = self.audio_tx.send(path) {
            warn!("audio channel send failed: {e}");
        }
        Ok(())
    }

    /// Speak text via the socket API. Supports high-priority (interrupt).
    fn speak(&mut self, voice: &str, text: &str, high_priority: bool) -> Result<()> {
        let path = self.generate_tts(voice, text)?;

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

    fn generate_tts(&self, voice: &str, text: &str) -> Result<PathBuf> {
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

        if !self.edge_tts_available {
            anyhow::bail!("edge-tts not available");
        }

        let status = std::process::Command::new("edge-tts")
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
fn spawn_audio_thread(current_child: CurrentChild) -> mpsc::Sender<PathBuf> {
    let (tx, rx) = mpsc::channel::<PathBuf>();

    thread::Builder::new()
        .name("thermal-audio-player".into())
        .spawn(move || {
            for path in rx {
                if let Err(e) = play_file(&path, &current_child) {
                    warn!("audio play error: {e}");
                }
            }
        })
        .expect("failed to spawn audio thread");

    tx
}

/// Play an audio file via mpv, registering the child process in the shared
/// mutex so it can be killed externally for interrupt support.
fn play_file(path: &Path, current_child: &CurrentChild) -> Result<()> {
    let child = std::process::Command::new("mpv")
        .arg("--no-video")
        .arg("--really-quiet")
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn mpv")?;

    // Register child so it can be killed on interrupt.
    {
        let mut guard = current_child.lock().unwrap();
        *guard = Some(child);
    }

    // Poll for completion (non-blocking) so that an external kill is
    // reflected promptly rather than blocking forever on wait().
    loop {
        // Take the child out briefly to check status.
        let mut guard = current_child.lock().unwrap();
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
/// falling back to /run/user/1000/thermal/audio.sock.
fn socket_path() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| "/run/user/1000".to_string());
    PathBuf::from(runtime_dir).join("thermal").join("audio.sock")
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
        (ClaudeStatus::Processing, ClaudeStatus::ToolUse)
        | (ClaudeStatus::ToolUse, ClaudeStatus::ToolUse) => {
            let tool = session.current_tool.as_deref().unwrap_or("a tool");
            let detail = tool_detail(tool, session);
            if let Some(d) = detail {
                Some(format!("{session_label}: {d}"))
            } else {
                Some(format!("{session_label} using {tool}"))
            }
        }
        (_, ClaudeStatus::AwaitingInput) => {
            Some(format!("{session_label} needs input"))
        }
        (ClaudeStatus::Processing, ClaudeStatus::Idle)
        | (ClaudeStatus::ToolUse, ClaudeStatus::Idle) => {
            Some(format!("{session_label} finished"))
        }
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
            let desc = args.description.as_deref()
                .or_else(|| args.command.as_deref().map(|c| if c.len() > 40 { &c[..40] } else { c }));
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
        "WebFetch" | "WebSearch" => {
            Some("web search".to_string())
        }
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
fn session_label(session: &ClaudeSessionState) -> String {
    // Try to extract project folder name from working_dir
    if let Some(ref dir) = session.working_dir {
        if let Some(name) = std::path::Path::new(dir).file_name() {
            if let Some(s) = name.to_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    if !session.session_id.is_empty() {
        let short = if session.session_id.len() > 8 {
            &session.session_id[..8]
        } else {
            &session.session_id
        };
        return short.to_string();
    }
    "session".to_string()
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "thermal_audio=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    let mut audio = AudioManager::new()?;
    let mut voices = VoicePool::new();

    // --test mode: speak text and exit.
    if let Some(text) = &cli.test {
        let voice = VOICES[0];
        info!("test mode: voice={voice}, text={text:?}");
        match audio.generate_tts(voice, text) {
            Ok(path) => {
                if let Err(e) = audio.audio_tx.send(path) {
                    warn!("audio send failed: {e}");
                }
                // Wait for playback.
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            }
            Err(e) => {
                warn!("TTS generation failed: {e}");
            }
        }
        return Ok(());
    }

    // Daemon mode.
    info!("thermal-audio daemon starting");

    // Set up the Unix socket listener.
    let sock_path = socket_path();
    if let Some(parent) = sock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating socket dir {:?}", parent))?;
    }
    // Remove stale socket if it exists.
    if sock_path.exists() {
        fs::remove_file(&sock_path)
            .with_context(|| format!("removing stale socket {:?}", sock_path))?;
    }
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding socket {:?}", sock_path))?;
    info!("socket API listening on {}", sock_path.display());

    // Use a tokio mpsc channel to forward socket requests to the main loop,
    // which owns the AudioManager (not Send-safe across tasks).
    let (sock_tx, mut sock_rx) = tokio::sync::mpsc::unbounded_channel::<TtsRequest>();

    // Spawn a task that accepts socket connections and parses requests.
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let tx = sock_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_socket_connection(stream, tx).await {
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

    // State poller (synchronous) — we run it on a timer.
    let mut poller = ClaudeStatePoller::new().context("creating state poller")?;
    let mut prev_states: HashMap<String, ClaudeStatus> = HashMap::new();
    let mut prev_context_alert: HashMap<String, u32> = HashMap::new();

    // Seed initial states without announcing.
    for session in poller.poll() {
        if !session.session_id.is_empty() {
            prev_states.insert(session.session_id.clone(), session.status.clone());
            if let Some(pct) = session.context_percent {
                let threshold = context_threshold(pct as u32);
                prev_context_alert.insert(session.session_id.clone(), threshold);
            }
        }
    }

    let mut poll_interval = tokio::time::interval(std::time::Duration::from_millis(500));

    loop {
        tokio::select! {
            _ = poll_interval.tick() => {
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
                        .unwrap_or(ClaudeStatus::Idle);

                    if prev != session.status {
                        if let Some(text) = transition_text(&label, &prev, &session.status, session) {
                            info!("[{}] {} -> {:?}: {text}", session.session_id, format!("{prev:?}"), session.status);
                            let voice = voices.assign(&session.session_id);
                            if let Err(e) = audio.announce(&session.session_id, voice, &text) {
                                warn!("announce failed: {e}");
                            }
                        }
                        prev_states.insert(session.session_id.clone(), session.status.clone());
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
                            let voice = voices.assign(&session.session_id);
                            if let Err(e) = audio.announce(&format!("{}-ctx", session.session_id), voice, &text) {
                                warn!("context announce failed: {e}");
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
            Some(req) = sock_rx.recv() => {
                let voice = req.voice.as_deref().unwrap_or(ASSISTANT_VOICE);
                let high_priority = req.priority == Priority::High;
                info!("socket TTS: voice={voice}, priority={:?}, text={:?}", req.priority, req.text);
                if let Err(e) = audio.speak(voice, &req.text, high_priority) {
                    warn!("socket TTS failed: {e}");
                }
            }
        }
    }
}

/// Handle a single socket connection: read one JSON line, parse, forward, respond.
async fn handle_socket_connection(
    stream: tokio::net::UnixStream,
    tx: tokio::sync::mpsc::UnboundedSender<TtsRequest>,
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

    match serde_json::from_str::<TtsRequest>(line) {
        Ok(req) => {
            if req.text.is_empty() {
                let resp = serde_json::to_string(&TtsResponse {
                    ok: false,
                    error: Some("text field is empty".to_string()),
                })?;
                writer.write_all(resp.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                return Ok(());
            }

            tx.send(req).context("forwarding TTS request to main loop")?;

            let resp = serde_json::to_string(&TtsResponse {
                ok: true,
                error: None,
            })?;
            writer.write_all(resp.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        Err(e) => {
            let resp = serde_json::to_string(&TtsResponse {
                ok: false,
                error: Some(format!("invalid JSON: {e}")),
            })?;
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
