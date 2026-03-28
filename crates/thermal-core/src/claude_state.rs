//! ClaudeStatePoller — monitors `/tmp/claude-code-state/`, `/tmp/codex-state/`,
//! and `/tmp/copilot-state/` for agent session state files using the `notify` crate.
//!
//! Supports Claude Code, OpenAI Codex, and GitHub Copilot sessions. Files in
//! each state directory get `agent_type` inferred from the parent directory
//! name (`"claude"`, `"codex"`, or `"copilot"`).

use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use time::OffsetDateTime;
use time::Duration;
use time::format_description::well_known::Rfc3339;

/// The directory where Claude Code state JSON files are written.
const CLAUDE_STATE_DIR: &str = "/tmp/claude-code-state";

/// The directory where Codex state JSON files are written (via adapter script).
const CODEX_STATE_DIR: &str = "/tmp/codex-state";

/// The directory where Copilot state JSON files are written (via hook script).
const COPILOT_STATE_DIR: &str = "/tmp/copilot-state";

/// Codex archive sessions should disappear after the adapter's stale window.
const CODEX_MAX_AGE: Duration = Duration::hours(1);

/// Status of a Claude session.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeStatus {
    #[default]
    Idle,
    Processing,
    ToolUse,
    AwaitingInput,
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
    pub model: Option<String>,
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
            model: None,
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

impl ClaudeSessionState {
    /// Return a short, human-friendly display name derived from the `model` field.
    ///
    /// Mapping rules (checked in order via substring match):
    /// - Claude family: "opus" / "sonnet" / "haiku"
    /// - GPT family: strips "gpt-" prefix and dashes (e.g. "gpt-5.4-mini" → "gpt5.4mini")
    /// - Gemini family: strips "gemini-" prefix and trailing preview tags
    ///   (e.g. "gemini-3-pro-preview" → "gemini3pro")
    /// - o-series (OpenAI reasoning): keeps as-is but strips leading "o" prefix
    ///   handling (e.g. "o3-pro" → "o3pro", "o4-mini" → "o4mini")
    /// - Unknown models: returned trimmed as-is
    /// - `None` model: falls back to `agent_type`, or "unknown"
    pub fn model_display_name(&self) -> String {
        let Some(raw) = self.model.as_deref() else {
            // No model field — fall back to agent_type
            return self
                .agent_type
                .as_deref()
                .unwrap_or("unknown")
                .to_string();
        };

        let m = raw.trim();
        if m.is_empty() {
            return self
                .agent_type
                .as_deref()
                .unwrap_or("unknown")
                .to_string();
        }

        // --- Claude family (substring match handles version suffixes) ---
        if m.contains("opus") {
            return "opus".to_string();
        }
        if m.contains("sonnet") {
            return "sonnet".to_string();
        }
        if m.contains("haiku") {
            return "haiku".to_string();
        }

        // --- OpenAI o-series reasoning models (o3, o3-pro, o4-mini, etc.) ---
        // Must come before GPT to avoid false matches.
        if m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") {
            return m.replace('-', "");
        }

        // --- GPT family ---
        if m.starts_with("gpt-") || m.starts_with("gpt4") || m.starts_with("gpt5") {
            // Strip "gpt-" prefix, then remove dashes
            let stripped = m.strip_prefix("gpt-").unwrap_or(m);
            return format!("gpt{}", stripped.replace('-', ""));
        }

        // --- Gemini family ---
        if m.starts_with("gemini") {
            let stripped = m.strip_prefix("gemini-").unwrap_or(m);
            // Remove "-preview" suffix and dashes
            let clean = stripped.replace("-preview", "").replace('-', "");
            return format!("gemini{}", clean);
        }

        // --- Unknown: return trimmed raw string ---
        m.to_string()
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
    if parent.contains("copilot-state") {
        Some("copilot".to_string())
    } else if parent.contains("codex-state") {
        Some("codex".to_string())
    } else {
        Some("claude".to_string())
    }
}

