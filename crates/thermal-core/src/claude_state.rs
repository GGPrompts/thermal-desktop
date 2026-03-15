//! ClaudeStatePoller — monitors `/tmp/claude-code-state/` for Claude session
//! state files using the `notify` crate.

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// The directory where Claude Code state JSON files are written.
const STATE_DIR: &str = "/tmp/claude-code-state";

/// Status of a Claude session.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeStatus {
    Idle,
    Processing,
    ToolUse,
    AwaitingInput,
}

impl Default for ClaudeStatus {
    fn default() -> Self {
        Self::Idle
    }
}

/// State of a single Claude session, deserialized from a JSON state file.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClaudeSessionState {
    pub session_id: String,
    pub status: ClaudeStatus,
    pub current_tool: Option<String>,
    pub subagent_count: Option<u32>,
    pub context_percent: Option<f32>,
    pub working_dir: Option<String>,
    pub last_updated: Option<String>,
}

impl Default for ClaudeSessionState {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            status: ClaudeStatus::Idle,
            current_tool: None,
            subagent_count: Some(0),
            context_percent: None,
            working_dir: None,
            last_updated: None,
        }
    }
}

/// Watches `/tmp/claude-code-state/` for Claude session state file changes.
///
/// Uses the `notify` crate's recommended (OS-native) watcher. Call
/// [`ClaudeStatePoller::poll`] regularly to drain events and re-read changed
/// files, or [`ClaudeStatePoller::get_all`] for a full snapshot.
pub struct ClaudeStatePoller {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<NotifyResult<Event>>,
    state_dir: PathBuf,
    /// Cached session states keyed by file path.
    sessions: HashMap<PathBuf, ClaudeSessionState>,
}

impl ClaudeStatePoller {
    /// Create a new poller watching `/tmp/claude-code-state/`.
    /// Creates the directory if it does not exist.
    pub fn new() -> NotifyResult<Self> {
        let state_dir = PathBuf::from(STATE_DIR);

        // Ensure the state directory exists.
        if !state_dir.exists() {
            let _ = std::fs::create_dir_all(&state_dir);
        }

        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(tx)?;
        watcher.watch(&state_dir, RecursiveMode::NonRecursive)?;

        // Read initial state.
        let sessions = Self::read_all_files(&state_dir);

        Ok(Self {
            _watcher: watcher,
            rx,
            state_dir,
            sessions,
        })
    }

    /// Drain pending file-change events, re-read changed JSON files, and
    /// return the current list of all sessions.
    pub fn poll(&mut self) -> Vec<ClaudeSessionState> {
        let mut dirty_paths: Vec<PathBuf> = Vec::new();
        let mut removed_paths: Vec<PathBuf> = Vec::new();

        while let Ok(result) = self.rx.try_recv() {
            match result {
                Ok(event) => {
                    match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) => {
                            for path in &event.paths {
                                if Self::is_json(path) && !dirty_paths.contains(path) {
                                    dirty_paths.push(path.clone());
                                }
                            }
                        }
                        EventKind::Remove(_) => {
                            for path in &event.paths {
                                if Self::is_json(path) {
                                    removed_paths.push(path.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Err(_) => {
                    // Watcher error — silently skip.
                }
            }
        }

        // Remove deleted sessions.
        for path in &removed_paths {
            self.sessions.remove(path);
        }

        // Re-read dirty files.
        for path in &dirty_paths {
            if let Some(state) = Self::read_file(path) {
                self.sessions.insert(path.clone(), state);
            }
        }

        self.sessions.values().cloned().collect()
    }

    /// Read all `*.json` files in the state directory and return the current
    /// snapshot of all sessions.
    pub fn get_all(&self) -> Vec<ClaudeSessionState> {
        Self::read_all_files(&self.state_dir)
            .into_values()
            .collect()
    }

    /// Read all JSON files in a directory into a map.
    fn read_all_files(dir: &Path) -> HashMap<PathBuf, ClaudeSessionState> {
        let mut map = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if Self::is_json(&path) {
                    if let Some(state) = Self::read_file(&path) {
                        map.insert(path, state);
                    }
                }
            }
        }
        map
    }

    /// Parse a single JSON state file.
    fn read_file(path: &Path) -> Option<ClaudeSessionState> {
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Check if a path has a `.json` extension.
    fn is_json(path: &Path) -> bool {
        path.extension().map_or(false, |ext| ext == "json")
    }
}
