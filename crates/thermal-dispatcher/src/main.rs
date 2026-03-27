//! thermal-dispatcher: AI voice command dispatcher daemon.
//!
//! Listens on a Unix socket for transcript JSON from thermal-voice,
//! sends transcripts to Claude Haiku via the Anthropic API with tool-use,
//! classifies tools by trust tier (AUTO/CONFIRM/BLOCK), executes or gates
//! them accordingly, and sends natural language responses to thermal-audio
//! for TTS playback.

mod api;
mod config;
mod context;
mod executor;
mod tools;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use config::TrustConfig;
use context::ConversationContext;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SOCKET_DIR: &str = "/run/user/1000/thermal";
const SOCKET_PATH: &str = "/run/user/1000/thermal/dispatcher.sock";
const AUDIO_SOCKET_PATH: &str = "/run/user/1000/thermal/audio.sock";
const HUD_STATE_FILE: &str = "/tmp/thermal-hud-state.json";
const VOICE_STATE_FILE: &str = "/tmp/thermal-voice-state.json";

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "thermal_dispatcher=info".parse().unwrap()),
        )
        .init();

    info!("thermal-dispatcher v{} starting", env!("CARGO_PKG_VERSION"));

    // Load API key
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable not set")?;

    // Load trust tier config
    let config_path = find_config_file();
    let trust_config = TrustConfig::load(&config_path)
        .with_context(|| format!("loading trust config from {}", config_path.display()))?;
    info!(
        "loaded trust config from {} ({} tool mappings)",
        config_path.display(),
        trust_config.tier_count()
    );

    // Build the tool schema list for the Haiku system prompt
    let tool_schemas = tools::build_tool_schemas();
    info!("registered {} tools for Haiku", tool_schemas.len());

    // Ensure socket directory exists
    tokio::fs::create_dir_all(SOCKET_DIR)
        .await
        .with_context(|| format!("creating socket dir {SOCKET_DIR}"))?;

    // Remove stale socket
    let socket_path = Path::new(SOCKET_PATH);
    if socket_path.exists() {
        tokio::fs::remove_file(socket_path)
            .await
            .context("removing stale socket")?;
    }

    let listener = UnixListener::bind(socket_path).context("binding Unix socket")?;
    info!("listening on {SOCKET_PATH}");

    // Shared state wrapped in Arc for concurrent access
    let shared = std::sync::Arc::new(SharedState {
        api_key,
        trust_config,
        tool_schemas,
        conversation: Mutex::new(ConversationContext::new()),
    });

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, &state).await {
                        error!("client handler error: {e:#}");
                    }
                });
            }
            Err(e) => {
                error!("accept error: {e}");
            }
        }
    }
}

/// State shared across client handler tasks.
struct SharedState {
    api_key: String,
    trust_config: TrustConfig,
    tool_schemas: Vec<serde_json::Value>,
    /// Multi-turn conversational context (persists across dispatch calls).
    conversation: Mutex<ConversationContext>,
}

// ---------------------------------------------------------------------------
// Client handler
// ---------------------------------------------------------------------------

/// Incoming transcript message from thermal-voice.
#[derive(serde::Deserialize, Debug)]
pub struct TranscriptMessage {
    pub transcript: String,
    #[serde(default)]
    pub confidence: f64,
}

/// Response sent back to thermal-voice.
#[derive(serde::Serialize)]
pub struct DispatcherResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

