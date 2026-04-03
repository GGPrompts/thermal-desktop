//! Transcript watcher — monitors Claude Code and Codex JSONL transcript files.
//!
//! Watches `~/.claude/projects/` for Claude Code session JSONL files and
//! `~/.codex/sessions/` for Codex transcript files using the `notify` crate.
//! Tracks per-file metadata including line count, last event type, and an
//! active-streaming heuristic based on write frequency.
//!
//! # Integration
//!
//! Spawned as a background tokio task from `daemon::run_daemon()`. Exposes
//! `active_sessions()` for UI components to query transcript state.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

// ── Constants ────────────────────────────────────────────────────────────────

/// If a file has been modified within this window, it is "actively streaming".
const STREAMING_THRESHOLD: Duration = Duration::from_secs(2);

/// If no modification for this long, the session is "idle".
const IDLE_THRESHOLD: Duration = Duration::from_secs(30);

/// How often we check for state changes and prune stale sessions.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Maximum content summary length extracted from JSONL events.
const SUMMARY_MAX_CHARS: usize = 80;

/// Sessions with no modifications for this long are pruned from tracking.
const SESSION_EXPIRY: Duration = Duration::from_secs(3600);

// ── Types ────────────────────────────────────────────────────────────────────

/// Which agent produced the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptAgent {
    ClaudeCode,
    Codex,
}

impl std::fmt::Display for TranscriptAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClaudeCode => write!(f, "claude"),
            Self::Codex => write!(f, "codex"),
        }
    }
}

/// Streaming status of a transcript session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingStatus {
    /// File modified within the last 2 seconds.
    Active,
    /// File not modified for 2..30 seconds.
    Recent,
    /// No modification for >30 seconds.
    Idle,
}

impl std::fmt::Display for StreamingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "streaming"),
            Self::Recent => write!(f, "recent"),
            Self::Idle => write!(f, "idle"),
        }
    }
}

/// Snapshot of a tracked transcript session.
#[derive(Debug, Clone)]
pub struct TranscriptSession {
    /// Full path to the JSONL file.
    pub path: PathBuf,
    /// Session ID extracted from filename.
    pub session_id: String,
    /// Agent type (Claude Code or Codex).
    pub agent: TranscriptAgent,
    /// Type of the most recently parsed event (e.g. "assistant", "user", "tool_use").
    pub last_event_type: Option<String>,
    /// When the file was last modified (monotonic).
    pub last_event_at: Instant,
    /// Whether the session is actively streaming.
    pub is_streaming: bool,
    /// Streaming status (active/recent/idle).
    pub status: StreamingStatus,
    /// Total number of JSONL lines read.
    pub event_count: u64,
    /// Brief summary of the last event content.
    pub last_summary: Option<String>,
}

/// Internal per-file tracking state.
struct TrackedFile {
    path: PathBuf,
    session_id: String,
    agent: TranscriptAgent,
    last_event_type: Option<String>,
    last_modified: Instant,
    event_count: u64,
    /// File offset for incremental reads.
    read_offset: u64,
    last_summary: Option<String>,
}

impl TrackedFile {
    fn streaming_status(&self) -> StreamingStatus {
        let elapsed = self.last_modified.elapsed();
        if elapsed < STREAMING_THRESHOLD {
            StreamingStatus::Active
        } else if elapsed < IDLE_THRESHOLD {
            StreamingStatus::Recent
        } else {
            StreamingStatus::Idle
        }
    }

    fn to_session(&self) -> TranscriptSession {
        let status = self.streaming_status();
        TranscriptSession {
            path: self.path.clone(),
            session_id: self.session_id.clone(),
            agent: self.agent,
            last_event_type: self.last_event_type.clone(),
            last_event_at: self.last_modified,
            is_streaming: status == StreamingStatus::Active,
            status,
            event_count: self.event_count,
            last_summary: self.last_summary.clone(),
        }
    }
}

// ── TranscriptWatcher ────────────────────────────────────────────────────────

