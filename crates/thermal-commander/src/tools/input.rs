//! Input tools — click, type_text, key_combo, scroll.
//!
//! Uses `ydotool` for mouse and `wtype` for keyboard on Wayland.

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

use crate::mcp::{ContentBlock, ToolResult};

/// Left/right/middle click at coordinates.
///
/// Arguments:
/// - `x` (required): X coordinate
/// - `y` (required): Y coordinate
/// - `button` (optional): "left" (default), "right", "middle"
pub async fn click(args: Value) -> Result<ToolResult> {
    let x = args
        .get("x")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: x"))?;
    let y = args
        .get("y")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: y"))?;

    let button = args
        .get("button")
        .and_then(|v| v.as_str())
        .unwrap_or("left");

    // ydotool button codes: left=0xC0, right=0xC1, middle=0xC2
    let button_code = match button {
        "left" => "0xC0",
        "right" => "0xC1",
        "middle" => "0xC2",
        other => {
            return Ok(ToolResult::error(format!(
                "unknown button: {other} (use left, right, or middle)"
            )));
        }
    };

    // Move mouse then click
    let move_output = Command::new("ydotool")
        .args([
            "mousemove",
            "--absolute",
            "-x",
            &x.to_string(),
            "-y",
            &y.to_string(),
        ])
        .output()
        .await
        .context("failed to run ydotool mousemove — is ydotool installed and ydotoold running?")?;

    if !move_output.status.success() {
        let stderr = String::from_utf8_lossy(&move_output.stderr);
        return Ok(ToolResult::error(format!(
            "ydotool mousemove failed: {stderr}"
        )));
    }

    let click_output = Command::new("ydotool")
        .args(["click", button_code])
        .output()
        .await
        .context("failed to run ydotool click")?;

    if !click_output.status.success() {
        let stderr = String::from_utf8_lossy(&click_output.stderr);
        return Ok(ToolResult::error(format!("ydotool click failed: {stderr}")));
    }

    Ok(ToolResult::success(vec![ContentBlock::text(format!(
        "clicked {button} at ({x}, {y})"
    ))]))
}

/// Type text using wtype.
///
/// Arguments:
/// - `text` (required): text to type
pub async fn type_text(args: Value) -> Result<ToolResult> {
    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: text"))?;

    let output = Command::new("wtype")
        .arg("--")
        .arg(text)
        .output()
        .await
        .context("failed to run wtype — is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!("wtype failed: {stderr}")));
    }

    Ok(ToolResult::success(vec![ContentBlock::text(format!(
        "typed {} characters",
        text.len()
    ))]))
}

/// Press a key combination.
///
/// Arguments:
/// - `combo` (required): key combo string like "ctrl+s", "alt+tab", "super+1"
pub async fn key_combo(args: Value) -> Result<ToolResult> {
    let combo = args
        .get("combo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: combo"))?;

    let parts: Vec<&str> = combo.split('+').collect();
    if parts.is_empty() {
        return Ok(ToolResult::error("empty key combo"));
    }

    let mut wtype_args: Vec<String> = Vec::new();

    // All parts except the last are modifiers
    for &modifier in &parts[..parts.len() - 1] {
        let mod_key = normalize_modifier(modifier);
        wtype_args.push("-M".into());
        wtype_args.push(mod_key);
    }

    // Last part is the key to press
    let key = parts[parts.len() - 1];
    wtype_args.push("-k".into());
    wtype_args.push(normalize_key(key));

    let output = Command::new("wtype")
        .args(&wtype_args)
        .output()
        .await
        .context("failed to run wtype — is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolResult::error(format!("wtype failed: {stderr}")));
    }

    Ok(ToolResult::success(vec![ContentBlock::text(format!(
        "pressed {combo}"
    ))]))
}

/// Scroll at a position.
///
/// Arguments:
/// - `x` (required): X coordinate
/// - `y` (required): Y coordinate
/// - `direction` (optional): "up" (default) or "down"
/// - `clicks` (optional): number of scroll clicks (default 3)
pub async fn scroll(args: Value) -> Result<ToolResult> {
    let x = args
        .get("x")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: x"))?;
    let y = args
        .get("y")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: y"))?;

    let direction = args
        .get("direction")
        .and_then(|v| v.as_str())
        .unwrap_or("up");

    let clicks = args.get("clicks").and_then(|v| v.as_i64()).unwrap_or(3);

    // Move to position first
    let move_output = Command::new("ydotool")
        .args([
            "mousemove",
            "--absolute",
            "-x",
            &x.to_string(),
            "-y",
            &y.to_string(),
        ])
        .output()
        .await
        .context("failed to run ydotool mousemove — is ydotool installed and ydotoold running?")?;

    if !move_output.status.success() {
        let stderr = String::from_utf8_lossy(&move_output.stderr);
        return Ok(ToolResult::error(format!(
            "ydotool mousemove failed: {stderr}"
        )));
    }

    // ydotool scroll: positive = up, negative = down
    let scroll_amount = match direction {
        "up" => clicks.to_string(),
        "down" => (-clicks).to_string(),
        other => {
            return Ok(ToolResult::error(format!(
                "unknown direction: {other} (use up or down)"
            )));
        }
    };

    let scroll_output = Command::new("ydotool")
        .args(["mousemove", "--wheel", "-x", "0", "-y", &scroll_amount])
        .output()
        .await
        .context("failed to run ydotool scroll")?;

    if !scroll_output.status.success() {
        let stderr = String::from_utf8_lossy(&scroll_output.stderr);
        return Ok(ToolResult::error(format!(
            "ydotool scroll failed: {stderr}"
        )));
    }

    Ok(ToolResult::success(vec![ContentBlock::text(format!(
        "scrolled {direction} {clicks} clicks at ({x}, {y})"
    ))]))
}

/// Normalize modifier names to wtype-compatible key names.
fn normalize_modifier(m: &str) -> String {
    match m.to_lowercase().as_str() {
        "ctrl" | "control" => "ctrl".into(),
        "alt" => "alt".into(),
        "shift" => "shift".into(),
        "super" | "win" | "mod4" | "logo" => "super".into(),
        other => other.into(),
    }
}

/// Normalize key names to XKB key names for wtype.
fn normalize_key(k: &str) -> String {
    match k.to_lowercase().as_str() {
        "enter" | "return" => "Return".into(),
        "tab" => "Tab".into(),
        "escape" | "esc" => "Escape".into(),
        "space" => "space".into(),
        "backspace" => "BackSpace".into(),
        "delete" | "del" => "Delete".into(),
        "up" => "Up".into(),
        "down" => "Down".into(),
        "left" => "Left".into(),
        "right" => "Right".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        "pageup" | "pgup" => "Prior".into(),
        "pagedown" | "pgdn" => "Next".into(),
        "f1" => "F1".into(),
        "f2" => "F2".into(),
        "f3" => "F3".into(),
        "f4" => "F4".into(),
        "f5" => "F5".into(),
        "f6" => "F6".into(),
        "f7" => "F7".into(),
        "f8" => "F8".into(),
        "f9" => "F9".into(),
        "f10" => "F10".into(),
        "f11" => "F11".into(),
        "f12" => "F12".into(),
        // Single chars and other keys pass through as-is
        other => other.into(),
    }
}