async fn handle_client(stream: UnixStream, state: &SharedState) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    let bytes = buf_reader.read_line(&mut line).await?;
    if bytes == 0 {
        return Ok(());
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    info!(raw = %trimmed, "received transcript message");

    let msg: TranscriptMessage = match serde_json::from_str(trimmed) {
        Ok(m) => m,
        Err(e) => {
            let resp = DispatcherResponse {
                status: "error".into(),
                response: None,
                error: Some(format!("invalid JSON: {e}")),
            };
            let out = serde_json::to_string(&resp)? + "\n";
            writer.write_all(out.as_bytes()).await?;
            return Ok(());
        }
    };

    if msg.transcript.is_empty() {
        let resp = DispatcherResponse {
            status: "empty".into(),
            response: None,
            error: Some("empty transcript".into()),
        };
        let out = serde_json::to_string(&resp)? + "\n";
        writer.write_all(out.as_bytes()).await?;
        return Ok(());
    }

    info!(
        transcript = %msg.transcript,
        confidence = msg.confidence,
        "processing voice command"
    );

    // Update HUD: thinking state
    write_hud_state(&HudState::Thinking {
        transcript: msg.transcript.clone(),
    })
    .await;

    // Send to Haiku and execute the tool-use loop
    match dispatch_command(&msg.transcript, state).await {
        Ok(response_text) => {
            info!(response = %response_text, "dispatch complete");

            // Update HUD: result
            write_hud_state(&HudState::Result {
                transcript: msg.transcript.clone(),
                summary: response_text.clone(),
            })
            .await;

            // Send TTS
            send_tts(&response_text).await;

            let resp = DispatcherResponse {
                status: "ok".into(),
                response: Some(response_text),
                error: None,
            };
            let out = serde_json::to_string(&resp)? + "\n";
            writer.write_all(out.as_bytes()).await?;
        }
        Err(e) => {
            error!(error = %e, "dispatch failed");

            write_hud_state(&HudState::Error {
                transcript: msg.transcript.clone(),
                error: format!("{e:#}"),
            })
            .await;

            send_tts("Sorry, something went wrong processing that command.").await;

            let resp = DispatcherResponse {
                status: "error".into(),
                response: None,
                error: Some(format!("{e:#}")),
            };
            let out = serde_json::to_string(&resp)? + "\n";
            writer.write_all(out.as_bytes()).await?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Core dispatch logic — Haiku tool-use loop
// ---------------------------------------------------------------------------

async fn dispatch_command(transcript: &str, state: &SharedState) -> Result<String> {
    let http = reqwest::Client::new();

    // Build messages with conversational history.
    // Lock the context briefly to build the initial messages, then release.
    let mut messages = {
        let mut ctx = state.conversation.lock().await;
        if ctx.is_expired() {
            info!("conversation context expired, resetting");
            ctx.reset();
        }
        ctx.touch();
        ctx.build_messages(transcript)
    };

    // Loop: send to Haiku, handle tool calls, feed results back
    loop {
        let response = api::call_haiku(&http, &state.api_key, &state.tool_schemas, &messages)
            .await
            .context("Haiku API call failed")?;

        // Check stop reason
        let stop_reason = response
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("end_turn");

        let content = response
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        if stop_reason == "end_turn" || stop_reason == "max_tokens" {
            // Extract final text response
            let text = extract_text_response(&content);
            // Record the completed turn in conversational context
            let mut ctx = state.conversation.lock().await;
            ctx.add_turn(transcript, &text);
            return Ok(text);
        }

        if stop_reason != "tool_use" {
            // Unexpected stop reason — return whatever text we have
            let text = extract_text_response(&content);
            let response = if text.is_empty() {
                format!("Unexpected response (stop_reason={stop_reason})")
            } else {
                text
            };
            // Record the completed turn in conversational context
            let mut ctx = state.conversation.lock().await;
            ctx.add_turn(transcript, &response);
            return Ok(response);
        }

        // Process tool calls
        // First, add the assistant's response to messages
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": content,
        }));

        // Collect tool results
        let mut tool_results = Vec::new();

        for block in &content {
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                continue;
            }

            let tool_id = block
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let tool_name = block
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let tool_input = block.get("input").cloned().unwrap_or(serde_json::json!({}));

            info!(tool = %tool_name, id = %tool_id, "Haiku wants to call tool");

            // Classify by trust tier
            let tier = state.trust_config.tier_for(tool_name);
            info!(tool = %tool_name, tier = ?tier, "trust classification");

            let result = match tier {
                config::TrustTier::Auto => {
                    // Execute immediately
                    executor::execute_tool(tool_name, &tool_input).await
                }
                config::TrustTier::Confirm => {
                    // Write action plan to HUD, wait for confirmation
                    let description = format_action_description(tool_name, &tool_input);

                    write_hud_state(&HudState::Confirming {
                        transcript: String::new(), // Will be filled by HUD from context
                        action: description.clone(),
                        tool_name: tool_name.to_string(),
                    })
                    .await;

                    match wait_for_confirmation(tool_name, &description).await {
                        ConfirmResult::Approved => {
                            write_hud_state(&HudState::Executing {
                                action: description,
                            })
                            .await;
                            executor::execute_tool(tool_name, &tool_input).await
                        }
                        ConfirmResult::Denied => {
                            Ok(format!("User denied execution of {tool_name}"))
                        }
                        ConfirmResult::Timeout => Ok(format!(
                            "Confirmation timed out for {tool_name} — action skipped"
                        )),
                    }
                }
                config::TrustTier::Block => {
                    // Reject and announce
                    let msg = format!("Tool {tool_name} is blocked by security policy");
                    warn!("{msg}");
                    send_tts(&format!("Blocked: {tool_name} is not allowed")).await;
                    Ok(msg)
                }
            };

            let result_text = match result {
                Ok(text) => text,
                Err(e) => format!("Tool execution error: {e:#}"),
            };

            tool_results.push(serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_id,
                "content": result_text,
            }));
        }

        // Add tool results as a user message
        messages.push(serde_json::json!({
            "role": "user",
            "content": tool_results,
        }));
    }
}

