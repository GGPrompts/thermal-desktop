//! Thermal Conductor — CLI for orchestrating Claude agent therminals via the session daemon.
//!
//! CLI commands communicate with the session daemon over a Unix socket.
//! The daemon owns PTY sessions; clients spawn, list, send input, and kill sessions.

mod agent_graph;
mod agent_timeline;
pub(crate) mod backend;
mod bar;
mod client;
mod color_mapping;
mod config;
mod context_environment;
mod daemon;
pub(crate) mod daemon_lifecycle;
mod daemon_subscriber;
mod dbus_interface;
mod doctor;
mod environment_pipeline;
mod font_config;
mod grid_renderer;
mod heatmap_pipeline;
mod hud;
mod hud_overlays;
mod image_pipeline;
mod inject;
mod input;
mod kitty;
mod kitty_graphics;
pub(crate) mod messages;
mod monitor;
mod osc633;
mod persist;
pub(crate) mod profiles_config;
mod protocol;
mod pty;
mod semantic_state;
mod session_log;
mod structured_output;
mod swarm_watcher;
mod terminal;
mod transcript_watcher;
mod viewer;
pub(crate) mod tui;
mod window;

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use thermal_core::message::{AgentId, Message, MessageType};
use thermal_core::{ClaudeSessionState, ClaudeStatePoller, ClaudeStatus};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tracing::{info, warn};

use backend::{Backend, BackendPreference, detect_backend};

/// Thermal Conductor — orchestrate Claude agent therminals via the session daemon.
///
/// Run with no arguments to launch the interactive TUI dashboard.
/// Use `thc tui` explicitly, or just `thc` to start the dashboard.
#[derive(Parser)]
#[command(
    name = "thermal-conductor",
    version,
    about,
    after_help = "Run `thc` or `thc tui` to launch the interactive dashboard."
)]
struct Cli {
    /// Session backend: auto (try kitty then daemon), kitty, or daemon
    #[arg(long, default_value = "auto", global = true)]
    backend: BackendPreference,

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

    /// Speak text via the running TTS daemon (thermal-audio)
    #[command(trailing_var_arg = true)]
    Say {
        /// Text to speak (multiple words joined automatically)
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,

        /// Voice name (e.g. en-US-GuyNeural, en-GB-SoniaNeural)
        #[arg(short, long)]
        voice: Option<String>,
    },

    /// Launch the GPU-rendered terminal window
    Window {
        /// Attach to an existing daemon session instead of spawning a new one.
        /// Used by the TUI profiles page to connect a visible window to a
        /// daemon-owned PTY session.
        #[arg(long)]
        session: Option<String>,

        /// Command to run instead of the default shell.
        /// Everything after `--` is treated as the command and its arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// Start the session daemon (PTY ownership, Unix socket server)
    Daemon,

    /// Launch the interactive TUI dashboard (default when no subcommand given)
    Tui,

    /// Check health of all thermal daemons (PID liveness, socket connectivity)
    Doctor {
        /// Auto-fix: clean stale PID/socket files and restart dead core daemons
        #[arg(long)]
        fix: bool,

        /// Write a full diagnostic report to /tmp/thermal-doctor-<timestamp>.txt
        #[arg(long)]
        report: bool,
    },

    /// Show all effective settings with their sources (env, toml, default)
    Config,

    /// Run headless self-tests: cargo check, unit tests, daemon health
    Smoke {
        /// Auto-fix daemon issues discovered by doctor checks
        #[arg(long)]
        fix: bool,
    },

    /// Send a message to the thermal-messages bus
    #[command(trailing_var_arg = true)]
    Dispatch {
        /// Target agent type (e.g. "claude", "system"). Defaults to "system".
        #[arg(short, long)]
        target: Option<String>,

        /// Message text (multiple words joined automatically)
        #[arg(required = true, num_args = 1..)]
        message: Vec<String>,
    },

    /// Launch the session monitor TUI (read-only dashboard)
    Monitor,

    /// View a JSONL session file with rich thermal-themed formatting
    View {
        /// Path to the JSONL file to view
        path: std::path::PathBuf,
    },
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

fn init_tui_tracing(env_filter: tracing_subscriber::EnvFilter) -> Option<std::path::PathBuf> {
    let primary = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .map(|dir| dir.join("thermal").join("conductor-tui.log"));
    let fallback = std::path::PathBuf::from("/tmp/thermal-conductor-tui.log");

    let mut candidates = Vec::new();
    if let Some(path) = primary {
        candidates.push(path);
    }
    if !candidates.iter().any(|path| path == &fallback) {
        candidates.push(fallback);
    }

    for path in candidates {
        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            continue;
        }

        let log_file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(_) => continue,
        };

        tracing_subscriber::fmt()
            .with_env_filter(env_filter.clone())
            .with_writer(log_file)
            .with_ansi(false)
            .init();
        return Some(path);
    }