/// Watches Claude Code and Codex JSONL transcript files for activity.
pub(crate) struct TranscriptWatcher {
    state: Arc<Mutex<HashMap<PathBuf, TrackedFile>>>,
}

impl TranscriptWatcher {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Return a snapshot of all tracked transcript sessions.
    #[allow(dead_code)]
    pub fn active_sessions(&self) -> Vec<TranscriptSession> {
        self.state.lock().values().map(|t| t.to_session()).collect()
    }

    /// Spawn the background watcher task. Returns a join handle.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = self.run().await {
                warn!("Transcript watcher exited with error: {e}");
            }
        })
    }

    async fn run(&self) -> anyhow::Result<()> {
        let (notify_tx, mut notify_rx) = mpsc::channel::<PathBuf>(256);

        // Set up inotify watcher for both directories.
        let tx = notify_tx.clone();
        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                if let Ok(event) = res {
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) => {
                            for path in event.paths {
                                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                                    let _ = tx.blocking_send(path);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            },
            notify::Config::default(),
        )?;

        // Watch directories.
        let claude_dir = claude_projects_dir();
        let codex_dir = codex_sessions_dir();

        if claude_dir.exists() {
            match watcher.watch(&claude_dir, RecursiveMode::Recursive) {
                Ok(()) => info!("Transcript watcher: watching {:?}", claude_dir),
                Err(e) => warn!("Cannot watch Claude projects dir: {e}"),
            }
        } else {
            debug!("Claude projects dir does not exist: {:?}", claude_dir);
        }

        if codex_dir.exists() {
            match watcher.watch(&codex_dir, RecursiveMode::Recursive) {
                Ok(()) => info!("Transcript watcher: watching {:?}", codex_dir),
                Err(e) => warn!("Cannot watch Codex sessions dir: {e}"),
            }
        } else {
            debug!("Codex sessions dir does not exist: {:?}", codex_dir);
        }

        info!("Transcript watcher started");

        // Initial scan: pick up .jsonl files that already exist (sessions active
        // before conductor started).
        let mut startup_count = 0usize;
        for dir in [&claude_dir, &codex_dir] {
            if dir.exists() {
                startup_count += self.scan_existing_jsonl(dir);
            }
        }
        if startup_count > 0 {
            info!(
                "Transcript watcher: picked up {startup_count} pre-existing session(s) on startup"
            );
        }

        // Keep watcher alive.
        let _watcher = watcher;

        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                // Process file change notifications.
                Some(path) = notify_rx.recv() => {
                    self.handle_file_change(&path);
                }
                // Periodic tick for pruning and status logging.
                _ = interval.tick() => {
                    self.prune_expired();
                    self.log_status();
                }
            }
        }
    }

    /// Recursively scan a directory for existing `.jsonl` files and process them.
    /// Returns the number of files found and processed.
    fn scan_existing_jsonl(&self, dir: &Path) -> usize {
        let mut count = 0;
        let mut stack = vec![dir.to_path_buf()];

        while let Some(current) = stack.pop() {
            let entries = match std::fs::read_dir(&current) {
                Ok(e) => e,
                Err(e) => {
                    debug!("Cannot read dir {:?} during startup scan: {e}", current);
                    continue;
                }
            };

            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    self.handle_file_change(&path);
                    count += 1;
                }
            }
        }

        count
    }

    /// Process a file modification event: read new JSONL lines incrementally.
    fn handle_file_change(&self, path: &Path) {
        let agent = classify_path(path);
        let agent = match agent {
            Some(a) => a,
            None => return,
        };

        let session_id = extract_session_id(path, agent);
        let session_id = match session_id {
            Some(id) => id,
            None => return,
        };

        let mut state = self.state.lock();
        let tracked = state
            .entry(path.to_path_buf())
            .or_insert_with(|| TrackedFile {
                path: path.to_path_buf(),
                session_id: session_id.clone(),
                agent,
                last_event_type: None,
                last_modified: Instant::now(),
                event_count: 0,
                read_offset: 0,
                last_summary: None,
            });

        tracked.last_modified = Instant::now();

        // Incrementally read new lines from the file.
        if let Ok(mut file) = File::open(path) {
            if let Ok(metadata) = file.metadata() {
                let file_len = metadata.len();
                if file_len > tracked.read_offset {
                    if file.seek(SeekFrom::Start(tracked.read_offset)).is_ok() {
                        let reader = BufReader::new(&file);
                        let mut new_lines = 0u64;
                        let mut last_type = None;
                        let mut last_summary = None;

                        for line in reader.lines() {
                            let line = match line {
                                Ok(l) => l,
                                Err(_) => break,
                            };
                            if line.trim().is_empty() {
                                continue;
                            }
                            new_lines += 1;

                            // Parse JSONL event — extract type and brief summary.
                            if let Some((event_type, summary)) = parse_jsonl_event(&line, agent) {
                                last_type = Some(event_type);
                                last_summary = summary;
                            }
                        }

                        tracked.event_count += new_lines;
                        if let Some(t) = last_type {
                            tracked.last_event_type = Some(t);
                        }
                        if last_summary.is_some() {
                            tracked.last_summary = last_summary;
                        }
                        tracked.read_offset = file_len;

                        if new_lines > 0 {
                            trace!(
                                session = %tracked.session_id,
                                agent = %tracked.agent,
                                new_lines,
                                total = tracked.event_count,
                                last_type = tracked.last_event_type.as_deref().unwrap_or("?"),
                                "Transcript update"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Remove sessions that haven't been modified in a long time.
    fn prune_expired(&self) {
        let mut state = self.state.lock();
        let before = state.len();
        state.retain(|_, tracked| tracked.last_modified.elapsed() < SESSION_EXPIRY);
        let pruned = before - state.len();
        if pruned > 0 {
            debug!("Pruned {pruned} expired transcript sessions");
        }
    }

    /// Log a summary of active sessions at debug level (rate-limited).
    fn log_status(&self) {
        let state = self.state.lock();
        let active: Vec<_> = state
            .values()
            .filter(|t| t.streaming_status() == StreamingStatus::Active)
            .collect();
        if !active.is_empty() {
            debug!(
                "Active transcripts: {}",
                active
                    .iter()
                    .map(|t| format!(
                        "{}:{} ({}evt, {})",
                        t.agent,
                        &t.session_id[..t.session_id.len().min(12)],
                        t.event_count,
                        t.last_event_type.as_deref().unwrap_or("?")
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
}

// ── Path helpers ─────────────────────────────────────────────────────────────

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/home/builder".into()))
}

fn claude_projects_dir() -> PathBuf {
    home_dir().join(".claude").join("projects")
}

fn codex_sessions_dir() -> PathBuf {
    home_dir().join(".codex").join("sessions")
}

/// Determine agent type from file path.
fn classify_path(path: &Path) -> Option<TranscriptAgent> {
    let path_str = path.to_string_lossy();
    if path_str.contains("/.claude/projects/") {
        Some(TranscriptAgent::ClaudeCode)
    } else if path_str.contains("/.codex/sessions/") {
        Some(TranscriptAgent::Codex)
    } else {
        None
    }
}

/// Extract a session ID from the JSONL filename.
///
/// Claude Code: `~/.claude/projects/{hash}/{session-uuid}.jsonl` -> the UUID
/// Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-{timestamp}-{uuid}.jsonl` -> the UUID
fn extract_session_id(path: &Path, agent: TranscriptAgent) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy();
    match agent {
        TranscriptAgent::ClaudeCode => {
            // The filename IS the session UUID.
            Some(stem.to_string())
        }
        TranscriptAgent::Codex => {
            // Format: rollout-YYYY-MM-DDTHH-MM-SS-{uuid}
            // Extract the UUID part after the timestamp.
            // The UUID is the last 36 chars (8-4-4-4-12 with hyphens).
            if stem.starts_with("rollout-") {
                // Find the UUID: last segment after the timestamp.
                // Pattern: rollout-2026-04-02T17-00-14-019d4fff-23de-7f40-af48-2a22f9921161
                // The UUID is embedded in the last part. Extract from the session_meta
                // event if possible, but for filename-based ID, use the full stem.
                Some(stem.to_string())
            } else {
                Some(stem.to_string())
            }
        }
    }
}

// ── JSONL parsing ────────────────────────────────────────────────────────────

/// Parse a single JSONL line and extract the event type and optional summary.
/// Returns `(event_type, optional_summary)`.
fn parse_jsonl_event(line: &str, agent: TranscriptAgent) -> Option<(String, Option<String>)> {
    // Use serde_json::Value for flexible parsing without requiring exact schema.
    let value: serde_json::Value = serde_json::from_str(line).ok()?;

    match agent {
        TranscriptAgent::ClaudeCode => parse_claude_event(&value),
        TranscriptAgent::Codex => parse_codex_event(&value),
    }
}

/// Parse a Claude Code JSONL event.
///
/// Known types: "permission-mode", "system", "user", "assistant", "attachment",
/// "last-prompt", "tool_use", "tool_result", "progress".
fn parse_claude_event(value: &serde_json::Value) -> Option<(String, Option<String>)> {
    let event_type = value.get("type")?.as_str()?.to_string();
    let summary = extract_content_summary(value);
    Some((event_type, summary))
}

/// Parse a Codex JSONL event.
///
/// Known types: "session_meta", "event_msg", "response_item".
fn parse_codex_event(value: &serde_json::Value) -> Option<(String, Option<String>)> {
    let event_type = value.get("type")?.as_str()?.to_string();

    // For response_item events, try to get the role for more detail.
    let detail = if event_type == "response_item" {
        value
            .get("payload")
            .and_then(|p| p.get("role"))
            .and_then(|r| r.as_str())
            .map(|role| format!("response:{role}"))
            .unwrap_or_else(|| event_type.clone())
    } else if event_type == "event_msg" {
        value
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(|t| t.as_str())
            .map(|t| format!("event:{t}"))
            .unwrap_or_else(|| event_type.clone())
    } else {
        event_type.clone()
    };

    let summary = value
        .get("payload")
        .and_then(extract_content_summary)
        .or_else(|| extract_content_summary(value));

    Some((detail, summary))
}

/// Try to extract a brief content summary from a JSONL value.
fn extract_content_summary(value: &serde_json::Value) -> Option<String> {
    // Try "content" as string first.
    if let Some(content) = value.get("content").and_then(|c| c.as_str()) {
        return Some(truncate(content, SUMMARY_MAX_CHARS));
    }

    // Try "content" as array of objects with "text" fields.
    if let Some(content_arr) = value.get("content").and_then(|c| c.as_array()) {
        for item in content_arr {
            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                return Some(truncate(text, SUMMARY_MAX_CHARS));
            }
        }
    }

    // Try "message" field.
    if let Some(msg) = value.get("message").and_then(|m| m.as_str()) {
        return Some(truncate(msg, SUMMARY_MAX_CHARS));
    }

    None
}

/// Truncate a string to max chars, appending "..." if truncated.
fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    // Take first line only.
    let first_line = s.lines().next().unwrap_or(s);
    if first_line.len() <= max {
        first_line.to_string()
    } else {
        let mut truncated: String = first_line.chars().take(max - 3).collect();
        truncated.push_str("...");
        truncated
    }
}

// ── Convenience spawn function ───────────────────────────────────────────────

/// Create and spawn a transcript watcher, returning both the handle and
/// a shared reference for querying active sessions.
pub(crate) fn spawn_transcript_watcher() -> (tokio::task::JoinHandle<()>, TranscriptWatcherHandle) {
    let watcher = TranscriptWatcher::new();
    let handle_ref = TranscriptWatcherHandle {
        state: Arc::clone(&watcher.state),
    };
    let join = watcher.spawn();
    (join, handle_ref)
}

/// Shared handle for querying transcript watcher state from other components.
#[allow(dead_code)]
pub(crate) struct TranscriptWatcherHandle {
    state: Arc<Mutex<HashMap<PathBuf, TrackedFile>>>,
}

#[allow(dead_code)]
impl TranscriptWatcherHandle {
    /// Return a snapshot of all tracked transcript sessions.
    pub fn active_sessions(&self) -> Vec<TranscriptSession> {
        self.state.lock().values().map(|t| t.to_session()).collect()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_claude_path() {
        let path = PathBuf::from("/home/user/.claude/projects/-home-user-project/abc123.jsonl");
        assert_eq!(classify_path(&path), Some(TranscriptAgent::ClaudeCode));
    }

    #[test]
    fn classify_codex_path() {
        let path = PathBuf::from("/home/user/.codex/sessions/2026/04/02/rollout-test.jsonl");
        assert_eq!(classify_path(&path), Some(TranscriptAgent::Codex));
    }

    #[test]
    fn classify_unrelated_path() {
        let path = PathBuf::from("/tmp/random/file.jsonl");
        assert_eq!(classify_path(&path), None);
    }

    #[test]
    fn extract_claude_session_id() {
        let path = PathBuf::from(
            "/home/user/.claude/projects/-home-project/4948583c-cadc-40e8-9a24-011c09cfa008.jsonl",
        );
        let id = extract_session_id(&path, TranscriptAgent::ClaudeCode).unwrap();
        assert_eq!(id, "4948583c-cadc-40e8-9a24-011c09cfa008");
    }

    #[test]
    fn extract_codex_session_id() {
        let path = PathBuf::from(
            "/home/user/.codex/sessions/2026/04/02/rollout-2026-04-02T17-00-14-019d4fff-23de-7f40-af48-2a22f9921161.jsonl",
        );
        let id = extract_session_id(&path, TranscriptAgent::Codex).unwrap();
        assert!(id.starts_with("rollout-"));
    }

    #[test]
    fn parse_claude_user_event() {
        let line = r#"{"type":"user","content":"hello world","timestamp":"2026-04-02T00:13:52.232Z","sessionId":"abc123"}"#;
        let (event_type, summary) = parse_jsonl_event(line, TranscriptAgent::ClaudeCode).unwrap();
        assert_eq!(event_type, "user");
        assert_eq!(summary.unwrap(), "hello world");
    }

    #[test]
    fn parse_claude_assistant_event() {
        let line = r#"{"type":"assistant","content":[{"type":"text","text":"Here is the answer"}],"sessionId":"abc123"}"#;
        let (event_type, summary) = parse_jsonl_event(line, TranscriptAgent::ClaudeCode).unwrap();
        assert_eq!(event_type, "assistant");
        assert_eq!(summary.unwrap(), "Here is the answer");
    }

    #[test]
    fn parse_codex_event_msg() {
        let line = r#"{"timestamp":"2026-04-02T22:04:28.567Z","type":"event_msg","payload":{"type":"task_started"}}"#;
        let (event_type, _summary) = parse_jsonl_event(line, TranscriptAgent::Codex).unwrap();
        assert_eq!(event_type, "event:task_started");
    }

    #[test]
    fn truncate_long_string() {
        let long = "a".repeat(200);
        let result = truncate(&long, 80);
        assert_eq!(result.len(), 80);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_short_string() {
        let short = "hello";
        assert_eq!(truncate(short, 80), "hello");
    }

    #[test]
    fn streaming_status_active() {
        let tracked = TrackedFile {
            path: PathBuf::from("/tmp/test.jsonl"),
            session_id: "test".into(),
            agent: TranscriptAgent::ClaudeCode,
            last_event_type: None,
            last_modified: Instant::now(),
            event_count: 0,
            read_offset: 0,
            last_summary: None,
        };
        assert_eq!(tracked.streaming_status(), StreamingStatus::Active);
    }
}