/// Extract concatenated text from an array of content blocks.
fn extract_text_response(content: &[serde_json::Value]) -> String {
    content
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                block.get("text").and_then(|v| v.as_str()).map(String::from)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format a human-readable description of a tool call for confirmation UI.
fn format_action_description(tool_name: &str, input: &serde_json::Value) -> String {
    match tool_name {
        "click" => {
            let x = input.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
            let y = input.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
            let btn = input
                .get("button")
                .and_then(|v| v.as_str())
                .unwrap_or("left");
            format!("Click {btn} at ({x}, {y})")
        }
        "type_text" => {
            let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("...");
            let preview = if text.len() > 50 {
                format!("{}...", &text[..50])
            } else {
                text.to_string()
            };
            format!("Type: \"{preview}\"")
        }
        "key_combo" => {
            let combo = input.get("combo").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Press {combo}")
        }
        "focus_window" => {
            let sel = input
                .get("selector")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("Focus window: {sel}")
        }
        "open_app" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Launch: {cmd}")
        }
        "open_browser" => {
            let url = input
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("new window");
            format!("Open browser: {url}")
        }
        "spawn_claude" => {
            let count = input.get("count").and_then(|v| v.as_i64()).unwrap_or(1);
            let project = input
                .get("project")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            format!("Spawn {count} Claude session(s) in {project}")
        }
        "kill_claude" => {
            let id = input
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("Kill Claude session {id}")
        }
        _ => {
            // Generic: show tool name + compact args
            let args_str = serde_json::to_string(input).unwrap_or_default();
            let preview = if args_str.len() > 80 {
                format!("{}...", &args_str[..80])
            } else {
                args_str
            };
            format!("{tool_name}({preview})")
        }
    }
}

// ---------------------------------------------------------------------------
// HUD state management
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
#[serde(tag = "state")]
enum HudState {
    #[serde(rename = "thinking")]
    Thinking { transcript: String },
    #[serde(rename = "confirming")]
    Confirming {
        transcript: String,
        action: String,
        tool_name: String,
    },
    #[serde(rename = "executing")]
    Executing { action: String },
    #[serde(rename = "result")]
    Result { transcript: String, summary: String },
    #[serde(rename = "error")]
    Error { transcript: String, error: String },
}