    None
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Default to TUI when no subcommand is given.
    let command = cli.command.unwrap_or(Commands::Tui);

    let env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive("thermal_conductor=info".parse().unwrap())
        .add_directive("thermal_core=info".parse().unwrap());

    let mut tui_log_path: Option<std::path::PathBuf> = None;

    // Doctor and Config are quick diagnostics — suppress tracing noise.
    if matches!(
        command,
        Commands::Doctor { .. } | Commands::Config | Commands::Smoke { .. }
    ) {
        // No tracing init — just run silently.
    } else if matches!(command, Commands::Tui | Commands::Monitor | Commands::View { .. }) {
        // In TUI mode, log only to files so tracing never corrupts the
        // alternate screen. If all file paths fail, tracing stays disabled.
        tui_log_path = init_tui_tracing(env_filter);
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let backend_pref = cli.backend;

    info!("thermal-conductor starting");

    // TUI runs its own synchronous event loop — no tokio needed.
    if matches!(command, Commands::Tui) {
        let result = tui::run(backend_pref);
        if let Some(ref path) = tui_log_path {
            eprintln!("TUI logs → {}", path.display());
        }
        return result;
    }

    // Monitor runs its own synchronous ratatui event loop.
    if matches!(command, Commands::Monitor) {
        return monitor::run();
    }

    // View runs its own ratatui event loop for JSONL viewing.
    if let Commands::View { ref path } = command {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        return rt.block_on(viewer::run(path.clone()));
    }

    // Window subcommand manages its own tokio runtime (for PTY async I/O),
    // so it must run outside of #[tokio::main] to avoid nested runtime panic.
    if let Commands::Window {
        ref session,
        ref command,
    } = command
    {
        let cmd = if command.is_empty() {
            None
        } else {
            Some(command.clone())
        };
        return window::run(session.clone(), cmd);
    }

    // Daemon subcommand runs a long-lived async event loop.
    if matches!(command, Commands::Daemon) {
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(daemon::run_daemon());
    }

    // All other subcommands use a detected backend (or are self-contained).
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
                } => cmd_spawn(count, project, command, worktree, backend_pref).await,
                Commands::Status => cmd_status().await,
                Commands::Send { session_id, prompt } => {
                    cmd_send(session_id, prompt, backend_pref).await
                }
                Commands::List { json } => cmd_list(json, backend_pref).await,
                Commands::Kill { session_id } => cmd_kill(session_id, backend_pref).await,
                Commands::Audio { action } => cmd_audio(action).await,
                Commands::Say { text, voice } => cmd_say(text.join(" "), voice).await,
                Commands::Doctor { fix, report } => doctor::cmd_doctor(fix, report).await,
                Commands::Config => config::cmd_config().await,
                Commands::Smoke { fix } => doctor::cmd_smoke(fix).await,
                Commands::Dispatch { target, message } => cmd_dispatch(target, message).await,
                Commands::Window { .. } => unreachable!(),
                Commands::Daemon => unreachable!(),
                Commands::Tui => unreachable!(),
                Commands::Monitor => unreachable!(),
                Commands::View { .. } => unreachable!(),
            }
        })
}

