//! td — thermal dispatch CLI.
//!
//! Send a message to the thermal-messages bus and print the response.
//!
//! Usage:
//!   td [@target] <message...>
//!
//! If the first argument starts with `@`, it is used as the target agent type.
//! Otherwise the target defaults to `@system`.
//!
//! Examples:
//!   td hello world              # sends to system/default
//!   td @claude build thermal    # sends to claude/default

use std::collections::HashMap;
use std::path::PathBuf;
use std::process;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use thermal_core::message::{AgentId, Message, MessageType};

fn socket_path() -> PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    PathBuf::from(format!("/run/user/{uid}/thermal/messages.sock"))
}

fn parse_args() -> Result<(String, String)> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: td [@target] <message...>");
        process::exit(1);
    }

    let (target, message_args) = if args[0].starts_with('@') {
        let target = args[0][1..].to_string();
        if args.len() < 2 {
            eprintln!("usage: td @target <message...>");
            process::exit(1);
        }
        (target, &args[1..])
    } else {
        ("system".to_string(), &args[..])
    };

    let content = message_args.join(" ");
    Ok((target, content))
}

#[tokio::main]
async fn main() -> Result<()> {
    let (target, content) = parse_args()?;

    let msg = Message {
        seq: 0,
        ts: 0,
        from: AgentId::new("cli", "td"),
        to: AgentId::new(&target, "default"),
        context_id: None,
        project: None,
        content,
        msg_type: MessageType::AgentMsg,
        metadata: HashMap::new(),
    };

    let sock = socket_path();
    let stream = UnixStream::connect(&sock).await.with_context(|| {
        format!(
            "could not connect to {} — is thermal-messages running?",
            sock.display()
        )
    })?;

    let (reader, mut writer) = stream.into_split();

    // Send JSONL message.
    let json = serde_json::to_string(&msg).context("serializing message")?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    // Read one response line.
    let mut lines = BufReader::new(reader).lines();
    match lines.next_line().await? {
        Some(line) => {
            // Try to pretty-print the content field; fall back to raw JSON.
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(ok) = val.get("ok") {
                    if ok.as_bool() == Some(true) {
                        println!("ok");
                    } else if let Some(err) = val.get("error") {
                        eprintln!("error: {}", err.as_str().unwrap_or(&line));
                        process::exit(1);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&val)?);
                    }
                } else if let Some(content) = val.get("content") {
                    println!("{}", content.as_str().unwrap_or(&line));
                } else {
                    println!("{}", serde_json::to_string_pretty(&val)?);
                }
            } else {
                println!("{line}");
            }
        }
        None => {
            eprintln!("no response from daemon");
            process::exit(1);
        }
    }

    Ok(())
}
