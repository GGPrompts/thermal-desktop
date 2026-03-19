//! Anthropic API client for Claude Haiku with tool use.

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{debug, info};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const MODEL: &str = "claude-haiku-4-20250414";
const MAX_TOKENS: u32 = 1024;

/// System prompt that gives Haiku its role as a voice assistant dispatcher.
const SYSTEM_PROMPT: &str = r#"You are the voice command dispatcher for Thermal Desktop, a custom Linux desktop environment.

Your job:
1. Interpret the user's spoken command (transcribed from speech-to-text).
2. Use the available tools to fulfill the command.
3. Respond with a brief, natural spoken confirmation suitable for TTS playback.

Guidelines:
- Be concise — responses will be spoken aloud via TTS.
- Use tools when the user asks to do something on the desktop (open apps, manage windows, check status, etc.).
- For ambiguous commands, pick the most likely interpretation.
- If you cannot fulfill a request, say so briefly.
- Never use markdown, code blocks, or formatting — plain spoken English only.
- Keep responses under 2 sentences when possible.
- For beads issue queries, summarize results conversationally for speech (e.g. "You have 3 ready issues: thermal monitor, voice pipeline, and dispatcher" instead of listing IDs or JSON)."#;

/// Call the Anthropic Messages API with tool definitions.
pub async fn call_haiku(
    http: &reqwest::Client,
    api_key: &str,
    tools: &[Value],
    messages: &[Value],
) -> Result<Value> {
    let body = serde_json::json!({
        "model": MODEL,
        "max_tokens": MAX_TOKENS,
        "system": SYSTEM_PROMPT,
        "tools": tools,
        "messages": messages,
    });

    debug!(
        model = MODEL,
        messages = messages.len(),
        tools = tools.len(),
        "calling Anthropic API"
    );

    let response = http
        .post(ANTHROPIC_API_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("HTTP request to Anthropic API failed")?;

    let status = response.status();
    let response_text = response
        .text()
        .await
        .context("reading Anthropic API response body")?;

    if !status.is_success() {
        anyhow::bail!(
            "Anthropic API returned {}: {}",
            status,
            truncate(&response_text, 500)
        );
    }

    let parsed: Value =
        serde_json::from_str(&response_text).context("parsing Anthropic API response JSON")?;

    let stop_reason = parsed
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let usage = parsed.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    info!(
        stop_reason = %stop_reason,
        input_tokens = input_tokens,
        output_tokens = output_tokens,
        "Haiku response"
    );

    Ok(parsed)
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
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

    // -----------------------------------------------------------------------
    // API message construction: verify body structure
    //
    // We test the JSON body that would be sent by constructing it the same way
    // call_haiku does — without making a real network call.
    // -----------------------------------------------------------------------

    fn build_api_body(tools: &[Value], messages: &[Value]) -> Value {
        serde_json::json!({
            "model": MODEL,
            "max_tokens": MAX_TOKENS,
            "system": SYSTEM_PROMPT,
            "tools": tools,
            "messages": messages,
        })
    }

    #[test]
    fn api_body_contains_correct_model() {
        let body = build_api_body(&[], &[]);
        assert_eq!(
            body.get("model").and_then(|v| v.as_str()),
            Some("claude-haiku-4-20250414")
        );
    }

    #[test]
    fn api_body_contains_max_tokens() {
        let body = build_api_body(&[], &[]);
        assert_eq!(
            body.get("max_tokens").and_then(|v| v.as_u64()),
            Some(1024)
        );
    }

    #[test]
    fn api_body_contains_system_prompt() {
        let body = build_api_body(&[], &[]);
        let system = body.get("system").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!system.is_empty(), "system prompt must not be empty");
        assert!(
            system.contains("Thermal Desktop"),
            "system prompt should mention Thermal Desktop"
        );
    }

    #[test]
    fn system_prompt_instructs_tts_friendly_output() {
        // The system prompt must guide Haiku towards brief, spoken responses.
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
    fn api_body_includes_provided_tools() {
        let tools = vec![
            json!({"name": "screenshot", "description": "take a screenshot", "input_schema": {"type": "object", "properties": {}}}),
        ];
        let body = build_api_body(&tools, &[]);
        let tools_arr = body.get("tools").and_then(|v| v.as_array()).unwrap();
        assert_eq!(tools_arr.len(), 1);
        assert_eq!(
            tools_arr[0].get("name").and_then(|v| v.as_str()),
            Some("screenshot")
        );
    }

    #[test]
    fn api_body_includes_provided_messages() {
        let messages = vec![
            json!({"role": "user", "content": "open firefox"}),
        ];
        let body = build_api_body(&[], &messages);
        let msgs = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].get("role").and_then(|v| v.as_str()),
            Some("user")
        );
        assert_eq!(
            msgs[0].get("content").and_then(|v| v.as_str()),
            Some("open firefox")
        );
    }

    #[test]
    fn user_message_format_matches_anthropic_schema() {
        // The dispatcher creates user messages like this; verify the structure.
        let transcript = "open the browser";
        let msg = json!({
            "role": "user",
            "content": transcript,
        });
        assert_eq!(msg.get("role").and_then(|v| v.as_str()), Some("user"));
        assert_eq!(
            msg.get("content").and_then(|v| v.as_str()),
            Some("open the browser")
        );
    }

    #[test]
    fn tool_result_message_format() {
        // The dispatcher builds tool_result messages like this; verify structure.
        let result_block = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_abc123",
            "content": "window list: firefox, kitty",
        });
        assert_eq!(
            result_block.get("type").and_then(|v| v.as_str()),
            Some("tool_result")
        );
        assert_eq!(
            result_block.get("tool_use_id").and_then(|v| v.as_str()),
            Some("toolu_abc123")
        );
        assert!(result_block.get("content").is_some());
    }

    // -----------------------------------------------------------------------
    // Constants are stable
    // -----------------------------------------------------------------------

    #[test]
    fn anthropic_api_url_is_correct() {
        assert_eq!(ANTHROPIC_API_URL, "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn model_constant_is_haiku() {
        assert!(MODEL.contains("haiku"), "MODEL should be a Haiku model");
    }
}
