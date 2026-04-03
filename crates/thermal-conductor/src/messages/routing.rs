//! Agent routing — dispatches messages to the appropriate backend based on
//! the `to` field's agent_type.
//!
//! Route table: maps agent types ("system", "claude", "codex", "planner", "user")
//! to backend implementations. Each backend knows how to dispatch a message and
//! return a response.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use thermal_core::message::{Message, MessageType, TaskState};

// ---------------------------------------------------------------------------
// Trust tiers (lightweight re-implementation for the message bus)
// ---------------------------------------------------------------------------

/// Execution policy for a tool routed via @system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustTier {
    /// Execute immediately.
    Auto,
    /// Require user confirmation (not yet implemented — treated as Auto for now).
    Confirm,
    /// Reject outright.
    Block,
}

/// Minimal trust tier config: maps tool names to tiers.
/// Tools not listed default to Confirm.
pub struct TrustConfig {
    tiers: HashMap<String, TrustTier>,
}

impl TrustConfig {
    /// Load trust tiers from the workspace config file.
    pub fn load_default() -> Self {
        let candidates = vec![
            // Workspace config (development)
            PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../config/trust-tiers.toml"
            )),
            // User config
            dirs_home().join(".config/thermal/trust-tiers.toml"),
        ];

        for path in &candidates {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(cfg) = Self::parse(&content) {
                    info!(path = %path.display(), tiers = cfg.tiers.len(), "loaded trust config");
                    return cfg;
                }
            }
        }

        warn!("no trust-tiers.toml found, all tools default to CONFIRM");
        Self {
            tiers: HashMap::new(),
        }
    }

    /// Parse trust tier config from TOML content.
    /// Expects `[tiers]\ntool_name = "AUTO"` format.
    fn parse(content: &str) -> Result<Self> {
        let mut tiers = HashMap::new();
        let mut in_tiers_section = false;

        for line in content.lines() {
            let line = line.trim();
            if line == "[tiers]" {
                in_tiers_section = true;
                continue;
            }
            if line.starts_with('[') {
                in_tiers_section = false;
                continue;
            }
            if !in_tiers_section || line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Parse `key = "VALUE"` or `"key" = "VALUE"`
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim().trim_matches('"').to_string();
                let value = value.trim().trim_matches('"');
                let tier = match value.to_uppercase().as_str() {
                    "AUTO" => TrustTier::Auto,
                    "CONFIRM" => TrustTier::Confirm,
                    "BLOCK" => TrustTier::Block,
                    _ => TrustTier::Confirm,
                };
                tiers.insert(key, tier);
            }
        }

        Ok(Self { tiers })
    }

    /// Look up the trust tier for a tool. Defaults to Confirm.
    pub fn tier_for(&self, tool_name: &str) -> TrustTier {
        self.tiers
            .get(tool_name)
            .copied()
            .unwrap_or(TrustTier::Confirm)
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

// ---------------------------------------------------------------------------
// Rate limiter for @system routes (therm-rrz3)
// ---------------------------------------------------------------------------

/// Maximum number of @system commands allowed within the rate limit window.
const SYSTEM_RATE_LIMIT_MAX: usize = 3;

/// Duration of the sliding window for @system rate limiting.
const SYSTEM_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// Sliding-window timestamps of recent @system command executions.
/// Protected by a Mutex for safe access from async dispatch.
static SYSTEM_COMMAND_TIMESTAMPS: Mutex<Option<VecDeque<Instant>>> = Mutex::new(None);

/// Check the @system rate limiter. Returns Ok(()) if the command is allowed,
/// or Err with a message if rate-limited.
fn check_system_rate_limit() -> Result<(), String> {
    let mut guard = SYSTEM_COMMAND_TIMESTAMPS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let timestamps = guard.get_or_insert_with(VecDeque::new);

    let now = Instant::now();

    // Evict entries older than the window
    while let Some(&front) = timestamps.front() {
        if now.duration_since(front) > SYSTEM_RATE_LIMIT_WINDOW {
            timestamps.pop_front();
        } else {
            break;
        }
    }

    if timestamps.len() >= SYSTEM_RATE_LIMIT_MAX {
        let oldest = timestamps.front().unwrap();
        let wait_secs = SYSTEM_RATE_LIMIT_WINDOW
            .saturating_sub(now.duration_since(*oldest))
            .as_secs();
        Err(format!(
            "RATE LIMITED: @system commands capped at {SYSTEM_RATE_LIMIT_MAX} per {}s — \
             try again in ~{wait_secs}s",
            SYSTEM_RATE_LIMIT_WINDOW.as_secs()
        ))
    } else {
        timestamps.push_back(now);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Dispatcher @system allowlist (therm-rrz3)
// ---------------------------------------------------------------------------

/// Tools that the dispatcher is allowed to invoke via @system.
/// Only read-only / informational tools — no desktop interaction.
/// Write tools (click, type_text, run_command, etc.) are blocked when
/// the message originates from the dispatcher to prevent prompt-injection
/// exploitation of the Ollama pipeline.
const DISPATCHER_SYSTEM_ALLOWLIST: &[&str] = &[
    "capture_pane",
    "screenshot",
    "list_windows",
    "active_window",
    "list_workspaces",
    "claude_status",
    "clipboard_get",
    "system_metrics",
];

/// Check whether a dispatcher-originated @system command is allowed.
fn is_dispatcher_allowed_tool(tool_name: &str) -> bool {
    DISPATCHER_SYSTEM_ALLOWLIST.contains(&tool_name)
}

/// Resolve a binary name to its full path, checking common locations
/// that may not be in the daemon's PATH (e.g. ~/.local/bin).
fn resolve_binary(name: &str) -> String {
    let extra_dirs = [
        dirs_home().join(".local/bin"),
        dirs_home().join(".cargo/bin"),
        PathBuf::from("/usr/local/bin"),
    ];

    for dir in &extra_dirs {
        let candidate = dir.join(name);
        if candidate.exists() {
            return candidate.to_string_lossy().to_string();
        }
    }

    // Fall back to bare name (rely on PATH)
    name.to_string()
}

// ---------------------------------------------------------------------------
// Backend enum (avoids async-trait dependency)
// ---------------------------------------------------------------------------

/// Known agent backends. Uses enum dispatch instead of trait objects to avoid
/// needing the `async-trait` crate.
enum Backend {
    System(TrustConfig),
    Claude,
    Codex,
    Planner,
    User,
    Dispatcher,
}

impl Backend {
    fn name(&self) -> &str {
        match self {
            Backend::System(_) => "system",
            Backend::Claude => "claude",
            Backend::Codex => "codex",
            Backend::Planner => "planner",
            Backend::User => "user",
            Backend::Dispatcher => "dispatcher",
        }
    }

    async fn dispatch(&self, msg: &Message) -> Result<Message> {
        match self {
            Backend::System(trust_config) => dispatch_system(msg, trust_config).await,
            Backend::Claude => dispatch_claude(msg).await,
            Backend::Codex => dispatch_codex(msg).await,
            Backend::Planner => dispatch_planner(msg).await,
            Backend::User => dispatch_user(msg).await,
            Backend::Dispatcher => dispatch_dispatcher(msg).await,
        }
    }
}

// ---------------------------------------------------------------------------
// SystemBackend — pipes to thermal-commander via JSON-RPC stdio
// ---------------------------------------------------------------------------

/// Monotonically increasing request ID for JSON-RPC calls.
static JSONRPC_ID: AtomicU64 = AtomicU64::new(1);

async fn dispatch_system(msg: &Message, trust_config: &TrustConfig) -> Result<Message> {
    // The content should contain a tool call. We expect either:
    // 1. A JSON object with "tool" and "input" fields
    // 2. Plain text (treated as a tool name with no args)
    let (tool_name, input) = if let Ok(parsed) = serde_json::from_str::<Value>(&msg.content) {
        let tool = parsed
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let input = parsed.get("input").cloned().unwrap_or_else(|| json!({}));
        (tool, input)
    } else {
        (msg.content.trim().to_string(), json!({}))
    };

    if tool_name.is_empty() {
        bail!("@system message must specify a tool name");
    }

    let from_dispatcher = msg.from.agent_type == "dispatcher";

    // Dispatcher-originated @system commands are restricted to read-only tools
    // to prevent prompt-injection attacks via the Ollama pipeline (therm-rrz3).
    if from_dispatcher && !is_dispatcher_allowed_tool(&tool_name) {
        warn!(
            tool = %tool_name,
            from = %msg.from,
            "BLOCKED: dispatcher not allowed to invoke write tool via @system"
        );
        return Ok(make_response(
            msg,
            format!(
                "BLOCKED: dispatcher is restricted to read-only tools via @system \
                 (allowed: {}). Tool '{tool_name}' is not permitted.",
                DISPATCHER_SYSTEM_ALLOWLIST.join(", ")
            ),
        ));
    }

    // Rate limit @system commands — max 3 per 60s (therm-rrz3).
    // Applies to all senders but primarily guards against runaway dispatcher loops.
    if let Err(reason) = check_system_rate_limit() {
        warn!(
            tool = %tool_name,
            from = %msg.from,
            "{reason}"
        );
        return Ok(make_response(msg, reason));
    }

    // Check trust tier
    let tier = trust_config.tier_for(&tool_name);
    match tier {
        TrustTier::Block => {
            return Ok(make_response(
                msg,
                format!("BLOCKED: tool '{tool_name}' is not allowed"),
            ));
        }
        TrustTier::Confirm => {
            // SECURITY NOTE (therm-rrz3, therm-wgss):
            // Trust tiers were intentionally simplified — CONFIRM-tier tools
            // auto-proceed without HUD confirmation. This is a known gap:
            // a prompt-injected Ollama response could trigger CONFIRM-tier
            // tools (click, type_text, etc.) without user approval.
            //
            // Mitigations in place:
            //   1. Rate limiter: max 3 @system commands per 60s
            //   2. Dispatcher allowlist: dispatcher can only invoke read-only tools
            //
            // Planned: Wire HUD confirmation flow so CONFIRM-tier tools
            // actually pause and wait for user approval before executing.
            warn!(tool = %tool_name, "tool requires confirmation — auto-proceeding (HUD confirmation not yet wired)");
        }
        TrustTier::Auto => {}
    }

    match execute_commander_tool(&tool_name, &input).await {
        Ok(result) => Ok(make_response(msg, result)),
        Err(e) => Ok(make_response(
            msg,
            format!("Error executing {tool_name}: {e}"),
        )),
    }
}

/// Execute a tool via thermal-commander MCP server (JSON-RPC over stdio).
async fn execute_commander_tool(tool_name: &str, input: &Value) -> Result<String> {
    info!(tool = %tool_name, "executing via thermal-commander");

    let mut child = Command::new(resolve_binary("thermal-commander"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn thermal-commander — is it installed?")?;

    let stdin = child.stdin.take().context("no stdin on child")?;
    let stdout = child.stdout.take().context("no stdout on child")?;

    let mut writer = tokio::io::BufWriter::new(stdin);
    let mut reader = BufReader::new(stdout);

    // Send initialize
    let init_id = JSONRPC_ID.fetch_add(1, Ordering::Relaxed);
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": init_id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "thermal-conductor",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }
    });
    let mut init_line = serde_json::to_string(&init_req)?;
    init_line.push('\n');
    writer.write_all(init_line.as_bytes()).await?;
    writer.flush().await?;

    // Read initialize response
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await?;
    debug!(init_response = %response_line.trim(), "commander init");

    // Send initialized notification
    let notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let mut notif_line = serde_json::to_string(&notif)?;
    notif_line.push('\n');
    writer.write_all(notif_line.as_bytes()).await?;
    writer.flush().await?;

    // Send tools/call
    let call_id = JSONRPC_ID.fetch_add(1, Ordering::Relaxed);
    let call_req = json!({
        "jsonrpc": "2.0",
        "id": call_id,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": input,
        }
    });
    let mut call_line = serde_json::to_string(&call_req)?;
    call_line.push('\n');
    writer.write_all(call_line.as_bytes()).await?;
    writer.flush().await?;

    // Read tools/call response
    response_line.clear();
    reader.read_line(&mut response_line).await?;
    debug!(call_response = %response_line.trim(), "commander response");

    // Close stdin, wait for exit
    drop(writer);
    let _ = child.wait().await;

    // Parse the MCP response
    parse_mcp_response(&response_line)
}

/// Parse MCP JSON-RPC response into a result string.
fn parse_mcp_response(response_line: &str) -> Result<String> {
    let resp: Value =
        serde_json::from_str(response_line.trim()).context("parsing thermal-commander response")?;

    if let Some(error) = resp.get("error") {
        let msg = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Ok(format!("Error: {msg}"));
    }

    let result = resp.get("result");

    if let Some(content) = result
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
    {
        let texts: Vec<&str> = content
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                    block.get("text").and_then(|v| v.as_str())
                } else {
                    None
                }
            })
            .collect();
        if !texts.is_empty() {
            return Ok(texts.join("\n"));
        }
    }

    Ok(result
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
        .unwrap_or_else(|| "no result".to_string()))
}

