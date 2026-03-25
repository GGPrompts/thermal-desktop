//! ClaudeStatePoller — monitors `/tmp/claude-code-state/` and `/tmp/codex-state/`
//! for agent session state files using the `notify` crate.
//!
//! Supports both Claude Code and OpenAI Codex sessions. Files in the Claude
//! state directory get `agent_type = Some("claude")`, files in the Codex state
//! directory get `agent_type = Some("codex")`.

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// The directory where Claude Code state JSON files are written.
const CLAUDE_STATE_DIR: &str = "/tmp/claude-code-state";

/// The directory where Codex state JSON files are written (via adapter script).
const CODEX_STATE_DIR: &str = "/tmp/codex-state";

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

/// Tool argument details from the Claude state file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ToolArgs {
    pub file_path: Option<String>,
    pub command: Option<String>,
    pub pattern: Option<String>,
    pub description: Option<String>,
}

/// Tool event details from the Claude state file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ToolDetails {
    pub event: Option<String>,
    pub tool: Option<String>,
    pub args: Option<ToolArgs>,
}

/// State of a single Claude session, deserialized from a JSON state file.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClaudeSessionState {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub status: ClaudeStatus,
    pub current_tool: Option<String>,
    pub subagent_count: Option<u32>,
    pub context_percent: Option<f32>,
    pub working_dir: Option<String>,
    pub last_updated: Option<String>,
    pub details: Option<ToolDetails>,
    pub hook_type: Option<String>,
    pub tmux_pane: Option<String>,
    pub pid: Option<u32>,
    pub workspace: Option<i64>,
}

