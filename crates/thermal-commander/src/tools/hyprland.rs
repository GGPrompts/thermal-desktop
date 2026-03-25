//! Hyprland window management tools.
//!
//! Wraps `hyprctl` CLI commands for window/workspace control.

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

use crate::mcp::{ContentBlock, ToolResult};

/// List all windows via hyprctl clients.
pub async fn list_windows(_args: Value) -> Result<ToolResult> {
    let output = Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .await
        .context("failed to run hyprctl — is Hyprland running?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "hyprctl clients failed: {stderr}"
        )));
    }

    let json: Value =
        serde_json::from_slice(&output.stdout).context("failed to parse hyprctl clients JSON")?;

    // Extract useful fields for a cleaner summary
    let summary = if let Some(clients) = json.as_array() {
        let windows: Vec<Value> = clients
            .iter()
            .map(|c| {
                serde_json::json!({
                    "address": c.get("address").unwrap_or(&Value::Null),
                    "class": c.get("class").unwrap_or(&Value::Null),
                    "title": c.get("title").unwrap_or(&Value::Null),
                    "workspace": c.get("workspace").and_then(|w| w.get("id")).unwrap_or(&Value::Null),
                    "at": c.get("at").unwrap_or(&Value::Null),
                    "size": c.get("size").unwrap_or(&Value::Null),
                    "floating": c.get("floating").unwrap_or(&Value::Null),
                    "pid": c.get("pid").unwrap_or(&Value::Null),
                    "focused": c.get("focusHistoryID").and_then(|v| v.as_i64()).map(|v| v == 0).unwrap_or(false),
                })
            })
            .collect();
        serde_json::to_string_pretty(&windows)?
    } else {
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    Ok(ToolResult::success(vec![ContentBlock::text(summary)]))
}

/// Focus a window by class, title, or address.
///
/// Arguments:
/// - `selector` (required): window class, title substring, or address (e.g. "kitty", "Firefox", "address:0x...")
pub async fn focus_window(args: Value) -> Result<ToolResult> {
    let selector = args
        .get("selector")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: selector"))?;

    let output = Command::new("hyprctl")
        .args(["dispatch", "focuswindow", selector])
        .output()
        .await
        .context("failed to run hyprctl dispatch focuswindow")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "hyprctl dispatch focuswindow failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(ToolResult::success(vec![ContentBlock::text(format!(
        "focused window: {selector} ({stdout})"
    ))]))
}

/// Move a window to a position or workspace.
///
/// Arguments:
/// - `selector` (required): window selector (class, title, or address)
/// - `x` (optional): X position for pixel move
/// - `y` (optional): Y position for pixel move
/// - `workspace` (optional): workspace ID/name to move to
pub async fn move_window(args: Value) -> Result<ToolResult> {
    let selector = args
        .get("selector")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: selector"))?;

    // Workspace move takes priority
    if let Some(workspace) = args.get("workspace").and_then(|v| v.as_str()) {
        let output = Command::new("hyprctl")
            .args([
                "dispatch",
                "movetoworkspace",
                &format!("{workspace},address:{selector}"),
            ])
            .output()
            .await
            .context("failed to run hyprctl dispatch movetoworkspace")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Ok(ToolResult::error(format!(
                "hyprctl movetoworkspace failed: {stderr}"
            )));
        }

        return Ok(ToolResult::success(vec![ContentBlock::text(format!(
            "moved {selector} to workspace {workspace}"
        ))]));
    }

    // Pixel move
    let x = args.get("x").and_then(|v| v.as_i64());
    let y = args.get("y").and_then(|v| v.as_i64());

    match (x, y) {
        (Some(x), Some(y)) => {
            let output = Command::new("hyprctl")
                .args([
                    "dispatch",
                    "movewindowpixel",
                    &format!("{x} {y},address:{selector}"),
                ])
                .output()
                .await
                .context("failed to run hyprctl dispatch movewindowpixel")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Ok(ToolResult::error(format!(
                    "hyprctl movewindowpixel failed: {stderr}"
                )));
            }

            Ok(ToolResult::success(vec![ContentBlock::text(format!(
                "moved {selector} to ({x}, {y})"
            ))]))
        }
        _ => Ok(ToolResult::error(
            "provide either 'workspace' or both 'x' and 'y' parameters",
        )),
    }
}

/// List all workspaces.
pub async fn list_workspaces(_args: Value) -> Result<ToolResult> {
    let output = Command::new("hyprctl")
        .args(["workspaces", "-j"])
        .output()
        .await
        .context("failed to run hyprctl workspaces")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "hyprctl workspaces failed: {stderr}"
        )));
    }

    let json: Value = serde_json::from_slice(&output.stdout)
        .context("failed to parse hyprctl workspaces JSON")?;

    let pretty = serde_json::to_string_pretty(&json)?;
    Ok(ToolResult::success(vec![ContentBlock::text(pretty)]))
}

/// Get the currently active (focused) window.
pub async fn active_window(_args: Value) -> Result<ToolResult> {
    let output = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .await
        .context("failed to run hyprctl activewindow")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "hyprctl activewindow failed: {stderr}"
        )));
    }

    let json: Value = serde_json::from_slice(&output.stdout)
        .context("failed to parse hyprctl activewindow JSON")?;

    let pretty = serde_json::to_string_pretty(&json)?;
    Ok(ToolResult::success(vec![ContentBlock::text(pretty)]))
}
