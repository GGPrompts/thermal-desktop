//! Swarm watcher — auto-spawns terminal windows for Claude subagent JSONL streams.
//!
//! Monitors the daemon's state file watcher for sessions with a non-null
//! `parent_session_id` (subagent detection). When a new subagent appears:
//!
//! 1. Resolves the JSONL path from `~/.claude/projects/{hash}/{parent}/{subagents}/agent-{id}.jsonl`
//! 2. Spawns a kitty terminal window tailing the JSONL with thermal styling
//! 3. Positions the window relative to the orchestrator via Hyprland IPC
//! 4. Tracks the window and cleans up when the subagent completes
//!
//! # Integration
//!
//! Spawned as a background tokio task from `daemon::run_daemon()`. Reads
//! subagent state from the same `ClaudeStatePoller` data that the state file
//! watcher already processes — no additional inotify watchers needed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tracing::{debug, error, info, warn};

use crate::semantic_state::SemanticEventBus;

// ── Types ────────────────────────────────────────────────────────────────────

/// A spawned swarm window tracking entry.
#[derive(Debug)]
#[allow(dead_code)]
struct SwarmWindow {
    /// The subagent's session ID (e.g. "parent.agent.abc123").
    session_id: String,
    /// The parent orchestrator's session ID.
    parent_session_id: String,
    /// Short agent ID (e.g. "a3c91a24a4ba1e505").
    agent_id: String,
    /// Path to the JSONL file being tailed.
    jsonl_path: Option<PathBuf>,
    /// Hyprland window address (from `hyprctl clients -j`), if known.
    window_address: Option<String>,
    /// When the window was spawned.
    spawned_at: Instant,
    /// When the subagent was last seen active (state file updated).
    last_active: Instant,
    /// Whether we've initiated the close sequence.
    closing: bool,
    /// When the close sequence started (for the delay).
    close_started_at: Option<Instant>,
}

/// Active swarm window count, exposed to the event bus.
#[allow(dead_code)]
pub(crate) struct SwarmState {
    pub active_count: usize,
}

// ── Constants ────────────────────────────────────────────────────────────────

/// How long to wait after a subagent completes before closing its window.
const CLOSE_DELAY: Duration = Duration::from_secs(3);

/// Poll interval for checking subagent state changes.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// If a subagent hasn't been seen in state files for this long, consider it done.
const INACTIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Base directory for Claude projects.
fn claude_projects_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/builder".into());
    PathBuf::from(home).join(".claude").join("projects")
}

// ── JSONL path resolution ────────────────────────────────────────────────────

/// Resolve the JSONL path for a subagent.
///
/// Claude Code stores subagent JSONLs at:
/// `~/.claude/projects/{project-hash}/{parent_session_id}/subagents/agent-{agent_id}.jsonl`
///
/// We search all project directories since the state file doesn't tell us which
/// project hash to use.
fn resolve_jsonl_path(parent_session_id: &str, agent_id: &str) -> Option<PathBuf> {
    let projects_dir = claude_projects_dir();
    let filename = format!("agent-{agent_id}.jsonl");

    // Try each project directory.
    let entries = match std::fs::read_dir(&projects_dir) {
        Ok(entries) => entries,
        Err(e) => {
            warn!("Cannot read Claude projects dir {:?}: {e}", projects_dir);
            return None;
        }
    };

    for entry in entries.flatten() {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }

        // Check {project}/{parent_session_id}/subagents/agent-{agent_id}.jsonl
        let jsonl_path = project_dir
            .join(parent_session_id)
            .join("subagents")
            .join(&filename);

        if jsonl_path.exists() {
            return Some(jsonl_path);
        }
    }

    debug!(
        parent = %parent_session_id,
        agent = %agent_id,
        "JSONL not found in any project directory"
    );
    None
}

// ── Hyprland IPC helpers ─────────────────────────────────────────────────────

