//! LLM backend for voice command dispatch.
//!
//! Supports three backends in priority order:
//! 1. **Claude CLI** (`claude -p`) — structured output via `--json-schema`
//! 2. **Copilot CLI** (`gh copilot -p`) — structured output via `--output-format json`
//! 3. **Ollama** (local HTTP API) — offline fallback via qwen3:8b

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{debug, info, warn};

const OLLAMA_BASE_URL: &str = "http://localhost:11434";
const DEFAULT_OLLAMA_MODEL: &str = "qwen3:8b";
const DEFAULT_CLAUDE_MODEL: &str = "sonnet";
const DEFAULT_COPILOT_MODEL: &str = "gpt-4.1";

/// LLM backend selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmBackend {
    /// Claude CLI (`claude -p`) with structured JSON output.
    ClaudeCli,
    /// GitHub Copilot CLI (`gh copilot -p`) with JSON output.
    CopilotCli,
    /// Local Ollama HTTP API (offline fallback).
    Ollama,
}

impl std::fmt::Display for LlmBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmBackend::ClaudeCli => write!(f, "claude-cli"),
            LlmBackend::CopilotCli => write!(f, "copilot-cli"),
            LlmBackend::Ollama => write!(f, "ollama"),
        }
    }
}

/// System prompt that gives the model its role as a voice assistant dispatcher.
///
/// Used by all backends. The `/no_think` suffix is only relevant for Qwen3
/// but harmless for other models.
const SYSTEM_PROMPT: &str = r#"You hear voice transcripts from a Linux desktop user. You have 3 tools:

- speak(text) — reply to the user via TTS
- read() — capture the active terminal screen, returns text
- route(to, message) — forward a request to an agent: @system, @planner, @claude, @codex

If the user is talking to you, use speak. To see the terminal, use read. For everything else, route to the right agent then speak a short confirmation.

Routing guide:
- @system — desktop control, apps, windows, screenshots, notifications, spawning sessions
- @planner — issues, tasks, planning
- @claude — coding questions, explanations
- @codex — coding tasks, implementation

Examples:
User: "hey what's on screen" → read(), then speak a summary
User: "open firefox" → route(to="@system", message="open firefox"), speak("Routed to system.")
User: "create an issue for the voice bug" → route(to="@planner", message="create issue for the voice pipeline bug"), speak("Sent to the planner.")
User: "ask claude about lifetimes" → route(to="@claude", message="explain rust lifetimes"), speak("Forwarded to Claude.")
User: "good morning" → speak("Good morning!")

Plain English only, no markdown. Keep speak text under 2 sentences. /no_think"#;

/// JSON schema for structured tool-call output from CLI backends.
/// Forces the model to return a `tool_calls` array with speak/read/route actions.
const TOOL_CALL_JSON_SCHEMA: &str = r#"{"type":"object","properties":{"tool_calls":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string","enum":["speak","read","route"]},"input":{"type":"object"}},"required":["name","input"]}}},"required":["tool_calls"]}"#;

/// Resolve the model name for a given backend.
///
/// `THERMAL_DISPATCHER_MODEL` env var overrides the default for any backend.
pub fn resolve_model(backend: &LlmBackend) -> String {
    if let Ok(model) = std::env::var("THERMAL_DISPATCHER_MODEL") {
        return model;
    }
    match backend {
        LlmBackend::ClaudeCli => DEFAULT_CLAUDE_MODEL.to_string(),
        LlmBackend::CopilotCli => DEFAULT_COPILOT_MODEL.to_string(),
        LlmBackend::Ollama => DEFAULT_OLLAMA_MODEL.to_string(),
    }
}

