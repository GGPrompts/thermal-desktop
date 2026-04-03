//! ClaudeStatePoller — monitors `/tmp/claude-code-state/`, `/tmp/codex-state/`,
//! and `/tmp/copilot-state/` for agent session state files using the `notify` crate.
//!
//! Supports Claude Code, OpenAI Codex, and GitHub Copilot sessions. Files in
//! each state directory get `agent_type` inferred from the parent directory
//! name (`"claude"`, `"codex"`, or `"copilot"`).
//!
//! # State authority boundary
//!
//! The conductor daemon (`thc daemon`) owns **a single `ClaudeStatePoller`**
//! instance and imports file-derived sessions into its `SemanticEventBus`.
//! Components that run alongside the daemon (TUI, HUD, window, audio) should
//! subscribe to the daemon's semantic event stream for real-time state. They
//! fall back to creating their own `ClaudeStatePoller` only when the daemon is
//! unavailable (standalone / unmanaged mode).
//!
//! Direct `/tmp` state file reads are still the normal path for:
//! - **Standalone CLI tools** (`thc status`, `thermal-monitor`) that run
//!   without the daemon.
//! - **thermal-bar agent module** — a lightweight Wayland bar that avoids
//!   async daemon connections for simplicity.
//! - **Stale-session GC** in the TUI, which checks file existence as a
//!   last-resort liveness signal.
//!
//! Voice state (`/tmp/thermal-voice-state.json`) is a **separate chain**
//! with its own producer (thermal-voice) and consumers (bar, HUD, audio).
//! It does not flow through the conductor daemon.

use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::{debug, trace, warn};

/// The directory where Claude Code state JSON files are written.
const CLAUDE_STATE_DIR: &str = "/tmp/claude-code-state";

/// The directory where Codex state JSON files are written (via adapter script).
const CODEX_STATE_DIR: &str = "/tmp/codex-state";

/// The directory where Copilot state JSON files are written (via hook script).
const COPILOT_STATE_DIR: &str = "/tmp/copilot-state";

/// Sessions older than this without a live PID are considered dead.
const SESSION_MAX_AGE: Duration = Duration::hours(2);

/// How often to run the PID liveness + staleness sweep (avoid syscall spam).
const PRUNE_INTERVAL: Duration = Duration::seconds(30);

// All state types are now ggl-generated.
// See ggl_types.rs for type aliases and Default impls.
// Re-export so downstream `use thermal_core::claude_state::{...}` still works.
pub use crate::ggl_types::{ClaudeStatus, ToolArgs, ToolDetails};

/// State of a single agent session, deserialized from a JSON state file.
///
/// This is a type alias for the ggl-generated `SessionState` type. The name
/// `ClaudeSessionState` is retained for backward compatibility across the
/// codebase.
pub type ClaudeSessionState = crate::ggl_types::SessionState;

/// Extension trait adding display-name logic to `SessionState` / `ClaudeSessionState`.
///
/// Since `SessionState` is defined in `thermal-protocol`, we cannot add inherent
/// methods in this crate. Import this trait to call `.model_display_name()`.
pub trait SessionStateExt {
    /// Return a short, human-friendly display name derived from the `model` field.
    ///
    /// Delegates to the free function [`model_display_name`] for the actual
    /// mapping. Falls back to `agent_type` when no model is set.
    fn model_display_name(&self) -> String;
}

impl SessionStateExt for ClaudeSessionState {
    fn model_display_name(&self) -> String {
        let Some(raw) = self.model.as_deref() else {
            // No model field — fall back to agent_type
            return self.agent_type.as_deref().unwrap_or("unknown").to_string();
        };

        let m = raw.trim();
        if m.is_empty() {
            return self.agent_type.as_deref().unwrap_or("unknown").to_string();
        }

        model_display_name(m)
    }
}

// ---------------------------------------------------------------------------
// Declarative model registry
// ---------------------------------------------------------------------------

