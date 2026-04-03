//! Adaptive trust tier learning from user confirmation behavior.
//!
//! Tracks per-tool approval/denial history and suggests tier promotions
//! (CONFIRM → AUTO) after consecutive approvals without denials.
//!
//! Persistence: `~/.config/thermal/confirmation_history.toml`

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Number of consecutive approvals required before suggesting promotion.
const PROMOTION_THRESHOLD: u32 = 3;

/// Tools that must never be auto-promoted, regardless of approval history.
/// These are destructive operations where human review is always required.
const NEVER_PROMOTE: &[&str] = &[
    "kill",
    "kill_claude",
    "rm",
    "reset",
    "delete",
    "destroy",
    "force_push",
    "git_reset",
    "reboot",
    "shutdown",
    "format",
];

/// Per-tool confirmation history entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolHistory {
    /// Number of consecutive approvals without any denial.
    pub consecutive_approvals: u32,
    /// Total number of times the user denied this tool.
    pub denial_count: u32,
    /// Last action taken: "approved" or "denied".
    pub last_action: String,
    /// Unix timestamp (seconds) of the last action.
    pub last_action_at: u64,
}

impl Default for ToolHistory {
    fn default() -> Self {
        Self {
            consecutive_approvals: 0,
            denial_count: 0,
            last_action: String::new(),
            last_action_at: 0,
        }
    }
}

/// Root structure for the confirmation history TOML file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfirmationHistory {
    /// Per-tool tracking data. Keys are tool names (e.g. "click", "open_app").
    #[serde(default)]
    pub tools: HashMap<String, ToolHistory>,
}

impl ConfirmationHistory {
    /// Load history from disk, or return an empty history if the file doesn't exist.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            debug!(path = %path.display(), "no confirmation history file, starting fresh");
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let history: Self = toml::from_str(&content)
            .with_context(|| format!("parsing {}", path.display()))?;

        debug!(
            path = %path.display(),
            tool_count = history.tools.len(),
            "loaded confirmation history"
        );
        Ok(history)
    }

    /// Persist history to disk (atomic write via temp file + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let content = toml::to_string_pretty(self)
            .context("serializing confirmation history")?;

        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, &content)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;

        debug!(path = %path.display(), "saved confirmation history");
        Ok(())
    }

    /// Record a user approval for a tool.
    pub fn record_approval(&mut self, tool_name: &str) {
        let now = now_unix_secs();
        let entry = self.tools.entry(tool_name.to_string()).or_default();
        entry.consecutive_approvals += 1;
        entry.last_action = "approved".to_string();
        entry.last_action_at = now;

        info!(
            tool = %tool_name,
            consecutive_approvals = entry.consecutive_approvals,
            "recorded approval"
        );

        // Check if we should suggest promotion
        if entry.consecutive_approvals >= PROMOTION_THRESHOLD && !is_never_promote(tool_name) {
            info!(
                tool = %tool_name,
                approvals = entry.consecutive_approvals,
                "promotion suggested: tool has {} consecutive approvals — consider promoting CONFIRM -> AUTO",
                entry.consecutive_approvals,
            );
        }
    }

    /// Record a user denial for a tool.
    pub fn record_denial(&mut self, tool_name: &str) {
        let now = now_unix_secs();
        let entry = self.tools.entry(tool_name.to_string()).or_default();
        entry.consecutive_approvals = 0; // Reset streak
        entry.denial_count += 1;
        entry.last_action = "denied".to_string();
        entry.last_action_at = now;

        info!(
            tool = %tool_name,
            denial_count = entry.denial_count,
            "recorded denial — consecutive approvals reset"
        );
    }

    /// Return tools that have reached the promotion threshold and are eligible.
    pub fn pending_promotions(&self) -> Vec<PendingPromotion> {
        let never_promote: HashSet<&str> = NEVER_PROMOTE.iter().copied().collect();

        self.tools
            .iter()
            .filter(|(name, history)| {
                history.consecutive_approvals >= PROMOTION_THRESHOLD
                    && !never_promote.contains(name.as_str())
            })
            .map(|(name, history)| PendingPromotion {
                tool_name: name.clone(),
                consecutive_approvals: history.consecutive_approvals,
                denial_count: history.denial_count,
                last_approved_at: history.last_action_at,
            })
            .collect()
    }
}