/// Detect the best available backend by probing CLI tools, then Ollama.
///
/// Priority: Claude CLI > Copilot CLI > Ollama.
/// Set `THERMAL_DISPATCHER_BACKEND` to force a specific backend:
/// `claude`, `copilot`, or `ollama`.
pub async fn detect_backend(http: &reqwest::Client) -> Result<LlmBackend> {
    // Allow explicit override via env var
    if let Ok(forced) = std::env::var("THERMAL_DISPATCHER_BACKEND") {
        match forced.to_lowercase().as_str() {
            "claude" | "claude-cli" => {
                if check_claude_cli_available().await {
                    info!("backend forced to claude-cli via THERMAL_DISPATCHER_BACKEND");
                    return Ok(LlmBackend::ClaudeCli);
                }
                anyhow::bail!("THERMAL_DISPATCHER_BACKEND=claude but claude CLI not available");
            }
            "copilot" | "copilot-cli" => {
                if check_copilot_cli_available().await {
                    info!("backend forced to copilot-cli via THERMAL_DISPATCHER_BACKEND");
                    return Ok(LlmBackend::CopilotCli);
                }
                anyhow::bail!("THERMAL_DISPATCHER_BACKEND=copilot but gh copilot not available");
            }
            "ollama" => {
                info!("backend forced to ollama via THERMAL_DISPATCHER_BACKEND");
                return Ok(LlmBackend::Ollama);
            }
            other => {
                anyhow::bail!(
                    "unknown THERMAL_DISPATCHER_BACKEND={other}; valid: claude, copilot, ollama"
                );
            }
        }
    }

    // Auto-detect: try Claude CLI first
    if check_claude_cli_available().await {
        info!("detected claude CLI — using as primary backend");
        return Ok(LlmBackend::ClaudeCli);
    }

    // Try Copilot CLI
    if check_copilot_cli_available().await {
        info!("detected copilot CLI — using as secondary backend");
        return Ok(LlmBackend::CopilotCli);
    }

    // Fall back to Ollama
    let model = resolve_model(&LlmBackend::Ollama);
    if check_ollama_health(http, &model).await.is_ok() {
        info!("falling back to Ollama (local offline backend)");
        return Ok(LlmBackend::Ollama);
    }

    anyhow::bail!(
        "no LLM backend available — install claude CLI, gh copilot, or start Ollama"
    );
}

/// Check if `claude` CLI is installed and authenticated.
async fn check_claude_cli_available() -> bool {
    match tokio::process::Command::new("claude")
        .args(["--version"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
    {
        Ok(output) => {
            let available = output.status.success();
            if available {
                let version = String::from_utf8_lossy(&output.stdout);
                debug!(version = %version.trim(), "claude CLI available");
            }
            available
        }
        Err(_) => false,
    }
}

/// Check if `gh copilot` is installed and working.
async fn check_copilot_cli_available() -> bool {
    match tokio::process::Command::new("gh")
        .args(["copilot", "--version"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
    {
        Ok(output) => {
            let available = output.status.success();
            if available {
                let version = String::from_utf8_lossy(&output.stdout);
                debug!(version = %version.trim(), "gh copilot available");
            }
            available
        }
        Err(_) => false,
    }
}

/// Check that Ollama is reachable and the configured model is available.
pub async fn check_ollama_health(http: &reqwest::Client, model: &str) -> Result<()> {
    let url = format!("{OLLAMA_BASE_URL}/api/tags");
    let response = http
        .get(&url)
        .send()
        .await
        .context("cannot reach Ollama at localhost:11434 — is it running?")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "Ollama health check returned HTTP {}",
            response.status()
        );
    }

    let body: Value = response
        .json()
        .await
        .context("parsing Ollama /api/tags response")?;

    // Check if the requested model is available
    let models = body
        .get("models")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let model_available = models.iter().any(|m| {
        m.get("name")
            .and_then(|v| v.as_str())
            .map(|name| name == model || name.starts_with(&format!("{model}:")))
            .unwrap_or(false)
    });

    if !model_available {
        let available: Vec<&str> = models
            .iter()
            .filter_map(|m| m.get("name").and_then(|v| v.as_str()))
            .collect();
        warn!(
            model = %model,
            available = ?available,
            "configured model not found in Ollama — pull it with: ollama pull {model}"
        );
        anyhow::bail!(
            "model '{}' not found in Ollama. Available: {:?}. Pull with: ollama pull {}",
            model,
            available,
            model
        );
    }

    info!(model = %model, "Ollama health check passed");
    Ok(())
}

/// Convert Anthropic-style tool schemas to Ollama/OpenAI function-calling format.
///
/// Anthropic: `{"name": "...", "description": "...", "input_schema": {...}}`
/// Ollama:    `{"type": "function", "function": {"name": "...", "description": "...", "parameters": {...}}}`
pub fn convert_tools_for_ollama(anthropic_tools: &[Value]) -> Vec<Value> {
    anthropic_tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.get("name").and_then(|v| v.as_str()).unwrap_or("unknown"),
                    "description": tool.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                    "parameters": tool.get("input_schema").cloned().unwrap_or(serde_json::json!({"type": "object", "properties": {}})),
                }
            })
        })
        .collect()
}

