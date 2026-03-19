//! Screenshot tool — captures the screen via `grim`.

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;
use tokio::process::Command;

use crate::mcp::{ContentBlock, ToolResult};

/// Take a screenshot, optionally of a specific region.
///
/// Arguments:
/// - `region` (optional): geometry string "X,Y WxH" for partial capture
pub async fn screenshot(args: Value) -> Result<ToolResult> {
    let path = "/tmp/thermal-commander-screenshot.png";

    let mut cmd = Command::new("grim");

    if let Some(region) = args.get("region").and_then(|v| v.as_str()) {
        cmd.arg("-g").arg(region);
    }

    cmd.arg(path);

    let output = cmd
        .output()
        .await
        .context("failed to run grim — is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!("grim failed: {stderr}")));
    }

    let png_bytes = tokio::fs::read(path)
        .await
        .context("failed to read screenshot file")?;

    let b64 = BASE64.encode(&png_bytes);

    // Clean up
    let _ = tokio::fs::remove_file(path).await;

    Ok(ToolResult::success(vec![ContentBlock::image(
        b64,
        "image/png",
    )]))
}