/// How a model family maps raw model IDs to short display names.
#[derive(Debug, Clone, Copy)]
enum ModelTransform {
    /// Substring match → fixed display name (e.g. "opus" in ID → "opus").
    Substring,
    /// Prefix match → strip prefix, remove dashes, prepend family prefix.
    /// For GPT: "gpt-5.4-mini" → strip "gpt-" → "5.4mini" → prepend "gpt" → "gpt5.4mini".
    /// Also matches bare prefix without dash (e.g. "gpt4o").
    StripPrefix,
    /// Prefix match → strip prefix, remove "-preview" and dashes, prepend family prefix.
    /// For Gemini: "gemini-3-pro-preview" → strip "gemini-" → "3pro" → prepend "gemini" → "gemini3pro".
    StripPrefixAndPreview,
    /// Prefix match → remove all dashes.
    /// For o-series: "o3-pro" → "o3pro".
    RemoveDashes,
}

/// A single entry in the model registry.
#[derive(Debug, Clone, Copy)]
struct ModelEntry {
    /// Pattern to match in the (trimmed, original-case) model ID.
    /// For `Substring`: checked via `contains()`.
    /// For `StripPrefix`/`StripPrefixAndPreview`: checked via `starts_with()` on
    ///   `"{pattern}-"` or `"{pattern}"` (bare, for IDs like "gpt4o").
    /// For `RemoveDashes`: checked via `starts_with()`.
    pattern: &'static str,
    /// The family prefix used in the output (e.g. "gpt", "gemini").
    /// Only relevant for `StripPrefix` and `StripPrefixAndPreview`.
    display_prefix: &'static str,
    /// How to transform matched model IDs.
    transform: ModelTransform,
}

/// Canonical model registry. Checked in order — first match wins.
///
/// To add a new model family: append an entry here. The `model_display_name()`
/// function and `state_inference.rs`'s model regex
/// (`crates/thermal-terminal/src/state_inference.rs`) should both stay in sync.
const MODEL_REGISTRY: &[ModelEntry] = &[
    // --- Anthropic Claude (substring match, order doesn't matter) ---
    ModelEntry {
        pattern: "opus",
        display_prefix: "opus",
        transform: ModelTransform::Substring,
    },
    ModelEntry {
        pattern: "sonnet",
        display_prefix: "sonnet",
        transform: ModelTransform::Substring,
    },
    ModelEntry {
        pattern: "haiku",
        display_prefix: "haiku",
        transform: ModelTransform::Substring,
    },
    // --- OpenAI o-series reasoning (must come before GPT to avoid false prefix match) ---
    ModelEntry {
        pattern: "o1",
        display_prefix: "",
        transform: ModelTransform::RemoveDashes,
    },
    ModelEntry {
        pattern: "o3",
        display_prefix: "",
        transform: ModelTransform::RemoveDashes,
    },
    ModelEntry {
        pattern: "o4",
        display_prefix: "",
        transform: ModelTransform::RemoveDashes,
    },
    // --- OpenAI GPT ---
    ModelEntry {
        pattern: "gpt",
        display_prefix: "gpt",
        transform: ModelTransform::StripPrefix,
    },
    // --- Google Gemini ---
    ModelEntry {
        pattern: "gemini",
        display_prefix: "gemini",
        transform: ModelTransform::StripPrefixAndPreview,
    },
];

