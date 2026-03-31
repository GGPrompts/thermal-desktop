//! Daemon event subscriber — connects to thermal-conductor's Unix socket
//! and receives semantic session events via MessagePack framing.
//!
//! The types here mirror the subset of `thermal-conductor::protocol` that
//! thermal-audio needs. They are wire-compatible (MessagePack serde).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tracing::{debug, warn};

// ── Wire protocol types (subset of conductor protocol.rs) ───────────────────

/// Scope filter for event subscriptions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventScope {
    All,
    Session(String),
    Categories(Vec<EventCategory>),
}

/// Event category for filtering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventCategory {
    SessionLifecycle,
    AgentRuntime,
    Tool,
    Context,
    Compatibility,
}

/// Detected agent runtime family.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentRuntime {
    Claude,
    Codex,
    Copilot,
    Unknown,
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self::Unknown
    }
}

/// High-level agent activity state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentActivity {
    Idle,
    Prompting,
    Thinking,
    ToolRunning,
    WaitingInput,
    StreamingOutput,
    Exited,
}

impl Default for AgentActivity {
    fn default() -> Self {
        Self::Idle
    }
}

/// Context/token usage state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ContextState {
    #[serde(default)]
    pub tokens_used: Option<u64>,
    #[serde(default)]
    pub tokens_limit: Option<u64>,
    #[serde(default)]
    pub saturation: Option<f64>,
}

/// Threshold level for context warnings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ContextThreshold {
    Warning,
    Critical,
}

/// Canonical session snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// A semantic session event with per-session sequencing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticEvent {
    pub session_id: String,
    pub seq: u64,
    pub kind: SemanticEventKind,
}

/// Semantic event variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SemanticEventKind {
    // Session lifecycle
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

    // Agent/runtime
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

    // Tool
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

    // Context
    ContextUpdated {
        state: ContextState,
    },
    ContextThresholdCrossed {
        level: ContextThreshold,
        #[serde(default)]
        saturation: Option<f64>,
    },

    // Compatibility
    ExternalStateImported {
        #[serde(default)]
        source: String,
    },
}

/// Initial snapshot delivery when subscribing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotSync {
    pub snapshot: SemanticSessionSnapshot,
    pub seq: u64,
}

/// A batch of semantic events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBatch {
    pub events: Vec<SemanticEvent>,
}

// ── Request/Response enums (subset) ─────────────────────────────────────────

/// Minimal request enum — we only need SubscribeEvents and Ping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    SubscribeEvents { scope: EventScope },
    Ping,
}

/// Response variants — must match thermal-conductor's Response enum order exactly.
/// MessagePack encodes enums by variant index, so ordering is critical.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    // 0: SessionSpawned
    SessionSpawned {
        id: String,
        name: String,
    },
    // 1: SessionList
    SessionList {
        sessions: Vec<serde_json::Value>,
    },
    // 2: SessionState
    SessionState {
        id: String,
        cols: u16,
        rows: u16,
        cells: Vec<serde_json::Value>,
        cursor: serde_json::Value,
        title: String,
        #[serde(default)]
        mode: u32,
    },
    // 3: ScreenUpdate
    ScreenUpdate {
        id: String,
        seq: u64,
        dirty_cells: Vec<serde_json::Value>,
        cursor: serde_json::Value,
        #[serde(default)]
        mode: u32,
    },
    // 4: TitleChanged
    TitleChanged {
        id: String,
        title: String,
    },
    // 5: SessionExited
    SessionExited {
        id: String,
        exit_code: Option<i32>,
        #[serde(default)]
        reason: String,
    },
    // 6: SnapshotSync
    SnapshotSync(SnapshotSync),
    // 7: EventStream
    EventStream(EventBatch),
    // 8: Ok
    Ok,
    // 9: Error
    Error { message: String },
    // 10: Pong
    Pong,
}

// ── Frame encoding/decoding ─────────────────────────────────────────────────

