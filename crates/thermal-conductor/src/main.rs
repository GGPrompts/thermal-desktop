//! Thermal Conductor — kitty remote control CLI for orchestrating Claude agent therminals.
//!
//! Spawns, tracks, polls, and sends to kitty windows running Claude sessions.
//! Hyprland auto-tiles the spawned OS windows.

mod kitty;

use anyhow::Result;
use clap::{Parser, Subcommand};
use thermal_core::{ClaudeSessionState, ClaudeStatePoller, ClaudeStatus};

use kitty::KittyController;

/// Thermal Conductor — orchestrate Claude agent therminals via kitty remote control.
#[derive(Parser)]
#[command(name = "thermal-conductor", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Spawn new kitty therminals running Claude
    Spawn {
        /// Number of therminals to spawn
        #[arg(short = 'n', long, default_value_t = 1)]
        count: u32,

        /// Project directory to start Claude in
        #[arg(short, long)]
        project: Option<String>,

        /// Title prefix for spawned windows
        #[arg(short, long, default_value = "Therminal")]
        title: String,
    },

    /// Show status of all tracked therminals with Claude state
    Status,

    /// Send text to a therminal
    Send {
        /// Kitty window id to send to
        window_id: u64,

        /// Text/prompt to send
        prompt: String,
    },

    /// List all kitty windows
    List {
        /// Output raw JSON instead of table
        #[arg(long)]
        json: bool,
    },

    /// Kill (close) a therminal
    Kill {
        /// Kitty window id to close
        window_id: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("thermal_conductor=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Spawn {
            count,
            project,
            title,
        } => cmd_spawn(count, project, title).await,
        Commands::Status => cmd_status().await,
        Commands::Send { window_id, prompt } => cmd_send(window_id, prompt).await,
        Commands::List { json } => cmd_list(json).await,
        Commands::Kill { window_id } => cmd_kill(window_id).await,
    }
}

/// Spawn N therminals running Claude.
async fn cmd_spawn(count: u32, project: Option<String>, title: String) -> Result<()> {
    let kitty = KittyController::new()?;

    println!("Spawning {count} therminal{}...", if count == 1 { "" } else { "s" });

    for i in 0..count {
        let window_title = if count == 1 {
            title.clone()
        } else {
            format!("{title} {}", i + 1)
        };

        let command_args: Vec<&str> = vec!["claude"];
        let cwd = project.as_deref();

        let window_id = kitty.spawn(&window_title, &command_args, cwd).await?;
        println!("  Therminal \"{}\" spawned (window id: {})", window_title, window_id);
    }

    println!(
        "{count} therminal{} active.",
        if count == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Show status of all therminals by cross-referencing kitty windows with Claude state.
async fn cmd_status() -> Result<()> {
    let kitty = KittyController::new()?;

    // Get kitty windows
    let windows_json = kitty.list_windows().await?;

    // Get Claude session states
    let sessions: Vec<ClaudeSessionState> = match ClaudeStatePoller::new() {
        Ok(poller) => poller.get_all(),
        Err(e) => {
            tracing::warn!("could not read Claude state: {e}");
            Vec::new()
        }
    };

    // Collect all kitty windows across OS windows and tabs
    let mut window_count = 0u32;

    if let Some(os_windows) = windows_json.as_array() {
        for os_window in os_windows {
            if let Some(tabs) = os_window.get("tabs").and_then(|t| t.as_array()) {
                for tab in tabs {
                    if let Some(windows) = tab.get("windows").and_then(|w| w.as_array()) {
                        for window in windows {
                            let id = window.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                            let title = window
                                .get("title")
                                .and_then(|v| v.as_str())
                                .unwrap_or("untitled");
                            let pid = window.get("pid").and_then(|v| v.as_u64());
                            let is_focused = window
                                .get("is_focused")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);

                            // Try to match with a Claude session by PID
                            let claude_state = pid.and_then(|window_pid| {
                                sessions.iter().find(|s| {
                                    s.pid.map_or(false, |sp| sp as u64 == window_pid)
                                })
                            });

                            let status_str = match claude_state {
                                Some(state) => format_claude_status(&state.status),
                                None => "unknown".to_string(),
                            };

                            let tool_str = claude_state
                                .and_then(|s| s.current_tool.as_deref())
                                .unwrap_or("-");

                            let context_str = claude_state
                                .and_then(|s| s.context_percent)
                                .map(|p| format!("{:.0}%", p))
                                .unwrap_or_else(|| "-".to_string());

                            let focus_marker = if is_focused { " *" } else { "" };

                            println!(
                                "  [{id}] {title}{focus_marker}  |  status: {status_str}  |  tool: {tool_str}  |  context: {context_str}"
                            );
                            window_count += 1;
                        }
                    }
                }
            }
        }
    }

    println!("\n{window_count} therminal{} tracked.", if window_count == 1 { "" } else { "s" });
    Ok(())
}

/// Send text/prompt to a specific therminal.
async fn cmd_send(window_id: u64, prompt: String) -> Result<()> {
    let kitty = KittyController::new()?;
    let match_arg = format!("id:{window_id}");

    kitty.send_text(&match_arg, &prompt).await?;
    println!("Sent to therminal {window_id}.");
    Ok(())
}

/// List all kitty windows.
async fn cmd_list(json: bool) -> Result<()> {
    let kitty = KittyController::new()?;
    let windows_json = kitty.list_windows().await?;

    if json {
        let pretty = serde_json::to_string_pretty(&windows_json)?;
        println!("{pretty}");
        return Ok(());
    }

    // Compact table output
    let mut count = 0u32;
    if let Some(os_windows) = windows_json.as_array() {
        for os_window in os_windows {
            if let Some(tabs) = os_window.get("tabs").and_then(|t| t.as_array()) {
                for tab in tabs {
                    if let Some(windows) = tab.get("windows").and_then(|w| w.as_array()) {
                        for window in windows {
                            let id = window.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                            let title = window.get("title").and_then(|v| v.as_str()).unwrap_or("untitled");
                            let pid = window.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
                            let is_focused = window.get("is_focused").and_then(|v| v.as_bool()).unwrap_or(false);
                            let focus = if is_focused { " *" } else { "" };
                            println!("  [{id}] {title}{focus}  (pid: {pid})");
                            count += 1;
                        }
                    }
                }
            }
        }
    }
    println!("\n{count} window{}.", if count == 1 { "" } else { "s" });
    Ok(())
}

/// Kill (close) a therminal.
async fn cmd_kill(window_id: u64) -> Result<()> {
    let kitty = KittyController::new()?;
    let match_arg = format!("id:{window_id}");

    kitty.close_window(&match_arg).await?;
    println!("Therminal {window_id} closed.");
    Ok(())
}

/// Format a ClaudeStatus for display.
fn format_claude_status(status: &ClaudeStatus) -> String {
    match status {
        ClaudeStatus::Idle => "idle".to_string(),
        ClaudeStatus::Processing => "processing".to_string(),
        ClaudeStatus::ToolUse => "tool_use".to_string(),
        ClaudeStatus::AwaitingInput => "awaiting_input".to_string(),
    }
}