impl Default for ClaudeSessionState {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            parent_session_id: None,
            agent_id: None,
            agent_type: None,
            status: ClaudeStatus::Idle,
            current_tool: None,
            subagent_count: Some(0),
            context_percent: None,
            working_dir: None,
            last_updated: None,
            details: None,
            hook_type: None,
            tmux_pane: None,
            pid: None,
            workspace: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Generalized type aliases — new code can use these cleaner names.
// ---------------------------------------------------------------------------

/// Alias for [`ClaudeStatePoller`] — watches both Claude and Codex state dirs.
pub type AgentStatePoller = ClaudeStatePoller;

/// Alias for [`ClaudeSessionState`] — represents any agent session.
pub type AgentSessionState = ClaudeSessionState;

/// Alias for [`ClaudeStatus`] — agent-agnostic status enum.
pub type AgentStatus = ClaudeStatus;

// ---------------------------------------------------------------------------
// Poller
// ---------------------------------------------------------------------------

/// Infer the `agent_type` string from a state file's parent directory.
fn agent_type_for_path(path: &Path) -> Option<String> {
    let parent = path.parent()?.to_str()?;
    if parent.contains("codex-state") {
        Some("codex".to_string())
    } else {
        Some("claude".to_string())
    }
}

/// Watches `/tmp/claude-code-state/` and `/tmp/codex-state/` for agent session
/// state file changes.
///
/// Uses the `notify` crate's recommended (OS-native) watcher. Call
/// [`ClaudeStatePoller::poll`] regularly to drain events and re-read changed
/// files, or [`ClaudeStatePoller::get_all`] for a full snapshot.
pub struct ClaudeStatePoller {
    _watchers: Vec<RecommendedWatcher>,
    rx: mpsc::Receiver<NotifyResult<Event>>,
    state_dirs: Vec<PathBuf>,
    /// Cached session states keyed by file path.
    sessions: HashMap<PathBuf, ClaudeSessionState>,
}

impl ClaudeStatePoller {
    /// Create a new poller watching both Claude and Codex state directories.
    /// Creates the directories if they do not exist.
    pub fn new() -> NotifyResult<Self> {
        let claude_dir = PathBuf::from(CLAUDE_STATE_DIR);
        let codex_dir = PathBuf::from(CODEX_STATE_DIR);

        let dirs = vec![claude_dir, codex_dir];

        // Ensure state directories exist.
        for dir in &dirs {
            if !dir.exists() {
                let _ = std::fs::create_dir_all(dir);
            }
        }

        let (tx, rx) = mpsc::channel();
        let mut watchers = Vec::new();

        for dir in &dirs {
            let tx_clone = tx.clone();
            let mut watcher = notify::recommended_watcher(tx_clone)?;
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
            watchers.push(watcher);
        }

        // Read initial state from all directories.
        let mut sessions = HashMap::new();
        for dir in &dirs {
            sessions.extend(Self::read_all_files(dir));
        }

        Ok(Self {
            _watchers: watchers,
            rx,
            state_dirs: dirs,
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

    /// Read all `*.json` files in all watched state directories and return
    /// the current snapshot of all sessions.
    pub fn get_all(&self) -> Vec<ClaudeSessionState> {
        let mut all = HashMap::new();
        for dir in &self.state_dirs {
            all.extend(Self::read_all_files(dir));
        }
        all.into_values().collect()
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

    /// Parse a single JSON state file, setting `agent_type` based on the
    /// parent directory if not already set in the JSON.
    fn read_file(path: &Path) -> Option<ClaudeSessionState> {
        let data = std::fs::read_to_string(path).ok()?;
        let mut state: ClaudeSessionState = serde_json::from_str(&data).ok()?;
        // Set agent_type from directory if not already specified in JSON.
        if state.agent_type.is_none() {
            state.agent_type = agent_type_for_path(path);
        }
        Some(state)
    }

    /// Check if a path has a `.json` extension.
    fn is_json(path: &Path) -> bool {
        path.extension().map_or(false, |ext| ext == "json")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: deserialise a JSON string into ClaudeSessionState.
    fn parse(json: &str) -> ClaudeSessionState {
        serde_json::from_str(json).expect("JSON should parse")
    }

    // --- ClaudeStatus deserialization ---

    #[test]
    fn status_idle_deserializes() {
        let s: ClaudeStatus = serde_json::from_str("\"idle\"").unwrap();
        assert_eq!(s, ClaudeStatus::Idle);
    }

    #[test]
    fn status_processing_deserializes() {
        let s: ClaudeStatus = serde_json::from_str("\"processing\"").unwrap();
        assert_eq!(s, ClaudeStatus::Processing);
    }

    #[test]
    fn status_tool_use_deserializes() {
        let s: ClaudeStatus = serde_json::from_str("\"tool_use\"").unwrap();
        assert_eq!(s, ClaudeStatus::ToolUse);
    }

    #[test]
    fn status_awaiting_input_deserializes() {
        let s: ClaudeStatus = serde_json::from_str("\"awaiting_input\"").unwrap();
        assert_eq!(s, ClaudeStatus::AwaitingInput);
    }

    #[test]
    fn status_unknown_string_fails() {
        let result: Result<ClaudeStatus, _> = serde_json::from_str("\"unknown_variant\"");
        assert!(result.is_err());
    }

    #[test]
    fn status_default_is_idle() {
        assert_eq!(ClaudeStatus::default(), ClaudeStatus::Idle);
    }

    // --- ClaudeSessionState happy path ---

    #[test]
    fn session_full_deserializes() {
        let json = r#"{
            "session_id": "abc-123",
            "status": "processing",
            "current_tool": "Bash",
            "subagent_count": 2,
            "context_percent": 42.5,
            "working_dir": "/home/user/project",
            "last_updated": "2026-03-16T12:00:00Z",
            "hook_type": "pre_tool",
            "tmux_pane": "%1",
            "pid": 9876
        }"#;
        let s = parse(json);
        assert_eq!(s.session_id, "abc-123");
        assert_eq!(s.status, ClaudeStatus::Processing);
        assert_eq!(s.current_tool.as_deref(), Some("Bash"));
        assert_eq!(s.subagent_count, Some(2));
        assert!((s.context_percent.unwrap() - 42.5).abs() < 1e-5);
        assert_eq!(s.working_dir.as_deref(), Some("/home/user/project"));
        assert_eq!(s.hook_type.as_deref(), Some("pre_tool"));
        assert_eq!(s.tmux_pane.as_deref(), Some("%1"));
        assert_eq!(s.pid, Some(9876));
    }

    #[test]
    fn session_minimal_uses_defaults() {
        // Only session_id provided; all other fields should fall back to defaults.
        let json = r#"{"session_id": "min-session"}"#;
        let s = parse(json);
        assert_eq!(s.session_id, "min-session");
        assert_eq!(s.status, ClaudeStatus::Idle);
        assert!(s.current_tool.is_none());
        assert!(s.context_percent.is_none());
        assert!(s.working_dir.is_none());
    }

    #[test]
    fn session_empty_object_uses_defaults() {
        let s: ClaudeSessionState = serde_json::from_str("{}").unwrap();
        assert_eq!(s.session_id, "");
        assert_eq!(s.status, ClaudeStatus::Idle);
    }

    #[test]
    fn session_default_subagent_count() {
        // Default impl sets subagent_count to Some(0).
        let s = ClaudeSessionState::default();
        assert_eq!(s.subagent_count, Some(0));
    }

    // --- ToolDetails / ToolArgs deserialization ---

    #[test]
    fn session_with_tool_details_deserializes() {
        let json = r#"{
            "session_id": "td-session",
            "status": "tool_use",
            "details": {
                "event": "tool_start",
                "tool": "Read",
                "args": {
                    "file_path": "/some/file.rs",
                    "command": null,
                    "pattern": null,
                    "description": "reading a file"
                }
            }
        }"#;
        let s = parse(json);
        assert_eq!(s.status, ClaudeStatus::ToolUse);
        let details = s.details.expect("details should be present");
        assert_eq!(details.event.as_deref(), Some("tool_start"));
        assert_eq!(details.tool.as_deref(), Some("Read"));
        let args = details.args.expect("args should be present");
        assert_eq!(args.file_path.as_deref(), Some("/some/file.rs"));
        assert_eq!(args.description.as_deref(), Some("reading a file"));
    }

    #[test]
    fn tool_args_all_none_when_omitted() {
        let json = r#"{"session_id": "x", "details": {"event": "e"}}"#;
        let s = parse(json);
        let details = s.details.unwrap();
        // args omitted → None
        assert!(details.args.is_none());
    }

    #[test]
    fn tool_args_partial_fields() {
        let json = r#"{
            "session_id": "partial",
            "details": {
                "args": {"command": "ls -la"}
            }
        }"#;
        let s = parse(json);
        let args = s.details.unwrap().args.unwrap();
        assert_eq!(args.command.as_deref(), Some("ls -la"));
        assert!(args.file_path.is_none());
        assert!(args.pattern.is_none());
        assert!(args.description.is_none());
    }

    // --- Edge cases ---

    #[test]
    fn malformed_json_returns_error() {
        let result: Result<ClaudeSessionState, _> = serde_json::from_str("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn truncated_json_returns_error() {
        let result: Result<ClaudeSessionState, _> = serde_json::from_str(r#"{"session_id":"#);
        assert!(result.is_err());
    }

    #[test]
    fn context_percent_zero() {
        let json = r#"{"session_id": "ctx", "context_percent": 0.0}"#;
        let s = parse(json);
        assert!((s.context_percent.unwrap() - 0.0).abs() < 1e-6);
    }

    #[test]
    fn context_percent_one_hundred() {
        let json = r#"{"session_id": "ctx", "context_percent": 100.0}"#;
        let s = parse(json);
        assert!((s.context_percent.unwrap() - 100.0).abs() < 1e-4);
    }

    #[test]
    fn pid_zero_is_valid() {
        let json = r#"{"session_id": "p", "pid": 0}"#;
        let s = parse(json);
        assert_eq!(s.pid, Some(0));
    }

    #[test]
    fn is_json_detects_json_extension() {
        use std::path::Path;
        assert!(ClaudeStatePoller::is_json(Path::new("state.json")));
        assert!(!ClaudeStatePoller::is_json(Path::new("state.toml")));
        assert!(!ClaudeStatePoller::is_json(Path::new("state")));
        assert!(!ClaudeStatePoller::is_json(Path::new("")));
    }

    // --- agent_type_for_path ---

    #[test]
    fn agent_type_claude_dir() {
        let path = Path::new("/tmp/claude-code-state/session-abc.json");
        assert_eq!(agent_type_for_path(path), Some("claude".to_string()));
    }

    #[test]
    fn agent_type_codex_dir() {
        let path = Path::new("/tmp/codex-state/session-xyz.json");
        assert_eq!(agent_type_for_path(path), Some("codex".to_string()));
    }

    #[test]
    fn agent_type_unknown_dir_defaults_to_claude() {
        let path = Path::new("/tmp/other-state/session.json");
        assert_eq!(agent_type_for_path(path), Some("claude".to_string()));
    }

    #[test]
    fn session_with_agent_type_preserves_it() {
        let json = r#"{"session_id": "typed", "agent_type": "codex"}"#;
        let s = parse(json);
        assert_eq!(s.agent_type.as_deref(), Some("codex"));
    }

    // --- type alias smoke tests ---

    #[test]
    fn type_aliases_compile() {
        // Ensure the generalized aliases are usable.
        let _s: AgentSessionState = ClaudeSessionState::default();
        let _st: AgentStatus = ClaudeStatus::Idle;
    }
}
