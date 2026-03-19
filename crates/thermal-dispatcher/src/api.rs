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
