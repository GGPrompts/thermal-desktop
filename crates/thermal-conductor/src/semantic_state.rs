//! Daemon-owned canonical session state and semantic event emission.
//!
//! Each daemon-managed PTY session has a [`SemanticSessionState`] that is the
//! single source of truth for agent activity, tool usage, context levels, and
//! session metadata.  Changes to this state are emitted as [`SemanticEvent`]s
//! that subscribers receive via the `SubscribeEvents` protocol request.
//!
//! # Data flow
//!
//! ```text
//! AgentStateInference (thermal-terminal)
//!   → StateChangeNotification (std::sync::mpsc)
//!     → SemanticSessionState (this module, updated in-place)
//!       → SemanticEvent (tokio::sync::broadcast to subscribers)
//!
//! PTY lifecycle (spawn/exit/title/cwd)
//!   → direct calls from daemon.rs
//!     → SemanticSessionState (this module)
//!       → SemanticEvent
//!
//! External file watchers (/tmp/*-state/)
//!   → ExternalStateImported event (non-authoritative)
//! ```

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::time::SystemTime;

use parking_lot::Mutex;
use tokio::sync::broadcast;
use tracing::{debug, trace};

use crate::protocol::{
    AgentActivity, AgentRuntime, ContextState, ContextThreshold, EventScope, SemanticEvent,
    SemanticEventKind, SemanticSessionSnapshot, SnapshotSync,
};
use thermal_terminal::state_inference::{AgentType, InferredStatus};
use thermal_terminal::StateChangeNotification;

// ── SemanticSessionState ───────────────────────────────────────────────────

/// Canonical per-session state owned by the daemon.
///
/// One instance per managed PTY session.  Updated from inference engine
/// notifications, PTY lifecycle events, and terminal metadata changes.
#[derive(Debug)]
pub(crate) struct SemanticSessionState {
    pub session_id: String,
    pub display_name: Option<String>,
    pub runtime: AgentRuntime,
    pub activity: AgentActivity,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub workspace_root: Option<String>,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    pub last_activity_at: Option<String>,
    pub exit_code: Option<i32>,
    pub is_alive: bool,
    pub current_tool: Option<String>,
    pub context_state: ContextState,
    /// Per-session monotonically increasing sequence number.
    seq: u64,
}

impl SemanticSessionState {
    /// Create a new state for a freshly spawned session.
    pub fn new(session_id: String, display_name: Option<String>, cwd: Option<String>, pid: Option<u32>) -> Self {
        Self {
            session_id,
            display_name,
            runtime: AgentRuntime::Unknown,
            activity: AgentActivity::Idle,
            title: None,
            cwd,
            workspace_root: None,
            pid,
            started_at: Some(now_rfc3339()),
            last_activity_at: Some(now_rfc3339()),
            exit_code: None,
            is_alive: true,
            current_tool: None,
            context_state: ContextState::default(),
            seq: 0,
        }
    }

    /// Build a snapshot for subscription delivery.
    pub fn snapshot(&self) -> SemanticSessionSnapshot {
        SemanticSessionSnapshot {
            session_id: self.session_id.clone(),
            backend: "daemon".to_string(),
            runtime: self.runtime.clone(),
            display_name: self.display_name.clone(),
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            workspace_root: self.workspace_root.clone(),
            pid: self.pid,
            started_at: self.started_at.clone(),
            last_activity_at: self.last_activity_at.clone(),
            exit_code: self.exit_code,
            is_alive: self.is_alive,
            agent_activity: self.activity.clone(),
            current_tool: self.current_tool.clone(),
            context_state: self.context_state.clone(),
        }
    }

    /// Current sequence number.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Bump sequence and return the new value.
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Touch the last_activity_at timestamp.
    fn touch(&mut self) {
        self.last_activity_at = Some(now_rfc3339());
    }
}

// ── SemanticEventBus ───────────────────────────────────────────────────────