/// Spawn a kitty terminal window tailing a JSONL file.
///
/// Returns `Ok(())` if the spawn command was issued (actual window creation is
/// async from Hyprland's perspective).
async fn spawn_swarm_window(
    agent_id: &str,
    jsonl_path: &PathBuf,
    parent_session_id: &str,
) -> anyhow::Result<()> {
    let window_class = format!("thermal-swarm-{}", &agent_id[..agent_id.len().min(12)]);
    let title = format!("swarm:{}", &agent_id[..agent_id.len().min(8)]);

    // Use hyprctl dispatch exec to spawn a kitty window tailing the JSONL.
    // The --class flag sets the window class for Hyprland matching.
    let spawn_cmd = format!(
        "kitty --class {window_class} --title {title} \
         -o background=#1a1a2e -o foreground=#c0c0d0 \
         -o font_size=9 \
         tail -f {}",
        jsonl_path.display()
    );

    let output = Command::new("hyprctl")
        .args(["dispatch", "exec", &spawn_cmd])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("hyprctl dispatch exec failed: {stderr}");
    }

    info!(
        agent = %agent_id,
        class = %window_class,
        jsonl = %jsonl_path.display(),
        "Spawned swarm window"
    );

    // After a short delay, try to position the window relative to the orchestrator.
    let class_clone = window_class.clone();
    let parent_clone = parent_session_id.to_string();
    tokio::spawn(async move {
        // Give Hyprland time to create the window.
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Err(e) = position_swarm_window(&class_clone, &parent_clone).await {
            debug!("Failed to position swarm window: {e}");
        }
    });

    Ok(())
}

/// Position a swarm window relative to the orchestrator's window using Hyprland IPC.
async fn position_swarm_window(
    window_class: &str,
    _parent_session_id: &str,
) -> anyhow::Result<()> {
    // Query all clients to find the orchestrator and our swarm window.
    let output = Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!("hyprctl clients failed");
    }

    let clients: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let clients_arr = clients.as_array().ok_or_else(|| {
        anyhow::anyhow!("hyprctl clients did not return array")
    })?;

    // Find our swarm window by class.
    let swarm_window = clients_arr.iter().find(|c| {
        c["class"].as_str() == Some(window_class)
    });

    if swarm_window.is_none() {
        debug!(class = %window_class, "Swarm window not yet visible in hyprctl clients");
        return Ok(());
    }

    // Hyprland's auto-tiling should handle basic positioning. For more
    // sophisticated layout (e.g. always tile right of orchestrator), we could
    // use `hyprctl dispatch movewindow` — but the default tiling is reasonable
    // for now.

    // Resize swarm windows to be smaller (they're just JSONL viewers).
    let resize_cmd = format!("class:{window_class}");
    let _ = Command::new("hyprctl")
        .args(["dispatch", "resizewindowpixel", "exact 600 400", &resize_cmd])
        .output()
        .await;

    Ok(())
}