// ---------------------------------------------------------------------------
// Sidecar + live-session routing (kitty + daemon)
// ---------------------------------------------------------------------------

/// Minimal sidecar entry — just the fields we need for routing.
/// Avoids coupling to thermal-conductor's full `SidecarEntry` type.
#[derive(Debug, Deserialize)]
struct RoutingSidecarEntry {
    session_id: String,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RoutingSidecarData {
    #[serde(default)]
    sessions: Vec<RoutingSidecarEntry>,
}

/// Where a sidecar session lives — determines which transport to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionSource {
    /// Session is owned by the conductor daemon (session-N pattern).
    Daemon,
    /// Session is a kitty window (UUID or hex ID pattern).
    Kitty,
}

/// A live session found in the sidecar, with its transport source.
#[derive(Debug)]
struct LiveSession {
    session_id: String,
    source: SessionSource,
}

/// Infer the session source from its ID.
///
/// Daemon sessions use the `session-<N>` pattern; kitty sessions use hex
/// IDs or UUIDs generated by kitty.
fn infer_source(session_id: &str) -> SessionSource {
    if session_id.starts_with("session-") {
        SessionSource::Daemon
    } else {
        SessionSource::Kitty
    }
}

/// Read and parse the sessions sidecar at `/run/user/{uid}/thermal/sessions.json`.
/// Returns `None` if the file doesn't exist or can't be parsed.
fn read_sidecar() -> Option<Vec<RoutingSidecarEntry>> {
    let path = thermal_core::runtime::runtime_dir().join("sessions.json");
    let content = std::fs::read_to_string(path).ok()?;
    let data: RoutingSidecarData = serde_json::from_str(&content).ok()?;
    Some(data.sessions)
}

