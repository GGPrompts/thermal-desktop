//! thermal-dispatcher: AI voice command dispatcher daemon.
//!
//! Listens on a Unix socket for transcript JSON from thermal-voice,
//! sends transcripts to an LLM (Claude CLI, Copilot CLI, or local Ollama)
//! with tool-use, classifies tools by trust tier (AUTO/CONFIRM/BLOCK),
//! executes or gates them accordingly, and sends natural language responses
//! to thermal-audio for TTS playback.
//!
//! Backend priority: Claude CLI > Copilot CLI > Ollama (offline fallback).
//! Override with `THERMAL_DISPATCHER_BACKEND=claude|copilot|ollama`.

mod api;
mod config;
mod context;
mod escalation;
mod executor;
mod learning;
mod tools;

use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use config::TrustConfig;
use context::ConversationContext;
use learning::ConfirmationHistory;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// HUD and voice state files: separate chains from agent session state.
// These are written by the dispatcher for its own UI coordination and do
// NOT flow through the conductor daemon's semantic event bus.
const HUD_STATE_FILE: &str = "/tmp/thermal-hud-state.json";
const VOICE_STATE_FILE: &str = "/tmp/thermal-voice-state.json";

// ---------------------------------------------------------------------------
// Runtime paths (XDG_RUNTIME_DIR with UID fallback)
// ---------------------------------------------------------------------------

/// Return the thermal runtime directory, respecting XDG_RUNTIME_DIR.
/// Falls back to `/run/user/<uid>/thermal` when the env var is unset.
pub fn runtime_dir() -> PathBuf {
    thermal_core::runtime::runtime_dir()
}

fn socket_path() -> PathBuf {
    thermal_core::runtime::socket_path("dispatcher")
}

pub fn audio_socket_path() -> PathBuf {
    thermal_core::runtime::socket_path("audio")
}

pub fn messages_socket_path() -> PathBuf {
    thermal_core::runtime::socket_path("messages")
}

/// Maximum number of tool-use iterations before bailing out.
/// Prevents infinite loops if the model never returns `end_turn`.
const MAX_TOOL_ITERATIONS: usize = 10;

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn pidfile_path() -> PathBuf {
    thermal_core::runtime::pidfile_path("dispatcher")
}

fn enforce_single_instance() {
    thermal_core::runtime::enforce_single_instance("thermal-dispatcher");
}

fn write_pidfile() {
    let path = pidfile_path();
    if let Err(e) = thermal_core::runtime::write_pidfile("thermal-dispatcher", &path) {
        tracing::warn!(error = %e, "Failed to write pidfile");
    }
}

