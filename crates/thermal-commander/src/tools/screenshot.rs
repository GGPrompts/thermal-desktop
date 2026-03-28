//! Capture pane tool — captures terminal content via `kitty @ get-text`.

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

use crate::mcp::{ContentBlock, ToolResult};

/// Capture terminal pane content via kitty remote control.
///
/// Arguments:
/// - `window_id` (optional): kitty window ID to capture a specific pane
pub async fn capture_pane(args: Value) -> Result<ToolResult> {
    let mut cmd = Command::new("kitty");
    cmd.arg("@")
        .arg("get-text")
        .arg("--extent=screen")
        .arg("--ansi");

    if let Some(window_id) = args.get("window_id").and_then(|v| v.as_str()) {
        cmd.arg("--match").arg(format!("id:{window_id}"));
    }

    let output = cmd
        .output()
        .await
        .context("failed to run kitty @ get-text — is kitty running with remote control enabled?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "kitty @ get-text failed: {stderr}"
        )));
    }

    let text = String::from_utf8_lossy(&output.stdout).into_owned();

    if text.is_empty() {
        return Ok(ToolResult::error(
            "kitty @ get-text returned empty output — is the pane visible?",
        ));
    }

    Ok(ToolResult::success(vec![ContentBlock::text(text)]))
}
