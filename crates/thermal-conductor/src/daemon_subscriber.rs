//! Daemon semantic event subscriber for TUI and window components.
//!
//! Connects to the thermal-conductor daemon via Unix socket, sends
//! `SubscribeEvents`, and converts incoming snapshots/events into
//! `ClaudeSessionState` values consumable by the existing UI code.
//!
//! Used by:
//! - TUI sessions tab (when backend = Daemon)
//! - Window agent overlay (when in client mode)

use std::collections::HashMap;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use thermal_core::{ClaudeSessionState, ClaudeStatus};

use crate::protocol::{
    self, AgentActivity, AgentRuntime, EventScope, Request, Response, SemanticEvent,
    SemanticEventKind, SemanticSessionSnapshot,
};

// ── Conversion helpers ──────────────────────────────────────────────────────

fn activity_to_status(activity: &AgentActivity) -> ClaudeStatus {
    match activity {
        AgentActivity::Idle | AgentActivity::Exited => ClaudeStatus::Idle,
        AgentActivity::Prompting | AgentActivity::WaitingInput => ClaudeStatus::AwaitingInput,
        AgentActivity::Thinking | AgentActivity::StreamingOutput => ClaudeStatus::Processing,
        AgentActivity::ToolRunning => ClaudeStatus::ToolUse,
    }
}

fn runtime_to_agent_type(runtime: &AgentRuntime) -> Option<String> {
    match runtime {
        AgentRuntime::Claude => Some("claude".into()),
        AgentRuntime::Codex => Some("codex".into()),
        AgentRuntime::Copilot => Some("copilot".into()),
        AgentRuntime::Unknown => None,
    }
}

fn snapshot_to_session_state(snap: &SemanticSessionSnapshot) -> ClaudeSessionState {
    // Tag the source so consumers can distinguish daemon-owned sessions from
    // external (file-derived) sessions imported by the daemon's state watcher.
    let source = if snap.backend == "external" {
        Some("daemon:external".into())
    } else {
        Some("daemon".into())
    };

    ClaudeSessionState {
        session_id: snap.session_id.clone(),
        status: activity_to_status(&snap.agent_activity),
        current_tool: snap.current_tool.clone(),
        working_dir: snap.cwd.clone(),
        context_percent: snap.context_state.saturation.map(|s| s * 100.0),
        agent_type: runtime_to_agent_type(&snap.runtime),
        last_updated: snap.last_activity_at.clone(),
        pid: snap.pid.map(|p| p as i64),
        source,
        ..ClaudeSessionState::default()
    }
}

// ── State aggregator ────────────────────────────────────────────────────────

struct SessionAggregator {
    sessions: HashMap<String, SemanticSessionSnapshot>,
}

impl SessionAggregator {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    fn apply_snapshot(&mut self, snap: SemanticSessionSnapshot) {
        self.sessions.insert(snap.session_id.clone(), snap);
    }

    fn apply_event(&mut self, event: SemanticEvent) {
        let id = &event.session_id;
        match event.kind {
            SemanticEventKind::SessionSpawned { display_name, cwd } => {
                self.sessions.insert(
                    id.clone(),
                    SemanticSessionSnapshot {
                        session_id: id.clone(),
                        backend: "daemon".into(),
                        runtime: AgentRuntime::Unknown,
                        display_name,
                        title: None,
                        cwd,
                        workspace_root: None,
                        pid: None,
                        started_at: None,
                        last_activity_at: None,
                        exit_code: None,
                        is_alive: true,
                        agent_activity: AgentActivity::default(),
                        current_tool: None,
                        context_state: Default::default(),
                    },
                );
            }
            SemanticEventKind::SessionExited { exit_code, .. } => {
                if let Some(s) = self.sessions.get_mut(id) {
                    s.is_alive = false;
                    s.exit_code = exit_code;
                    s.agent_activity = AgentActivity::Exited;
                }
            }
            SemanticEventKind::AgentActivityChanged { activity, .. } => {
                if let Some(s) = self.sessions.get_mut(id) {
                    s.agent_activity = activity;
                    if s.agent_activity != AgentActivity::ToolRunning {
                        s.current_tool = None;
                    }
                }
            }
            SemanticEventKind::ToolStarted { tool_name } => {
                if let Some(s) = self.sessions.get_mut(id) {
                    s.current_tool = Some(tool_name);
                    s.agent_activity = AgentActivity::ToolRunning;
                }
            }
            SemanticEventKind::ToolCompleted { .. } | SemanticEventKind::ToolFailed { .. } => {
                if let Some(s) = self.sessions.get_mut(id) {
                    s.current_tool = None;
                }
            }
            SemanticEventKind::RuntimeDetected { runtime } => {
                if let Some(s) = self.sessions.get_mut(id) {
                    s.runtime = runtime;
                }
            }
            SemanticEventKind::ContextUpdated { state } => {
                if let Some(s) = self.sessions.get_mut(id) {
                    s.context_state = state;
                }
            }
            SemanticEventKind::SessionRetitled { title } => {
                if let Some(s) = self.sessions.get_mut(id) {
                    s.title = Some(title);
                }
            }
            SemanticEventKind::SessionCwdChanged { cwd } => {
                if let Some(s) = self.sessions.get_mut(id) {
                    s.cwd = Some(cwd);
                }
            }
            SemanticEventKind::PromptStarted
            | SemanticEventKind::ResponseCompleted
            | SemanticEventKind::ContextThresholdCrossed { .. }
            | SemanticEventKind::ExternalStateImported { .. } => {}
        }
    }

