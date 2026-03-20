//! Thermal Conductor — CLI for orchestrating Claude agent therminals via the session daemon.
//!
//! CLI commands communicate with the session daemon over a Unix socket.
//! The daemon owns PTY sessions; clients spawn, list, send input, and kill sessions.

mod agent_timeline;
mod client;
mod daemon;
mod grid_renderer;
mod inject;
mod input;
mod kitty;
mod kitty_graphics;
mod osc633;
mod protocol;
mod pty;
mod terminal;
pub(crate) mod tui;
mod window;

use anyhow::{Result, Context, bail};
use clap::{Parser, Subcommand};
use thermal_core::{ClaudeSessionState, ClaudeStatePoller, ClaudeStatus};

use client::DaemonClient;

/// Thermal Conductor — orchestrate Claude agent therminals via the session daemon.
///
/// Run with no arguments to launch the interactive TUI dashboard.
/// Use `thc tui` explicitly, or just `thc` to start the dashboard.
#[derive(Parser)]
#[command(name = "thermal-conductor", version, about, after_help = "Run `thc` or `thc tui` to launch the interactive dashboard.")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Spawn new therminal sessions on the daemon
    Spawn {
        /// Number of therminals to spawn
        #[arg(short = 'n', long, default_value_t = 1)]
        count: u32,

        /// Project directory to start Claude in
        #[arg(short, long)]
        project: Option<String>,

        /// Shell to use (defaults to $SHELL)
        #[arg(short, long)]
        shell: Option<String>,

        /// Create a git worktree per session to avoid file-edit conflicts
        #[arg(short = 'w', long)]
        worktree: bool,
    },

    /// Show status of all tracked therminals with Claude state
    Status,

    /// Send text to a therminal session
    Send {
        /// Session id to send to
        session_id: String,

        /// Text/prompt to send
        prompt: String,
    },

    /// List all daemon sessions
    List {
        /// Output raw JSON instead of table
        #[arg(long)]
        json: bool,
    },

    /// Kill (close) a therminal session
    Kill {
        /// Session id to close
        session_id: String,
    },

    /// Toggle TTS audio announcements on/off
    Audio {
        #[command(subcommand)]
        action: AudioAction,
    },

    /// Launch the GPU-rendered terminal window
    Window,

    /// Start the session daemon (PTY ownership, Unix socket server)
    Daemon,

    /// Launch the interactive TUI dashboard (default when no subcommand given)
    Tui,
}

#[derive(Subcommand)]
enum AudioAction {
    /// Start TTS audio daemon
    On,
    /// Stop TTS audio daemon
    Off,
    /// Check if audio daemon is running
    Status,
    /// Test TTS with a message
    Test {
        /// Text to speak
        text: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("thermal_conductor=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    // Default to TUI when no subcommand is given.
    let command = cli.command.unwrap_or(Commands::Tui);

    // TUI runs its own synchronous event loop — no tokio needed.
    if matches!(command, Commands::Tui) {
        return tui::run();
    }

    // Window subcommand manages its own tokio runtime (for PTY async I/O),
    // so it must run outside of #[tokio::main] to avoid nested runtime panic.
    if matches!(command, Commands::Window) {
        return window::run();
    }

    // Daemon subcommand runs a long-lived async event loop.
    if matches!(command, Commands::Daemon) {
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(daemon::run_daemon());
    }

    // All other subcommands talk to the session daemon via DaemonClient.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            match command {
                Commands::Spawn {
                    count,
                    project,
                    shell,
                    worktree,
                } => cmd_spawn(count, project, shell, worktree).await,
                Commands::Status => cmd_status().await,
                Commands::Send { session_id, prompt } => cmd_send(session_id, prompt).await,
                Commands::List { json } => cmd_list(json).await,
                Commands::Kill { session_id } => cmd_kill(session_id).await,
                Commands::Audio { action } => cmd_audio(action).await,
                Commands::Window => unreachable!(),
                Commands::Daemon => unreachable!(),
                Commands::Tui => unreachable!(),
            }
        })
}

/// Connect to the session daemon, or print an error and exit if it's not running.
async fn connect_daemon() -> Result<DaemonClient> {
    match DaemonClient::connect().await? {
        Some(client) => Ok(client),
        None => {
            eprintln!("Session daemon not running. Start with: thc daemon");
            std::process::exit(1);
        }
    }
}