/// Central event bus for semantic session events.
///
/// Holds per-session state and a broadcast channel for subscribers.
/// Thread-safe: the inner state map is protected by a `parking_lot::Mutex`.
#[allow(dead_code)]
pub(crate) struct SemanticEventBus {
    states: Mutex<HashMap<String, SemanticSessionState>>,
    /// Broadcast channel for all semantic events.
    event_tx: broadcast::Sender<SemanticEvent>,
    /// Global sequence counter for ordering across sessions.
    global_seq: AtomicU64,
}

#[allow(dead_code)]
impl SemanticEventBus {
    /// Create a new event bus with the given broadcast capacity.
    pub fn new(capacity: usize) -> Self {
        let (event_tx, _) = broadcast::channel(capacity);
        Self {
            states: Mutex::new(HashMap::new()),
            event_tx,
            global_seq: AtomicU64::new(0),
        }
    }

    /// Subscribe to the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<SemanticEvent> {
        self.event_tx.subscribe()
    }

    /// Get snapshots and current sequence for all sessions matching a scope.
    pub fn snapshot_syncs(&self, scope: &EventScope) -> Vec<SnapshotSync> {
        let states = self.states.lock();
        states
            .values()
            .filter(|s| match scope {
                EventScope::All => true,
                EventScope::Session(id) => s.session_id == *id,
                EventScope::Categories(_) => true, // categories filter events, not sessions
            })
            .map(|s| SnapshotSync {
                snapshot: s.snapshot(),
                seq: s.seq(),
            })
            .collect()
    }

    /// Get the current snapshot for a single session.
    pub fn snapshot(&self, session_id: &str) -> Option<SemanticSessionSnapshot> {
        let states = self.states.lock();
        states.get(session_id).map(|s| s.snapshot())
    }

    // ── Session lifecycle ────────────────────────────────────────────────

    /// Register a newly spawned session.
    pub fn session_spawned(
        &self,
        session_id: &str,
        display_name: Option<String>,
        cwd: Option<String>,
        pid: Option<u32>,
    ) {
        let mut state = SemanticSessionState::new(
            session_id.to_string(),
            display_name.clone(),
            cwd.clone(),
            pid,
        );
        let seq = state.next_seq();
        self.states.lock().insert(session_id.to_string(), state);
        self.emit(SemanticEvent {
            session_id: session_id.to_string(),
            seq,
            kind: SemanticEventKind::SessionSpawned {
                display_name,
                cwd,
            },
        });
        debug!(session = %session_id, "Semantic: session spawned");
    }

    /// Mark a session as exited.
    pub fn session_exited(&self, session_id: &str, exit_code: Option<i32>, reason: String) {
        let seq = {
            let mut states = self.states.lock();
            if let Some(state) = states.get_mut(session_id) {
                state.is_alive = false;
                state.exit_code = exit_code;
                state.activity = AgentActivity::Exited;
                state.touch();
                state.next_seq()
            } else {
                return;
            }
        };
        self.emit(SemanticEvent {
            session_id: session_id.to_string(),
            seq,
            kind: SemanticEventKind::SessionExited { exit_code, reason },
        });
        debug!(session = %session_id, "Semantic: session exited");
    }

    /// Remove a session from tracking (after kill).
    pub fn session_removed(&self, session_id: &str) {
        self.states.lock().remove(session_id);
        trace!(session = %session_id, "Semantic: session removed from tracking");
    }

    /// Update the terminal title.
    pub fn title_changed(&self, session_id: &str, title: String) {
        let seq = {
            let mut states = self.states.lock();
            if let Some(state) = states.get_mut(session_id) {
                if state.title.as_deref() == Some(&title) {
                    return; // No change
                }
                state.title = Some(title.clone());
                state.touch();
                state.next_seq()
            } else {
                return;
            }
        };
        self.emit(SemanticEvent {
            session_id: session_id.to_string(),
            seq,
            kind: SemanticEventKind::SessionRetitled { title },
        });
    }

    /// Update the working directory.
    pub fn cwd_changed(&self, session_id: &str, cwd: String) {
        let seq = {
            let mut states = self.states.lock();
            if let Some(state) = states.get_mut(session_id) {
                if state.cwd.as_deref() == Some(&cwd) {
                    return;
                }
                state.cwd = Some(cwd.clone());
                state.touch();
                state.next_seq()
            } else {
                return;
            }
        };
        self.emit(SemanticEvent {
            session_id: session_id.to_string(),
            seq,
            kind: SemanticEventKind::SessionCwdChanged { cwd },
        });
    }

    // ── Inference engine bridge ──────────────────────────────────────────

    /// Process a batch of state change notifications from the inference engine.
    ///
    /// Called from the daemon's notification relay task.
    pub fn process_notification(&self, session_id: &str, notif: StateChangeNotification) {
        match notif {
            StateChangeNotification::StatusChanged { old, new } => {
                let (activity, seq) = {
                    let mut states = self.states.lock();
                    if let Some(state) = states.get_mut(session_id) {
                        let activity = inferred_to_activity(&new);
                        let previous_activity = state.activity.clone();
                        if activity == previous_activity {
                            return; // No semantic change
                        }
                        state.activity = activity.clone();
                        // Clear current_tool if no longer in ToolRunning
                        if activity != AgentActivity::ToolRunning {
                            state.current_tool = None;
                        }
                        state.touch();
                        let seq = state.next_seq();
                        (activity, seq)
                    } else {
                        return;
                    }
                };
                let previous = Some(inferred_to_activity(&old));
                self.emit(SemanticEvent {
                    session_id: session_id.to_string(),
                    seq,
                    kind: SemanticEventKind::AgentActivityChanged {
                        activity,
                        previous,
                    },
                });
            }

            StateChangeNotification::ToolStarted { ref tool_name } => {
                let seq = {
                    let mut states = self.states.lock();
                    if let Some(state) = states.get_mut(session_id) {
                        state.current_tool = Some(tool_name.clone());
                        state.activity = AgentActivity::ToolRunning;
                        state.touch();
                        state.next_seq()
                    } else {
                        return;
                    }
                };
                self.emit(SemanticEvent {
                    session_id: session_id.to_string(),
                    seq,
                    kind: SemanticEventKind::ToolStarted {
                        tool_name: tool_name.clone(),
                    },
                });
            }

            StateChangeNotification::ToolCompleted { ref tool_name } => {
                let seq = {
                    let mut states = self.states.lock();
                    if let Some(state) = states.get_mut(session_id) {
                        state.current_tool = None;
                        state.touch();
                        state.next_seq()
                    } else {
                        return;
                    }
                };
                self.emit(SemanticEvent {
                    session_id: session_id.to_string(),
                    seq,
                    kind: SemanticEventKind::ToolCompleted {
                        tool_name: tool_name.clone(),
                        duration_ms: None, // Not tracked at tool level currently
                    },
                });
            }

            StateChangeNotification::AgentDetected { agent_type } => {
                let runtime = agent_type_to_runtime(agent_type);
                let seq = {
                    let mut states = self.states.lock();
                    if let Some(state) = states.get_mut(session_id) {
                        state.runtime = runtime.clone();
                        state.touch();
                        state.next_seq()
                    } else {
                        return;
                    }
                };
                self.emit(SemanticEvent {
                    session_id: session_id.to_string(),
                    seq,
                    kind: SemanticEventKind::RuntimeDetected { runtime },
                });
            }

            StateChangeNotification::ModelDetected { .. } => {
                // Model name is tracked in the inference engine and reflected
                // in state file writes, but not currently surfaced as a
                // separate semantic event. The display_name in the snapshot
                // is derived from the session sidecar, not the model name.
            }

            StateChangeNotification::ContextUpdated { percent } => {
                let saturation = (percent as f64) / 100.0;
                let context_state = ContextState {
                    tokens_used: None,
                    tokens_limit: None,
                    saturation: Some(saturation),
                };
                let (seq, crossed_threshold) = {
                    let mut states = self.states.lock();
                    if let Some(state) = states.get_mut(session_id) {
                        let old_saturation = state.context_state.saturation;
                        state.context_state = context_state.clone();
                        state.touch();
                        let seq = state.next_seq();
                        // Check if we crossed a threshold boundary.
                        let crossed = check_threshold_crossing(old_saturation, Some(saturation));
                        (seq, crossed)
                    } else {
                        return;
                    }
                };
                self.emit(SemanticEvent {
                    session_id: session_id.to_string(),
                    seq,
                    kind: SemanticEventKind::ContextUpdated {
                        state: context_state,
                    },
                });
                if let Some(level) = crossed_threshold {
                    let seq2 = {
                        let mut states = self.states.lock();
                        let Some(seq2) = states.get_mut(session_id).map(|s| s.next_seq()) else {
                            return;
                        };
                        seq2
                    };
                    self.emit(SemanticEvent {
                        session_id: session_id.to_string(),
                        seq: seq2,
                        kind: SemanticEventKind::ContextThresholdCrossed {
                            level,
                            saturation: Some(saturation),
                        },
                    });
                }
            }

            StateChangeNotification::CommandStarted { ref command } => {
                // OSC 633 command start — update state silently.
                // The inference engine emits ToolStarted separately with the
                // actual agent tool name, so we don't emit a duplicate event here.
                if let Some(cmd) = command {
                    let mut states = self.states.lock();
                    if let Some(state) = states.get_mut(session_id) {
                        state.current_tool = Some(cmd.clone());
                        state.activity = AgentActivity::ToolRunning;
                        state.touch();
                    }
                }
            }

            StateChangeNotification::CommandFinished {
                command,
                exit_code,
                duration_ms,
            } => {
                let cmd_name = command.unwrap_or_default();
                if !cmd_name.is_empty() {
                    let failed = matches!(exit_code, Some(ec) if ec != 0);
                    let seq = {
                        let mut states = self.states.lock();
                        if let Some(state) = states.get_mut(session_id) {
                            state.touch();
                            state.next_seq()
                        } else {
                            return;
                        }
                    };
                    if failed {
                        self.emit(SemanticEvent {
                            session_id: session_id.to_string(),
                            seq,
                            kind: SemanticEventKind::ToolFailed {
                                tool_name: cmd_name.clone(),
                                error: Some(format!("exit code {}", exit_code.unwrap())),
                            },
                        });
                    } else {
                        self.emit(SemanticEvent {
                            session_id: session_id.to_string(),
                            seq,
                            kind: SemanticEventKind::ToolCompleted {
                                tool_name: cmd_name.clone(),
                                duration_ms: Some(duration_ms),
                            },
                        });
                    }
                }
            }
        }
    }

    // ── External state import ────────────────────────────────────────────

    /// Record that state was imported from an external file watcher.
    ///
    /// This is non-authoritative — the daemon's own inference is canonical.
    pub fn external_state_imported(&self, session_id: &str, source: String) {
        let seq = {
            let mut states = self.states.lock();
            if let Some(state) = states.get_mut(session_id) {
                state.touch();
                state.next_seq()
            } else {
                // External state for an unknown session — create a placeholder.
                let mut state = SemanticSessionState::new(
                    session_id.to_string(),
                    None,
                    None,
                    None,
                );
                let seq = state.next_seq();
                states.insert(session_id.to_string(), state);
                seq
            }
        };
        self.emit(SemanticEvent {
            session_id: session_id.to_string(),
            seq,
            kind: SemanticEventKind::ExternalStateImported { source },
        });
    }

    // ── Internal ─────────────────────────────────────────────────────────

    fn emit(&self, event: SemanticEvent) {
        trace!(
            session = %event.session_id,
            seq = event.seq,
            kind = ?std::mem::discriminant(&event.kind),
            "Semantic event emitted"
        );
        // Best-effort: if no subscribers, the event is dropped.
        let _ = self.event_tx.send(event);
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn now_rfc3339() -> String {
    use std::time::UNIX_EPOCH;
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs();
    let millis = dur.subsec_nanos() / 1_000_000;

    // Convert Unix timestamp to calendar date/time (UTC).
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    let days = (total_secs / 86400) as i64;
    let time_of_day = (total_secs % 86400) as u32;
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;

    // Civil from days (epoch = 1970-01-01 = day 0).
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
    )
}