/// Map a raw model ID string to a short, human-friendly display name.
///
/// This is the canonical mapping used across the thermal ecosystem.
/// See [`MODEL_REGISTRY`] for the full list of recognized model families.
///
/// Mapping rules (checked in order):
/// - Claude family: substring match → "opus" / "sonnet" / "haiku"
/// - o-series: prefix match → remove dashes (e.g. "o3-pro" → "o3pro")
/// - GPT family: prefix match → strip "gpt-" prefix and dashes (e.g. "gpt-5.4-mini" → "gpt5.4mini")
/// - Gemini family: prefix match → strip "gemini-" prefix, "-preview" suffix, dashes
///   (e.g. "gemini-3-pro-preview" → "gemini3pro")
/// - Unknown: returned as-is
pub fn model_display_name(model_id: &str) -> String {
    let m = model_id.trim();
    if m.is_empty() {
        return "unknown".to_string();
    }

    for entry in MODEL_REGISTRY {
        match entry.transform {
            ModelTransform::Substring => {
                if m.contains(entry.pattern) {
                    return entry.display_prefix.to_string();
                }
            }
            ModelTransform::RemoveDashes => {
                if m.starts_with(entry.pattern) {
                    return m.replace('-', "");
                }
            }
            ModelTransform::StripPrefix => {
                // Match "gpt-..." or bare "gpt4o" / "gpt5..."
                let prefix_dash = format!("{}-", entry.pattern);
                if m.starts_with(&prefix_dash) || m.starts_with(entry.pattern) {
                    let stripped = m.strip_prefix(&prefix_dash).unwrap_or(m);
                    return format!("{}{}", entry.display_prefix, stripped.replace('-', ""));
                }
            }
            ModelTransform::StripPrefixAndPreview => {
                if m.starts_with(entry.pattern) {
                    let prefix_dash = format!("{}-", entry.pattern);
                    let stripped = m.strip_prefix(&prefix_dash).unwrap_or(m);
                    let clean = stripped.replace("-preview", "").replace('-', "");
                    return format!("{}{}", entry.display_prefix, clean);
                }
            }
        }
    }

    // Unknown model — return trimmed as-is
    m.to_string()
}