/// Build Ollama chat messages array from conversation history messages.
///
/// Prepends the system prompt as a system message.
fn build_ollama_messages(messages: &[Value]) -> Vec<Value> {
    let mut ollama_messages = Vec::with_capacity(messages.len() + 1);

    // System message first
    ollama_messages.push(serde_json::json!({
        "role": "system",
        "content": SYSTEM_PROMPT,
    }));

    // Append conversation messages, converting Anthropic-format content arrays
    // to Ollama's expected format (plain string content + tool_calls field).
    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

        if role == "assistant" {
            if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
                // Anthropic-format content array — convert to Ollama format
                let text_parts: String = arr
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

                let tool_calls: Vec<Value> = arr
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

                let mut ollama_msg = serde_json::json!({
                    "role": "assistant",
                    "content": text_parts,
                });
                if !tool_calls.is_empty() {
                    ollama_msg["tool_calls"] = serde_json::json!(tool_calls);
                }
                ollama_messages.push(ollama_msg);
            } else {
                // Already a plain string content — pass through
                ollama_messages.push(msg.clone());
            }
        } else {
            ollama_messages.push(msg.clone());
        }
    }

    ollama_messages
}

// ---------------------------------------------------------------------------
// CLI backend: Claude CLI / Copilot CLI
// ---------------------------------------------------------------------------

/// Call a CLI-based LLM (Claude or Copilot) with the dispatcher's tool schema.
///
/// Uses `--json-schema` to force structured `tool_calls` output. The transcript
/// is passed as the prompt; conversation history is included in the system prompt
/// as context. Returns the same normalised format as `call_ollama()`.
pub async fn call_cli_llm(
    backend: &LlmBackend,
    model: &str,
    messages: &[Value],
) -> Result<Value> {
    // Build the full prompt: system prompt + conversation history + current message
    let prompt = build_cli_prompt(messages);

    let start = std::time::Instant::now();

    let output = match backend {
        LlmBackend::ClaudeCli => call_claude_cli(model, &prompt).await?,
        LlmBackend::CopilotCli => call_copilot_cli(model, &prompt).await?,
        LlmBackend::Ollama => unreachable!("call_cli_llm should not be called with Ollama backend"),
    };

    let duration_ms = start.elapsed().as_millis();

    // Parse tool_calls from the structured output
    let tool_calls = output
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // Convert to normalised Anthropic-compatible content blocks
    let mut content_blocks = Vec::new();
    let has_tool_calls = !tool_calls.is_empty();

    for (i, tc) in tool_calls.iter().enumerate() {
        let name = tc
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let input = tc.get("input").cloned().unwrap_or(serde_json::json!({}));

        content_blocks.push(serde_json::json!({
            "type": "tool_use",
            "id": format!("cli_tool_{i}"),
            "name": name,
            "input": input,
        }));
    }

    let stop_reason = if has_tool_calls { "tool_use" } else { "end_turn" };

    info!(
        backend = %backend,
        model = %model,
        stop_reason = %stop_reason,
        tool_count = tool_calls.len(),
        duration_ms = duration_ms,
        "CLI LLM response"
    );

    Ok(serde_json::json!({
        "stop_reason": stop_reason,
        "content": content_blocks,
    }))
}