/// Send text to a specific kitty window via `kitty @ send-text`.
/// `window_match` is a kitty match expression (e.g. `title:^thermal-abc$`).
/// The text is sent with a trailing carriage return to submit it.
async fn kitty_send_text(window_match: &str, text: &str) -> Result<String> {
    // Build the text payload with a trailing \r to press Enter
    let payload = format!("{text}\r");

    let output = Command::new("kitty")
        .args(["@", "send-text", "--match", window_match, "--"])
        .arg(&payload)
        .output()
        .await
        .context("failed to run kitty @ send-text")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitty @ send-text failed: {stderr}");
    }

    Ok("ok".to_string())
}

// ---------------------------------------------------------------------------
// Conductor daemon protocol (lightweight re-implementation)
// ---------------------------------------------------------------------------
//
// We re-implement the minimal protocol types here rather than depending on
// the full protocol module (which may pull in extra types). The wire format
// is length-prefixed MessagePack, identical to protocol.rs.

/// Request sent to the conductor daemon — only the variant we need.
#[derive(Debug, Serialize)]
enum DaemonRequest {
    SendText { id: String, text: String },
}

/// Response from the conductor daemon — only the variants we care about.
#[derive(Debug, Deserialize)]
enum DaemonResponse {
    Ok,
    Error { message: String },
}

