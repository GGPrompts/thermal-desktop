//! Daemon semantic event subscriber for thermal-hud.
//!
//! Connects to the thermal-conductor session daemon via its Unix socket
//! and subscribes to semantic session events (`SubscribeEvents`).  Received
//! snapshots and incremental events are converted into `ClaudeSessionState`
//! values that the existing HUD renderer consumes unchanged.
//!
//! When the daemon is not running, callers fall back to `ClaudeStatePoller`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use thermal_core::{ClaudeSessionState, ClaudeStatus};

// ── Minimal protocol types (deserialize-compatible with thermal-conductor) ──

/// Scope filter — we always subscribe to `All`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventScope {
    All,
    Session(String),
    Categories(Vec<String>),
}

/// Agent runtime family.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum AgentRuntime {
    Claude,
    Codex,
    Copilot,
    #[default]
    Unknown,
}

/// High-level agent activity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum AgentActivity {
    #[default]
    Idle,
    Prompting,
    Thinking,
    ToolRunning,
    WaitingInput,
    StreamingOutput,
    Exited,
}

/// Context/token usage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ContextState {
    #[serde(default)]
    pub tokens_used: Option<u64>,
    #[serde(default)]
    pub tokens_limit: Option<u64>,
    #[serde(default)]
    pub saturation: Option<f64>,
}

/// Session snapshot from the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SemanticSessionSnapshot {
    pub session_id: String,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub runtime: AgentRuntime,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub workspace_root: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub last_activity_at: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub is_alive: bool,
    #[serde(default)]
    pub agent_activity: AgentActivity,
    #[serde(default)]
    pub current_tool: Option<String>,
    #[serde(default)]
    pub context_state: ContextState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotSync {
    pub snapshot: SemanticSessionSnapshot,
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SemanticEvent {
    pub session_id: String,
    pub seq: u64,
    pub kind: SemanticEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SemanticEventKind {
    SessionSpawned {
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    SessionExited {
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        reason: String,
    },
    SessionRetitled {
        title: String,
    },
    SessionCwdChanged {
        cwd: String,
    },
    RuntimeDetected {
        runtime: AgentRuntime,
    },
    AgentActivityChanged {
        activity: AgentActivity,
        #[serde(default)]
        previous: Option<AgentActivity>,
    },
    PromptStarted,
    ResponseCompleted,
    ToolStarted {
        tool_name: String,
    },
    ToolCompleted {
        tool_name: String,
        #[serde(default)]
        duration_ms: Option<u64>,
    },
    ToolFailed {
        tool_name: String,
        #[serde(default)]
        error: Option<String>,
    },
    ContextUpdated {
        state: ContextState,
    },
    ContextThresholdCrossed {
        level: String,
        #[serde(default)]
        saturation: Option<f64>,
    },
    ExternalStateImported {
        #[serde(default)]
        source: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventBatch {
    pub events: Vec<SemanticEvent>,
}

/// Minimal subset of the daemon Request enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    SubscribeEvents { scope: EventScope },
    Ping,
}

/// Minimal subset of the daemon Response enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    SnapshotSync(SnapshotSync),
    EventStream(EventBatch),
    Pong,
    Ok,
    Error { message: String },
    // Catch-all for responses we don't care about in the HUD.
    #[serde(other)]
    Other,
}

// ── Wire framing (MessagePack, length-prefixed) ─────────────────────────────

fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let payload = rmp_serde::to_vec(value)?;
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

async fn read_frame<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 64 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes"),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

// ── Conversion: SemanticSessionSnapshot → ClaudeSessionState ────────────────

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
    ClaudeSessionState {
        session_id: snap.session_id.clone(),
        status: activity_to_status(&snap.agent_activity),
        current_tool: snap.current_tool.clone(),
        working_dir: snap.cwd.clone(),
        context_percent: snap.context_state.saturation.map(|s| s * 100.0),
        agent_type: runtime_to_agent_type(&snap.runtime),
        last_updated: snap.last_activity_at.clone(),
        pid: snap.pid.map(|p| p as i64),
        // Fields the HUD renderer doesn't use but need to be populated.
        ..ClaudeSessionState::default()
    }
}

// ── State aggregator ────────────────────────────────────────────────────────

/// Holds per-session state from daemon snapshots/events and produces
/// `Vec<ClaudeSessionState>` on demand.
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
                        agent_activity: AgentActivity::Idle,
                        current_tool: None,
                        context_state: ContextState::default(),
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
            // Events we don't need to track for HUD rendering.
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

/// Return the daemon socket path.
fn socket_path() -> std::path::PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    std::path::PathBuf::from(format!("/run/user/{uid}/thermal/conductor.sock"))
}

/// Spawn a background task that subscribes to daemon semantic events and
/// publishes `Vec<ClaudeSessionState>` via a `watch` channel.
///
/// Returns `Some(watch::Receiver)` if the daemon is reachable, `None` if not.
pub fn try_spawn_subscriber() -> Option<watch::Receiver<Vec<ClaudeSessionState>>> {
    let sock = socket_path();
    if !sock.exists() {
        info!("Daemon socket not found — will use ClaudeStatePoller fallback");
        return None;
    }

    let (tx, rx) = watch::channel(Vec::new());

    tokio::spawn(async move {
        // Retry loop: reconnect if the daemon restarts.
        loop {
            match run_subscription(&tx).await {
                Ok(()) => {
                    info!("Daemon subscription ended cleanly");
                }
                Err(e) => {
                    warn!("Daemon subscription error: {e}");
                }
            }

            // Clear sessions on disconnect so the HUD doesn't show stale data.
            let _ = tx.send(Vec::new());

            // Wait before reconnecting.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            if !socket_path().exists() {
                info!("Daemon socket gone — stopping subscriber");
                break;
            }
        }
    });

    Some(rx)
}

async fn run_subscription(
    tx: &watch::Sender<Vec<ClaudeSessionState>>,
) -> anyhow::Result<()> {
    let sock = socket_path();
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        UnixStream::connect(&sock),
    )
    .await??;

    let (mut reader, mut writer) = stream.into_split();

    // Send SubscribeEvents { scope: All }.
    let frame = encode_frame(&Request::SubscribeEvents {
        scope: EventScope::All,
    })
    .map_err(|e| anyhow::anyhow!("encode error: {e}"))?;
    writer.write_all(&frame).await?;

    info!("Daemon subscription established");

    let mut aggregator = SessionAggregator::new();

    loop {
        let payload = match read_frame(&mut reader).await? {
            Some(p) => p,
            None => {
                debug!("Daemon connection closed");
                return Ok(());
            }
        };

        let response: Response = match rmp_serde::from_slice(&payload) {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to decode daemon response: {e}");
                continue;
            }
        };

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
            Response::Pong | Response::Ok | Response::Other => {}
            Response::Error { message } => {
                warn!("Daemon error: {message}");
            }
        }
    }
}
