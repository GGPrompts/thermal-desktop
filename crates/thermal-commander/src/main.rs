//! thermal-commander: MCP server for Wayland/Hyprland desktop control.
//!
//! Implements the Model Context Protocol (JSON-RPC 2.0 over stdio) providing
//! screenshot, click, type, key combo, scroll, and Hyprland window management
//! tools. Designed to give Claude full visual desktop control.

mod mcp;
mod tools;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::Level;

use mcp::Response;
use tools::ToolRegistry;

const SERVER_NAME: &str = "thermal-commander";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2024-11-05";

#[tokio::main]
async fn main() -> Result<()> {
    // Log to stderr so it doesn't interfere with JSON-RPC on stdout
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    tracing::info!("{SERVER_NAME} v{SERVER_VERSION} starting");

    let registry = ToolRegistry::new();

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            // EOF — client disconnected
            tracing::info!("stdin closed, shutting down");
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        tracing::debug!(request = %trimmed, "received");

        let response = handle_message(trimmed, &registry).await;

        if let Some(resp) = response {
            let mut out = serde_json::to_string(&resp)?;
            out.push('\n');
            stdout.write_all(out.as_bytes()).await?;
            stdout.flush().await?;
            tracing::debug!(response = %out.trim(), "sent");
        }
    }

    Ok(())
}

async fn handle_message(raw: &str, registry: &ToolRegistry) -> Option<Response> {
    let request: mcp::Request = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
            return Some(Response::error(
                None,
                -32700,
                format!("parse error: {e}"),
            ));
        }
    };

    let id = request.id.clone();

    // Notifications (no id) don't get responses
    if id.is_none() {
        // Handle notifications silently
        match request.method.as_str() {
            "notifications/initialized" => {
                tracing::info!("client initialized notification received");
            }
            "notifications/cancelled" => {
                tracing::debug!("cancellation notification received");
            }
            other => {
                tracing::debug!(method = %other, "unknown notification");
            }
        }
        return None;
    }

    let result = match request.method.as_str() {
        "initialize" => handle_initialize(),
        "tools/list" => handle_tools_list(registry),
        "tools/call" => handle_tools_call(request.params.unwrap_or(Value::Null), registry).await,
        "ping" => Ok(json!({})),
        other => Err(Response::error(
            id.clone(),
            -32601,
            format!("method not found: {other}"),
        )),
    };

    Some(match result {
        Ok(value) => Response::success(id, value),
        Err(resp) => resp,
    })
}

fn handle_initialize() -> Result<Value, Response> {
    Ok(json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION
        }
    }))
}

fn handle_tools_list(registry: &ToolRegistry) -> Result<Value, Response> {
    let defs = registry.definitions();
    let tools: Vec<Value> = defs
        .into_iter()
        .map(|d| {
            json!({
                "name": d.name,
                "description": d.description,
                "inputSchema": d.input_schema,
            })
        })
        .collect();

    Ok(json!({ "tools": tools }))
}

async fn handle_tools_call(params: Value, registry: &ToolRegistry) -> Result<Value, Response> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Response::error(None, -32602, "missing tool name in params".into())
        })?;

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    tracing::info!(tool = %name, args = %arguments, "calling tool");

    match registry.call(name, arguments).await {
        Ok(result) => Ok(serde_json::to_value(result).unwrap()),
        Err(e) => {
            tracing::error!(tool = %name, error = %e, "tool execution error");
            Ok(serde_json::to_value(mcp::ToolResult::error(format!(
                "internal error: {e}"
            )))
            .unwrap())
        }
    }
}