#[allow(dead_code)] // Available for future graceful shutdown
fn cleanup_pidfile() {
    thermal_core::runtime::remove_pidfile("thermal-dispatcher", &pidfile_path());
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "thermal_dispatcher=info".parse().unwrap()),
        )
        .init();

    // Parse CLI flags
    let args: Vec<String> = std::env::args().collect();
    let learning_enabled = !args.iter().any(|a| a == "--no-learning");

    // Handle --show-promotions subcommand (print and exit)
    if args.iter().any(|a| a == "--show-promotions") {
        let history_path = learning::default_history_path();
        let history = ConfirmationHistory::load(&history_path)?;
        let promotions = history.pending_promotions();
        if promotions.is_empty() {
            println!("No pending promotions.");
        } else {
            println!("Pending trust tier promotions:");
            for p in &promotions {
                println!("  {p}");
            }
            println!(
                "\nTo promote a tool, add it as AUTO in your trust-tiers.toml"
            );
        }
        return Ok(());
    }

    enforce_single_instance();
    write_pidfile();

    info!("thermal-dispatcher v{} starting", env!("CARGO_PKG_VERSION"));
    info!(learning = learning_enabled, "adaptive trust learning");

    // Detect best available backend (Claude CLI > Copilot CLI > Ollama)
    let http = reqwest::Client::new();
    let backend = api::detect_backend(&http)
        .await
        .context("no LLM backend available")?;

    // Resolve model name for the selected backend
    let model = api::resolve_model(&backend);
    info!(backend = %backend, model = %model, "LLM backend selected");

    // If using Ollama, verify the model is available
    if backend == api::LlmBackend::Ollama {
        api::check_ollama_health(&http, &model)
            .await
            .context("Ollama health check failed")?;
    }

    // Load trust tier config
    let config_path = find_config_file();
    let trust_config = TrustConfig::load(&config_path)
        .with_context(|| format!("loading trust config from {}", config_path.display()))?;
    info!(
        "loaded trust config from {} ({} tool mappings)",
        config_path.display(),
        trust_config.tier_count()
    );

    // Build the slim tool schema list (~6 tools) for qwen3:8b accuracy
    let tool_schemas = tools::build_slim_tool_schemas();
    info!("registered {} slim tools for dispatch", tool_schemas.len());

    // Ensure runtime directory exists
    thermal_core::runtime::ensure_runtime_dir()
        .with_context(|| "creating thermal runtime directory")?;

    // Remove stale socket if present (checks whether a listener is alive)
    let sock_path = socket_path();
    thermal_core::runtime::cleanup_stale_socket("thermal-dispatcher", &sock_path);

    let listener = UnixListener::bind(&sock_path).context("binding Unix socket")?;
    info!("listening on {}", sock_path.display());

    // Load confirmation history for adaptive learning
    let history_path = learning::default_history_path();
    let confirmation_history = ConfirmationHistory::load(&history_path)
        .unwrap_or_else(|e| {
            warn!(error = %e, "failed to load confirmation history, starting fresh");
            ConfirmationHistory::default()
        });

    // Log any pending promotions at startup
    if learning_enabled {
        let promotions = confirmation_history.pending_promotions();
        if !promotions.is_empty() {
            info!(count = promotions.len(), "pending trust tier promotions at startup:");
            for p in &promotions {
                info!("  {p}");
            }
        }
    }

    // Shared state wrapped in Arc for concurrent access
    let shared = std::sync::Arc::new(SharedState {
        backend,
        model,
        trust_config,
        tool_schemas,
        http,
        conversation: Mutex::new(ConversationContext::new()),
        learning_enabled,
        history_path,
        confirmation_history: Mutex::new(confirmation_history),
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
    backend: api::LlmBackend,
    model: String,
    #[allow(dead_code)] // Retained for future use; agents handle their own permissions now
    trust_config: TrustConfig,
    tool_schemas: Vec<serde_json::Value>,
    /// Reusable HTTP client (connection pool shared across requests).
    http: reqwest::Client,
    /// Multi-turn conversational context (persists across dispatch calls).
    conversation: Mutex<ConversationContext>,
    /// Whether adaptive trust learning is enabled.
    #[allow(dead_code)] // Used by record_tool_approval/denial and --show-promotions
    learning_enabled: bool,
    /// Path to the confirmation history TOML file.
    #[allow(dead_code)]
    history_path: PathBuf,
    /// Confirmation history for adaptive trust tier learning.
    #[allow(dead_code)]
    confirmation_history: Mutex<ConfirmationHistory>,
}

#[allow(dead_code)] // Infrastructure for confirmation-gated tool execution
impl SharedState {
    /// Record that a user approved a tool execution. Persists to disk.
    async fn record_tool_approval(&self, tool_name: &str) {
        if !self.learning_enabled {
            return;
        }
        let mut history = self.confirmation_history.lock().await;
        history.record_approval(tool_name);
        if let Err(e) = history.save(&self.history_path) {
            warn!(error = %e, "failed to persist confirmation history");
        }
    }

    /// Record that a user denied a tool execution. Persists to disk.
    async fn record_tool_denial(&self, tool_name: &str) {
        if !self.learning_enabled {
            return;
        }
        let mut history = self.confirmation_history.lock().await;
        history.record_denial(tool_name);
        if let Err(e) = history.save(&self.history_path) {
            warn!(error = %e, "failed to persist confirmation history");
        }
    }
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

    // Publish voice transcript to message bus so TUI can show it
    executor::publish_to_bus("user", "voice", "dispatcher", &msg.transcript).await;

    // Send to LLM backend and execute the tool-use loop
    match dispatch_command(&msg.transcript, state).await {
        Ok(response_text) => {
            info!(response = %response_text, "dispatch complete");

            // Publish dispatcher response to message bus for TUI visibility
            executor::publish_to_bus("dispatcher", "voice", "user", &response_text).await;

            // Update HUD: result
            write_hud_state(&HudState::Result {
                transcript: msg.transcript.clone(),
                summary: response_text.clone(),
            })
            .await;

            // TTS is handled by the `speak` tool — the model calls it explicitly
            // when it wants to talk to the user. No automatic TTS here.

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
// Core dispatch logic — routes to CLI or Ollama backend
// ---------------------------------------------------------------------------

async fn dispatch_command(transcript: &str, state: &SharedState) -> Result<String> {
    // Classify transcript complexity (logged for observability)
    let complexity = escalation::classify_complexity(transcript);
    info!(
        complexity = ?complexity,
        backend = %state.backend,
        model = %state.model,
        "classified transcript"
    );

    // Build messages with conversational history.
    // Lock the context briefly to build the initial messages, then release.
    let messages = {
        let mut ctx = state.conversation.lock().await;
        if ctx.is_expired() {
            info!("conversation context expired, resetting");
            ctx.reset();
        }
        ctx.touch();
        ctx.build_messages(transcript)
    };

    // Route to the appropriate backend
    match &state.backend {
        api::LlmBackend::ClaudeCli | api::LlmBackend::CopilotCli => {
            dispatch_via_cli(transcript, state, messages).await
        }
        api::LlmBackend::Ollama => dispatch_via_ollama(transcript, state, messages).await,
    }
}

/// Dispatch via CLI backend (Claude CLI or Copilot CLI).
///
/// Uses structured JSON output to get tool calls, executes them, and if
/// `read()` was called, makes a follow-up call with the screen content
/// to get a summary for the user.
async fn dispatch_via_cli(
    transcript: &str,
    state: &SharedState,
    mut messages: Vec<serde_json::Value>,
) -> Result<String> {
    let mut iterations = 0usize;

    loop {
        iterations += 1;
        if iterations > MAX_TOOL_ITERATIONS {
            warn!(iterations, "CLI dispatch hit max iterations");
            let mut ctx = state.conversation.lock().await;
            let response =
                "I hit the maximum number of steps. Please try a simpler request.".to_string();
            ctx.add_turn(transcript, &response);
            return Ok(response);
        }

        let response = api::call_cli_llm(&state.backend, &state.model, &messages)
            .await
            .context("CLI LLM call failed")?;

        let stop_reason = response
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("end_turn");

        let content = response
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        if stop_reason != "tool_use" || content.is_empty() {
            // No tool calls — nothing to do (shouldn't happen with JSON schema)
            let text = extract_text_response(&content);
            let response = if text.is_empty() {
                "I didn't understand that command.".to_string()
            } else {
                text
            };
            let mut ctx = state.conversation.lock().await;
            ctx.add_turn(transcript, &response);
            return Ok(response);
        }

        // Execute all tool calls
        let mut tool_results = Vec::new();
        let mut has_read = false;
        let mut last_speak_text = String::new();

        for block in &content {
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                continue;
            }

            let tool_name = block
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let tool_input = block.get("input").cloned().unwrap_or(serde_json::json!({}));

            info!(tool = %tool_name, "CLI backend: executing tool");

            let result = executor::execute_tool(tool_name, &tool_input).await;
            let result_text = match result {
                Ok(text) => text,
                Err(e) => format!("Tool execution error: {e:#}"),
            };

            if tool_name == "read" {
                has_read = true;
            }
            if tool_name == "speak" {
                last_speak_text = tool_input
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
            }

            tool_results.push((tool_name.to_string(), result_text));
        }

        // If read() was called, we need to follow up with the screen content
        // so the model can summarize it for the user.
        if has_read {
            let screen_content = tool_results
                .iter()
                .find(|(name, _)| name == "read")
                .map(|(_, text)| text.as_str())
                .unwrap_or("");

            // Append assistant + tool result to conversation for follow-up
            messages.push(serde_json::json!({
                "role": "assistant",
                "content": format!("[called read() — screen captured]"),
            }));
            messages.push(serde_json::json!({
                "role": "user",
                "content": format!("Terminal screen content:\n{screen_content}\n\nSummarize what you see for the user via speak()."),
            }));

            // Continue the loop — the next iteration will get a speak() call
            continue;
        }

        // Determine the response text
        let response_text = if !last_speak_text.is_empty() {
            last_speak_text
        } else {
            tool_results
                .iter()
                .map(|(name, text)| format!("{name}: {text}"))
                .collect::<Vec<_>>()
                .join("; ")
        };

        let mut ctx = state.conversation.lock().await;
        ctx.add_turn(transcript, &response_text);
        return Ok(response_text);
    }
}

/// Dispatch via Ollama backend (local offline fallback).
///
/// Multi-turn tool-use loop: sends messages to Ollama, processes tool calls,
/// feeds results back until the model returns `end_turn`.
async fn dispatch_via_ollama(
    transcript: &str,
    state: &SharedState,
    mut messages: Vec<serde_json::Value>,
) -> Result<String> {
    let http = &state.http;

    let mut iterations = 0usize;
    loop {
        iterations += 1;
        if iterations > MAX_TOOL_ITERATIONS {
            warn!(
                iterations = iterations - 1,
                "hit max tool iterations, returning partial response"
            );
            let text = extract_text_response(
                &messages
                    .last()
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default(),
            );
            let response = if text.is_empty() {
                "I hit the maximum number of tool calls. Please try again with a simpler request."
                    .to_string()
            } else {
                text
            };
            let mut ctx = state.conversation.lock().await;
            ctx.add_turn(transcript, &response);
            return Ok(response);
        }

        let response = api::call_ollama(http, &state.model, &state.tool_schemas, &messages)
            .await
            .context("Ollama API call failed")?;

        // Check stop reason (normalised by call_ollama)
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

            // Fallback: qwen3 sometimes outputs tool calls as plain text
            // (e.g. `speak("hello")`) instead of structured tool_calls.
            // Parse and execute these before returning.
            if let Some(result) = try_parse_text_tool_calls(&text).await {
                let mut ctx = state.conversation.lock().await;
                ctx.add_turn(transcript, &result);
                return Ok(result);
            }

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
        // Build Ollama-native assistant message: plain string content + tool_calls array
        let text_parts: String = content
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                    b.get("text").and_then(|v| v.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" ");

        let tool_calls_ollama: Vec<serde_json::Value> = content
            .iter()
            .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .map(|b| {
                serde_json::json!({
                    "function": {
                        "name": b.get("name").and_then(|v| v.as_str()).unwrap_or("unknown"),
                        "arguments": b.get("input").cloned().unwrap_or(serde_json::json!({})),
                    }
                })
            })
            .collect();

        let mut assistant_msg = serde_json::json!({
            "role": "assistant",
            "content": text_parts,
        });
        if !tool_calls_ollama.is_empty() {
            assistant_msg["tool_calls"] = serde_json::json!(tool_calls_ollama);
        }
        messages.push(assistant_msg);

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

            info!(tool = %tool_name, id = %tool_id, "model wants to call tool");

            // All 3 dispatcher tools (speak/read/route) execute immediately.
            // Trust-tier gating is handled by the routed agents themselves.
            let result = executor::execute_tool(tool_name, &tool_input).await;

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

        // Add tool results as individual "tool" role messages for Ollama,
        // wrapped in the Anthropic-compatible format for context building
        let tool_result_msg = serde_json::json!({
            "role": "user",
            "content": tool_results,
        });

        // Convert to Ollama's tool result format (one "role: tool" message per result)
        let ollama_results = api::convert_tool_results_for_ollama(&tool_result_msg);
        messages.extend(ollama_results);
    }
}

/// Fallback parser for when qwen3 outputs tool calls as plain text instead of
/// structured tool_calls. Matches patterns like `speak("text")`, `read()`,
/// `route(to="@claude", message="...")`.
/// Returns Some(result) if any tool calls were found and executed, None otherwise.
async fn try_parse_text_tool_calls(text: &str) -> Option<String> {
    let mut results = Vec::new();

    // Parse speak("...") or speak('...')
    for arg in extract_single_arg_calls(text, "speak") {
        info!(text = %arg, "fallback: parsed speak() from text output");
        let input = serde_json::json!({"text": arg});
        if let Ok(r) = executor::execute_tool("speak", &input).await {
            results.push(r);
        }
    }

    // Parse read()
    if text.contains("read()") {
        info!("fallback: parsed read() from text output");
        let input = serde_json::json!({});
        if let Ok(r) = executor::execute_tool("read", &input).await {
            results.push(r);
        }
    }

    // Parse route(to="@agent", message="...")
    if let Some((to, message)) = extract_route_call(text) {
        info!(to = %to, message = %message, "fallback: parsed route() from text output");
        let input = serde_json::json!({"to": to, "message": message});
        if let Ok(r) = executor::execute_tool("route", &input).await {
            results.push(r);
        }
    }

    if results.is_empty() {
        None
    } else {
        Some(results.join("; "))
    }
}

/// Extract arguments from `funcname("arg")` or `funcname('arg')` patterns.
fn extract_single_arg_calls(text: &str, func: &str) -> Vec<String> {
    let mut results = Vec::new();
    let pattern = format!("{func}(");
    let mut search_from = 0;
    while let Some(start) = text[search_from..].find(&pattern) {
        let abs_start = search_from + start + pattern.len();
        if abs_start >= text.len() {
            break;
        }
        // Skip whitespace, find quote char
        let rest = text[abs_start..].trim_start();
        let quote = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => q,
            _ => {
                search_from = abs_start;
                continue;
            }
        };
        let after_quote = &rest[1..];
        if let Some(end) = after_quote.find(quote) {
            results.push(after_quote[..end].to_string());
        }
        search_from = abs_start;
    }
    results
}

/// Extract `route(to="@agent", message="...")` from text.
///
/// Tracks quote depth so parentheses inside quoted strings (e.g.
/// `message="explain foo()"`) are not mistaken for the closing paren.
fn extract_route_call(text: &str) -> Option<(String, String)> {
    let start = text.find("route(")?;
    let rest = &text[start + 6..];

    // Find the closing ')' that is outside double quotes.
    let mut in_quotes = false;
    let mut close = None;
    for (i, ch) in rest.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ')' if !in_quotes => {
                close = Some(i);
                break;
            }
            _ => {}
        }
    }
    let args = &rest[..close?];

    // Parse to="..." and message="..."
    let to = extract_kwarg(args, "to")?;
    let message = extract_kwarg(args, "message")?;
    Some((to, message))
}