fn status_priority(status: &ClaudeStatus) -> u8 {
    match status {
        ClaudeStatus::ToolUse => 3,
        ClaudeStatus::Processing => 2,
        ClaudeStatus::AwaitingInput => 1,
        ClaudeStatus::Idle => 0,
    }
}

fn state_supersedes(candidate: &ClaudeSessionState, current: &ClaudeSessionState) -> bool {
    let candidate_updated = candidate.last_updated.as_deref().unwrap_or("");
    let current_updated = current.last_updated.as_deref().unwrap_or("");

    if candidate_updated != current_updated {
        return candidate_updated > current_updated;
    }

    let candidate_priority = status_priority(&candidate.status);
    let current_priority = status_priority(&current.status);
    if candidate_priority != current_priority {
        return candidate_priority > current_priority;
    }

    let candidate_detail_score = [
        candidate.current_tool.is_some(),
        candidate.details.is_some(),
        candidate.working_dir.is_some(),
        candidate.pid.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    let current_detail_score = [
        current.current_tool.is_some(),
        current.details.is_some(),
        current.working_dir.is_some(),
        current.pid.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();

    candidate_detail_score > current_detail_score
}

fn collapse_sessions_by_id(
    states: impl IntoIterator<Item = ClaudeSessionState>,
) -> Vec<ClaudeSessionState> {
    let mut by_id: HashMap<String, ClaudeSessionState> = HashMap::new();
    let mut anonymous = Vec::new();

    for state in states {
        if state.session_id.is_empty() {
            anonymous.push(state);
            continue;
        }

        match by_id.entry(state.session_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(state);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if state_supersedes(&state, entry.get()) {
                    entry.insert(state);
                }
            }
        }
    }

    let mut collapsed: Vec<_> = by_id.into_values().collect();
    collapsed.extend(anonymous);
    collapsed
}

fn codex_state_is_stale(state: &ClaudeSessionState) -> bool {
    if state.agent_type.as_deref() != Some("codex") {
        return false;
    }

    let Some(last_updated) = state.last_updated.as_deref() else {
        return false;
    };
    let Ok(updated_at) = OffsetDateTime::parse(last_updated, &Rfc3339) else {
        return false;
    };
    let age = OffsetDateTime::now_utc() - updated_at;

    age > CODEX_MAX_AGE
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
    /// Create a new poller watching Claude, Codex, and Copilot state directories.
    /// Creates the directories if they do not exist.
    pub fn new() -> NotifyResult<Self> {
        let claude_dir = PathBuf::from(CLAUDE_STATE_DIR);
        let codex_dir = PathBuf::from(CODEX_STATE_DIR);
        let copilot_dir = PathBuf::from(COPILOT_STATE_DIR);

        let dirs = vec![claude_dir, codex_dir, copilot_dir];

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
                Ok(event) => match event.kind {
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
                },
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

        collapse_sessions_by_id(self.sessions.values().cloned())
    }

    /// Read all `*.json` files in all watched state directories and return
    /// the current snapshot of all sessions.
    pub fn get_all(&self) -> Vec<ClaudeSessionState> {
        let mut all = HashMap::new();
        for dir in &self.state_dirs {
            all.extend(Self::read_all_files(dir));
        }
        collapse_sessions_by_id(all.into_values())
    }

    /// Read all JSON files in a directory into a map.
    fn read_all_files(dir: &Path) -> HashMap<PathBuf, ClaudeSessionState> {
        let mut map = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if Self::is_json(&path)
                    && let Some(state) = Self::read_file(&path)
                {
                    map.insert(path, state);
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
        if codex_state_is_stale(&state) {
            return None;
        }
        Some(state)
    }

    /// Check if a path has a `.json` extension.
    fn is_json(path: &Path) -> bool {
        path.extension().is_some_and(|ext| ext == "json")
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
    fn agent_type_copilot_dir() {
        let path = Path::new("/tmp/copilot-state/session-abc.json");
        assert_eq!(agent_type_for_path(path), Some("copilot".to_string()));
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

    #[test]
    fn session_with_model_field() {
        let json = r#"{"session_id": "m", "model": "gemini-3-pro-preview"}"#;
        let s = parse(json);
        assert_eq!(s.model.as_deref(), Some("gemini-3-pro-preview"));
    }

    #[test]
    fn session_without_model_field() {
        let json = r#"{"session_id": "m"}"#;
        let s = parse(json);
        assert!(s.model.is_none());
    }

    #[test]
    fn collapse_sessions_prefers_latest_timestamp_for_same_id() {
        let older = ClaudeSessionState {
            session_id: "dup".into(),
            status: ClaudeStatus::Idle,
            last_updated: Some("2026-03-26T21:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        let newer = ClaudeSessionState {
            session_id: "dup".into(),
            status: ClaudeStatus::ToolUse,
            current_tool: Some("Bash".into()),
            last_updated: Some("2026-03-26T21:00:01Z".into()),
            ..ClaudeSessionState::default()
        };

        let collapsed = collapse_sessions_by_id(vec![older, newer]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].status, ClaudeStatus::ToolUse);
        assert_eq!(collapsed[0].current_tool.as_deref(), Some("Bash"));
    }

    #[test]
    fn collapse_sessions_prefers_richer_state_when_timestamps_match() {
        let sparse = ClaudeSessionState {
            session_id: "dup".into(),
            status: ClaudeStatus::Processing,
            last_updated: Some("2026-03-26T21:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        let rich = ClaudeSessionState {
            session_id: "dup".into(),
            status: ClaudeStatus::Processing,
            working_dir: Some("/tmp/project".into()),
            pid: Some(42),
            last_updated: Some("2026-03-26T21:00:00Z".into()),
            ..ClaudeSessionState::default()
        };

        let collapsed = collapse_sessions_by_id(vec![sparse, rich]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].working_dir.as_deref(), Some("/tmp/project"));
        assert_eq!(collapsed[0].pid, Some(42));
    }

    #[test]
    fn stale_codex_state_is_filtered() {
        let state = ClaudeSessionState {
            session_id: "old-codex".into(),
            agent_type: Some("codex".into()),
            last_updated: Some("2026-03-16T01:44:17.354Z".into()),
            ..ClaudeSessionState::default()
        };
        assert!(codex_state_is_stale(&state));
    }

    #[test]
    fn non_codex_state_is_not_filtered_by_age() {
        let state = ClaudeSessionState {
            session_id: "old-claude".into(),
            agent_type: Some("claude".into()),
            last_updated: Some("2026-03-16T01:44:17.354Z".into()),
            ..ClaudeSessionState::default()
        };
        assert!(!codex_state_is_stale(&state));
    }

    // --- model_display_name ---

    /// Helper: build a session with the given model and agent_type.
    fn session_with_model(model: Option<&str>, agent_type: Option<&str>) -> ClaudeSessionState {
        ClaudeSessionState {
            model: model.map(|s| s.to_string()),
            agent_type: agent_type.map(|s| s.to_string()),
            ..ClaudeSessionState::default()
        }
    }

    #[test]
    fn display_name_opus_variants() {
        assert_eq!(session_with_model(Some("claude-opus-4-6"), None).model_display_name(), "opus");
        assert_eq!(session_with_model(Some("claude-opus-4-20250115"), None).model_display_name(), "opus");
        assert_eq!(session_with_model(Some("opus-4-6"), None).model_display_name(), "opus");
    }

    #[test]
    fn display_name_sonnet_variants() {
        assert_eq!(session_with_model(Some("claude-sonnet-4-6"), None).model_display_name(), "sonnet");
        assert_eq!(session_with_model(Some("claude-sonnet-4-20250514"), None).model_display_name(), "sonnet");
        assert_eq!(session_with_model(Some("sonnet-4-6"), None).model_display_name(), "sonnet");
    }

    #[test]
    fn display_name_haiku_variants() {
        assert_eq!(session_with_model(Some("claude-haiku-4-5"), None).model_display_name(), "haiku");
        assert_eq!(session_with_model(Some("claude-haiku-4-20250514"), None).model_display_name(), "haiku");
        assert_eq!(session_with_model(Some("haiku-3-5"), None).model_display_name(), "haiku");
    }

    #[test]
    fn display_name_gpt_variants() {
        assert_eq!(session_with_model(Some("gpt-5.4"), None).model_display_name(), "gpt5.4");
        assert_eq!(session_with_model(Some("gpt-5.4-mini"), None).model_display_name(), "gpt5.4mini");
        assert_eq!(session_with_model(Some("gpt-4o"), None).model_display_name(), "gpt4o");
        assert_eq!(session_with_model(Some("gpt-4o-mini"), None).model_display_name(), "gpt4omini");
        assert_eq!(session_with_model(Some("gpt-4-turbo"), None).model_display_name(), "gpt4turbo");
    }

    #[test]
    fn display_name_o_series() {
        assert_eq!(session_with_model(Some("o3-pro"), None).model_display_name(), "o3pro");
        assert_eq!(session_with_model(Some("o4-mini"), None).model_display_name(), "o4mini");
        assert_eq!(session_with_model(Some("o3"), None).model_display_name(), "o3");
        assert_eq!(session_with_model(Some("o1-preview"), None).model_display_name(), "o1preview");
    }

    #[test]
    fn display_name_gemini_variants() {
        assert_eq!(session_with_model(Some("gemini-3-pro-preview"), None).model_display_name(), "gemini3pro");
        assert_eq!(session_with_model(Some("gemini-2.5-flash"), None).model_display_name(), "gemini2.5flash");
        assert_eq!(session_with_model(Some("gemini-2.5-pro-preview"), None).model_display_name(), "gemini2.5pro");
    }

    #[test]
    fn display_name_unknown_model() {
        assert_eq!(session_with_model(Some("llama-3-70b"), None).model_display_name(), "llama-3-70b");
        assert_eq!(session_with_model(Some("qwen3:8b"), None).model_display_name(), "qwen3:8b");
        assert_eq!(session_with_model(Some("mistral-large"), None).model_display_name(), "mistral-large");
    }

    #[test]
    fn display_name_none_falls_back_to_agent_type() {
        assert_eq!(session_with_model(None, Some("claude")).model_display_name(), "claude");
        assert_eq!(session_with_model(None, Some("codex")).model_display_name(), "codex");
        assert_eq!(session_with_model(None, Some("copilot")).model_display_name(), "copilot");
    }

    #[test]
    fn display_name_none_model_none_agent_type() {
        assert_eq!(session_with_model(None, None).model_display_name(), "unknown");
    }

    #[test]
    fn display_name_empty_string_falls_back() {
        assert_eq!(session_with_model(Some(""), Some("claude")).model_display_name(), "claude");
        assert_eq!(session_with_model(Some("  "), Some("codex")).model_display_name(), "codex");
        assert_eq!(session_with_model(Some(""), None).model_display_name(), "unknown");
    }

    #[test]
    fn display_name_whitespace_trimmed() {
        assert_eq!(session_with_model(Some("  claude-opus-4-6  "), None).model_display_name(), "opus");
        assert_eq!(session_with_model(Some(" gpt-5.4 "), None).model_display_name(), "gpt5.4");
    }

    // --- type alias smoke tests ---

    #[test]
    fn type_aliases_compile() {
        // Ensure the generalized aliases are usable.
        let _s: AgentSessionState = ClaudeSessionState::default();
        let _st: AgentStatus = ClaudeStatus::Idle;
    }
}
