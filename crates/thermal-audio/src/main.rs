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

/// Spawn a dedicated audio thread that owns the rodio OutputStream.
/// Returns a sender to push file paths for playback.
fn spawn_audio_thread() -> mpsc::Sender<PathBuf> {
    let (tx, rx) = mpsc::channel::<PathBuf>();

    thread::Builder::new()
        .name("thermal-audio-player".into())
        .spawn(move || {
            // OutputStream must live on this thread (it is !Send).
            let Ok((_stream, handle)) = rodio::OutputStream::try_default() else {
                warn!("no default audio output — audio disabled");
                return;
            };

            for path in rx {
                if let Err(e) = play_file(&handle, &path) {
                    warn!("audio play error: {e}");
                }
            }
        })
        .expect("failed to spawn audio thread");

    tx
}

fn play_file(handle: &rodio::OutputStreamHandle, path: &Path) -> Result<()> {
    let file = fs::File::open(path).context("opening audio file")?;
    let reader = BufReader::new(file);
    let source =
        rodio::Decoder::new(reader).context("decoding audio file")?;
    handle
        .play_raw(rodio::Source::convert_samples(source))
        .context("playing audio")?;
    // Give time for playback before accepting the next file.
    // A short sleep is acceptable here — the audio thread is dedicated.
    thread::sleep(std::time::Duration::from_secs(3));
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
        (ClaudeStatus::Processing, ClaudeStatus::ToolUse) => {
            let tool = session
                .current_tool
                .as_deref()
                .unwrap_or("a tool");
            Some(format!("{session_label} using {tool}"))
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

/// Derive a short human-readable label from a session.
fn session_label(session: &ClaudeSessionState) -> String {
    if !session.session_id.is_empty() {
        // Use first 8 chars of the session id for brevity.
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

    // Seed initial states without announcing.
    for session in poller.poll() {
        if !session.session_id.is_empty() {
            prev_states.insert(session.session_id.clone(), session.status.clone());
        }
    }

    loop {
        thread::sleep(std::time::Duration::from_millis(500));

        let sessions = poller.poll();

        for session in &sessions {
            if session.session_id.is_empty() {
                continue;
            }

            let prev = prev_states
                .get(&session.session_id)
                .cloned()
                .unwrap_or(ClaudeStatus::Idle);

            if prev != session.status {
                let label = session_label(session);
                if let Some(text) = transition_text(&label, &prev, &session.status, session) {
                    info!("[{}] {} -> {:?}: {text}", session.session_id, format!("{prev:?}"), session.status);
                    let voice = voices.assign(&session.session_id);
                    if let Err(e) = audio.announce(&session.session_id, voice, &text) {
                        warn!("announce failed: {e}");
                    }
                }

                prev_states.insert(session.session_id.clone(), session.status.clone());
            }
        }

        // Clean up sessions that are no longer present.
        let active_ids: Vec<String> = sessions
            .iter()
            .filter(|s| !s.session_id.is_empty())
            .map(|s| s.session_id.clone())
            .collect();
        prev_states.retain(|id, _| active_ids.contains(id));
    }
}