/// Spawn N therminal sessions on the daemon.
async fn cmd_spawn(count: u32, project: Option<String>, shell: Option<String>, worktree: bool) -> Result<()> {
    let mut client = connect_daemon().await?;

    let wt_label = if worktree { " (with worktrees)" } else { "" };
    println!("Spawning {count} therminal{}{wt_label}...", if count == 1 { "" } else { "s" });

    for _i in 0..count {
        let id = client.spawn_session(shell.clone(), project.clone(), worktree).await?;
        println!("  Therminal spawned (session: {id})");
    }

    println!(
        "{count} therminal{} spawned.",
        if count == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Show status of all Claude sessions from state files.
/// This reads directly from /tmp/claude-code-state/ — no daemon dependency needed.
async fn cmd_status() -> Result<()> {
    let sessions: Vec<ClaudeSessionState> = match ClaudeStatePoller::new() {
        Ok(poller) => poller.get_all(),
        Err(e) => {
            println!("No Claude state available: {e}");
            return Ok(());
        }
    };

    if sessions.is_empty() {
        println!("No active Claude sessions.");
        return Ok(());
    }

    for session in &sessions {
        let label = session.working_dir.as_deref()
            .and_then(|d| std::path::Path::new(d).file_name())
            .and_then(|n| n.to_str())
            .unwrap_or(&session.session_id);

        let status = format_claude_status(&session.status);

        let tool = session.current_tool.as_deref().unwrap_or("-");

        let context = session.context_percent
            .map(|p| format!("{:.0}%", p))
            .unwrap_or_else(|| "-".to_string());

        let agents = session.subagent_count.unwrap_or(0);
        let agent_str = if agents > 0 { format!("  agents: {agents}") } else { String::new() };

        println!("  {label}  |  {status}  |  tool: {tool}  |  ctx: {context}{agent_str}");
    }

    println!("\n{} session{}.", sessions.len(), if sessions.len() == 1 { "" } else { "s" });
    Ok(())
}

/// Send text/prompt to a specific therminal session.
async fn cmd_send(session_id: String, prompt: String) -> Result<()> {
    let mut client = connect_daemon().await?;

    // Send the text as bytes, appending a newline to simulate pressing Enter.
    let mut data = prompt.into_bytes();
    data.push(b'\n');

    client.send_input(&session_id, data).await?;
    println!("Sent to session {session_id}.");
    Ok(())
}

/// List all sessions from the daemon.
async fn cmd_list(json: bool) -> Result<()> {
    let mut client = connect_daemon().await?;
    let sessions = client.list_sessions().await?;

    if json {
        let pretty = serde_json::to_string_pretty(&sessions)?;
        println!("{pretty}");
        return Ok(());
    }

    if sessions.is_empty() {
        println!("No active sessions.");
        return Ok(());
    }

    // Compact table output
    for session in &sessions {
        let alive = if session.is_alive { "" } else { " (exited)" };
        let clients = if session.connected_client_count > 0 {
            format!("  clients: {}", session.connected_client_count)
        } else {
            String::new()
        };
        println!(
            "  [{}] {}  ({}x{})  pid: {}  cwd: {}{}{}",
            session.id,
            session.title,
            session.cols,
            session.rows,
            session.shell_pid,
            session.cwd,
            alive,
            clients,
        );
    }
    println!(
        "\n{} session{}.",
        sessions.len(),
        if sessions.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Kill (close) a therminal session.
async fn cmd_kill(session_id: String) -> Result<()> {
    let mut client = connect_daemon().await?;

    client.kill_session(&session_id).await?;
    println!("Session {session_id} killed.");
    Ok(())
}

/// Toggle thermal-audio daemon.
async fn cmd_audio(action: AudioAction) -> Result<()> {
    match action {
        AudioAction::On => {
            // Check if already running
            let check = tokio::process::Command::new("pgrep")
                .arg("-x")
                .arg("thermal-audio")
                .output()
                .await?;
            if check.status.success() {
                println!("Audio daemon already running.");
                return Ok(());
            }
            tokio::process::Command::new("thermal-audio")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("failed to start thermal-audio — is it installed?")?;
            println!("Audio daemon started.");
        }
        AudioAction::Off => {
            let result = tokio::process::Command::new("pkill")
                .arg("-x")
                .arg("thermal-audio")
                .output()
                .await?;
            if result.status.success() {
                println!("Audio daemon stopped.");
            } else {
                println!("Audio daemon not running.");
            }
        }
        AudioAction::Status => {
            let check = tokio::process::Command::new("pgrep")
                .arg("-x")
                .arg("thermal-audio")
                .output()
                .await?;
            if check.status.success() {
                let pid = String::from_utf8_lossy(&check.stdout).trim().to_string();
                println!("Audio daemon running (pid: {pid}).");
            } else {
                println!("Audio daemon not running.");
            }
        }
        AudioAction::Test { text } => {
            let status = tokio::process::Command::new("thermal-audio")
                .arg("--test")
                .arg(&text)
                .status()
                .await
                .context("failed to run thermal-audio --test")?;
            if !status.success() {
                bail!("thermal-audio test failed");
            }
        }
    }
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