/// Extract a keyword argument value: `key="value"` or `key='value'`.
fn extract_kwarg(text: &str, key: &str) -> Option<String> {
    let patterns = [
        format!("{key}=\""),
        format!("{key}='"),
        format!("{key} = \""),
        format!("{key} = '"),
    ];
    for pat in &patterns {
        if let Some(start) = text.find(pat.as_str()) {
            let quote = pat.chars().last()?;
            let after = &text[start + pat.len()..];
            if let Some(end) = after.find(quote) {
                return Some(after[..end].to_string());
            }
        }
    }
    None
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

/// Format a human-readable description of a tool call for HUD display.
#[cfg(test)]
fn format_action_description(tool_name: &str, input: &serde_json::Value) -> String {
    match tool_name {
        "speak" => {
            let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("...");
            let preview = if text.len() > 60 {
                let mut end = 60;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}...", &text[..end])
            } else {
                text.to_string()
            };
            format!("Speak: \"{preview}\"")
        }
        "read" => "Read active terminal".to_string(),
        "route" => {
            let to = input.get("to").and_then(|v| v.as_str()).unwrap_or("?");
            let message = input
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("...");
            let preview = if message.len() > 60 {
                let mut end = 60;
                while end > 0 && !message.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}...", &message[..end])
            } else {
                message.to_string()
            };
            format!("Route to {to}: \"{preview}\"")
        }
        _ => {
            let args_str = serde_json::to_string(input).unwrap_or_default();
            let preview = if args_str.len() > 80 {
                let mut end = 80;
                while end > 0 && !args_str.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}...", &args_str[..end])
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
// TTS via thermal-audio
// ---------------------------------------------------------------------------

/// Send text to thermal-audio for TTS playback.
///
/// Tries the Unix socket first; falls back to writing to the voice state file
/// so the TTS daemon can pick it up.
async fn send_tts(text: &str) {
    info!(text = %text, "sending TTS");

    // Try connecting to audio socket
    match UnixStream::connect(audio_socket_path()).await {
        Ok(stream) => {
            let msg = serde_json::json!({
                "action": "tts",
                "text": text,
            });
            let (_, mut writer) = stream.into_split();
            let payload = serde_json::to_string(&msg).unwrap_or_default() + "\n";
            if let Err(e) = writer.write_all(payload.as_bytes()).await {
                warn!("failed to write to audio socket: {e}");
                send_tts_via_state_file(text).await;
                return;
            }
            if let Err(e) = writer.flush().await {
                warn!("failed to flush audio socket: {e}");
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
    // format_action_description (speak/read/route)
    // -----------------------------------------------------------------------

    #[test]
    fn format_speak_description() {
        let input = json!({"text": "hello there"});
        let desc = format_action_description("speak", &input);
        assert!(desc.contains("hello there"));
        assert!(desc.starts_with("Speak:"));
    }

    #[test]
    fn format_speak_long_text_truncated() {
        let long_text = "a".repeat(100);
        let input = json!({"text": long_text});
        let desc = format_action_description("speak", &input);
        assert!(
            desc.contains("..."),
            "long text should be truncated with ..."
        );
    }

    #[test]
    fn format_read_description() {
        let input = json!({});
        let desc = format_action_description("read", &input);
        assert_eq!(desc, "Read active terminal");
    }

    #[test]
    fn format_route_description() {
        let input = json!({"to": "@claude", "message": "explain lifetimes"});
        let desc = format_action_description("route", &input);
        assert!(desc.contains("@claude"));
        assert!(desc.contains("explain lifetimes"));
        assert!(desc.starts_with("Route to"));
    }

    #[test]
    fn format_route_long_message_truncated() {
        let long_msg = "x".repeat(100);
        let input = json!({"to": "@planner", "message": long_msg});
        let desc = format_action_description("route", &input);
        assert!(
            desc.contains("..."),
            "long message should be truncated with ..."
        );
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

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    #[test]
    fn runtime_dir_ends_with_thermal() {
        let dir = runtime_dir();
        assert!(
            dir.to_str().unwrap().ends_with("/thermal"),
            "runtime dir should end with /thermal, got {:?}",
            dir
        );
    }

    #[test]
    fn socket_path_ends_with_sock() {
        let p = socket_path();
        assert!(p.to_str().unwrap().ends_with(".sock"));
    }

    #[test]
    fn audio_socket_path_ends_with_sock() {
        let p = audio_socket_path();
        assert!(p.to_str().unwrap().ends_with(".sock"));
    }

    #[test]
    fn messages_socket_path_ends_with_sock() {
        let p = messages_socket_path();
        assert!(p.to_str().unwrap().ends_with("messages.sock"));
    }

    #[test]
    fn hud_state_file_is_in_tmp() {
        assert!(HUD_STATE_FILE.starts_with("/tmp/"));
        assert!(HUD_STATE_FILE.ends_with(".json"));
    }
}
