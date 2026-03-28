//! Agent routing — dispatches messages to the appropriate backend based on
//! the `to` field's agent_type.
//!
//! Route table: maps agent types ("system", "claude", "codex", "planner", "user")
//! to backend implementations. Each backend knows how to dispatch a message and
//! return a response.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
}

impl Backend {
    fn name(&self) -> &str {
        match self {
            Backend::System(_) => "system",
            Backend::Claude => "claude",
            Backend::Codex => "codex",
            Backend::Planner => "planner",
            Backend::User => "user",
        }
    }

    async fn dispatch(&self, msg: &Message) -> Result<Message> {
        match self {
            Backend::System(trust_config) => dispatch_system(msg, trust_config).await,
            Backend::Claude => dispatch_claude(msg).await,
            Backend::Codex => dispatch_codex(msg).await,
            Backend::Planner => dispatch_planner(msg).await,
            Backend::User => dispatch_user(msg).await,
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
        let input = parsed
            .get("input")
            .cloned()
            .unwrap_or_else(|| json!({}));
        (tool, input)
    } else {
        (msg.content.trim().to_string(), json!({}))
    };

    if tool_name.is_empty() {
        bail!("@system message must specify a tool name");
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
            // For now, log a warning and proceed. Full confirmation flow
            // requires HUD integration (future work).
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
                "name": "thermal-messages",
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
    let resp: Value = serde_json::from_str(response_line.trim())
        .context("parsing thermal-commander response")?;

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
// ClaudeBackend — spawns `claude -p` with the message content
// ---------------------------------------------------------------------------

async fn dispatch_claude(msg: &Message) -> Result<Message> {
    info!(content_len = msg.content.len(), "dispatching to claude");

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
        .arg("--output-format")
        .arg("json")
        .arg("--system")
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

    // The message itself will be broadcast to subscribers by the daemon's
    // normal ingest path. The UserBackend just returns an ack.
    Ok(make_response(msg, "delivered to user".to_string()))
}

/// Send a TTS request to thermal-audio via its Unix socket.
async fn send_tts(text: &str) -> Result<()> {
    let uid = nix::unistd::getuid().as_raw();
    let sock_path = format!("/run/user/{uid}/thermal/audio.sock");

    let stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .with_context(|| format!("connecting to thermal-audio at {sock_path}"))?;

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
// Route dispatcher — called by the daemon after ingesting a message
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
        // The actual dispatch happens in a background task (wired by the daemon).
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
        seq: 0, // Will be assigned by daemon ingest
        ts: 0,  // Will be assigned by daemon ingest
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

    // ── TrustConfig parsing ──────────────────────────────────────────────────

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
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/trust-tiers.toml");
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap();
            let cfg = TrustConfig::parse(&content).unwrap();
            assert_eq!(cfg.tier_for("screenshot"), TrustTier::Auto);
            assert_eq!(cfg.tier_for("click"), TrustTier::Confirm);
            assert_eq!(cfg.tier_for("kill_claude"), TrustTier::Block);
        }
    }

    // ── MCP response parsing ─────────────────────────────────────────────────

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

    // ── make_response ────────────────────────────────────────────────────────

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

    // ── RouteTable ───────────────────────────────────────────────────────────

    #[test]
    fn route_table_has_all_backends() {
        let table = RouteTable::new();
        let targets = table.registered_targets();
        assert!(targets.contains(&"system"));
        assert!(targets.contains(&"claude"));
        assert!(targets.contains(&"codex"));
        assert!(targets.contains(&"planner"));
        assert!(targets.contains(&"user"));
    }

    #[test]
    fn route_table_has_backend_check() {
        let table = RouteTable::new();
        assert!(table.has_backend("system"));
        assert!(table.has_backend("claude"));
        assert!(!table.has_backend("nonexistent"));
    }

    // ── route_message filtering ──────────────────────────────────────────────

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
        msg.metadata
            .insert("async".to_string(), Value::Bool(true));
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

    // ── SystemBackend trust tier blocking ────────────────────────────────────

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
        // When content is not valid JSON, it's treated as a tool name
        let msg = sample_msg("system", "screenshot");
        let content = &msg.content;
        // Replicate the parsing logic from dispatch_system:
        let (tool_name, _input) = if let Ok(parsed) = serde_json::from_str::<Value>(content) {
            let tool = parsed
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input = parsed
                .get("input")
                .cloned()
                .unwrap_or_else(|| json!({}));
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

    // ── Async dispatch task_id format ────────────────────────────────────────

    #[tokio::test]
    async fn async_dispatch_task_id_includes_seq() {
        let table = RouteTable::new();
        let mut msg = sample_msg("user", "hello");
        msg.seq = 42;
        msg.metadata
            .insert("async".to_string(), Value::Bool(true));
        let resp = route_message(&msg, &table).await.unwrap();
        if let MessageType::TaskStatus { task_id, .. } = &resp.msg_type {
            assert_eq!(task_id, "task-42");
        } else {
            panic!("expected TaskStatus");
        }
    }
}