/// Map `InferredStatus` to `AgentActivity`.
fn inferred_to_activity(status: &InferredStatus) -> AgentActivity {
    match status {
        InferredStatus::Idle => AgentActivity::Idle,
        InferredStatus::Processing => AgentActivity::Thinking,
        InferredStatus::ToolUse { .. } => AgentActivity::ToolRunning,
        InferredStatus::AwaitingInput => AgentActivity::WaitingInput,
    }
}

/// Map `AgentType` to `AgentRuntime`.
fn agent_type_to_runtime(at: AgentType) -> AgentRuntime {
    match at {
        AgentType::Claude => AgentRuntime::Claude,
        AgentType::Codex => AgentRuntime::Codex,
        AgentType::Copilot => AgentRuntime::Copilot,
    }
}

/// Check if saturation crossed a warning (0.75) or critical (0.90) threshold.
fn check_threshold_crossing(
    old: Option<f64>,
    new: Option<f64>,
) -> Option<ContextThreshold> {
    let new_val = new?;
    let old_val = old.unwrap_or(0.0);
    if old_val < 0.90 && new_val >= 0.90 {
        Some(ContextThreshold::Critical)
    } else if old_val < 0.75 && new_val >= 0.75 {
        Some(ContextThreshold::Warning)
    } else {
        None
    }
}

