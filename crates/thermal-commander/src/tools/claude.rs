//! Claude orchestration tools via `thermal-conductor` CLI.

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

use crate::mcp::{ContentBlock, ToolResult};

/// Spawn one or more Claude sessions via thermal-conductor.
///
/// Arguments:
/// - `count` (optional): number of sessions to spawn (default: 1)
/// - `project` (optional): project directory to associate with the sessions
pub async fn spawn_claude(args: Value) -> Result<ToolResult> {
    let mut cmd_args = vec!["spawn".to_string()];

    if let Some(count) = args.get("count").and_then(|v| v.as_i64()) {
        cmd_args.push("-n".to_string());
        cmd_args.push(count.to_string());
    }

    if let Some(project) = args.get("project").and_then(|v| v.as_str()) {
        cmd_args.push("-p".to_string());
        cmd_args.push(project.to_string());
    }

    let output = Command::new("thermal-conductor")
        .args(&cmd_args)
        .output()
        .await
        .context("failed to run thermal-conductor spawn — is thermal-conductor installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "thermal-conductor spawn failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(ToolResult::success(vec![ContentBlock::text(
        stdout.trim().to_string(),
    )]))
}

/// Get status of all Claude sessions.
pub async fn claude_status(_args: Value) -> Result<ToolResult> {
    let output = Command::new("thermal-conductor")
        .arg("status")
        .output()
        .await
        .context("failed to run thermal-conductor status — is thermal-conductor installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "thermal-conductor status failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(ToolResult::success(vec![ContentBlock::text(
        stdout.trim().to_string(),
    )]))
}

/// Kill a Claude session by ID.
///
/// Arguments:
/// - `session_id` (required): session ID to kill
pub async fn kill_claude(args: Value) -> Result<ToolResult> {
    let session_id = args
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: session_id"))?;

    let output = Command::new("thermal-conductor")
        .args(["kill", session_id])
        .output()
        .await
        .context("failed to run thermal-conductor kill — is thermal-conductor installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "thermal-conductor kill failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(ToolResult::success(vec![ContentBlock::text(
        if stdout.trim().is_empty() {
            format!("killed session {session_id}")
        } else {
            stdout.trim().to_string()
        },
    )]))
}
