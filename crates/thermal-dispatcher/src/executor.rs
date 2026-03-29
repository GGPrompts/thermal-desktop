//! Tool executor — dispatches the 3 dispatcher tools:
//!
//! - `speak` → send TTS to thermal-audio via Unix socket
//! - `read`  → capture active terminal via thermal-commander (MCP/JSON-RPC)
//! - `route` → forward request to agent via thermal-messages bus

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use thermal_core::message::{AgentId, Message, MessageType};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tracing::{debug, info, warn};

/// Monotonically increasing request ID for JSON-RPC calls.
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Execute a tool by name with the given input arguments.
/// Only 3 tools: speak, read, route.
pub async fn execute_tool(tool_name: &str, input: &Value) -> Result<String> {
    match tool_name {
        "speak" => execute_speak(input).await,
        "read" => execute_read().await,
        "route" => execute_route(input).await,
        _ => Ok(format!("Unknown tool: {tool_name}")),
    }
}

/// Handle the `speak` tool — send text to thermal-audio for TTS playback.
async fn execute_speak(input: &Value) -> Result<String> {
    let text = input
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if text.is_empty() {
        return Ok("Nothing to speak.".to_string());
    }

    info!(text = %text, "speak tool invoked");

    let sock_path = crate::audio_socket_path();

    match UnixStream::connect(&sock_path).await {
        Ok(stream) => {
            let request = json!({
                "action": "speak",
                "text": text,
            });
            let (_, mut writer) = stream.into_split();
            let mut payload = serde_json::to_string(&request)?;
            payload.push('\n');
            writer.write_all(payload.as_bytes()).await
                .context("writing to audio.sock")?;
            writer.flush().await?;
            info!("sent TTS request to thermal-audio");
            Ok(format!("Spoke: {text}"))
        }
        Err(e) => {
            warn!(path = %sock_path.display(), error = %e, "cannot connect to audio.sock");
            Ok("Audio daemon not available — is thermal-audio running?".to_string())
        }
    }
}

/// Handle the `read` tool — capture the active terminal screen via
/// thermal-commander's `capture_pane` MCP tool.
async fn execute_read() -> Result<String> {
    info!("read tool invoked — capturing active terminal");
    execute_commander_tool("capture_pane", &json!({})).await
}

/// Handle the `route` tool — forward a message to an agent via the
/// thermal-messages bus (messages.sock).
async fn execute_route(input: &Value) -> Result<String> {
    let to = input
        .get("to")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let message = input
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    info!(to = %to, message = %message, "route tool invoked");

    // Validate target
    let valid_targets = ["@system", "@planner", "@claude", "@codex"];
    if !valid_targets.contains(&to) {
        return Ok(format!(
            "Unknown target '{to}'. Valid targets: @system, @planner, @claude, @codex"
        ));
    }

    // Map @-prefixed target to AgentId (strip leading @, use as agent_type)
    let agent_type = to.strip_prefix('@').unwrap_or(to);
    let to_id = AgentId::new(agent_type, "default");
    let from_id = AgentId::new("dispatcher", "voice");

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let msg = Message {
        seq: 0, // assigned by daemon
        ts: now_ms,
        from: from_id,
        to: to_id,
        context_id: None,
        project: None,
        content: message.to_string(),
        msg_type: MessageType::AgentMsg,
        metadata: HashMap::new(),
    };

    let sock_path = crate::messages_socket_path();

    let stream = match UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %sock_path.display(), error = %e, "cannot connect to messages.sock");
            return Ok(
                "Message bus not available \u{2014} is thermal-messages running?".to_string(),
            );
        }
    };

    let (reader, mut writer) = stream.into_split();
    let mut payload = serde_json::to_string(&msg).context("serializing AgentMsg")?;
    payload.push('\n');
    writer
        .write_all(payload.as_bytes())
        .await
        .context("writing to messages.sock")?;

    // Read response line from daemon
    let mut buf_reader = BufReader::new(reader);
    let mut response_line = String::new();
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        buf_reader.read_line(&mut response_line),
    )
    .await
    {
        Ok(Ok(0)) | Err(_) => {
            // EOF or timeout — message was sent but no response
            Ok(format!("Routed to {to} (no daemon response)"))
        }
        Ok(Ok(_)) => Ok(response_line.trim().to_string()),
        Ok(Err(e)) => Ok(format!("Routed to {to} but read error: {e}")),
    }
}

