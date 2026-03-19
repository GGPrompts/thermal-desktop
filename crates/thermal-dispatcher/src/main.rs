//! thermal-dispatcher: AI voice command dispatcher daemon.
//!
//! Listens on a Unix socket for transcript JSON from thermal-voice,
//! sends transcripts to Claude Haiku via the Anthropic API with tool-use,
//! classifies tools by trust tier (AUTO/CONFIRM/BLOCK), executes or gates
//! them accordingly, and sends natural language responses to thermal-audio
//! for TTS playback.

mod api;
mod config;
mod executor;
mod tools;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};

use config::TrustConfig;

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

    info!(
        "thermal-dispatcher v{} starting",
        env!("CARGO_PKG_VERSION")
    );

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
}

// ---------------------------------------------------------------------------
// Client handler
// ---------------------------------------------------------------------------

/// Incoming transcript message from thermal-voice.
#[derive(serde::Deserialize, Debug)]
struct TranscriptMessage {
    transcript: String,
    #[serde(default)]
    confidence: f64,
}

/// Response sent back to thermal-voice.
#[derive(serde::Serialize)]
struct DispatcherResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
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

    // Initial conversation: user transcript
    let mut messages = vec![serde_json::json!({
        "role": "user",
        "content": transcript,
    })];

    // Loop: send to Haiku, handle tool calls, feed results back
    loop {
        let response = api::call_haiku(
            &http,
            &state.api_key,
            &state.tool_schemas,
            &messages,
        )
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
            return Ok(text);
        }

        if stop_reason != "tool_use" {
            // Unexpected stop reason — return whatever text we have
            let text = extract_text_response(&content);
            return Ok(if text.is_empty() {
                format!("Unexpected response (stop_reason={stop_reason})")
            } else {
                text
            });
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
            let tool_input = block
                .get("input")
                .cloned()
                .unwrap_or(serde_json::json!({}));

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
                        ConfirmResult::Timeout => {
                            Ok(format!(
                                "Confirmation timed out for {tool_name} — action skipped"
                            ))
                        }
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
            let text = input
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("...");
            let preview = if text.len() > 50 {
                format!("{}...", &text[..50])
            } else {
                text.to_string()
            };
            format!("Type: \"{preview}\"")
        }
        "key_combo" => {
            let combo = input
                .get("combo")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
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
            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
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