/// Return the conductor daemon socket path.
fn conductor_socket_path() -> PathBuf {
    thermal_core::runtime::socket_path("conductor")
}

/// Send text to a daemon session via the conductor's `SendText` request.
///
/// Connects to the conductor socket, sends a single SendText request,
/// reads the response, and disconnects.
async fn daemon_send_text(session_id: &str, text: &str) -> Result<String> {
    let sock_path = conductor_socket_path();
    if !sock_path.exists() {
        bail!(
            "conductor daemon socket not found at {}",
            sock_path.display()
        );
    }

    let stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .with_context(|| format!("connecting to conductor daemon at {}", sock_path.display()))?;

    let (mut reader, mut writer) = stream.into_split();

    // Encode the request as a length-prefixed MessagePack frame.
    let request = DaemonRequest::SendText {
        id: session_id.to_string(),
        text: text.to_string(),
    };
    let payload = rmp_serde::to_vec(&request).context("failed to encode SendText request")?;
    let len = payload.len() as u32;
    writer.write_all(&len.to_le_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;

    // Read the response frame.
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .context("reading response length from conductor")?;
    let resp_len = u32::from_le_bytes(len_buf) as usize;

    if resp_len > 64 * 1024 * 1024 {
        bail!("conductor response frame too large: {resp_len} bytes");
    }

    let mut resp_buf = vec![0u8; resp_len];
    reader
        .read_exact(&mut resp_buf)
        .await
        .context("reading response payload from conductor")?;

    let response: DaemonResponse =
        rmp_serde::from_slice(&resp_buf).context("decoding conductor response")?;

    match response {
        DaemonResponse::Ok => Ok("ok".to_string()),
        DaemonResponse::Error { message } => bail!("conductor error: {message}"),
    }
}

/// Known display name prefixes that indicate a Claude session.
const CLAUDE_DISPLAY_HINTS: &[&str] = &["opus", "sonnet", "haiku", "claude"];

/// Known display name prefixes that indicate a Codex session.
const CODEX_DISPLAY_HINTS: &[&str] = &["codex", "gpt"];

/// Try to find a live session matching the given display name hints.
///
/// Returns all matching sessions, preferring daemon sessions (which survive
/// window close) over kitty sessions. The caller tries daemon routing first.
fn find_live_session(hints: &[&str]) -> Option<LiveSession> {
    let entries = read_sidecar()?;

    let mut daemon_match: Option<LiveSession> = None;
    let mut kitty_match: Option<LiveSession> = None;

    for entry in &entries {
        if let Some(ref name) = entry.display_name {
            let lower = name.to_lowercase();
            for hint in hints {
                if lower.starts_with(hint) {
                    let source = infer_source(&entry.session_id);
                    debug!(
                        session_id = %entry.session_id,
                        display_name = %name,
                        hint = %hint,
                        ?source,
                        "found live session matching hint"
                    );
                    match source {
                        SessionSource::Daemon if daemon_match.is_none() => {
                            daemon_match = Some(LiveSession {
                                session_id: entry.session_id.clone(),
                                source,
                            });
                        }
                        SessionSource::Kitty if kitty_match.is_none() => {
                            kitty_match = Some(LiveSession {
                                session_id: entry.session_id.clone(),
                                source,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Prefer daemon sessions — they survive window close.
    daemon_match.or(kitty_match)
}

/// Send text to a live session, dispatching via the appropriate transport.
async fn send_to_live_session(session: &LiveSession, text: &str) -> Result<String> {
    match session.source {
        SessionSource::Daemon => daemon_send_text(&session.session_id, text).await,
        SessionSource::Kitty => {
            // Validate session_id before interpolating into a regex match
            // string to prevent regex injection via crafted sidecar entries.
            if !session
                .session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
            {
                bail!(
                    "session_id contains invalid characters: {:?}",
                    session.session_id
                );
            }
            let window_match = format!("title:^thermal-{}$", session.session_id);
            kitty_send_text(&window_match, text).await
        }
    }
}

// ---------------------------------------------------------------------------
// ClaudeBackend — routes to live kitty session, falls back to `claude -p`
// ---------------------------------------------------------------------------

async fn dispatch_claude(msg: &Message) -> Result<Message> {
    info!(content_len = msg.content.len(), "dispatching to claude");

    // Try to route to a live session (daemon or kitty)
    if let Some(session) = find_live_session(CLAUDE_DISPLAY_HINTS) {
        match send_to_live_session(&session, &msg.content).await {
            Ok(_) => {
                info!(
                    session_id = %session.session_id,
                    source = ?session.source,
                    "sent message to live claude session"
                );
                return Ok(make_response(
                    msg,
                    format!("Message sent to live session '{}'", session.session_id),
                ));
            }
            Err(e) => {
                warn!(
                    session_id = %session.session_id,
                    source = ?session.source,
                    error = %e,
                    "failed to send to live session, falling back to one-shot"
                );
            }
        }
    }

    // Fall back to one-shot `claude -p`
    let mut cmd = Command::new(resolve_binary("claude"));
    cmd.arg("-p")
        .arg(&msg.content)
        .arg("--output-format")
        .arg("json");

    // If metadata has an mcp_config path, pass it
    if let Some(mcp_config) = msg.metadata.get("mcp_config").and_then(|v| v.as_str()) {
        cmd.arg("--mcp-config").arg(mcp_config);
    }

    // If metadata has a model override
    if let Some(model) = msg.metadata.get("model").and_then(|v| v.as_str()) {
        cmd.arg("--model").arg(model);
    }

    let output = cmd
        .output()
        .await
        .context("failed to spawn claude CLI — is it installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        let err_msg = if stderr.is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        return Ok(make_response(msg, format!("claude error: {err_msg}")));
    }

    // Try to extract just the text result from Claude's JSON output
    let content = if let Ok(parsed) = serde_json::from_str::<Value>(stdout.trim()) {
        parsed
            .get("result")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| stdout.trim().to_string())
    } else {
        stdout.trim().to_string()
    };

    Ok(make_response(msg, content))
}

// ---------------------------------------------------------------------------
// CodexBackend — spawns `codex` with the message content
// ---------------------------------------------------------------------------

async fn dispatch_codex(msg: &Message) -> Result<Message> {
    info!(content_len = msg.content.len(), "dispatching to codex");

    // Try to route to a live session (daemon or kitty)
    if let Some(session) = find_live_session(CODEX_DISPLAY_HINTS) {
        match send_to_live_session(&session, &msg.content).await {
            Ok(_) => {
                info!(
                    session_id = %session.session_id,
                    source = ?session.source,
                    "sent message to live codex session"
                );
                return Ok(make_response(
                    msg,
                    format!("Message sent to live session '{}'", session.session_id),
                ));
            }
            Err(e) => {
                warn!(
                    session_id = %session.session_id,
                    source = ?session.source,
                    error = %e,
                    "failed to send to live session, falling back to one-shot"
                );
            }
        }
    }

    // Fall back to one-shot `codex`
    let output = Command::new(resolve_binary("codex"))
        .arg("--quiet")
        .arg(&msg.content)
        .output()
        .await
        .context("failed to spawn codex CLI — is it installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        let err_msg = if stderr.is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        return Ok(make_response(msg, format!("codex error: {err_msg}")));
    }

    Ok(make_response(msg, stdout.trim().to_string()))
}

// ---------------------------------------------------------------------------
// PlannerBackend — delegates to Claude with a planner system prompt
// ---------------------------------------------------------------------------

async fn dispatch_planner(msg: &Message) -> Result<Message> {
    info!(content_len = msg.content.len(), "dispatching to planner");

    let mut cmd = Command::new(resolve_binary("claude"));
    cmd.arg("-p")
        .arg(&msg.content)
        .arg("--model")
        .arg("haiku")
        .arg("--output-format")
        .arg("json")
        .arg("--system-prompt")
        .arg(concat!(
            "You are a planning agent. Break down tasks, create structured plans, ",
            "and coordinate work across agents. Be concise and actionable."
        ));

    // If metadata has an mcp_config path, pass it
    if let Some(mcp_config) = msg.metadata.get("mcp_config").and_then(|v| v.as_str()) {
        cmd.arg("--mcp-config").arg(mcp_config);
    }

    let output = cmd
        .output()
        .await
        .context("failed to spawn claude CLI for planner — is it installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        let err_msg = if stderr.is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        return Ok(make_response(msg, format!("planner error: {err_msg}")));
    }

    let content = if let Ok(parsed) = serde_json::from_str::<Value>(stdout.trim()) {
        parsed
            .get("result")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| stdout.trim().to_string())
    } else {
        stdout.trim().to_string()
    };

    Ok(make_response(msg, content))
}

// ---------------------------------------------------------------------------
// UserBackend — broadcasts to TUI subscribers + optional TTS
// ---------------------------------------------------------------------------

async fn dispatch_user(msg: &Message) -> Result<Message> {
    info!(content_len = msg.content.len(), "routing to user");

    // Try to send TTS via thermal-audio socket (best-effort)
    if let Some(tts_text) = msg.metadata.get("tts").and_then(|v| v.as_str()) {
        if let Err(e) = send_tts(tts_text).await {
            warn!(error = %e, "failed to send TTS to thermal-audio");
        }
    }

    // The message itself will be broadcast to subscribers by the
    // normal ingest path. The UserBackend just returns an ack.
    Ok(make_response(msg, "delivered to user".to_string()))
}

// ---------------------------------------------------------------------------
// DispatcherBackend — sends transcript to thermal-dispatcher via Unix socket
// ---------------------------------------------------------------------------

async fn dispatch_dispatcher(msg: &Message) -> Result<Message> {
    info!(content_len = msg.content.len(), "dispatching to dispatcher");

    let sock_path = thermal_core::runtime::socket_path("dispatcher");

    let stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .with_context(|| {
            format!(
                "connecting to thermal-dispatcher at {}",
                sock_path.display()
            )
        })?;

    let request = json!({
        "transcript": msg.content,
        "confidence": 1.0
    });

    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_string(&request)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;

    // Read the response
    let mut buf_reader = BufReader::new(reader);
    let mut response_line = String::new();
    buf_reader.read_line(&mut response_line).await?;

    // Try to extract a meaningful response from the dispatcher's JSON
    let content = if let Ok(parsed) = serde_json::from_str::<Value>(response_line.trim()) {
        parsed
            .get("response")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                parsed
                    .get("result")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| response_line.trim().to_string())
    } else {
        response_line.trim().to_string()
    };

    Ok(make_response(msg, content))
}

/// Send a TTS request to thermal-audio via its Unix socket.
async fn send_tts(text: &str) -> Result<()> {
    let sock_path = thermal_core::runtime::socket_path("audio");

    let stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .with_context(|| format!("connecting to thermal-audio at {}", sock_path.display()))?;

    let request = json!({
        "action": "speak",
        "text": text
    });

    let (_, mut writer) = stream.into_split();
    let mut line = serde_json::to_string(&request)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;

    info!("sent TTS request to thermal-audio");
    Ok(())
}

// ---------------------------------------------------------------------------
// Route table
// ---------------------------------------------------------------------------

/// The route table maps agent_type strings to backend implementations.
pub struct RouteTable {
    backends: HashMap<String, Backend>,
}

impl RouteTable {
    /// Build the default route table with all known backends.
    pub fn new() -> Self {
        let trust_config = TrustConfig::load_default();
        let mut backends = HashMap::new();

        backends.insert("system".to_string(), Backend::System(trust_config));
        backends.insert("claude".to_string(), Backend::Claude);
        backends.insert("codex".to_string(), Backend::Codex);
        backends.insert("planner".to_string(), Backend::Planner);
        backends.insert("user".to_string(), Backend::User);
        backends.insert("dispatcher".to_string(), Backend::Dispatcher);

        Self { backends }
    }

    /// Check whether a given agent type has a registered backend.
    #[allow(dead_code)]
    pub fn has_backend(&self, agent_type: &str) -> bool {
        self.backends.contains_key(agent_type)
    }

    /// List all registered backend names.
    pub fn registered_targets(&self) -> Vec<&str> {
        self.backends.keys().map(|k| k.as_str()).collect()
    }
}

// ---------------------------------------------------------------------------
// Route dispatcher — called after ingesting a message
// ---------------------------------------------------------------------------

/// Attempt to route a message to the appropriate backend.
/// Returns None if the message is not routable (e.g., Subscribe, Ack, broadcast).
/// Returns Some(response) if a backend handled it.
pub async fn route_message(msg: &Message, table: &RouteTable) -> Option<Message> {
    // Only route AgentMsg messages
    if !matches!(msg.msg_type, MessageType::AgentMsg) {
        return None;
    }

    // Wildcard target — broadcast, not routed
    if msg.to.agent_type == "*" {
        return None;
    }

    let agent_type = &msg.to.agent_type;

    let backend = match table.backends.get(agent_type.as_str()) {
        Some(b) => b,
        None => {
            warn!(target = %agent_type, "no backend registered for target");
            return Some(make_response(
                msg,
                format!(
                    "unknown target '@{agent_type}' — known targets: {}",
                    table.registered_targets().join(", ")
                ),
            ));
        }
    };

    // Check for async dispatch mode
    let is_async = msg
        .metadata
        .get("async")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if is_async {
        // Return TaskStatus::Submitted immediately.
        // The actual dispatch happens in a background task (wired by the caller).
        let task_id = format!("task-{}", msg.seq);
        info!(task_id = %task_id, backend = backend.name(), "async dispatch — returning Submitted");

        return Some(Message {
            seq: 0,
            ts: 0,
            from: msg.to.clone(),
            to: msg.from.clone(),
            context_id: msg.context_id.clone(),
            project: msg.project.clone(),
            content: String::new(),
            msg_type: MessageType::TaskStatus {
                task_id,
                state: TaskState::Submitted,
            },
            metadata: HashMap::new(),
        });
    }

    // Synchronous dispatch
    info!(backend = backend.name(), "routing message");

    match backend.dispatch(msg).await {
        Ok(response) => Some(response),
        Err(e) => {
            error!(backend = backend.name(), error = %e, "backend dispatch failed");
            Some(make_response(msg, format!("dispatch error: {e}")))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a response message with from/to swapped.
fn make_response(original: &Message, content: String) -> Message {
    Message {
        seq: 0, // Will be assigned by ingest
        ts: 0,  // Will be assigned by ingest
        from: original.to.clone(),
        to: original.from.clone(),
        context_id: original.context_id.clone(),
        project: original.project.clone(),
        content,
        msg_type: MessageType::AgentMsg,
        metadata: HashMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use thermal_core::message::AgentId;

    fn sample_msg(to_type: &str, content: &str) -> Message {
        Message {
            seq: 1,
            ts: 1000,
            from: AgentId::new("user", "alice"),
            to: AgentId::new(to_type, "default"),
            context_id: None,
            project: None,
            content: content.to_string(),
            msg_type: MessageType::AgentMsg,
            metadata: HashMap::new(),
        }
    }

    // -- TrustConfig parsing --

    #[test]
    fn trust_config_parse_basic() {
        let toml = r#"
[tiers]
screenshot = "AUTO"
click = "CONFIRM"
kill_claude = "BLOCK"
"#;
        let cfg = TrustConfig::parse(toml).unwrap();
        assert_eq!(cfg.tier_for("screenshot"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("click"), TrustTier::Confirm);
        assert_eq!(cfg.tier_for("kill_claude"), TrustTier::Block);
        assert_eq!(cfg.tier_for("unknown"), TrustTier::Confirm);
    }

    #[test]
    fn trust_config_parse_quoted_keys() {
        let toml = r#"
[tiers]
"beads:list" = "AUTO"
"beads:close" = "AUTO"
"#;
        let cfg = TrustConfig::parse(toml).unwrap();
        assert_eq!(cfg.tier_for("beads:list"), TrustTier::Auto);
        assert_eq!(cfg.tier_for("beads:close"), TrustTier::Auto);
    }

    #[test]
    fn trust_config_parse_empty() {
        let cfg = TrustConfig::parse("").unwrap();
        assert_eq!(cfg.tier_for("anything"), TrustTier::Confirm);
    }

    #[test]
    fn trust_config_parse_no_tiers_section() {
        let cfg = TrustConfig::parse("# just a comment").unwrap();
        assert_eq!(cfg.tier_for("anything"), TrustTier::Confirm);
    }

    #[test]
    fn trust_config_load_real_file() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/trust-tiers.toml");
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap();
            let cfg = TrustConfig::parse(&content).unwrap();
            assert_eq!(cfg.tier_for("screenshot"), TrustTier::Auto);
            assert_eq!(cfg.tier_for("click"), TrustTier::Confirm);
            assert_eq!(cfg.tier_for("kill_claude"), TrustTier::Block);
        }
    }

    // -- MCP response parsing --

    #[test]
    fn mcp_text_content_extracted() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"hello"}]}}"#;
        assert_eq!(parse_mcp_response(json).unwrap(), "hello");
    }

    #[test]
    fn mcp_error_response() {
        let json = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"fail"}}"#;
        assert_eq!(parse_mcp_response(json).unwrap(), "Error: fail");
    }

    #[test]
    fn mcp_missing_result() {
        let json = r#"{"jsonrpc":"2.0","id":1}"#;
        assert_eq!(parse_mcp_response(json).unwrap(), "no result");
    }

    #[test]
    fn mcp_multiple_text_blocks() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#;
        assert_eq!(parse_mcp_response(json).unwrap(), "a\nb");
    }

    #[test]
    fn mcp_invalid_json() {
        assert!(parse_mcp_response("not json").is_err());
    }

    // -- make_response --

    #[test]
    fn make_response_swaps_from_to() {
        let msg = sample_msg("system", "screenshot");
        let resp = make_response(&msg, "done".to_string());
        assert_eq!(resp.from.agent_type, "system");
        assert_eq!(resp.to.agent_type, "user");
        assert_eq!(resp.content, "done");
        assert_eq!(resp.seq, 0); // unassigned
    }

    #[test]
    fn make_response_preserves_context_id() {
        let mut msg = sample_msg("claude", "hello");
        msg.context_id = Some("ctx-123".to_string());
        let resp = make_response(&msg, "reply".to_string());
        assert_eq!(resp.context_id, Some("ctx-123".to_string()));
    }

    #[test]
    fn make_response_preserves_project() {
        let mut msg = sample_msg("claude", "hello");
        msg.project = Some("thermal-desktop".to_string());
        let resp = make_response(&msg, "reply".to_string());
        assert_eq!(resp.project, Some("thermal-desktop".to_string()));
    }

    // -- RouteTable --

    #[test]
    fn route_table_has_all_backends() {
        let table = RouteTable::new();
        let targets = table.registered_targets();
        assert!(targets.contains(&"system"));
        assert!(targets.contains(&"claude"));
        assert!(targets.contains(&"codex"));
        assert!(targets.contains(&"planner"));
        assert!(targets.contains(&"user"));
        assert!(targets.contains(&"dispatcher"));
    }

    #[test]
    fn route_table_has_backend_check() {
        let table = RouteTable::new();
        assert!(table.has_backend("system"));
        assert!(table.has_backend("claude"));
        assert!(!table.has_backend("nonexistent"));
    }

    // -- route_message filtering --

    #[tokio::test]
    async fn route_skips_subscribe_messages() {
        let table = RouteTable::new();
        let msg = Message {
            seq: 1,
            ts: 0,
            from: AgentId::new("user", "a"),
            to: AgentId::new("daemon", "bus"),
            context_id: None,
            project: None,
            content: String::new(),
            msg_type: MessageType::Subscribe { since_seq: None },
            metadata: HashMap::new(),
        };
        assert!(route_message(&msg, &table).await.is_none());
    }

    #[tokio::test]
    async fn route_skips_broadcast_messages() {
        let table = RouteTable::new();
        let msg = Message {
            seq: 1,
            ts: 0,
            from: AgentId::new("claude", "x"),
            to: AgentId::new("*", "*"),
            context_id: None,
            project: None,
            content: "hello all".to_string(),
            msg_type: MessageType::AgentMsg,
            metadata: HashMap::new(),
        };
        assert!(route_message(&msg, &table).await.is_none());
    }

    #[tokio::test]
    async fn route_unknown_target_returns_error_msg() {
        let table = RouteTable::new();
        let msg = sample_msg("alien", "hello");
        let resp = route_message(&msg, &table).await.unwrap();
        assert!(resp.content.contains("unknown target"));
        assert!(resp.content.contains("@alien"));
    }

    #[tokio::test]
    async fn route_user_backend_succeeds() {
        let table = RouteTable::new();
        let msg = sample_msg("user", "hello from agent");
        let resp = route_message(&msg, &table).await.unwrap();
        assert!(resp.content.contains("delivered to user"));
    }

    #[tokio::test]
    async fn route_async_returns_submitted() {
        let table = RouteTable::new();
        let mut msg = sample_msg("user", "hello");
        msg.metadata.insert("async".to_string(), Value::Bool(true));
        let resp = route_message(&msg, &table).await.unwrap();
        assert!(matches!(
            resp.msg_type,
            MessageType::TaskStatus {
                state: TaskState::Submitted,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn route_skips_ack_messages() {
        let table = RouteTable::new();
        let msg = Message {
            seq: 1,
            ts: 0,
            from: AgentId::new("user", "a"),
            to: AgentId::new("claude", "x"),
            context_id: None,
            project: None,
            content: String::new(),
            msg_type: MessageType::Ack { ref_seq: 1 },
            metadata: HashMap::new(),
        };
        assert!(route_message(&msg, &table).await.is_none());
    }

    #[tokio::test]
    async fn route_skips_task_status_messages() {
        let table = RouteTable::new();
        let msg = Message {
            seq: 1,
            ts: 0,
            from: AgentId::new("system", "x"),
            to: AgentId::new("user", "a"),
            context_id: None,
            project: None,
            content: String::new(),
            msg_type: MessageType::TaskStatus {
                task_id: "t-1".into(),
                state: TaskState::Completed,
            },
            metadata: HashMap::new(),
        };
        assert!(route_message(&msg, &table).await.is_none());
    }

    // -- SystemBackend trust tier blocking --

    #[tokio::test]
    async fn system_backend_blocks_tool() {
        let mut cfg = TrustConfig {
            tiers: HashMap::new(),
        };
        cfg.tiers
            .insert("kill_claude".to_string(), TrustTier::Block);

        let msg = sample_msg("system", r#"{"tool":"kill_claude"}"#);
        let resp = dispatch_system(&msg, &cfg).await.unwrap();
        assert!(resp.content.contains("BLOCKED"));
        assert!(resp.content.contains("kill_claude"));
    }

    #[tokio::test]
    async fn system_backend_empty_tool_name_errors() {
        let cfg = TrustConfig {
            tiers: HashMap::new(),
        };

        let msg = sample_msg("system", r#"{"tool":""}"#);
        let resp = dispatch_system(&msg, &cfg).await;
        assert!(resp.is_err());
    }

    #[test]
    fn system_backend_parses_plain_text_tool_name() {
        let msg = sample_msg("system", "screenshot");
        let content = &msg.content;
        let (tool_name, _input) = if let Ok(parsed) = serde_json::from_str::<Value>(content) {
            let tool = parsed
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input = parsed.get("input").cloned().unwrap_or_else(|| json!({}));
            (tool, input)
        } else {
            (content.trim().to_string(), json!({}))
        };
        assert_eq!(tool_name, "screenshot");
    }

    #[test]
    fn system_backend_parses_json_tool_call() {
        let msg = sample_msg("system", r#"{"tool":"click","input":{"x":100,"y":200}}"#);
        let parsed: Value = serde_json::from_str(&msg.content).unwrap();
        let tool = parsed.get("tool").and_then(|v| v.as_str()).unwrap();
        assert_eq!(tool, "click");
        let input = parsed.get("input").unwrap();
        assert_eq!(input["x"], 100);
        assert_eq!(input["y"], 200);
    }

    // -- Async dispatch task_id format --

    #[tokio::test]
    async fn async_dispatch_task_id_includes_seq() {
        let table = RouteTable::new();
        let mut msg = sample_msg("user", "hello");
        msg.seq = 42;
        msg.metadata.insert("async".to_string(), Value::Bool(true));
        let resp = route_message(&msg, &table).await.unwrap();
        if let MessageType::TaskStatus { task_id, .. } = &resp.msg_type {
            assert_eq!(task_id, "task-42");
        } else {
            panic!("expected TaskStatus");
        }
    }
}
