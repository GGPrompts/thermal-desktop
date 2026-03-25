//! Utility tools — clipboard, notifications.

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

use crate::mcp::{ContentBlock, ToolResult};

/// Get recent clipboard history via cliphist.
pub async fn clipboard_get(_args: Value) -> Result<ToolResult> {
    let output = Command::new("cliphist")
        .arg("list")
        .output()
        .await
        .context("failed to run cliphist — is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!("cliphist list failed: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().take(20).collect();
    let text = if lines.is_empty() {
        "clipboard is empty".to_string()
    } else {
        lines.join("\n")
    };

    Ok(ToolResult::success(vec![ContentBlock::text(text)]))
}

/// Set clipboard contents via wl-copy.
///
/// Arguments:
/// - `text` (required): text to copy to clipboard
pub async fn clipboard_set(args: Value) -> Result<ToolResult> {
    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: text"))?;

    let mut child = Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("failed to run wl-copy — is it installed?")?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(text.as_bytes()).await?;
        // Drop stdin to close the pipe so wl-copy can finish
    }

    let status = child.wait().await?;

    if !status.success() {
        return Ok(ToolResult::error("wl-copy failed"));
    }

    Ok(ToolResult::success(vec![ContentBlock::text(format!(
        "copied {} characters to clipboard",
        text.len()
    ))]))
}

/// Send a desktop notification via notify-send.
///
/// Arguments:
/// - `message` (required): notification message text
/// - `urgency` (optional): "low", "normal" (default), or "critical"
pub async fn notify(args: Value) -> Result<ToolResult> {
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: message"))?;

    let urgency = args
        .get("urgency")
        .and_then(|v| v.as_str())
        .unwrap_or("normal");

    // Validate urgency level
    if !matches!(urgency, "low" | "normal" | "critical") {
        return Ok(ToolResult::error(format!(
            "invalid urgency: {urgency} (use low, normal, or critical)"
        )));
    }

    let output = Command::new("notify-send")
        .args(["-u", urgency, "-a", "thermal-commander", message])
        .output()
        .await
        .context("failed to run notify-send — is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!("notify-send failed: {stderr}")));
    }

    Ok(ToolResult::success(vec![ContentBlock::text(format!(
        "sent notification (urgency={urgency}): {message}"
    ))]))
}