async fn write_hud_state(state: &HudState) {
    let json = match serde_json::to_string_pretty(state) {
        Ok(j) => j,
        Err(e) => {
            warn!("failed to serialize HUD state: {e}");
            return;
        }
    };

    let tmp = format!("{HUD_STATE_FILE}.tmp");
    if let Err(e) = tokio::fs::write(&tmp, json.as_bytes()).await {
        warn!("failed to write HUD state tmp: {e}");
        return;
    }
    if let Err(e) = tokio::fs::rename(&tmp, HUD_STATE_FILE).await {
        warn!("failed to rename HUD state: {e}");
    }
}

// ---------------------------------------------------------------------------
// Confirmation flow
// ---------------------------------------------------------------------------

enum ConfirmResult {
    Approved,
    Denied,
    Timeout,
}

/// Wait for user confirmation via the HUD state file.
///
/// The HUD writes `{"confirmed": true}` or `{"confirmed": false}` to
/// `/tmp/thermal-hud-confirm.json` when the user responds.
/// We poll this file with a timeout.
async fn wait_for_confirmation(tool_name: &str, description: &str) -> ConfirmResult {
    const CONFIRM_FILE: &str = "/tmp/thermal-hud-confirm.json";
    const TIMEOUT_SECS: u64 = 30;

    // Clear any stale confirmation
    let _ = tokio::fs::remove_file(CONFIRM_FILE).await;

    info!(
        tool = %tool_name,
        description = %description,
        "waiting for user confirmation ({}s timeout)",
        TIMEOUT_SECS
    );

    send_tts(&format!("Confirm: {description}?")).await;

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(TIMEOUT_SECS);

    loop {
        if tokio::time::Instant::now() >= deadline {
            info!(tool = %tool_name, "confirmation timed out");
            return ConfirmResult::Timeout;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;

        let data = match tokio::fs::read_to_string(CONFIRM_FILE).await {
            Ok(d) => d,
            Err(_) => continue,
        };

        #[derive(serde::Deserialize)]
        struct Confirm {
            confirmed: bool,
        }

        if let Ok(c) = serde_json::from_str::<Confirm>(&data) {
            // Clean up
            let _ = tokio::fs::remove_file(CONFIRM_FILE).await;

            if c.confirmed {
                info!(tool = %tool_name, "user confirmed");
                return ConfirmResult::Approved;
            } else {
                info!(tool = %tool_name, "user denied");
                return ConfirmResult::Denied;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TTS via thermal-audio
// ---------------------------------------------------------------------------

/// Send text to thermal-audio for TTS playback.
///
/// Tries the Unix socket first; falls back to writing to the voice state file
/// so the TTS daemon can pick it up.
async fn send_tts(text: &str) {
    info!(text = %text, "sending TTS");

    // Try connecting to audio socket
    match UnixStream::connect(AUDIO_SOCKET_PATH).await {
        Ok(stream) => {
            let msg = serde_json::json!({
                "action": "speak",
                "text": text,
            });
            let (_, mut writer) = stream.into_split();
            let payload = serde_json::to_string(&msg).unwrap_or_default() + "\n";
            if let Err(e) = writer.write_all(payload.as_bytes()).await {
                warn!("failed to write to audio socket: {e}");
                // Fall back to voice state file
                send_tts_via_state_file(text).await;
            }
        }
        Err(_) => {
            // Audio socket not available — update voice state file with result
            // so the existing thermal-audio polling can pick it up
            send_tts_via_state_file(text).await;
        }
    }
}

/// Fallback: write a result to the voice state file for thermal-audio.
async fn send_tts_via_state_file(text: &str) {
    let state = serde_json::json!({
        "listening": false,
        "last_transcript": "",
        "result": text,
    });
    let json = serde_json::to_string_pretty(&state).unwrap_or_default();
    let tmp = format!("{VOICE_STATE_FILE}.tmp");
    if let Err(e) = tokio::fs::write(&tmp, json.as_bytes()).await {
        warn!("failed to write voice state: {e}");
        return;
    }
    if let Err(e) = tokio::fs::rename(&tmp, VOICE_STATE_FILE).await {
        warn!("failed to rename voice state: {e}");
    }
}

// ---------------------------------------------------------------------------
// Config file discovery
// ---------------------------------------------------------------------------

fn find_config_file() -> PathBuf {
    // Check XDG_CONFIG_HOME first
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let path = PathBuf::from(xdg).join("thermal/trust-tiers.toml");
        if path.exists() {
            return path;
        }
    }

    // Check ~/.config/thermal/
    if let Ok(home) = std::env::var("HOME") {
        let path = PathBuf::from(home).join(".config/thermal/trust-tiers.toml");
        if path.exists() {
            return path;
        }
    }

    // Check repo config/ directory (development)
    let repo_config = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("config/trust-tiers.toml"))
        .unwrap_or_default();
    if repo_config.exists() {
        return repo_config;
    }

    // Default path (will trigger helpful error message)
    PathBuf::from("config/trust-tiers.toml")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Socket message parsing: TranscriptMessage (thermal-voice → dispatcher)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_transcript_message_with_confidence() {
        let json = r#"{"transcript": "open firefox", "confidence": 0.95}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).expect("parse failed");
        assert_eq!(msg.transcript, "open firefox");
        assert!((msg.confidence - 0.95).abs() < 1e-9);
    }

    #[test]
    fn parse_transcript_message_without_confidence_defaults_to_zero() {
        let json = r#"{"transcript": "take a screenshot"}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).expect("parse failed");
        assert_eq!(msg.transcript, "take a screenshot");
        assert_eq!(msg.confidence, 0.0);
    }

    #[test]
    fn parse_transcript_message_empty_transcript() {
        let json = r#"{"transcript": ""}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).expect("parse failed");
        assert!(msg.transcript.is_empty());
    }

    #[test]
    fn parse_transcript_message_missing_transcript_field_errors() {
        let json = r#"{"confidence": 0.9}"#;
        let result: Result<TranscriptMessage, _> = serde_json::from_str(json);
        assert!(result.is_err(), "missing 'transcript' should fail");
    }

    #[test]
    fn parse_transcript_message_invalid_json_errors() {
        let result: Result<TranscriptMessage, _> = serde_json::from_str("not json");
        assert!(result.is_err());
    }

    #[test]
    fn parse_transcript_message_with_unicode() {
        let json = r#"{"transcript": "schreib eine Datei", "confidence": 0.8}"#;
        let msg: TranscriptMessage = serde_json::from_str(json).expect("parse failed");
        assert_eq!(msg.transcript, "schreib eine Datei");
    }

    // -----------------------------------------------------------------------
    // DispatcherResponse serialisation
    // -----------------------------------------------------------------------

    #[test]
    fn dispatcher_response_ok_serialises() {
        let resp = DispatcherResponse {
            status: "ok".into(),
            response: Some("Done!".into()),
            error: None,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(json.get("status").and_then(|v| v.as_str()), Some("ok"));
        assert_eq!(json.get("response").and_then(|v| v.as_str()), Some("Done!"));
        assert!(
            json.get("error").is_none(),
            "error should be omitted when None"
        );
    }

    #[test]
    fn dispatcher_response_error_serialises() {
        let resp = DispatcherResponse {
            status: "error".into(),
            response: None,
            error: Some("something broke".into()),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(json.get("status").and_then(|v| v.as_str()), Some("error"));
        assert!(
            json.get("response").is_none(),
            "response should be omitted when None"
        );
        assert_eq!(
            json.get("error").and_then(|v| v.as_str()),
            Some("something broke")
        );
    }

    #[test]
    fn dispatcher_response_empty_status_serialises() {
        let resp = DispatcherResponse {
            status: "empty".into(),
            response: None,
            error: Some("empty transcript".into()),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(json.get("status").and_then(|v| v.as_str()), Some("empty"));
    }

    // -----------------------------------------------------------------------
    // format_action_description
    // -----------------------------------------------------------------------

    #[test]
    fn format_click_description() {
        let input = json!({"x": 100, "y": 200, "button": "right"});
        let desc = format_action_description("click", &input);
        assert!(desc.contains("right"), "should mention button");
        assert!(desc.contains("100"), "should contain x");
        assert!(desc.contains("200"), "should contain y");
    }

    #[test]
    fn format_click_defaults_to_left_button() {
        let input = json!({"x": 50, "y": 75});
        let desc = format_action_description("click", &input);
        assert!(desc.contains("left"));
    }

    #[test]
    fn format_type_text_description_short() {
        let input = json!({"text": "hello"});
        let desc = format_action_description("type_text", &input);
        assert!(desc.contains("hello"));
        assert!(desc.starts_with("Type:"));
    }

    #[test]
    fn format_type_text_description_long_truncated() {
        let long_text = "a".repeat(100);
        let input = json!({"text": long_text});
        let desc = format_action_description("type_text", &input);
        assert!(
            desc.contains("..."),
            "long text should be truncated with ..."
        );
        // The preview is at most 50 chars + "..."
        assert!(
            desc.len() < 100,
            "description should be shorter than full text"
        );
    }

    #[test]
    fn format_key_combo_description() {
        let input = json!({"combo": "ctrl+s"});
        let desc = format_action_description("key_combo", &input);
        assert!(desc.contains("ctrl+s"));
        assert!(desc.starts_with("Press"));
    }

    #[test]
    fn format_focus_window_description() {
        let input = json!({"selector": "firefox"});
        let desc = format_action_description("focus_window", &input);
        assert!(desc.contains("firefox"));
        assert!(desc.to_lowercase().contains("focus"));
    }

    #[test]
    fn format_open_app_description() {
        let input = json!({"command": "gimp"});
        let desc = format_action_description("open_app", &input);
        assert!(desc.contains("gimp"));
        assert!(desc.to_lowercase().contains("launch"));
    }

    #[test]
    fn format_open_browser_with_url() {
        let input = json!({"url": "https://example.com"});
        let desc = format_action_description("open_browser", &input);
        assert!(desc.contains("https://example.com"));
    }

    #[test]
    fn format_open_browser_without_url() {
        let input = json!({});
        let desc = format_action_description("open_browser", &input);
        assert!(desc.contains("new window"));
    }

    #[test]
    fn format_spawn_claude_description() {
        let input = json!({"count": 3, "project": "thermal-desktop"});
        let desc = format_action_description("spawn_claude", &input);
        assert!(desc.contains("3"));
        assert!(desc.contains("thermal-desktop"));
    }

    #[test]
    fn format_spawn_claude_defaults() {
        let input = json!({});
        let desc = format_action_description("spawn_claude", &input);
        assert!(desc.contains("1"), "default count should be 1");
        assert!(desc.contains("default"));
    }

    #[test]
    fn format_kill_claude_description() {
        let input = json!({"session_id": "sess-abc123"});
        let desc = format_action_description("kill_claude", &input);
        assert!(desc.contains("sess-abc123"));
        assert!(desc.to_lowercase().contains("kill"));
    }

    #[test]
    fn format_unknown_tool_generic_description() {
        let input = json!({"foo": "bar"});
        let desc = format_action_description("some_unknown_tool", &input);
        assert!(desc.starts_with("some_unknown_tool("));
        assert!(desc.contains("foo"));
    }

    #[test]
    fn format_unknown_tool_long_args_truncated() {
        let big_val: String = "x".repeat(200);
        let input = json!({"key": big_val});
        let desc = format_action_description("some_tool", &input);
        assert!(
            desc.contains("..."),
            "long args should be truncated with ..."
        );
    }

    // -----------------------------------------------------------------------
    // extract_text_response
    // -----------------------------------------------------------------------

    #[test]
    fn extract_text_from_single_block() {
        let content = vec![json!({"type": "text", "text": "Hello!"})];
        let result = extract_text_response(&content);
        assert_eq!(result, "Hello!");
    }

    #[test]
    fn extract_text_from_multiple_blocks() {
        let content = vec![
            json!({"type": "text", "text": "first"}),
            json!({"type": "text", "text": "second"}),
        ];
        let result = extract_text_response(&content);
        assert_eq!(result, "first second");
    }

    #[test]
    fn extract_text_ignores_tool_use_blocks() {
        let content = vec![
            json!({"type": "tool_use", "name": "screenshot", "id": "t1", "input": {}}),
            json!({"type": "text", "text": "done"}),
        ];
        let result = extract_text_response(&content);
        assert_eq!(result, "done");
    }

    #[test]
    fn extract_text_empty_content_returns_empty_string() {
        let result = extract_text_response(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn extract_text_only_non_text_blocks_returns_empty() {
        let content =
            vec![json!({"type": "tool_use", "name": "screenshot", "id": "t2", "input": {}})];
        let result = extract_text_response(&content);
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // HUD state serialisation
    // -----------------------------------------------------------------------

    #[test]
    fn hud_state_thinking_serialises_correctly() {
        let state = HudState::Thinking {
            transcript: "open the browser".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(json.get("state").and_then(|v| v.as_str()), Some("thinking"));
        assert_eq!(
            json.get("transcript").and_then(|v| v.as_str()),
            Some("open the browser")
        );
    }

    #[test]
    fn hud_state_result_serialises_correctly() {
        let state = HudState::Result {
            transcript: "show windows".into(),
            summary: "Found 5 windows".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(json.get("state").and_then(|v| v.as_str()), Some("result"));
        assert_eq!(
            json.get("summary").and_then(|v| v.as_str()),
            Some("Found 5 windows")
        );
    }

    #[test]
    fn hud_state_error_serialises_correctly() {
        let state = HudState::Error {
            transcript: "do thing".into(),
            error: "timeout".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(json.get("state").and_then(|v| v.as_str()), Some("error"));
        assert_eq!(json.get("error").and_then(|v| v.as_str()), Some("timeout"));
    }

    #[test]
    fn hud_state_confirming_serialises_correctly() {
        let state = HudState::Confirming {
            transcript: String::new(),
            action: "Launch gimp".into(),
            tool_name: "open_app".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(
            json.get("state").and_then(|v| v.as_str()),
            Some("confirming")
        );
        assert_eq!(
            json.get("tool_name").and_then(|v| v.as_str()),
            Some("open_app")
        );
    }

    #[test]
    fn hud_state_executing_serialises_correctly() {
        let state = HudState::Executing {
            action: "Clicking at (100, 200)".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(
            json.get("state").and_then(|v| v.as_str()),
            Some("executing")
        );
    }

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    #[test]
    fn socket_path_is_in_run_user() {
        assert!(SOCKET_PATH.starts_with("/run/user/"));
        assert!(SOCKET_PATH.ends_with(".sock"));
    }

    #[test]
    fn audio_socket_path_is_in_run_user() {
        assert!(AUDIO_SOCKET_PATH.starts_with("/run/user/"));
        assert!(AUDIO_SOCKET_PATH.ends_with(".sock"));
    }

    #[test]
    fn hud_state_file_is_in_tmp() {
        assert!(HUD_STATE_FILE.starts_with("/tmp/"));
        assert!(HUD_STATE_FILE.ends_with(".json"));
    }
}
