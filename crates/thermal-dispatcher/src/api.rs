//! Ollama API client for local LLM inference via qwen3:8b.

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{debug, info, warn};

const OLLAMA_BASE_URL: &str = "http://localhost:11434";
const DEFAULT_MODEL: &str = "qwen3:8b";

/// System prompt that gives the model its role as a voice assistant dispatcher.
const SYSTEM_PROMPT: &str = r#"You are the voice assistant for Thermal Desktop, a custom Linux desktop.
You receive speech-to-text transcripts and use tools to execute commands.
Respond with brief spoken confirmations for TTS. No markdown, no formatting, plain English only, under 2 sentences.

Input is speech-to-text and may contain filler words, hesitations, or transcription errors. Interpret the intent, not literal text.

TOOL GUIDE:
- "open [app]" → open_app, open_browser, open_terminal, or open_files
- "what's on screen" / "what do I have open" → screenshot or list_windows
- "focus [app]" / "switch to [app]" → focus_window
- "how are my issues" / "what's ready" → beads:list
- "spawn claude" / "start a session" → spawn_claude
- "check system" / "how's the machine" → system_metrics
- For issue queries, summarize conversationally (e.g. "You have 3 ready issues" not JSON or IDs)

EXAMPLES:

User: "open Firefox"
Action: open_browser()
Response: Opening Firefox.

User: "uh what issues are ready"
Action: beads:list(status="ready")
Response: You have 3 ready issues, the voice pipeline, dispatcher chat, and monitor fix.

User: "switch to the terminal"
Action: focus_window(selector="kitty")
Response: Switched to the terminal.

User: "um how's the system doing"
Action: system_metrics()
Response: CPU is at 34 percent, 12 gigs of RAM used, GPU at 45 percent.

User: "take a screenshot"
Action: screenshot()
Response: Got it. You have a terminal and Firefox open, with the terminal focused. /no_think"#;

/// Resolve the model name: env var `THERMAL_DISPATCHER_MODEL` overrides the default.
pub fn resolve_model() -> String {
    std::env::var("THERMAL_DISPATCHER_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string())
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

    // Append conversation messages as-is (role/content format is compatible)
    for msg in messages {
        ollama_messages.push(msg.clone());
    }

    ollama_messages
}

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
    if s.len() <= max { s } else { &s[..max] }
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
        assert!(result[0]["content"].as_str().unwrap().contains("Thermal Desktop"));
        assert_eq!(result[1]["role"], "user");
        assert_eq!(result[1]["content"], "hello");
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
    fn default_model_is_qwen3() {
        assert_eq!(DEFAULT_MODEL, "qwen3:8b");
    }

    // -----------------------------------------------------------------------
    // System prompt
    // -----------------------------------------------------------------------

    #[test]
    fn system_prompt_contains_thermal_desktop() {
        assert!(SYSTEM_PROMPT.contains("Thermal Desktop"));
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
            "system prompt should end with /no_think to disable Qwen3 thinking mode"
        );
    }

    // -----------------------------------------------------------------------
    // Ollama URL
    // -----------------------------------------------------------------------

    #[test]
    fn ollama_base_url_is_localhost() {
        assert_eq!(OLLAMA_BASE_URL, "http://localhost:11434");
    }
}