/// Encode a value into a length-prefixed MessagePack frame.
fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let payload = rmp_serde::to_vec(value).context("msgpack encode")?;
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Read a length-prefixed MessagePack frame.
/// Returns `None` on clean EOF.
async fn read_frame(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 64 * 1024 * 1024 {
        anyhow::bail!("frame too large: {len} bytes");
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Message received from the daemon event stream.
pub enum DaemonMessage {
    /// Initial snapshot for a session.
    Snapshot(SnapshotSync),
    /// A batch of semantic events.
    Events(EventBatch),
    /// Session exited (from the Response::SessionExited variant).
    SessionExited {
        id: String,
        #[allow(dead_code)]
        exit_code: Option<i32>,
        #[allow(dead_code)]
        reason: String,
    },
}

/// Return the daemon socket path.
pub fn daemon_socket_path() -> std::path::PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    std::path::PathBuf::from(format!("/run/user/{uid}/thermal/conductor.sock"))
}

/// Connect to the conductor daemon, subscribe to all events, and return a
/// stream-like async reader. Returns `None` if the daemon is unreachable.
///
/// Uses a 2-second timeout on the entire connect+subscribe handshake to avoid
/// hanging on stale sockets left behind after a daemon crash.
pub async fn connect_and_subscribe() -> Result<Option<DaemonEventStream>> {
    let sock_path = daemon_socket_path();
    if !sock_path.exists() {
        return Ok(None);
    }

    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        connect_and_subscribe_inner(&sock_path),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            warn!(
                "timed out connecting to conductor daemon at {} — stale socket?",
                sock_path.display()
            );
            let _ = std::fs::remove_file(&sock_path);
            Ok(None)
        }
    }
}

async fn connect_and_subscribe_inner(
    sock_path: &std::path::Path,
) -> Result<Option<DaemonEventStream>> {
    let stream = match UnixStream::connect(sock_path).await {
        Ok(s) => s,
        Err(e) => {
            warn!("cannot connect to conductor daemon at {}: {e}", sock_path.display());
            return Ok(None);
        }
    };

    let (reader, mut writer) = tokio::io::split(stream);

    // Send SubscribeEvents request.
    let req = Request::SubscribeEvents {
        scope: EventScope::All,
    };
    let frame = encode_frame(&req)?;
    tokio::io::AsyncWriteExt::write_all(&mut writer, &frame)
        .await
        .context("sending SubscribeEvents")?;

    debug!("subscribed to daemon events at {}", sock_path.display());

    Ok(Some(DaemonEventStream {
        reader: Box::new(tokio::io::BufReader::new(reader)),
    }))
}

/// Async event stream from the conductor daemon.
pub struct DaemonEventStream {
    reader: Box<tokio::io::BufReader<tokio::io::ReadHalf<UnixStream>>>,
}

impl DaemonEventStream {
    /// Read the next message from the daemon. Returns `None` on disconnect.
    pub async fn next_message(&mut self) -> Result<Option<DaemonMessage>> {
        let payload = match read_frame(&mut self.reader).await? {
            Some(p) => p,
            None => return Ok(None),
        };

        // Try to decode as Response.
        let resp: Response = match rmp_serde::from_slice(&payload) {
            Ok(r) => r,
            Err(e) => {
                warn!("failed to decode daemon response: {e}");
                return Err(anyhow::anyhow!("failed to decode daemon response: {e}"));
            }
        };

        match resp {
            Response::SnapshotSync(sync) => Ok(Some(DaemonMessage::Snapshot(sync))),
            Response::EventStream(batch) => Ok(Some(DaemonMessage::Events(batch))),
            Response::SessionExited {
                id,
                exit_code,
                reason,
            } => Ok(Some(DaemonMessage::SessionExited {
                id,
                exit_code,
                reason,
            })),
            Response::Error { message } => {
                warn!("daemon error: {message}");
                Ok(Some(DaemonMessage::Events(EventBatch { events: vec![] })))
            }
            _ => {
                // Other responses (Pong, Ok, SessionSpawned, etc.) — ignore.
                Ok(Some(DaemonMessage::Events(EventBatch { events: vec![] })))
            }
        }
    }
}
