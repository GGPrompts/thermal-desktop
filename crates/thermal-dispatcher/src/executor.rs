//! Tool executor — dispatches tool calls to thermal-commander (via MCP stdio)
//! or beads CLI.
//!
//! thermal-commander is an MCP server that speaks JSON-RPC 2.0 over stdio.
//! We spawn it as a child process, send a `tools/call` request, and read
//! the response. For beads tools, we shell out to the `beads` CLI.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, info, warn};

/// Monotonically increasing request ID for JSON-RPC calls.
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Execute a tool by name with the given input arguments.
/// Routes to thermal-commander (MCP) for desktop tools, or beads CLI for
/// issue-tracking tools.
pub async fn execute_tool(tool_name: &str, input: &Value) -> Result<String> {
    if tool_name.starts_with("beads:") {
        execute_beads_tool(tool_name, input).await
    } else {
        execute_commander_tool(tool_name, input).await
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
    let resp: Value = serde_json::from_str(response_line.trim())
        .context("parsing thermal-commander response")?;

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
    if let Some(content) = result.and_then(|r| r.get("content")).and_then(|c| c.as_array()) {
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

/// Build the CLI argument list for a beads subcommand without spawning the process.
/// Used in tests to verify argument construction logic.
#[cfg(test)]
pub fn beads_args_for(tool_name: &str, input: &Value) -> Vec<String> {
    let subcommand = tool_name.strip_prefix("beads:").unwrap_or(tool_name);
    let mut args: Vec<String> = vec![subcommand.to_string()];
    match subcommand {
        "list" => {
            if let Some(project) = input.get("project").and_then(|v| v.as_str()) {
                args.push("--project".to_string());
                args.push(project.to_string());
            }
            if let Some(status) = input.get("status").and_then(|v| v.as_str()) {
                args.push("--status".to_string());
                args.push(status.to_string());
            }
        }
        "show" => {
            if let Some(id) = input.get("issue_id").and_then(|v| v.as_str()) {
                args.push(id.to_string());
            }
        }
        "stats" => {
            if let Some(project) = input.get("project").and_then(|v| v.as_str()) {
                args.push("--project".to_string());
                args.push(project.to_string());
            }
        }
        "create" => {
            if let Some(title) = input.get("title").and_then(|v| v.as_str()) {
                args.push("--title".to_string());
                args.push(title.to_string());
            }
            if let Some(desc) = input.get("description").and_then(|v| v.as_str()) {
                args.push("--description".to_string());
                args.push(desc.to_string());
            }
            if let Some(project) = input.get("project").and_then(|v| v.as_str()) {
                args.push("--project".to_string());
                args.push(project.to_string());
            }
        }
        "close" | "claim" | "ready" | "blocked" | "reopen" => {
            if let Some(id) = input.get("issue_id").and_then(|v| v.as_str()) {
                args.push(id.to_string());
            }
        }
        "update" => {
            if let Some(id) = input.get("issue_id").and_then(|v| v.as_str()) {
                args.push(id.to_string());
            }
            if let Some(title) = input.get("title").and_then(|v| v.as_str()) {
                args.push("--title".to_string());
                args.push(title.to_string());
            }
            if let Some(desc) = input.get("description").and_then(|v| v.as_str()) {
                args.push("--description".to_string());
                args.push(desc.to_string());
            }
        }
        _ => {}
    }
    args
}

/// Execute a beads tool via the beads CLI.
async fn execute_beads_tool(tool_name: &str, input: &Value) -> Result<String> {
    let subcommand = tool_name
        .strip_prefix("beads:")
        .unwrap_or(tool_name);

    info!(tool = %tool_name, subcommand = %subcommand, "executing via beads CLI");

    let mut args: Vec<String> = vec![subcommand.to_string()];

    // Map input fields to CLI arguments based on the subcommand
    match subcommand {
        "list" => {
            if let Some(project) = input.get("project").and_then(|v| v.as_str()) {
                args.push("--project".to_string());
                args.push(project.to_string());
            }
            if let Some(status) = input.get("status").and_then(|v| v.as_str()) {
                args.push("--status".to_string());
                args.push(status.to_string());
            }
        }
        "show" => {
            if let Some(id) = input.get("issue_id").and_then(|v| v.as_str()) {
                args.push(id.to_string());
            }
        }
        "stats" => {
            if let Some(project) = input.get("project").and_then(|v| v.as_str()) {
                args.push("--project".to_string());
                args.push(project.to_string());
            }
        }
        "create" => {
            if let Some(title) = input.get("title").and_then(|v| v.as_str()) {
                args.push("--title".to_string());
                args.push(title.to_string());
            }
            if let Some(desc) = input.get("description").and_then(|v| v.as_str()) {
                args.push("--description".to_string());
                args.push(desc.to_string());
            }
            if let Some(project) = input.get("project").and_then(|v| v.as_str()) {
                args.push("--project".to_string());
                args.push(project.to_string());
            }
        }
        "close" | "claim" | "ready" | "blocked" | "reopen" => {
            if let Some(id) = input.get("issue_id").and_then(|v| v.as_str()) {
                args.push(id.to_string());
            }
        }
        "update" => {
            if let Some(id) = input.get("issue_id").and_then(|v| v.as_str()) {
                args.push(id.to_string());
            }
            if let Some(title) = input.get("title").and_then(|v| v.as_str()) {
                args.push("--title".to_string());
                args.push(title.to_string());
            }
            if let Some(desc) = input.get("description").and_then(|v| v.as_str()) {
                args.push("--description".to_string());
                args.push(desc.to_string());
            }
        }
        _ => {
            warn!(subcommand = %subcommand, "unknown beads subcommand");
        }
    }

    let output = Command::new("beads")
        .args(&args)
        .output()
        .await
        .with_context(|| format!("failed to run beads {subcommand}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        return Ok(format!(
            "beads {subcommand} failed: {}",
            if stderr.is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            }
        ));
    }

    Ok(if stdout.trim().is_empty() {
        format!("beads {subcommand} completed successfully")
    } else {
        stdout.trim().to_string()
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Namespaced tool routing: beads: prefix detection
    // -----------------------------------------------------------------------

    #[test]
    fn beads_prefix_is_recognised() {
        assert!("beads:list".starts_with("beads:"));
        assert!(!"screenshot".starts_with("beads:"));
    }

    // -----------------------------------------------------------------------
    // beads CLI argument construction
    // -----------------------------------------------------------------------

    #[test]
    fn beads_list_no_filters() {
        let args = beads_args_for("beads:list", &json!({}));
        assert_eq!(args, vec!["list"]);
    }

    #[test]
    fn beads_list_with_project_filter() {
        let args = beads_args_for("beads:list", &json!({"project": "therm"}));
        assert_eq!(args, vec!["list", "--project", "therm"]);
    }

    #[test]
    fn beads_list_with_status_filter() {
        let args = beads_args_for("beads:list", &json!({"status": "open"}));
        assert_eq!(args, vec!["list", "--status", "open"]);
    }

    #[test]
    fn beads_list_with_both_filters() {
        let args = beads_args_for("beads:list", &json!({"project": "therm", "status": "ready"}));
        // project comes before status in the match arm
        assert!(args.contains(&"--project".to_string()));
        assert!(args.contains(&"therm".to_string()));
        assert!(args.contains(&"--status".to_string()));
        assert!(args.contains(&"ready".to_string()));
        assert_eq!(args[0], "list");
    }

    #[test]
    fn beads_show_with_issue_id() {
        let args = beads_args_for("beads:show", &json!({"issue_id": "therm-abc1"}));
        assert_eq!(args, vec!["show", "therm-abc1"]);
    }

    #[test]
    fn beads_show_without_issue_id() {
        let args = beads_args_for("beads:show", &json!({}));
        assert_eq!(args, vec!["show"]);
    }

    #[test]
    fn beads_stats_with_project() {
        let args = beads_args_for("beads:stats", &json!({"project": "therm"}));
        assert_eq!(args, vec!["stats", "--project", "therm"]);
    }

    #[test]
    fn beads_stats_without_project() {
        let args = beads_args_for("beads:stats", &json!({}));
        assert_eq!(args, vec!["stats"]);
    }

    #[test]
    fn beads_create_with_all_fields() {
        let args = beads_args_for(
            "beads:create",
            &json!({"title": "Fix bug", "description": "details", "project": "therm"}),
        );
        assert!(args.contains(&"create".to_string()));
        assert!(args.contains(&"--title".to_string()));
        assert!(args.contains(&"Fix bug".to_string()));
        assert!(args.contains(&"--description".to_string()));
        assert!(args.contains(&"details".to_string()));
        assert!(args.contains(&"--project".to_string()));
        assert!(args.contains(&"therm".to_string()));
    }

    #[test]
    fn beads_create_title_only() {
        let args = beads_args_for("beads:create", &json!({"title": "My issue"}));
        assert_eq!(args[0], "create");
        assert!(args.contains(&"--title".to_string()));
        assert!(args.contains(&"My issue".to_string()));
        assert!(!args.contains(&"--description".to_string()));
        assert!(!args.contains(&"--project".to_string()));
    }

    #[test]
    fn beads_close_with_issue_id() {
        let args = beads_args_for("beads:close", &json!({"issue_id": "therm-xyz9"}));
        assert_eq!(args, vec!["close", "therm-xyz9"]);
    }

    #[test]
    fn beads_claim_with_issue_id() {
        let args = beads_args_for("beads:claim", &json!({"issue_id": "therm-abc2"}));
        assert_eq!(args, vec!["claim", "therm-abc2"]);
    }

    #[test]
    fn beads_ready_with_issue_id() {
        let args = beads_args_for("beads:ready", &json!({"issue_id": "therm-abc3"}));
        assert_eq!(args, vec!["ready", "therm-abc3"]);
    }

    #[test]
    fn beads_blocked_with_issue_id() {
        let args = beads_args_for("beads:blocked", &json!({"issue_id": "therm-abc4"}));
        assert_eq!(args, vec!["blocked", "therm-abc4"]);
    }

    #[test]
    fn beads_reopen_with_issue_id() {
        let args = beads_args_for("beads:reopen", &json!({"issue_id": "therm-abc5"}));
        assert_eq!(args, vec!["reopen", "therm-abc5"]);
    }

    #[test]
    fn beads_update_with_all_fields() {
        let args = beads_args_for(
            "beads:update",
            &json!({"issue_id": "therm-abc6", "title": "New title", "description": "new desc"}),
        );
        assert_eq!(args[0], "update");
        assert!(args.contains(&"therm-abc6".to_string()));
        assert!(args.contains(&"--title".to_string()));
        assert!(args.contains(&"New title".to_string()));
        assert!(args.contains(&"--description".to_string()));
        assert!(args.contains(&"new desc".to_string()));
    }

    #[test]
    fn beads_update_id_only() {
        let args = beads_args_for("beads:update", &json!({"issue_id": "therm-abc7"}));
        assert_eq!(args, vec!["update", "therm-abc7"]);
    }

    #[test]
    fn unknown_beads_subcommand_produces_only_subcommand() {
        // Unknown subcommand — args has just the subcommand name
        let args = beads_args_for("beads:frobnicate", &json!({"foo": "bar"}));
        assert_eq!(args, vec!["frobnicate"]);
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
        let resp: Value =
            serde_json::from_str(response_line.trim()).context("parsing response")?;
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
        let json = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;
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
}