/// A tool that has met the promotion threshold.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields read via Display impl and tests
pub struct PendingPromotion {
    pub tool_name: String,
    pub consecutive_approvals: u32,
    pub denial_count: u32,
    pub last_approved_at: u64,
}

impl std::fmt::Display for PendingPromotion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} consecutive approvals ({} total denials) — suggest CONFIRM -> AUTO",
            self.tool_name, self.consecutive_approvals, self.denial_count,
        )
    }
}

/// Check whether a tool name is on the never-promote deny list.
pub fn is_never_promote(tool_name: &str) -> bool {
    NEVER_PROMOTE.iter().any(|&blocked| tool_name == blocked)
}

/// Default path for the confirmation history file.
pub fn default_history_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".config")
        });
    config_dir.join("thermal/confirmation_history.toml")
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_history_path(dir: &TempDir) -> PathBuf {
        dir.path().join("thermal/confirmation_history.toml")
    }

    // -----------------------------------------------------------------------
    // Load / save round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn load_nonexistent_returns_empty() {
        let path = Path::new("/tmp/nonexistent_thermal_test_history.toml");
        let history = ConfirmationHistory::load(path).unwrap();
        assert!(history.tools.is_empty());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = tmp_history_path(&dir);

        let mut history = ConfirmationHistory::default();
        history.record_approval("click");
        history.record_approval("click");
        history.record_denial("open_app");
        history.save(&path).unwrap();

        let loaded = ConfirmationHistory::load(&path).unwrap();
        assert_eq!(loaded.tools.len(), 2);
        assert_eq!(loaded.tools["click"].consecutive_approvals, 2);
        assert_eq!(loaded.tools["open_app"].denial_count, 1);
    }

    // -----------------------------------------------------------------------
    // Approval tracking
    // -----------------------------------------------------------------------

    #[test]
    fn record_approval_increments_streak() {
        let mut history = ConfirmationHistory::default();
        history.record_approval("click");
        assert_eq!(history.tools["click"].consecutive_approvals, 1);
        history.record_approval("click");
        assert_eq!(history.tools["click"].consecutive_approvals, 2);
        history.record_approval("click");
        assert_eq!(history.tools["click"].consecutive_approvals, 3);
    }

    #[test]
    fn record_approval_sets_last_action() {
        let mut history = ConfirmationHistory::default();
        history.record_approval("click");
        assert_eq!(history.tools["click"].last_action, "approved");
        assert!(history.tools["click"].last_action_at > 0);
    }

    // -----------------------------------------------------------------------
    // Denial tracking
    // -----------------------------------------------------------------------

    #[test]
    fn record_denial_resets_streak() {
        let mut history = ConfirmationHistory::default();
        history.record_approval("click");
        history.record_approval("click");
        assert_eq!(history.tools["click"].consecutive_approvals, 2);

        history.record_denial("click");
        assert_eq!(history.tools["click"].consecutive_approvals, 0);
        assert_eq!(history.tools["click"].denial_count, 1);
    }

    #[test]
    fn record_denial_increments_denial_count() {
        let mut history = ConfirmationHistory::default();
        history.record_denial("open_app");
        history.record_denial("open_app");
        assert_eq!(history.tools["open_app"].denial_count, 2);
    }

    #[test]
    fn record_denial_sets_last_action() {
        let mut history = ConfirmationHistory::default();
        history.record_denial("click");
        assert_eq!(history.tools["click"].last_action, "denied");
        assert!(history.tools["click"].last_action_at > 0);
    }

    // -----------------------------------------------------------------------
    // Promotion logic
    // -----------------------------------------------------------------------

    #[test]
    fn pending_promotions_empty_for_new_history() {
        let history = ConfirmationHistory::default();
        assert!(history.pending_promotions().is_empty());
    }

    #[test]
    fn pending_promotions_after_threshold() {
        let mut history = ConfirmationHistory::default();
        for _ in 0..PROMOTION_THRESHOLD {
            history.record_approval("click");
        }
        let promotions = history.pending_promotions();
        assert_eq!(promotions.len(), 1);
        assert_eq!(promotions[0].tool_name, "click");
        assert_eq!(promotions[0].consecutive_approvals, PROMOTION_THRESHOLD);
    }

    #[test]
    fn pending_promotions_reset_after_denial() {
        let mut history = ConfirmationHistory::default();
        for _ in 0..PROMOTION_THRESHOLD {
            history.record_approval("click");
        }
        assert_eq!(history.pending_promotions().len(), 1);

        history.record_denial("click");
        assert!(history.pending_promotions().is_empty());
    }

    #[test]
    fn pending_promotions_below_threshold_not_included() {
        let mut history = ConfirmationHistory::default();
        for _ in 0..(PROMOTION_THRESHOLD - 1) {
            history.record_approval("click");
        }
        assert!(history.pending_promotions().is_empty());
    }

    // -----------------------------------------------------------------------
    // Never-promote deny list
    // -----------------------------------------------------------------------

    #[test]
    fn never_promote_tools_excluded_from_promotions() {
        let mut history = ConfirmationHistory::default();
        for _ in 0..10 {
            history.record_approval("kill_claude");
        }
        assert!(history.pending_promotions().is_empty());
    }

    #[test]
    fn is_never_promote_matches_deny_list() {
        assert!(is_never_promote("kill"));
        assert!(is_never_promote("kill_claude"));
        assert!(is_never_promote("rm"));
        assert!(is_never_promote("reset"));
        assert!(is_never_promote("delete"));
        assert!(is_never_promote("destroy"));
    }

    #[test]
    fn is_never_promote_allows_safe_tools() {
        assert!(!is_never_promote("click"));
        assert!(!is_never_promote("open_app"));
        assert!(!is_never_promote("type_text"));
        assert!(!is_never_promote("focus_window"));
    }

    // -----------------------------------------------------------------------
    // Multiple tools tracked independently
    // -----------------------------------------------------------------------

    #[test]
    fn independent_tool_tracking() {
        let mut history = ConfirmationHistory::default();
        history.record_approval("click");
        history.record_approval("click");
        history.record_approval("click");
        history.record_approval("open_app");

        assert_eq!(history.tools["click"].consecutive_approvals, 3);
        assert_eq!(history.tools["open_app"].consecutive_approvals, 1);

        let promotions = history.pending_promotions();
        assert_eq!(promotions.len(), 1);
        assert_eq!(promotions[0].tool_name, "click");
    }

    // -----------------------------------------------------------------------
    // PendingPromotion display
    // -----------------------------------------------------------------------

    #[test]
    fn pending_promotion_display() {
        let p = PendingPromotion {
            tool_name: "click".to_string(),
            consecutive_approvals: 5,
            denial_count: 1,
            last_approved_at: 1000,
        };
        let s = format!("{p}");
        assert!(s.contains("click"));
        assert!(s.contains("5 consecutive approvals"));
        assert!(s.contains("1 total denials"));
        assert!(s.contains("CONFIRM -> AUTO"));
    }

    // -----------------------------------------------------------------------
    // Default history path
    // -----------------------------------------------------------------------

    #[test]
    fn default_history_path_contains_thermal() {
        let path = default_history_path();
        let s = path.to_str().unwrap();
        assert!(s.contains("thermal"));
        assert!(s.ends_with("confirmation_history.toml"));
    }

    // -----------------------------------------------------------------------
    // Persistence: creates parent directories
    // -----------------------------------------------------------------------

    #[test]
    fn save_creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("deep/nested/dir/history.toml");

        let history = ConfirmationHistory::default();
        history.save(&path).unwrap();
        assert!(path.exists());
    }

    // -----------------------------------------------------------------------
    // TOML format verification
    // -----------------------------------------------------------------------

    #[test]
    fn saved_toml_is_human_readable() {
        let dir = TempDir::new().unwrap();
        let path = tmp_history_path(&dir);

        let mut history = ConfirmationHistory::default();
        history.record_approval("click");
        history.record_approval("click");
        history.record_approval("click");
        history.save(&path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("[tools.click]"));
        assert!(content.contains("consecutive_approvals = 3"));
        assert!(content.contains("last_action = \"approved\""));
    }

    // -----------------------------------------------------------------------
    // Edge case: approval after denial restarts streak
    // -----------------------------------------------------------------------

    #[test]
    fn approval_after_denial_restarts_streak() {
        let mut history = ConfirmationHistory::default();
        history.record_approval("click");
        history.record_approval("click");
        history.record_denial("click");
        assert_eq!(history.tools["click"].consecutive_approvals, 0);

        history.record_approval("click");
        assert_eq!(history.tools["click"].consecutive_approvals, 1);
        assert_eq!(history.tools["click"].denial_count, 1);
    }
}
