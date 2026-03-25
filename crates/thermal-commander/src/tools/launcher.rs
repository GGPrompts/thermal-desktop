//! App launching tools via Hyprland's `hyprctl dispatch exec`.

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

use crate::mcp::{ContentBlock, ToolResult};

fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Launch an arbitrary application, optionally on a specific workspace.
///
/// Arguments:
/// - `command` (required): command to execute
/// - `workspace` (optional): workspace to move the app to after launching
pub async fn open_app(args: Value) -> Result<ToolResult> {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: command"))?;

    let output = Command::new("hyprctl")
        .args(["dispatch", "exec", command])
        .output()
        .await
        .context("failed to run hyprctl dispatch exec")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "hyprctl dispatch exec failed: {stderr}"
        )));
    }

    // Optionally move to a workspace
    if let Some(workspace) = args.get("workspace").and_then(|v| v.as_str()) {
        // Small delay to let the window appear before moving it
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let move_output = Command::new("hyprctl")
            .args(["dispatch", "movetoworkspace", workspace])
            .output()
            .await
            .context("failed to run hyprctl dispatch movetoworkspace")?;

        if !move_output.status.success() {
            let stderr = String::from_utf8_lossy(&move_output.stderr);
            return Ok(ToolResult::error(format!(
                "launched {command} but failed to move to workspace {workspace}: {stderr}"
            )));
        }

        return Ok(ToolResult::success(vec![ContentBlock::text(format!(
            "launched {command} on workspace {workspace}"
        ))]));
    }

    Ok(ToolResult::success(vec![ContentBlock::text(format!(
        "launched {command}"
    ))]))
}

/// Open Firefox browser, optionally with a URL.
///
/// Arguments:
/// - `url` (optional): URL to open
pub async fn open_browser(args: Value) -> Result<ToolResult> {
    let mut exec_cmd = "firefox".to_string();

    if let Some(url) = args.get("url").and_then(|v| v.as_str()) {
        exec_cmd = format!("firefox -- {}", shell_quote(url));
    }

    let output = Command::new("hyprctl")
        .args(["dispatch", "exec", &exec_cmd])
        .output()
        .await
        .context("failed to run hyprctl dispatch exec firefox")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "hyprctl dispatch exec firefox failed: {stderr}"
        )));
    }

    let msg = if let Some(url) = args.get("url").and_then(|v| v.as_str()) {
        format!("opened firefox with URL: {url}")
    } else {
        "opened firefox".to_string()
    };

    Ok(ToolResult::success(vec![ContentBlock::text(msg)]))
}

/// Open Thunar file manager, optionally at a specific path.
///
/// Arguments:
/// - `path` (optional): directory or file path to open
pub async fn open_files(args: Value) -> Result<ToolResult> {
    let mut exec_cmd = "thunar".to_string();

    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        exec_cmd = format!("thunar {}", shell_quote(path));
    }

    let output = Command::new("hyprctl")
        .args(["dispatch", "exec", &exec_cmd])
        .output()
        .await
        .context("failed to run hyprctl dispatch exec thunar")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "hyprctl dispatch exec thunar failed: {stderr}"
        )));
    }

    let msg = if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        format!("opened thunar at: {path}")
    } else {
        "opened thunar".to_string()
    };

    Ok(ToolResult::success(vec![ContentBlock::text(msg)]))
}

/// Open kitty terminal, optionally with a working directory.
///
/// Arguments:
/// - `cwd` (optional): working directory for the terminal
pub async fn open_terminal(args: Value) -> Result<ToolResult> {
    let mut exec_cmd = "kitty".to_string();

    if let Some(cwd) = args.get("cwd").and_then(|v| v.as_str()) {
        exec_cmd = format!("kitty --directory {}", shell_quote(cwd));
    }

    let output = Command::new("hyprctl")
        .args(["dispatch", "exec", &exec_cmd])
        .output()
        .await
        .context("failed to run hyprctl dispatch exec kitty")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!(
            "hyprctl dispatch exec kitty failed: {stderr}"
        )));
    }

    let msg = if let Some(cwd) = args.get("cwd").and_then(|v| v.as_str()) {
        format!("opened kitty terminal at: {cwd}")
    } else {
        "opened kitty terminal".to_string()
    };

    Ok(ToolResult::success(vec![ContentBlock::text(msg)]))
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn shell_quote_wraps_plain_text() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn shell_quote_preserves_shell_metacharacters_as_data() {
        assert_eq!(shell_quote("x; rm -rf /"), "'x; rm -rf /'");
    }
}