    fn to_session_states(&self) -> Vec<ClaudeSessionState> {
        self.sessions
            .values()
            .filter(|s| s.is_alive)
            .map(snapshot_to_session_state)
            .collect()
    }
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Spawn a background task that subscribes to daemon semantic events and
/// publishes `Vec<ClaudeSessionState>` via a `watch` channel.
///
/// Returns `Some(watch::Receiver)` if the daemon socket exists, `None` otherwise.
pub(crate) fn try_spawn_subscriber() -> Option<watch::Receiver<Vec<ClaudeSessionState>>> {
    let sock = protocol::socket_path();
    if !sock.exists() {
        info!("Daemon socket not found — daemon subscription unavailable");
        return None;
    }

    let (tx, rx) = watch::channel(Vec::new());

    tokio::spawn(async move {
        loop {
            match run_subscription(&tx).await {
                Ok(()) => info!("Daemon subscription ended cleanly"),
                Err(e) => warn!("Daemon subscription error: {e}"),
            }

            // Grace period: retain the last-good snapshot so short disconnects
            // (daemon restart, socket hiccup) don't cause visible flicker.
            // Retry rapidly during the window; only clear if every attempt fails.
            const GRACE_SECS: u64 = 8;
            const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(GRACE_SECS);
            let mut reconnected = false;

            info!("Disconnect — entering {GRACE_SECS}s grace period (retaining last snapshot)");

            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(RETRY_INTERVAL).await;

                if !protocol::socket_path().exists() {
                    // Socket removed — daemon is fully gone, no point retrying.
                    break;
                }

                // Attempt to reconnect.  `run_subscription` will push fresh
                // data through `tx` on success, so the stale snapshot is
                // replaced automatically.
                match run_subscription(&tx).await {
                    Ok(()) => {
                        info!("Reconnected during grace — subscription ended cleanly");
                        reconnected = true;
                        break; // back to outer loop for a new grace cycle
                    }
                    Err(_) => {
                        // Daemon not ready yet — keep retrying.
                    }
                }
            }

            if reconnected {
                // Subscription ran and ended — the outer loop will handle it
                // with a fresh grace period.
                continue;
            }

            // Grace expired without reconnection — clear stale data.
            info!("Grace period expired — clearing sessions");
            let _ = tx.send(Vec::new());

            if !protocol::socket_path().exists() {
                info!("Daemon socket gone — stopping subscriber");
                break;
            }

            // Back off before the next attempt.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });

    Some(rx)
}

async fn run_subscription(
    tx: &watch::Sender<Vec<ClaudeSessionState>>,
) -> anyhow::Result<()> {
    let sock = protocol::socket_path();
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        UnixStream::connect(&sock),
    )
    .await??;

    let (mut reader, mut writer) = stream.into_split();

    let frame = protocol::encode_frame(&Request::SubscribeEvents {
        scope: EventScope::All,
    })
    .map_err(|e| anyhow::anyhow!("encode error: {e}"))?;
    writer.write_all(&frame).await?;

    info!("Daemon semantic subscription established");

    let mut aggregator = SessionAggregator::new();

    loop {
        let payload = match protocol::read_frame(&mut reader).await? {
            Some(p) => p,
            None => {
                debug!("Daemon connection closed");
                return Ok(());
            }
        };

        let response: Response = protocol::decode_payload(&payload)
            .map_err(|e| anyhow::anyhow!("decode error: {e}"))?;

        match response {
            Response::SnapshotSync(sync) => {
                aggregator.apply_snapshot(sync.snapshot);
                let _ = tx.send(aggregator.to_session_states());
            }
            Response::EventStream(batch) => {
                for event in batch.events {
                    aggregator.apply_event(event);
                }
                let _ = tx.send(aggregator.to_session_states());
            }
            _ => {}
        }
    }
}