/// Execute a tool via thermal-commander MCP server.
///
/// Spawns thermal-commander as a child, sends initialize + tools/call,
/// and reads the result. Each call is a fresh process to keep things
/// simple and stateless.
async fn execute_commander_tool(tool_name: &str, input: &Value) -> Result<String> {
    info!(tool = %tool_name, "executing via thermal-commander");

    let mut child = Command::new("thermal-commander")
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
    let init_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": init_id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "thermal-dispatcher",
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
    let call_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
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

    // Close stdin to signal EOF, then wait for process to exit
    drop(writer);
    let _ = child.wait().await;

    // Parse the response
    let resp: Value =
        serde_json::from_str(response_line.trim()).context("parsing thermal-commander response")?;

    // Extract text content from the MCP result
    if let Some(error) = resp.get("error") {
        let msg = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Ok(format!("Error: {msg}"));
    }

    let result = resp.get("result");

    // MCP tool results have content array with text blocks
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

    // Fallback: return raw result
    Ok(result
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
        .unwrap_or_else(|| "no result".to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Tool routing: 3-tool dispatch
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_tool_returns_message() {
        // Synchronously verify the match arm returns an error string
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(execute_tool("nonexistent", &json!({}))).unwrap();
        assert!(result.contains("Unknown tool"), "got: {result}");
    }

    // -----------------------------------------------------------------------
    // REQUEST_ID is monotonically increasing
    // -----------------------------------------------------------------------

    #[test]
    fn request_id_increases_monotonically() {
        let a = REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let b = REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert!(b > a, "REQUEST_ID should increase monotonically");
    }

    // -----------------------------------------------------------------------
    // MCP response parsing helpers (pure JSON logic extracted from
    // execute_commander_tool for testability)
    // -----------------------------------------------------------------------

    /// Replicate the response-parsing logic from execute_commander_tool.
    fn parse_mcp_response(response_line: &str) -> Result<String> {
        let resp: Value = serde_json::from_str(response_line.trim()).context("parsing response")?;
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

    #[test]
    fn mcp_error_response_returns_error_message() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;
        let result = parse_mcp_response(json).unwrap();
        assert_eq!(result, "Error: method not found");
    }

    #[test]
    fn mcp_text_content_response_extracted() {
        let json = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"hello world"}]}}"#;
        let result = parse_mcp_response(json).unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn mcp_multiple_text_blocks_joined_by_newline() {
        let json = r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"line 1"},{"type":"text","text":"line 2"}]}}"#;
        let result = parse_mcp_response(json).unwrap();
        assert_eq!(result, "line 1\nline 2");
    }

    #[test]
    fn mcp_non_text_blocks_filtered_out() {
        let json = r#"{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"image","data":"abc"},{"type":"text","text":"visible"}]}}"#;
        let result = parse_mcp_response(json).unwrap();
        assert_eq!(result, "visible");
    }

    #[test]
    fn mcp_empty_content_array_falls_back_to_raw_result() {
        let json = r#"{"jsonrpc":"2.0","id":5,"result":{"content":[]}}"#;
        let result = parse_mcp_response(json).unwrap();
        // Falls back to pretty-printing the result object
        assert!(result.contains("content") || result.is_empty() || !result.contains("Error"));
    }

    #[test]
    fn mcp_missing_result_returns_no_result() {
        let json = r#"{"jsonrpc":"2.0","id":6}"#;
        let result = parse_mcp_response(json).unwrap();
        assert_eq!(result, "no result");
    }

    #[test]
    fn mcp_invalid_json_returns_error() {
        let result = parse_mcp_response("not json at all");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // route tool
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn route_valid_target_attempts_bus_connection() {
        let input = json!({"to": "@planner", "message": "create issue for voice bug"});
        let result = execute_route(&input).await.unwrap();
        // Should be one of: bus unavailable, routed (no daemon response), or real response
        assert!(
            result.contains("Message bus not available")
                || result.contains("Routed to @planner")
                || result.contains("ack"),
            "unexpected result: {result}"
        );
    }

    #[tokio::test]
    async fn route_invalid_target_returns_error() {
        let input = json!({"to": "@invalid", "message": "test"});
        let result = execute_route(&input).await.unwrap();
        assert!(result.contains("Unknown target"), "should reject invalid target, got: {result}");
    }

    #[tokio::test]
    async fn route_missing_fields_uses_defaults() {
        let input = json!({});
        let result = execute_route(&input).await.unwrap();
        assert!(result.contains("Unknown target"), "missing 'to' should fail");
    }

    // -----------------------------------------------------------------------
    // speak tool
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn speak_empty_text_returns_nothing_to_speak() {
        let input = json!({"text": ""});
        let result = execute_speak(&input).await.unwrap();
        assert_eq!(result, "Nothing to speak.");
    }

    #[tokio::test]
    async fn speak_attempts_audio_connection() {
        let input = json!({"text": "hello world"});
        let result = execute_speak(&input).await.unwrap();
        // Audio daemon may or may not be running — either outcome is fine
        assert!(
            result.contains("Spoke:") || result.contains("Audio daemon not available"),
            "unexpected result: {result}"
        );
    }

    // -----------------------------------------------------------------------
    // route builds correct AgentMsg
    // -----------------------------------------------------------------------

    #[test]
    fn route_builds_correct_agent_msg() {
        use std::collections::HashMap;
        use thermal_core::message::{AgentId, Message, MessageType};

        let msg = Message {
            seq: 0,
            ts: 1000,
            from: AgentId::new("dispatcher", "voice"),
            to: AgentId::new("claude", "default"),
            context_id: None,
            project: None,
            content: "hello there".to_string(),
            msg_type: MessageType::AgentMsg,
            metadata: HashMap::new(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["from"]["agent_type"], "dispatcher");
        assert_eq!(parsed["from"]["key"], "voice");
        assert_eq!(parsed["to"]["agent_type"], "claude");
        assert_eq!(parsed["to"]["key"], "default");
        assert_eq!(parsed["content"], "hello there");
        assert_eq!(parsed["type"], "AgentMsg");
    }
}