/// Returns `true` if the given model name matches any known family in the
/// [`MODEL_REGISTRY`].
///
/// Useful for validating model strings detected from terminal output.
pub fn is_known_model(model_id: &str) -> bool {
    let m = model_id.trim();
    if m.is_empty() {
        return false;
    }
    for entry in MODEL_REGISTRY {
        let matched = match entry.transform {
            ModelTransform::Substring => m.contains(entry.pattern),
            ModelTransform::RemoveDashes => m.starts_with(entry.pattern),
            ModelTransform::StripPrefix => {
                let prefix_dash = format!("{}-", entry.pattern);
                m.starts_with(&prefix_dash) || m.starts_with(entry.pattern)
            }
            ModelTransform::StripPrefixAndPreview => m.starts_with(entry.pattern),
        };
        if matched {
            return true;
        }
    }
    false
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

/// Returns `true` if `source` is `Some("hook")`, indicating the session
/// identity was reported directly by Claude Code hooks (ground truth) rather
/// than inferred heuristically from PTY or tmux state.
fn is_hook_sourced(state: &ClaudeSessionState) -> bool {
    state.source.as_deref() == Some("hook")
}

fn state_supersedes(candidate: &ClaudeSessionState, current: &ClaudeSessionState) -> bool {
    // Hook-sourced sessions are authoritative — prefer them over heuristic ones.
    let candidate_hook = is_hook_sourced(candidate);
    let current_hook = is_hook_sourced(current);
    if candidate_hook != current_hook {
        return candidate_hook;
    }

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
    // Stable ordering so UI tab positions don't shuffle every poll cycle.
    collapsed.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    collapsed
}

/// Check if a process with the given PID is still alive.
fn pid_is_alive(pid: i64) -> bool {
    use nix::sys::signal;
    use nix::unistd::Pid;
    // kill(pid, 0) checks existence without sending a signal.
    i32::try_from(pid).map_or(false, |p| signal::kill(Pid::from_raw(p), None).is_ok())
}

/// A session is considered dead if:
/// 1. It has a PID and that process is no longer running, OR
/// 2. It has no PID and `last_updated` is older than SESSION_MAX_AGE.
fn session_is_dead(state: &ClaudeSessionState) -> bool {
    // Grace period: if the state was updated recently, trust it regardless of PID.
    // Hook PIDs may be ephemeral (short-lived subprocesses), so a live session can
    // have a dead PID between hook invocations.
    const RECENT_UPDATE_GRACE: Duration = Duration::seconds(120);
    if let Some(last_updated) = state.last_updated.as_deref() {
        if let Ok(updated_at) = OffsetDateTime::parse(last_updated, &Rfc3339) {
            let now = OffsetDateTime::now_utc();
            if (now - updated_at) < RECENT_UPDATE_GRACE {
                trace!(session_id = %state.session_id, "session in grace period, skipping dead check");
                return false;
            }
        }
    }

    // PID-based liveness check.
    if let Some(pid) = state.pid {
        if pid > 0 && !pid_is_alive(pid) {
            debug!(session_id = %state.session_id, pid, "session dead: PID not alive");
            return true;
        }
        // PID is alive — session is live regardless of age.
        return false;
    }

    // No PID — fall back to age-based staleness.
    let Some(last_updated) = state.last_updated.as_deref() else {
        return false;
    };
    let Ok(updated_at) = OffsetDateTime::parse(last_updated, &Rfc3339) else {
        return false;
    };
    let dead = (OffsetDateTime::now_utc() - updated_at) > SESSION_MAX_AGE;
    if dead {
        debug!(session_id = %state.session_id, last_updated, "session dead: no PID and timestamp is stale");
    }
    dead
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
    /// Last time we ran the dead-session prune sweep.
    last_prune: std::time::Instant,
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
            last_prune: std::time::Instant::now(),
        })
    }

    /// Drain pending file-change events, re-read changed JSON files, and
    /// return the current list of all sessions.
    pub fn poll(&mut self) -> Vec<ClaudeSessionState> {
        let mut dirty_paths: Vec<PathBuf> = Vec::new();
        let mut removed_paths: Vec<PathBuf> = Vec::new();
        let mut event_count: usize = 0;

        while let Ok(result) = self.rx.try_recv() {
            event_count += 1;
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
                Err(e) => {
                    warn!(error = %e, "file watcher error");
                }
            }
        }

        if event_count > 0 {
            debug!(
                events = event_count,
                dirty = dirty_paths.len(),
                removed = removed_paths.len(),
                "poll drain batch"
            );
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

        // Periodically prune dead sessions (PID gone or stale timestamp).
        let prune_interval = std::time::Duration::try_from(PRUNE_INTERVAL)
            .unwrap_or(std::time::Duration::from_secs(30));
        if self.last_prune.elapsed() >= prune_interval {
            self.prune_dead_sessions();
            self.last_prune = std::time::Instant::now();
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
        let start = std::time::Instant::now();
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to read state file");
                return None;
            }
        };
        let mut state: ClaudeSessionState = match serde_json::from_str(&data) {
            Ok(s) => s,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to parse state file JSON");
                return None;
            }
        };
        let parse_elapsed = start.elapsed();
        // Set agent_type from directory if not already specified in JSON.
        if state.agent_type.is_none() {
            state.agent_type = agent_type_for_path(path);
        }
        if session_is_dead(&state) {
            return None;
        }
        trace!(
            session_id = %state.session_id,
            status = ?state.status,
            parse_us = parse_elapsed.as_micros() as u64,
            path = %path.display(),
            "read state file"
        );
        Some(state)
    }

    /// Check if a path has a `.json` extension.
    fn is_json(path: &Path) -> bool {
        path.extension().is_some_and(|ext| ext == "json")
    }

    /// Remove cached sessions whose PID is dead or whose timestamp is stale,
    /// and delete the corresponding state files from disk.
    fn prune_dead_sessions(&mut self) {
        let dead_paths: Vec<PathBuf> = self
            .sessions
            .iter()
            .filter(|(_, state)| session_is_dead(state))
            .map(|(path, _)| path.clone())
            .collect();

        for path in dead_paths {
            let session_id = self
                .sessions
                .get(&path)
                .map(|s| s.session_id.as_str())
                .unwrap_or("?")
                .to_string();
            debug!(session_id = %session_id, path = %path.display(), "pruning dead session");
            self.sessions.remove(&path);
            // Best-effort cleanup of the orphaned state file.
            if let Err(e) = std::fs::remove_file(&path) {
                warn!(path = %path.display(), error = %e, "failed to remove dead state file");
            }
        }
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
    fn stale_session_without_pid_is_dead() {
        // No PID, old timestamp — should be detected as dead.
        let state = ClaudeSessionState {
            session_id: "old-codex".into(),
            agent_type: Some("codex".into()),
            last_updated: Some("2024-01-01T00:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        assert!(session_is_dead(&state));
    }

    #[test]
    fn stale_copilot_without_pid_is_dead() {
        // Copilot sessions without a live PID and old timestamp should be pruned.
        let state = ClaudeSessionState {
            session_id: "old-copilot".into(),
            agent_type: Some("copilot".into()),
            last_updated: Some("2024-01-01T00:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        assert!(session_is_dead(&state));
    }

    #[test]
    fn session_with_dead_pid_is_dead() {
        // PID 999999999 should not exist.
        let state = ClaudeSessionState {
            session_id: "dead-pid".into(),
            agent_type: Some("claude".into()),
            pid: Some(999_999_999),
            last_updated: Some("2026-03-30T12:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        assert!(session_is_dead(&state));
    }

    #[test]
    fn session_with_live_pid_is_not_dead() {
        // Use our own PID — guaranteed to be alive.
        let state = ClaudeSessionState {
            session_id: "live".into(),
            agent_type: Some("claude".into()),
            pid: Some(std::process::id() as i64),
            last_updated: Some("2024-01-01T00:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        assert!(!session_is_dead(&state));
    }

    #[test]
    fn fresh_session_without_pid_is_not_dead() {
        use time::OffsetDateTime;
        use time::format_description::well_known::Rfc3339;
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        let state = ClaudeSessionState {
            session_id: "fresh".into(),
            agent_type: Some("copilot".into()),
            last_updated: Some(now),
            ..ClaudeSessionState::default()
        };
        assert!(!session_is_dead(&state));
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
        assert_eq!(
            session_with_model(Some("claude-opus-4-6"), None).model_display_name(),
            "opus"
        );
        assert_eq!(
            session_with_model(Some("claude-opus-4-20250115"), None).model_display_name(),
            "opus"
        );
        assert_eq!(
            session_with_model(Some("opus-4-6"), None).model_display_name(),
            "opus"
        );
    }

    #[test]
    fn display_name_sonnet_variants() {
        assert_eq!(
            session_with_model(Some("claude-sonnet-4-6"), None).model_display_name(),
            "sonnet"
        );
        assert_eq!(
            session_with_model(Some("claude-sonnet-4-20250514"), None).model_display_name(),
            "sonnet"
        );
        assert_eq!(
            session_with_model(Some("sonnet-4-6"), None).model_display_name(),
            "sonnet"
        );
    }

    #[test]
    fn display_name_haiku_variants() {
        assert_eq!(
            session_with_model(Some("claude-haiku-4-5"), None).model_display_name(),
            "haiku"
        );
        assert_eq!(
            session_with_model(Some("claude-haiku-4-20250514"), None).model_display_name(),
            "haiku"
        );
        assert_eq!(
            session_with_model(Some("haiku-3-5"), None).model_display_name(),
            "haiku"
        );
    }

    #[test]
    fn display_name_gpt_variants() {
        assert_eq!(
            session_with_model(Some("gpt-5.4"), None).model_display_name(),
            "gpt5.4"
        );
        assert_eq!(
            session_with_model(Some("gpt-5.4-mini"), None).model_display_name(),
            "gpt5.4mini"
        );
        assert_eq!(
            session_with_model(Some("gpt-4o"), None).model_display_name(),
            "gpt4o"
        );
        assert_eq!(
            session_with_model(Some("gpt-4o-mini"), None).model_display_name(),
            "gpt4omini"
        );
        assert_eq!(
            session_with_model(Some("gpt-4-turbo"), None).model_display_name(),
            "gpt4turbo"
        );
    }

    #[test]
    fn display_name_o_series() {
        assert_eq!(
            session_with_model(Some("o3-pro"), None).model_display_name(),
            "o3pro"
        );
        assert_eq!(
            session_with_model(Some("o4-mini"), None).model_display_name(),
            "o4mini"
        );
        assert_eq!(
            session_with_model(Some("o3"), None).model_display_name(),
            "o3"
        );
        assert_eq!(
            session_with_model(Some("o1-preview"), None).model_display_name(),
            "o1preview"
        );
    }

    #[test]
    fn display_name_gemini_variants() {
        assert_eq!(
            session_with_model(Some("gemini-3-pro-preview"), None).model_display_name(),
            "gemini3pro"
        );
        assert_eq!(
            session_with_model(Some("gemini-2.5-flash"), None).model_display_name(),
            "gemini2.5flash"
        );
        assert_eq!(
            session_with_model(Some("gemini-2.5-pro-preview"), None).model_display_name(),
            "gemini2.5pro"
        );
    }

    #[test]
    fn display_name_unknown_model() {
        assert_eq!(
            session_with_model(Some("llama-3-70b"), None).model_display_name(),
            "llama-3-70b"
        );
        assert_eq!(
            session_with_model(Some("qwen3:8b"), None).model_display_name(),
            "qwen3:8b"
        );
        assert_eq!(
            session_with_model(Some("mistral-large"), None).model_display_name(),
            "mistral-large"
        );
    }

    #[test]
    fn display_name_none_falls_back_to_agent_type() {
        assert_eq!(
            session_with_model(None, Some("claude")).model_display_name(),
            "claude"
        );
        assert_eq!(
            session_with_model(None, Some("codex")).model_display_name(),
            "codex"
        );
        assert_eq!(
            session_with_model(None, Some("copilot")).model_display_name(),
            "copilot"
        );
    }

    #[test]
    fn display_name_none_model_none_agent_type() {
        assert_eq!(
            session_with_model(None, None).model_display_name(),
            "unknown"
        );
    }

    #[test]
    fn display_name_empty_string_falls_back() {
        assert_eq!(
            session_with_model(Some(""), Some("claude")).model_display_name(),
            "claude"
        );
        assert_eq!(
            session_with_model(Some("  "), Some("codex")).model_display_name(),
            "codex"
        );
        assert_eq!(
            session_with_model(Some(""), None).model_display_name(),
            "unknown"
        );
    }

    #[test]
    fn display_name_whitespace_trimmed() {
        assert_eq!(
            session_with_model(Some("  claude-opus-4-6  "), None).model_display_name(),
            "opus"
        );
        assert_eq!(
            session_with_model(Some(" gpt-5.4 "), None).model_display_name(),
            "gpt5.4"
        );
    }

    // --- session_is_dead edge cases ---

    #[test]
    fn dead_pid_within_grace_period_is_not_dead() {
        // A session with a dead PID but a recent last_updated timestamp should
        // survive the 120s grace period (hook PIDs are ephemeral).
        use time::OffsetDateTime;
        use time::format_description::well_known::Rfc3339;
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        let state = ClaudeSessionState {
            session_id: "grace-period".into(),
            pid: Some(999_999_999), // dead PID
            last_updated: Some(now),
            ..ClaudeSessionState::default()
        };
        assert!(
            !session_is_dead(&state),
            "session within 120s grace should not be dead even with dead PID"
        );
    }

    #[test]
    fn no_pid_field_does_not_panic() {
        // State file with no `pid` field at all should not panic, just fall through
        // to age-based check.
        let state = ClaudeSessionState {
            session_id: "no-pid".into(),
            pid: None,
            last_updated: None,
            ..ClaudeSessionState::default()
        };
        // No PID + no last_updated → session_is_dead returns false (no evidence of death).
        assert!(!session_is_dead(&state));
    }

    #[test]
    fn pid_one_init_liveness() {
        // PID 1 (init/systemd) — test that session_is_dead handles it without
        // panicking. In containers/sandboxes, kill(1, 0) may fail with EPERM so
        // we just verify no panic occurs (liveness depends on environment).
        let state = ClaudeSessionState {
            session_id: "pid-one".into(),
            pid: Some(1),
            last_updated: Some("2024-01-01T00:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        // Should not panic regardless of whether PID 1 is visible.
        let _ = session_is_dead(&state);
    }

    #[test]
    fn pid_zero_skips_liveness_check() {
        // PID 0 is the idle process — the code checks `pid > 0` before calling
        // pid_is_alive, so PID 0 should fall through to the PID-alive branch
        // (returning false = not dead) without attempting kill(0, 0).
        let state = ClaudeSessionState {
            session_id: "pid-zero".into(),
            pid: Some(0),
            last_updated: Some("2024-01-01T00:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        // pid=0 → pid > 0 is false → skip liveness → PID exists → return false.
        assert!(!session_is_dead(&state));
    }

    #[test]
    fn malformed_json_in_state_dir_skips_cleanly() {
        // Simulate what happens when read_file encounters malformed JSON —
        // it should return None (skip the file) without panicking.
        let dir = tempfile::tempdir().unwrap();
        let bad_file = dir.path().join("bad.json");
        std::fs::write(&bad_file, "this is not json {{{").unwrap();

        let result = ClaudeStatePoller::read_file(&bad_file);
        assert!(
            result.is_none(),
            "malformed JSON should be skipped, not panic"
        );
    }

    #[test]
    fn empty_json_file_skips_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let empty_file = dir.path().join("empty.json");
        std::fs::write(&empty_file, "").unwrap();

        let result = ClaudeStatePoller::read_file(&empty_file);
        assert!(result.is_none(), "empty file should be skipped");
    }

    #[test]
    fn truncated_json_file_skips_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let trunc_file = dir.path().join("truncated.json");
        std::fs::write(&trunc_file, r#"{"session_id": "trunc"#).unwrap();

        let result = ClaudeStatePoller::read_file(&trunc_file);
        assert!(result.is_none(), "truncated JSON should be skipped");
    }

    #[test]
    fn nonexistent_file_skips_cleanly() {
        let path = Path::new("/tmp/thermal-test-nonexistent-file-12345.json");
        let result = ClaudeStatePoller::read_file(path);
        assert!(result.is_none(), "nonexistent file should be skipped");
    }

    #[test]
    fn rapid_file_churn_does_not_thrash() {
        // Simulate rapid write/delete churn — read_all_files should handle
        // files disappearing between readdir and read without panicking.
        let dir = tempfile::tempdir().unwrap();

        // Write 20 files, delete half, read_all_files should succeed.
        for i in 0..20 {
            let path = dir.path().join(format!("session-{i}.json"));
            let json = format!(
                r#"{{"session_id": "churn-{i}", "status": "idle", "pid": {pid}}}"#,
                pid = std::process::id()
            );
            std::fs::write(&path, json).unwrap();
        }
        // Delete every other file to simulate churn.
        for i in (0..20).step_by(2) {
            let path = dir.path().join(format!("session-{i}.json"));
            let _ = std::fs::remove_file(&path);
        }

        let files = ClaudeStatePoller::read_all_files(dir.path());
        // Should have ~10 remaining files (the odd-numbered ones).
        assert_eq!(files.len(), 10, "should read exactly the surviving files");
        for (_, state) in &files {
            assert!(state.session_id.starts_with("churn-"));
        }
    }

    #[test]
    fn read_all_files_ignores_non_json() {
        use time::OffsetDateTime;
        use time::format_description::well_known::Rfc3339;
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Use our own PID so the session is alive and doesn't get filtered.
        let json = format!(
            r#"{{"session_id": "valid", "pid": {}, "last_updated": "{}"}}"#,
            std::process::id(),
            now,
        );
        std::fs::write(dir.path().join("session.json"), json).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a state file").unwrap();
        std::fs::write(dir.path().join("config.toml"), "[section]").unwrap();

        let files = ClaudeStatePoller::read_all_files(dir.path());
        assert_eq!(files.len(), 1);
        assert_eq!(files.values().next().unwrap().session_id, "valid");
    }

    #[test]
    fn session_with_unparseable_timestamp_not_dead() {
        // If last_updated is present but not valid RFC 3339, the grace period
        // parse fails and falls through — should not panic.
        let state = ClaudeSessionState {
            session_id: "bad-ts".into(),
            pid: None,
            last_updated: Some("not-a-timestamp".into()),
            ..ClaudeSessionState::default()
        };
        // Bad timestamp → Rfc3339 parse fails → returns false (not dead).
        assert!(!session_is_dead(&state));
    }

    #[test]
    fn dead_pid_past_grace_period_is_dead() {
        // Dead PID + old timestamp (well past 120s grace) → dead.
        let state = ClaudeSessionState {
            session_id: "dead-past-grace".into(),
            pid: Some(999_999_999),
            last_updated: Some("2024-01-01T00:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        assert!(
            session_is_dead(&state),
            "dead PID past grace period should be dead"
        );
    }

    #[test]
    fn collapse_handles_mixed_dead_and_alive() {
        // Collapse with multiple sessions: some dead PIDs, some alive.
        use time::OffsetDateTime;
        use time::format_description::well_known::Rfc3339;
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();

        let alive = ClaudeSessionState {
            session_id: "alive-session".into(),
            pid: Some(std::process::id() as i64),
            last_updated: Some(now.clone()),
            ..ClaudeSessionState::default()
        };
        let dead = ClaudeSessionState {
            session_id: "dead-session".into(),
            pid: Some(999_999_999),
            last_updated: Some("2024-01-01T00:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        let fresh_no_pid = ClaudeSessionState {
            session_id: "fresh-no-pid".into(),
            pid: None,
            last_updated: Some(now),
            ..ClaudeSessionState::default()
        };

        // collapse_sessions_by_id doesn't filter dead sessions (that's read_file's job),
        // but we verify it handles diverse states without panicking.
        let collapsed = collapse_sessions_by_id(vec![alive, dead, fresh_no_pid]);
        assert_eq!(collapsed.len(), 3);
    }

    #[test]
    fn collapse_sessions_prefers_hook_sourced() {
        // A hook-sourced session should win over a heuristic one with the same ID,
        // even if the heuristic one has a newer timestamp.
        let heuristic = ClaudeSessionState {
            session_id: "dup".into(),
            status: ClaudeStatus::Processing,
            working_dir: Some("/tmp/guessed".into()),
            last_updated: Some("2026-03-31T22:00:01Z".into()),
            ..ClaudeSessionState::default()
        };
        let hook = ClaudeSessionState {
            session_id: "dup".into(),
            status: ClaudeStatus::Idle,
            working_dir: Some("/home/builder/projects/real".into()),
            source: Some("hook".into()),
            last_updated: Some("2026-03-31T22:00:00Z".into()),
            ..ClaudeSessionState::default()
        };

        let collapsed = collapse_sessions_by_id(vec![heuristic, hook]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].source.as_deref(), Some("hook"));
        assert_eq!(
            collapsed[0].working_dir.as_deref(),
            Some("/home/builder/projects/real")
        );
    }

    #[test]
    fn collapse_sessions_hook_vs_hook_uses_timestamp() {
        // When both are hook-sourced, fall back to timestamp comparison.
        let older_hook = ClaudeSessionState {
            session_id: "dup".into(),
            status: ClaudeStatus::Idle,
            source: Some("hook".into()),
            last_updated: Some("2026-03-31T22:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        let newer_hook = ClaudeSessionState {
            session_id: "dup".into(),
            status: ClaudeStatus::ToolUse,
            source: Some("hook".into()),
            last_updated: Some("2026-03-31T22:00:01Z".into()),
            ..ClaudeSessionState::default()
        };

        let collapsed = collapse_sessions_by_id(vec![older_hook, newer_hook]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].status, ClaudeStatus::ToolUse);
    }

    // --- type alias smoke tests ---

    #[test]
    fn type_aliases_compile() {
        // Ensure the generalized aliases are usable.
        let _s: AgentSessionState = ClaudeSessionState::default();
        let _st: AgentStatus = ClaudeStatus::Idle;
    }
}