/// Build the prompt string for CLI backends from conversation messages.
///
/// Includes the system prompt context and conversation history, then the
/// current user message as the prompt.
fn build_cli_prompt(messages: &[Value]) -> String {
    let mut parts = Vec::new();

    // Add conversation history (skip the last message which is the current prompt)
    let history = if messages.len() > 1 {
        &messages[..messages.len() - 1]
    } else {
        &[]
    };

    if !history.is_empty() {
        parts.push("Previous conversation:".to_string());
        for msg in history {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("?");
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            parts.push(format!("{role}: {content}"));
        }
        parts.push(String::new()); // blank line separator
    }

    // The current user message
    if let Some(last) = messages.last() {
        let content = last.get("content").and_then(|v| v.as_str()).unwrap_or("");
        parts.push(format!("User: {content}"));
    }

    parts.join("\n")
}

/// Invoke `claude -p` with structured JSON output.
async fn call_claude_cli(model: &str, prompt: &str) -> Result<Value> {
    debug!(model = %model, prompt_len = prompt.len(), "calling claude CLI");

    let output = tokio::process::Command::new("claude")
        .args([
            "-p",
            "--output-format", "json",
            "--no-session-persistence",
            "--model", model,
            "--system-prompt", SYSTEM_PROMPT,
            "--json-schema", TOOL_CALL_JSON_SCHEMA,
            prompt,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to spawn claude CLI — is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Try to extract error from JSON output
        if let Ok(json) = serde_json::from_str::<Value>(&stdout) {
            if json.get("is_error") == Some(&Value::Bool(true)) {
                let result = json
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                anyhow::bail!("claude CLI error: {result}");
            }
        }
        anyhow::bail!(
            "claude CLI exited with {}: {}",
            output.status,
            truncate(stderr.trim(), 500)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse the JSON result — claude outputs a single JSON object
    let parsed: Value =
        serde_json::from_str(&stdout).context("parsing claude CLI JSON output")?;

    // Extract structured_output which contains our tool_calls
    if let Some(structured) = parsed.get("structured_output") {
        debug!(structured = %structured, "claude CLI structured output");
        return Ok(structured.clone());
    }

    // Fallback: try to parse the result text as JSON (shouldn't happen with --json-schema)
    if let Some(result) = parsed.get("result").and_then(|v| v.as_str()) {
        if let Ok(json) = serde_json::from_str::<Value>(result) {
            return Ok(json);
        }
        // Model returned plain text instead of tool calls — wrap in a speak call
        warn!(
            result = %truncate(result, 200),
            "claude CLI returned plain text instead of structured output"
        );
        return Ok(serde_json::json!({
            "tool_calls": [{"name": "speak", "input": {"text": result}}]
        }));
    }

    anyhow::bail!("unexpected claude CLI output format: {}", truncate(&stdout, 500));
}

/// Invoke `gh copilot -p` with JSON output.
async fn call_copilot_cli(model: &str, prompt: &str) -> Result<Value> {
    debug!(model = %model, prompt_len = prompt.len(), "calling copilot CLI");

    // Build the full prompt with tool-call instructions baked in,
    // since gh copilot doesn't support --json-schema directly.
    let full_prompt = format!(
        "{SYSTEM_PROMPT}\n\n\
        IMPORTANT: Respond with ONLY a JSON object in this exact format:\n\
        {{\"tool_calls\": [{{\"name\": \"speak|read|route\", \"input\": {{...}}}}]}}\n\n\
        User transcript: {prompt}"
    );

    let output = tokio::process::Command::new("gh")
        .args([
            "copilot",
            "-p",
            &full_prompt,
            "--model", model,
            "--output-format", "json",
            "--no-custom-instructions",
            "--allow-all-tools",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to spawn gh copilot — is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "gh copilot exited with {}: {}",
            output.status,
            truncate(stderr.trim(), 500)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Copilot outputs streaming JSONL — find the assistant.message with content
    let mut assistant_text = String::new();
    for line in stdout.lines() {
        if let Ok(event) = serde_json::from_str::<Value>(line) {
            // Look for assistant.message events with content
            if event.get("type").and_then(|v| v.as_str()) == Some("assistant.message") {
                if let Some(content) = event
                    .get("data")
                    .and_then(|d| d.get("content"))
                    .and_then(|v| v.as_str())
                {
                    assistant_text = content.to_string();
                }
            }
        }
    }

    if assistant_text.is_empty() {
        anyhow::bail!("no assistant response in copilot output");
    }

    // Try to parse as JSON tool_calls
    if let Ok(json) = serde_json::from_str::<Value>(&assistant_text) {
        if json.get("tool_calls").is_some() {
            return Ok(json);
        }
    }

    // Try to extract JSON from markdown code blocks
    let extracted = extract_json_from_text(&assistant_text);
    if let Some(json) = extracted {
        if json.get("tool_calls").is_some() {
            return Ok(json);
        }
    }

    // Fallback: wrap plain text in speak
    warn!(
        text = %truncate(&assistant_text, 200),
        "copilot returned plain text, wrapping in speak"
    );
    Ok(serde_json::json!({
        "tool_calls": [{"name": "speak", "input": {"text": assistant_text}}]
    }))
}

/// Try to extract a JSON object from text that might be wrapped in markdown code fences.
fn extract_json_from_text(text: &str) -> Option<Value> {
    // Try direct parse first
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Some(v);
    }

    // Try extracting from ```json ... ``` blocks
    if let Some(start) = text.find("```json") {
        let after_fence = &text[start + 7..];
        if let Some(end) = after_fence.find("```") {
            let json_str = after_fence[..end].trim();
            if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                return Some(v);
            }
        }
    }

    // Try extracting from ``` ... ``` blocks (no language tag)
    if let Some(start) = text.find("```") {
        let after_fence = &text[start + 3..];
        // Skip optional language tag on the same line
        let json_start = after_fence.find('\n').unwrap_or(0);
        let rest = &after_fence[json_start..];
        if let Some(end) = rest.find("```") {
            let json_str = rest[..end].trim();
            if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                return Some(v);
            }
        }
    }

    // Try finding { ... } in the text
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            let json_str = &text[start..=end];
            if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                return Some(v);
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Ollama backend (offline fallback)
// ---------------------------------------------------------------------------

/// Call the Ollama chat API with tool definitions.
///
/// Returns a normalised response with `stop_reason` and `content` fields
/// matching the format expected by the dispatch loop in main.rs:
///
/// - `stop_reason`: `"end_turn"` or `"tool_use"`
/// - `content`: array of `{"type": "text", "text": "..."}` and/or
///   `{"type": "tool_use", "id": "...", "name": "...", "input": {...}}`
pub async fn call_ollama(
    http: &reqwest::Client,
    model: &str,
    tools: &[Value],
    messages: &[Value],
) -> Result<Value> {
    let url = format!("{OLLAMA_BASE_URL}/api/chat");
    let ollama_tools = convert_tools_for_ollama(tools);
    let ollama_messages = build_ollama_messages(messages);

    let body = serde_json::json!({
        "model": model,
        "messages": ollama_messages,
        "tools": ollama_tools,
        "stream": false,
    });

    debug!(
        model = model,
        messages = ollama_messages.len(),
        tools = ollama_tools.len(),
        "calling Ollama API"
    );

    let response = http
        .post(&url)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("HTTP request to Ollama API failed")?;

    let status = response.status();
    let response_text = response
        .text()
        .await
        .context("reading Ollama API response body")?;

    if !status.is_success() {
        anyhow::bail!(
            "Ollama API returned {}: {}",
            status,
            truncate(&response_text, 500)
        );
    }

    let parsed: Value =
        serde_json::from_str(&response_text).context("parsing Ollama API response JSON")?;

    // Log timing info from Ollama
    let total_duration_ns = parsed.get("total_duration").and_then(|v| v.as_u64()).unwrap_or(0);
    let total_duration_ms = total_duration_ns / 1_000_000;
    let eval_count = parsed.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0);

    // Extract the message object
    let message = parsed
        .get("message")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    let tool_calls = message
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let assistant_text = message
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Strip any <think>...</think> blocks from the response (Qwen3 thinking mode leakage)
    let clean_text = strip_think_blocks(&assistant_text);

    // Normalise into Anthropic-compatible content blocks
    let mut content_blocks = Vec::new();

    if !clean_text.is_empty() {
        content_blocks.push(serde_json::json!({
            "type": "text",
            "text": clean_text,
        }));
    }

    let has_tool_calls = !tool_calls.is_empty();

    for (i, tc) in tool_calls.iter().enumerate() {
        let function = tc.get("function").cloned().unwrap_or(serde_json::json!({}));
        let name = function
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        // Ollama returns arguments as an object (already parsed JSON)
        let arguments = function.get("arguments").cloned().unwrap_or(serde_json::json!({}));

        content_blocks.push(serde_json::json!({
            "type": "tool_use",
            "id": format!("ollama_tool_{i}"),
            "name": name,
            "input": arguments,
        }));
    }

    let stop_reason = if has_tool_calls { "tool_use" } else { "end_turn" };

    info!(
        %model,
        stop_reason = %stop_reason,
        duration_ms = total_duration_ms,
        eval_tokens = eval_count,
        "Ollama response"
    );

    // Return normalised response matching Anthropic format
    Ok(serde_json::json!({
        "stop_reason": stop_reason,
        "content": content_blocks,
    }))
}

/// Build Ollama tool result messages from Anthropic-format tool_result blocks.
///
/// Anthropic format (user message with content array):
///   `{"role": "user", "content": [{"type": "tool_result", "tool_use_id": "...", "content": "..."}]}`
///
/// Ollama format (one message per tool result):
///   `{"role": "tool", "content": "..."}`
///
/// This is called from the dispatch loop to convert tool result messages.
pub fn convert_tool_results_for_ollama(user_msg: &Value) -> Vec<Value> {
    let content = user_msg
        .get("content")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    content
        .iter()
        .filter(|block| {
            block.get("type").and_then(|v| v.as_str()) == Some("tool_result")
        })
        .map(|block| {
            let result_content = block
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            serde_json::json!({
                "role": "tool",
                "content": result_content,
            })
        })
        .collect()
}

/// Strip `<think>...</think>` blocks that Qwen3 may emit even with /no_think.
fn strip_think_blocks(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;

    while let Some(start) = remaining.find("<think>") {
        result.push_str(&remaining[..start]);
        if let Some(end) = remaining[start..].find("</think>") {
            remaining = &remaining[start + end + "</think>".len()..];
        } else {
            // Unclosed <think> tag — skip everything after it
            remaining = "";
            break;
        }
    }
    result.push_str(remaining);
    result.trim().to_string()
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Find the last char boundary at or before `max` to avoid panicking
        // on multi-byte UTF-8 sequences.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // truncate helper
    // -----------------------------------------------------------------------

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string_cut() {
        let result = truncate("hello world", 5);
        assert_eq!(result, "hello");
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn truncate_multibyte_utf8_does_not_panic() {
        // "héllo" — 'é' is 2 bytes (0xC3 0xA9), so byte index 2 is mid-char
        let s = "héllo";
        let result = truncate(s, 2);
        // Should back up to byte 1 (before 'é') rather than panicking
        assert_eq!(result, "h");
    }

    #[test]
    fn truncate_emoji_boundary() {
        // "🔥ab" — fire emoji is 4 bytes
        let s = "🔥ab";
        let result = truncate(s, 3);
        // Should back up to byte 0 (before the emoji)
        assert_eq!(result, "");
    }

    // -----------------------------------------------------------------------
    // strip_think_blocks
    // -----------------------------------------------------------------------

    #[test]
    fn strip_think_blocks_no_tags() {
        assert_eq!(strip_think_blocks("hello world"), "hello world");
    }

    #[test]
    fn strip_think_blocks_removes_think_section() {
        assert_eq!(
            strip_think_blocks("<think>reasoning here</think>Opening Firefox now."),
            "Opening Firefox now."
        );
    }

    #[test]
    fn strip_think_blocks_multiple() {
        assert_eq!(
            strip_think_blocks("<think>a</think>hello <think>b</think>world"),
            "hello world"
        );
    }

    #[test]
    fn strip_think_blocks_unclosed() {
        assert_eq!(strip_think_blocks("before <think>rest"), "before");
    }

    // -----------------------------------------------------------------------
    // Tool schema conversion
    // -----------------------------------------------------------------------

    #[test]
    fn convert_tools_maps_anthropic_to_ollama_format() {
        let anthropic = vec![json!({
            "name": "screenshot",
            "description": "Take a screenshot",
            "input_schema": {"type": "object", "properties": {}}
        })];
        let ollama = convert_tools_for_ollama(&anthropic);
        assert_eq!(ollama.len(), 1);
        assert_eq!(ollama[0]["type"], "function");
        assert_eq!(ollama[0]["function"]["name"], "screenshot");
        assert_eq!(ollama[0]["function"]["description"], "Take a screenshot");
        assert_eq!(ollama[0]["function"]["parameters"]["type"], "object");
    }

    // -----------------------------------------------------------------------
    // Message building
    // -----------------------------------------------------------------------

    #[test]
    fn build_ollama_messages_prepends_system() {
        let msgs = vec![json!({"role": "user", "content": "hello"})];
        let result = build_ollama_messages(&msgs);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["role"], "system");
        assert!(result[0]["content"].as_str().unwrap().contains("voice transcripts"));
        assert_eq!(result[1]["role"], "user");
        assert_eq!(result[1]["content"], "hello");
    }

    #[test]
    fn build_ollama_messages_converts_assistant_content_arrays() {
        let msgs = vec![
            json!({"role": "user", "content": "open firefox"}),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Opening Firefox."},
                    {"type": "tool_use", "id": "t1", "name": "open_browser", "input": {}}
                ]
            }),
        ];
        let result = build_ollama_messages(&msgs);
        assert_eq!(result.len(), 3); // system + user + assistant
        let assistant = &result[2];
        assert_eq!(assistant["role"], "assistant");
        // Content should be a plain string, not an array
        assert_eq!(assistant["content"], "Opening Firefox.");
        // tool_calls should be present
        let tool_calls = assistant["tool_calls"].as_array().expect("should have tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["function"]["name"], "open_browser");
    }

    #[test]
    fn build_ollama_messages_passes_plain_assistant_through() {
        let msgs = vec![json!({"role": "assistant", "content": "plain text"})];
        let result = build_ollama_messages(&msgs);
        assert_eq!(result[1]["content"], "plain text");
        assert!(result[1].get("tool_calls").is_none());
    }

    // -----------------------------------------------------------------------
    // Tool result conversion
    // -----------------------------------------------------------------------

    #[test]
    fn convert_tool_results_maps_correctly() {
        let user_msg = json!({
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "result text"},
            ]
        });
        let results = convert_tool_results_for_ollama(&user_msg);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["role"], "tool");
        assert_eq!(results[0]["content"], "result text");
    }

    // -----------------------------------------------------------------------
    // resolve_model
    // -----------------------------------------------------------------------

    #[test]
    fn default_ollama_model_is_qwen3() {
        assert_eq!(DEFAULT_OLLAMA_MODEL, "qwen3:8b");
    }

    #[test]
    fn default_claude_model_is_sonnet() {
        assert_eq!(DEFAULT_CLAUDE_MODEL, "sonnet");
    }

    #[test]
    fn default_copilot_model_is_gpt41() {
        assert_eq!(DEFAULT_COPILOT_MODEL, "gpt-4.1");
    }

    #[test]
    fn resolve_model_returns_default_per_backend() {
        // Clear the env var to test defaults
        // SAFETY: single-threaded test, no other threads reading this var
        unsafe { std::env::remove_var("THERMAL_DISPATCHER_MODEL") };
        assert_eq!(resolve_model(&LlmBackend::ClaudeCli), "sonnet");
        assert_eq!(resolve_model(&LlmBackend::CopilotCli), "gpt-4.1");
        assert_eq!(resolve_model(&LlmBackend::Ollama), "qwen3:8b");
    }

    // -----------------------------------------------------------------------
    // System prompt
    // -----------------------------------------------------------------------

    #[test]
    fn system_prompt_describes_voice_role() {
        assert!(SYSTEM_PROMPT.contains("voice transcripts"));
    }

    #[test]
    fn system_prompt_instructs_tts_friendly_output() {
        assert!(
            SYSTEM_PROMPT.contains("TTS") || SYSTEM_PROMPT.contains("spoken"),
            "system prompt should reference TTS or spoken output"
        );
        assert!(
            SYSTEM_PROMPT.contains("markdown") || SYSTEM_PROMPT.contains("concise"),
            "system prompt should discourage markdown or instruct conciseness"
        );
    }

    #[test]
    fn system_prompt_ends_with_no_think() {
        assert!(
            SYSTEM_PROMPT.ends_with("/no_think"),
            "system prompt should end with /no_think (harmless for non-Qwen models)"
        );
    }

    // -----------------------------------------------------------------------
    // Ollama URL
    // -----------------------------------------------------------------------

    #[test]
    fn ollama_base_url_is_localhost() {
        assert_eq!(OLLAMA_BASE_URL, "http://localhost:11434");
    }

    // -----------------------------------------------------------------------
    // LlmBackend display
    // -----------------------------------------------------------------------

    #[test]
    fn backend_display_names() {
        assert_eq!(format!("{}", LlmBackend::ClaudeCli), "claude-cli");
        assert_eq!(format!("{}", LlmBackend::CopilotCli), "copilot-cli");
        assert_eq!(format!("{}", LlmBackend::Ollama), "ollama");
    }

    #[test]
    fn backend_equality() {
        assert_eq!(LlmBackend::ClaudeCli, LlmBackend::ClaudeCli);
        assert_ne!(LlmBackend::ClaudeCli, LlmBackend::Ollama);
    }

    // -----------------------------------------------------------------------
    // CLI prompt building
    // -----------------------------------------------------------------------

    #[test]
    fn build_cli_prompt_single_message() {
        let msgs = vec![json!({"role": "user", "content": "hello"})];
        let prompt = build_cli_prompt(&msgs);
        assert!(prompt.contains("User: hello"));
        // No history section for single message
        assert!(!prompt.contains("Previous conversation"));
    }

    #[test]
    fn build_cli_prompt_with_history() {
        let msgs = vec![
            json!({"role": "user", "content": "open firefox"}),
            json!({"role": "assistant", "content": "Routed to system."}),
            json!({"role": "user", "content": "thanks"}),
        ];
        let prompt = build_cli_prompt(&msgs);
        assert!(prompt.contains("Previous conversation"));
        assert!(prompt.contains("user: open firefox"));
        assert!(prompt.contains("assistant: Routed to system."));
        assert!(prompt.contains("User: thanks"));
    }

    // -----------------------------------------------------------------------
    // JSON extraction from text
    // -----------------------------------------------------------------------

    #[test]
    fn extract_json_direct_parse() {
        let text = r#"{"tool_calls":[{"name":"speak","input":{"text":"hi"}}]}"#;
        let result = extract_json_from_text(text).unwrap();
        assert!(result.get("tool_calls").is_some());
    }

    #[test]
    fn extract_json_from_code_fence() {
        let text = "Here is the result:\n```json\n{\"tool_calls\":[{\"name\":\"read\",\"input\":{}}]}\n```\n";
        let result = extract_json_from_text(text).unwrap();
        assert!(result.get("tool_calls").is_some());
    }

    #[test]
    fn extract_json_from_braces() {
        let text = "I'll route that: {\"tool_calls\":[{\"name\":\"route\",\"input\":{\"to\":\"@claude\",\"message\":\"hi\"}}]}";
        let result = extract_json_from_text(text).unwrap();
        assert!(result.get("tool_calls").is_some());
    }

    #[test]
    fn extract_json_returns_none_for_plain_text() {
        let result = extract_json_from_text("just plain text here");
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // TOOL_CALL_JSON_SCHEMA is valid JSON
    // -----------------------------------------------------------------------

    #[test]
    fn tool_call_schema_is_valid_json() {
        let parsed: Value = serde_json::from_str(TOOL_CALL_JSON_SCHEMA)
            .expect("TOOL_CALL_JSON_SCHEMA should be valid JSON");
        assert_eq!(parsed["type"], "object");
        assert!(parsed["properties"]["tool_calls"].is_object());
    }
}
