use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;
use std::{fs, thread};

use anyhow::{Context, Result};
use clap::Parser;
use md5::{Digest, Md5};
use thermal_core::{ClaudeSessionState, ClaudeStatePoller, ClaudeStatus};
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
// Audio manager
// ---------------------------------------------------------------------------

/// Debounce window per session.
const DEBOUNCE_MS: u128 = 500;

struct AudioManager {
    cache_dir: PathBuf,
    last_play: HashMap<String, Instant>,
    audio_tx: mpsc::Sender<PathBuf>,
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

        let audio_tx = spawn_audio_thread();

        Ok(Self {
            cache_dir,
            last_play: HashMap::new(),
            audio_tx,
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
/// Returns a sender to push file paths for playback.
fn spawn_audio_thread() -> mpsc::Sender<PathBuf> {
    let (tx, rx) = mpsc::channel::<PathBuf>();

    thread::Builder::new()
        .name("thermal-audio-player".into())
        .spawn(move || {
            for path in rx {
                if let Err(e) = play_file(&path) {
                    warn!("audio play error: {e}");
                }
            }
        })
        .expect("failed to spawn audio thread");

    tx
}

fn play_file(path: &Path) -> Result<()> {
    let status = std::process::Command::new("mpv")
        .arg("--no-video")
        .arg("--really-quiet")
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("failed to run mpv")?;
    if !status.success() {
        anyhow::bail!("mpv exited with status {status}");
    }
    Ok(())
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

fn main() -> Result<()> {
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
                thread::sleep(std::time::Duration::from_secs(4));
            }
            Err(e) => {
                warn!("TTS generation failed: {e}");
            }
        }
        return Ok(());
    }

    // Daemon mode.
    info!("thermal-audio daemon starting");

    let mut poller = ClaudeStatePoller::new().context("creating state poller")?;
    let mut prev_states: HashMap<String, ClaudeStatus> = HashMap::new();
    // Track which context % threshold was last announced per session
    // 0 = none, 50 = warned at 50%, 75 = warned at 75%, 90 = warned at 90%
    let mut prev_context_alert: HashMap<String, u32> = HashMap::new();

    // Seed initial states without announcing.
    for session in poller.poll() {
        if !session.session_id.is_empty() {
            prev_states.insert(session.session_id.clone(), session.status.clone());
            // Seed context alerts so we don't fire on startup
            if let Some(pct) = session.context_percent {
                let threshold = context_threshold(pct as u32);
                prev_context_alert.insert(session.session_id.clone(), threshold);
            }
        }
    }

    loop {
        thread::sleep(std::time::Duration::from_millis(500));

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