/// Close a swarm window by its Hyprland window class.
async fn close_swarm_window(agent_id: &str) -> anyhow::Result<()> {
    let window_class = format!("thermal-swarm-{}", &agent_id[..agent_id.len().min(12)]);

    let output = Command::new("hyprctl")
        .args([
            "dispatch",
            "closewindow",
            &format!("class:{window_class}"),
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!("closewindow for {window_class} returned: {stderr}");
    } else {
        info!(agent = %agent_id, "Closed swarm window");
    }

    Ok(())
}

// ── Main watcher loop ────────────────────────────────────────────────────────

/// Extract agent_id from a subagent session ID.
///
/// State file session IDs look like: `{parent_session_id}.agent.{agent_id}`
/// Returns the `agent_id` portion.
fn extract_agent_id(session_id: &str) -> Option<&str> {
    // Format: "parent-uuid.agent.hexstring"
    let idx = session_id.find(".agent.")?;
    Some(&session_id[idx + 7..])
}

/// Spawn the swarm watcher as a background tokio task.
///
/// Piggybacks on the existing state file watcher by polling `ClaudeStatePoller`
/// for sessions with `parent_session_id` set.
pub(crate) fn spawn_swarm_watcher(_event_bus: Arc<SemanticEventBus>) {
    use thermal_core::ClaudeStatePoller;

    tokio::spawn(async move {
        let mut poller = match ClaudeStatePoller::new() {
            Ok(p) => p,
            Err(e) => {
                warn!("Swarm watcher failed to start: {e}");
                return;
            }
        };

        info!("Swarm watcher started — monitoring for subagent spawns");

        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Track spawned swarm windows: session_id -> SwarmWindow
        let mut windows: HashMap<String, SwarmWindow> = HashMap::new();

        loop {
            interval.tick().await;

            let sessions = poller.poll();

            // Identify subagents (sessions with parent_session_id set).
            let subagents: Vec<_> = sessions
                .iter()
                .filter(|s| s.parent_session_id.is_some())
                .collect();

            // Spawn windows for new subagents.
            for session in &subagents {
                let sid = &session.session_id;

                if windows.contains_key(sid) {
                    // Already tracked — update last_active.
                    if let Some(w) = windows.get_mut(sid) {
                        w.last_active = Instant::now();
                    }
                    continue;
                }

                let parent_id = match &session.parent_session_id {
                    Some(p) => p.clone(),
                    None => continue,
                };

                let agent_id = match extract_agent_id(sid) {
                    Some(id) => id.to_string(),
                    None => {
                        // Try agent_id field from the state file.
                        match &session.agent_id {
                            Some(id) => id.clone(),
                            None => {
                                debug!(session = %sid, "Cannot extract agent_id from subagent");
                                continue;
                            }
                        }
                    }
                };

                // Resolve JSONL path.
                let jsonl_path = resolve_jsonl_path(&parent_id, &agent_id);

                let now = Instant::now();
                let mut window = SwarmWindow {
                    session_id: sid.clone(),
                    parent_session_id: parent_id.clone(),
                    agent_id: agent_id.clone(),
                    jsonl_path: jsonl_path.clone(),
                    window_address: None,
                    spawned_at: now,
                    last_active: now,
                    closing: false,
                    close_started_at: None,
                };

                // Spawn the terminal window if we found a JSONL path.
                if let Some(ref path) = jsonl_path {
                    match spawn_swarm_window(&agent_id, path, &parent_id).await {
                        Ok(()) => {
                            info!(
                                session = %sid,
                                parent = %parent_id,
                                agent = %agent_id,
                                "Swarm window spawned for subagent"
                            );
                        }
                        Err(e) => {
                            error!(
                                session = %sid,
                                error = %e,
                                "Failed to spawn swarm window"
                            );
                        }
                    }
                } else {
                    warn!(
                        session = %sid,
                        parent = %parent_id,
                        agent = %agent_id,
                        "No JSONL found for subagent — window will not show content"
                    );
                    // Still track it so we don't retry every poll cycle.
                    window.jsonl_path = None;
                }

                windows.insert(sid.clone(), window);
            }

            // Check for completed subagents and initiate close sequence.
            let current_subagent_ids: std::collections::HashSet<String> =
                subagents.iter().map(|s| s.session_id.clone()).collect();

            let now = Instant::now();
            let mut to_remove = Vec::new();

            for (sid, window) in windows.iter_mut() {
                if window.closing {
                    // Already in close sequence — check if delay has elapsed.
                    if let Some(close_start) = window.close_started_at {
                        if now.duration_since(close_start) >= CLOSE_DELAY {
                            // Time to close the window.
                            if let Err(e) = close_swarm_window(&window.agent_id).await {
                                debug!(session = %sid, error = %e, "Error closing swarm window");
                            }
                            to_remove.push(sid.clone());
                        }
                    }
                    continue;
                }

                // Subagent no longer in state files or inactive for too long.
                let gone = !current_subagent_ids.contains(sid);
                let timed_out = now.duration_since(window.last_active) > INACTIVE_TIMEOUT;

                if gone || timed_out {
                    info!(
                        session = %sid,
                        reason = if gone { "state file removed" } else { "inactive timeout" },
                        "Subagent completed — closing window in {}s",
                        CLOSE_DELAY.as_secs()
                    );
                    window.closing = true;
                    window.close_started_at = Some(now);
                }
            }

            for sid in to_remove {
                windows.remove(&sid);
            }

            // Emit swarm count to the event bus (via a lightweight mechanism).
            // The active count is the number of non-closing windows.
            let active_count = windows.values().filter(|w| !w.closing).count();
            let _ = active_count; // Available for future event bus integration.

            // Log swarm state changes at debug level.
            if !windows.is_empty() {
                let closing_count = windows.values().filter(|w| w.closing).count();
                debug!(
                    total = windows.len(),
                    active = windows.len() - closing_count,
                    closing = closing_count,
                    "Swarm watcher tick"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_agent_id() {
        assert_eq!(
            extract_agent_id("4948583c-cadc-40e8-9a24-011c09cfa008.agent.a3c91a24a4ba1e505"),
            Some("a3c91a24a4ba1e505")
        );
        assert_eq!(extract_agent_id("plain-session-id"), None);
        assert_eq!(extract_agent_id(""), None);
    }

    #[test]
    fn test_resolve_jsonl_path_missing() {
        // With a fake parent/agent, should return None.
        assert!(resolve_jsonl_path("nonexistent-parent", "nonexistent-agent").is_none());
    }
}