/// Spawn N therminal sessions via the detected backend.
async fn cmd_spawn(
    count: u32,
    project: Option<String>,
    command: Option<String>,
    worktree: bool,
    pref: BackendPreference,
) -> Result<()> {
    let mut backend = detect_backend(pref).await?;

    let cmd =
        command.unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));

    let cwd = project.unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()))
    });

    let wt_label = if worktree { " (with worktrees)" } else { "" };
    println!(
        "Spawning {count} therminal{}{wt_label} via {}...",
        if count == 1 { "" } else { "s" },
        backend.name()
    );

    for i in 0..count {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let id = format!("session-{ts}-{i}");

        match &mut backend {
            Backend::Kitty(controller) => {
                // Optionally create a git worktree for this session.
                let (effective_cwd, wt_path) = if worktree {
                    match cmd_create_worktree(&cwd, &id) {
                        Ok(wt) => (wt.clone(), Some(wt)),
                        Err(e) => {
                            warn!(
                                error = %e,
                                "worktree creation failed, using original cwd"
                            );
                            (cwd.clone(), None)
                        }
                    }
                } else {
                    (cwd.clone(), None)
                };

                controller
                    .spawn(&id, &cmd, &effective_cwd, None, wt_path.as_deref())
                    .await?;
            }
            Backend::Daemon(client) => {
                let spawned_id = client
                    .spawn_session(Some(cmd.clone()), Some(cwd.clone()), worktree)
                    .await?;
                println!("  Therminal spawned (session: {spawned_id})");
                continue; // daemon prints its own ID
            }
        }
        println!("  Therminal spawned (session: {id})");
    }

    println!(
        "{count} therminal{} spawned via {}.",
        if count == 1 { "" } else { "s" },
        backend.name(),
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
///
/// # State source: compatibility/file-derived (standalone CLI)
///
/// This reads directly from `/tmp/*-state/` files via `ClaudeStatePoller`.
/// It is intentionally daemon-independent so `thc status` works even when
/// the daemon is not running. Sessions shown here are always file-derived.
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
        let label = session
            .working_dir
            .as_deref()
            .and_then(|d| std::path::Path::new(d).file_name())
            .and_then(|n| n.to_str())
            .unwrap_or(&session.session_id);

        let status = format_claude_status(&session.status);

        let tool = session.current_tool.as_deref().unwrap_or("-");

        let context = session
            .context_percent
            .map(|p| format!("{:.0}%", p))
            .unwrap_or_else(|| "-".to_string());

        let agents = session.subagent_count.unwrap_or(0);
        let agent_str = if agents > 0 {
            format!("  agents: {agents}")
        } else {
            String::new()
        };

        println!("  {label}  |  {status}  |  tool: {tool}  |  ctx: {context}{agent_str}");
    }

    println!(
        "\n{} session{}.",
        sessions.len(),
        if sessions.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Send text/prompt to a specific therminal session.
async fn cmd_send(session_id: String, prompt: String, pref: BackendPreference) -> Result<()> {
    let mut backend = detect_backend(pref).await?;

    match &mut backend {
        Backend::Kitty(controller) => {
            // Append newline to simulate pressing Enter (kitty send-text sends literal text).
            let text = format!("{prompt}\n");
            controller.send_text(&session_id, &text).await?;
        }
        Backend::Daemon(client) => {
            let data = format!("{prompt}\n").into_bytes();
            client.send_input(&session_id, data).await?;
        }
    }

    println!("Sent to session {session_id} via {}.", backend.name());
    Ok(())
}

/// List all thermal sessions.
async fn cmd_list(json: bool, pref: BackendPreference) -> Result<()> {
    let mut backend = detect_backend(pref).await?;

    match &mut backend {
        Backend::Kitty(controller) => {
            let windows = controller.list_windows().await?;

            if json {
                // WindowInfo doesn't derive Serialize, so build JSON manually.
                let json_array: Vec<serde_json::Value> = windows
                    .iter()
                    .map(|w| {
                        serde_json::json!({
                            "backend": "kitty",
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
                println!("No active thermal sessions (backend: kitty).");
                return Ok(());
            }

            for w in &windows {
                let cmd = w.foreground_command.as_deref().unwrap_or("-");
                let focused = if w.is_focused { " *" } else { "" };
                let profile = w
                    .profile_name
                    .as_deref()
                    .map(|p| format!("  profile: {p}"))
                    .unwrap_or_default();
                println!(
                    "  [{}]  cmd: {}  cwd: {}{}{}",
                    w.session_id, cmd, w.cwd, focused, profile,
                );
            }
            println!(
                "\n{} session{} (backend: kitty).",
                windows.len(),
                if windows.len() == 1 { "" } else { "s" }
            );
        }
        Backend::Daemon(client) => {
            let sessions = client.list_sessions().await?;

            if json {
                let json_array: Vec<serde_json::Value> = sessions
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "backend": "daemon",
                            "session_id": s.id,
                            "shell_command": s.shell_command,
                            "cwd": s.cwd,
                            "title": s.title,
                            "shell_pid": s.shell_pid,
                            "cols": s.cols,
                            "rows": s.rows,
                            "start_time": s.start_time,
                            "connected_clients": s.connected_client_count,
                            "is_alive": s.is_alive,
                            "worktree_path": s.worktree_path,
                        })
                    })
                    .collect();
                let pretty = serde_json::to_string_pretty(&json_array)?;
                println!("{pretty}");
                return Ok(());
            }

            if sessions.is_empty() {
                println!("No active thermal sessions (backend: daemon).");
                return Ok(());
            }

            for s in &sessions {
                let alive = if s.is_alive { "" } else { " (dead)" };
                let wt = s
                    .worktree_path
                    .as_deref()
                    .map(|p| format!("  wt: {p}"))
                    .unwrap_or_default();
                println!(
                    "  [{}]  shell: {}  cwd: {}  pid: {}{}{}",
                    s.id, s.shell_command, s.cwd, s.shell_pid, alive, wt,
                );
            }
            println!(
                "\n{} session{} (backend: daemon).",
                sessions.len(),
                if sessions.len() == 1 { "" } else { "s" }
            );
        }
    }
    Ok(())
}

/// Kill (close) a therminal session.
async fn cmd_kill(session_id: String, pref: BackendPreference) -> Result<()> {
    let mut backend = detect_backend(pref).await?;

    match &mut backend {
        Backend::Kitty(controller) => {
            controller.close_window(&session_id).await?;
        }
        Backend::Daemon(client) => {
            client.kill_session(&session_id).await?;
        }
    }

    println!("Session {session_id} closed via {}.", backend.name());
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

/// Speak text via the running thermal-audio daemon's Unix socket.
async fn cmd_say(text: String, voice: Option<String>) -> Result<()> {
    let sock_path = thermal_core::runtime::socket_path("audio");

    let stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .with_context(|| {
            format!(
                "cannot connect to audio daemon at {} — is thermal-audio running?",
                sock_path.display()
            )
        })?;

    let mut request = serde_json::json!({
        "action": "tts",
        "text": text,
    });
    if let Some(v) = voice {
        request["voice"] = serde_json::Value::String(v);
    }

    let (reader, mut writer) = stream.into_split();
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;
    writer.flush().await?;

    // Read ack
    let mut buf_reader = tokio::io::BufReader::new(reader);
    let mut response = String::new();
    buf_reader.read_line(&mut response).await?;

    let parsed: serde_json::Value = serde_json::from_str(response.trim()).unwrap_or_default();
    if parsed.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        // Silent success
    } else {
        let err = parsed
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        bail!("TTS failed: {err}");
    }

    Ok(())
}

/// Send a message through an ephemeral message bus instance.
/// The bus handles routing (e.g. @system -> thermal-commander) and
/// prints any response.
async fn cmd_dispatch(target: Option<String>, message: Vec<String>) -> Result<()> {
    let target = target.unwrap_or_else(|| "system".to_string());
    let content = message.join(" ");

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

    // Create an ephemeral bus (no persistence for one-shot dispatch).
    let bus = std::sync::Arc::new(messages::MessageBus::new(false)?);

    // Subscribe before sending so we can capture the routing response.
    let mut rx = bus.subscribe();

    bus.send(msg).await;

    // Drain broadcast channel to find the response message.
    // Give routing up to 30 seconds to complete.
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(arc_msg) => {
                        // Skip the original message (seq 1) — look for the response.
                        if arc_msg.seq > 1 {
                            if !arc_msg.content.is_empty() {
                                println!("{}", arc_msg.content);
                            } else {
                                println!("ok");
                            }
                            break;
                        }
                    }
                    Err(_) => {
                        println!("ok");
                        break;
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                eprintln!("timeout waiting for routing response");
                std::process::exit(1);
            }
        }
    }

    bus.flush_persist().await;
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