/// Check if an event matches a set of category filters.
pub(crate) fn event_matches_categories(
    kind: &SemanticEventKind,
    categories: &[crate::protocol::EventCategory],
) -> bool {
    use crate::protocol::EventCategory;
    let event_cat = match kind {
        SemanticEventKind::SessionSpawned { .. }
        | SemanticEventKind::SessionExited { .. }
        | SemanticEventKind::SessionRetitled { .. }
        | SemanticEventKind::SessionCwdChanged { .. } => EventCategory::SessionLifecycle,

        SemanticEventKind::RuntimeDetected { .. }
        | SemanticEventKind::AgentActivityChanged { .. }
        | SemanticEventKind::PromptStarted
        | SemanticEventKind::ResponseCompleted => EventCategory::AgentRuntime,

        SemanticEventKind::ToolStarted { .. }
        | SemanticEventKind::ToolCompleted { .. }
        | SemanticEventKind::ToolFailed { .. } => EventCategory::Tool,

        SemanticEventKind::ContextUpdated { .. }
        | SemanticEventKind::ContextThresholdCrossed { .. } => EventCategory::Context,

        SemanticEventKind::ExternalStateImported { .. } => EventCategory::Compatibility,
    };
    categories.contains(&event_cat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_lifecycle_events() {
        let bus = SemanticEventBus::new(64);
        let mut rx = bus.subscribe();

        // Spawn
        bus.session_spawned("s1", Some("opus".into()), Some("/tmp".into()), Some(1234));

        let event = rx.try_recv().unwrap();
        assert_eq!(event.session_id, "s1");
        assert!(matches!(event.kind, SemanticEventKind::SessionSpawned { .. }));

        // Snapshot
        let syncs = bus.snapshot_syncs(&EventScope::All);
        assert_eq!(syncs.len(), 1);
        assert_eq!(syncs[0].snapshot.session_id, "s1");
        assert!(syncs[0].snapshot.is_alive);

        // Title change
        bus.title_changed("s1", "new title".into());
        let event = rx.try_recv().unwrap();
        assert!(matches!(event.kind, SemanticEventKind::SessionRetitled { ref title } if title == "new title"));

        // Duplicate title — no event
        bus.title_changed("s1", "new title".into());
        assert!(rx.try_recv().is_err());

        // Exit
        bus.session_exited("s1", Some(0), "PtyEof".into());
        let event = rx.try_recv().unwrap();
        assert!(matches!(event.kind, SemanticEventKind::SessionExited { exit_code: Some(0), .. }));

        let syncs = bus.snapshot_syncs(&EventScope::All);
        assert!(!syncs[0].snapshot.is_alive);
    }

    #[test]
    fn inference_bridge() {
        let bus = SemanticEventBus::new(64);
        let mut rx = bus.subscribe();

        bus.session_spawned("s1", None, None, None);
        let _ = rx.try_recv(); // consume spawned event

        // Status change
        bus.process_notification(
            "s1",
            StateChangeNotification::StatusChanged {
                old: InferredStatus::Idle,
                new: InferredStatus::Processing,
            },
        );
        let event = rx.try_recv().unwrap();
        assert!(matches!(
            event.kind,
            SemanticEventKind::AgentActivityChanged {
                activity: AgentActivity::Thinking,
                ..
            }
        ));

        // Tool started
        bus.process_notification(
            "s1",
            StateChangeNotification::ToolStarted {
                tool_name: "Edit".into(),
            },
        );
        let event = rx.try_recv().unwrap();
        assert!(matches!(event.kind, SemanticEventKind::ToolStarted { ref tool_name } if tool_name == "Edit"));

        // Verify snapshot reflects tool
        let snap = bus.snapshot("s1").unwrap();
        assert_eq!(snap.current_tool, Some("Edit".into()));
        assert_eq!(snap.agent_activity, AgentActivity::ToolRunning);

        // Tool completed
        bus.process_notification(
            "s1",
            StateChangeNotification::ToolCompleted {
                tool_name: "Edit".into(),
            },
        );
        let event = rx.try_recv().unwrap();
        assert!(matches!(event.kind, SemanticEventKind::ToolCompleted { .. }));

        let snap = bus.snapshot("s1").unwrap();
        assert_eq!(snap.current_tool, None);
    }

    #[test]
    fn context_threshold_crossing() {
        assert_eq!(
            check_threshold_crossing(Some(0.5), Some(0.8)),
            Some(ContextThreshold::Warning)
        );
        assert_eq!(
            check_threshold_crossing(Some(0.5), Some(0.95)),
            Some(ContextThreshold::Critical)
        );
        assert_eq!(
            check_threshold_crossing(Some(0.8), Some(0.85)),
            None
        );
        assert_eq!(
            check_threshold_crossing(Some(0.85), Some(0.95)),
            Some(ContextThreshold::Critical)
        );
    }

    #[test]
    fn scope_filtering() {
        let bus = SemanticEventBus::new(64);
        bus.session_spawned("s1", None, None, None);
        bus.session_spawned("s2", None, None, None);

        let all = bus.snapshot_syncs(&EventScope::All);
        assert_eq!(all.len(), 2);

        let one = bus.snapshot_syncs(&EventScope::Session("s1".into()));
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].snapshot.session_id, "s1");
    }

    #[test]
    fn event_category_matching() {
        use crate::protocol::EventCategory;
        assert!(event_matches_categories(
            &SemanticEventKind::SessionSpawned {
                display_name: None,
                cwd: None,
            },
            &[EventCategory::SessionLifecycle],
        ));
        assert!(!event_matches_categories(
            &SemanticEventKind::SessionSpawned {
                display_name: None,
                cwd: None,
            },
            &[EventCategory::Tool],
        ));
        assert!(event_matches_categories(
            &SemanticEventKind::ToolStarted {
                tool_name: "Read".into(),
            },
            &[EventCategory::Tool],
        ));
    }
}
