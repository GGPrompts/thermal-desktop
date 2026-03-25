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
pub(crate) mod profiles_config;
mod protocol;
mod pty;
mod terminal;
pub(crate) mod tui;
mod window;

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, Context, bail};
use clap::{Parser, Subcommand};
use thermal_core::{ClaudeSessionState, ClaudeStatePoller, ClaudeStatus};

use kitty::KittyController;

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
    /// Spawn new therminal sessions in kitty
    Spawn {
        /// Number of therminals to spawn
        #[arg(short = 'n', long, default_value_t = 1)]
        count: u32,

        /// Project directory to start in
        #[arg(short, long)]
        project: Option<String>,

        /// Command to run (defaults to $SHELL)
        #[arg(short, long)]
        command: Option<String>,

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

    // All other subcommands use KittyController (or are self-contained).
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            match command {
                Commands::Spawn {
                    count,
                    project,
                    command,
                    worktree,
                } => cmd_spawn(count, project, command, worktree).await,
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

/// Ensure KittyController is available, or exit with a clear error.
async fn require_kitty(controller: &KittyController) {
    if !controller.is_available().await {
        eprintln!("Kitty remote control not available. Is kitty running with allow_remote_control enabled?");
        std::process::exit(1);
    }
}

/// Spawn N therminal sessions via kitty remote control.
async fn cmd_spawn(count: u32, project: Option<String>, command: Option<String>, worktree: bool) -> Result<()> {
    let controller = KittyController::new();
    require_kitty(&controller).await;

    let cmd = command.unwrap_or_else(|| {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    });

    let cwd = project.unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()))
    });

    let wt_label = if worktree { " (with worktrees)" } else { "" };
    println!("Spawning {count} therminal{}{wt_label}...", if count == 1 { "" } else { "s" });

    for _i in 0..count {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let id = format!("session-{ts}");

        // Optionally create a git worktree for this session.
        let (effective_cwd, wt_path) = if worktree {
            match cmd_create_worktree(&cwd, &id) {
                Ok(wt) => (wt.clone(), Some(wt)),
                Err(e) => {
                    eprintln!("  Warning: worktree creation failed ({e}), using original cwd");
                    (cwd.clone(), None)
                }
            }
        } else {
            (cwd.clone(), None)
        };

        controller
            .spawn(
                &id,
                &cmd,
                &effective_cwd,
                None,
                wt_path.as_deref(),
            )
            .await?;
        println!("  Therminal spawned (session: {id})");
    }

    println!(
        "{count} therminal{} spawned.",
        if count == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Create a git worktree for a session.
///
/// Mirrors the logic in `daemon::SessionDaemon::create_worktree()`: resolves the
/// git repo root from `cwd`, then creates a detached worktree at
/// `/tmp/thermal-worktrees/{repo_name}-{session_id}`.
///
/// Public so the TUI spawn page can also use it.
pub fn cmd_create_worktree(cwd: &str, session_id: &str) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .context("Failed to run git rev-parse")?;

    if !output.status.success() {
        bail!("Not a git repository: {cwd}");
    }

    let repo_root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let repo_name = std::path::Path::new(&repo_root)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());

    let worktree_dir = format!("/tmp/thermal-worktrees/{repo_name}-{session_id}");

    std::fs::create_dir_all("/tmp/thermal-worktrees")
        .context("Failed to create /tmp/thermal-worktrees")?;

    let wt_output = std::process::Command::new("git")
        .args(["worktree", "add", &worktree_dir, "HEAD"])
        .current_dir(&repo_root)
        .output()
        .context("Failed to run git worktree add")?;

    if !wt_output.status.success() {
        let stderr = String::from_utf8_lossy(&wt_output.stderr);
        bail!("git worktree add failed: {stderr}");
    }

    Ok(worktree_dir)
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

/// Send text/prompt to a specific therminal session via kitty.
async fn cmd_send(session_id: String, prompt: String) -> Result<()> {
    let controller = KittyController::new();
    require_kitty(&controller).await;

    // Append newline to simulate pressing Enter (kitty send-text sends literal text).
    let text = format!("{prompt}\n");
    controller.send_text(&session_id, &text).await?;
    println!("Sent to session {session_id}.");
    Ok(())
}

/// List all thermal sessions from kitty.
async fn cmd_list(json: bool) -> Result<()> {
    let controller = KittyController::new();
    require_kitty(&controller).await;

    let windows = controller.list_windows().await?;

    if json {
        // WindowInfo doesn't derive Serialize, so build JSON manually.
        let json_array: Vec<serde_json::Value> = windows
            .iter()
            .map(|w| {
                serde_json::json!({
                    "session_id": w.session_id,
                    "kitty_window_id": w.kitty_window_id,
                    "cwd": w.cwd,
                    "title": w.title,
                    "is_focused": w.is_focused,
                    "foreground_command": w.foreground_command,
                    "worktree_path": w.worktree_path,
                    "profile_name": w.profile_name,
                    "original_cwd": w.original_cwd,
                    "spawn_time": w.spawn_time,
                })
            })
            .collect();
        let pretty = serde_json::to_string_pretty(&json_array)?;
        println!("{pretty}");
        return Ok(());
    }

    if windows.is_empty() {
        println!("No active thermal sessions.");
        return Ok(());
    }

    // Compact table output: session ID, foreground command, focused, cwd
    for w in &windows {
        let cmd = w.foreground_command.as_deref().unwrap_or("-");
        let focused = if w.is_focused { " *" } else { "" };
        let profile = w.profile_name.as_deref().map(|p| format!("  profile: {p}")).unwrap_or_default();
        println!(
            "  [{}]  cmd: {}  cwd: {}{}{}",
            w.session_id,
            cmd,
            w.cwd,
            focused,
            profile,
        );
    }
    println!(
        "\n{} session{}.",
        windows.len(),
        if windows.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Kill (close) a therminal session via kitty.
async fn cmd_kill(session_id: String) -> Result<()> {
    let controller = KittyController::new();
    require_kitty(&controller).await;

    controller.close_window(&session_id).await?;
    println!("Session {session_id} closed.");
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
