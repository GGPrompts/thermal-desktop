//! Thermal Conductor — CLI for orchestrating Claude agent therminals via the session daemon.
//!
//! CLI commands communicate with the session daemon over a Unix socket.
//! The daemon owns PTY sessions; clients spawn, list, send input, and kill sessions.

mod agent_graph;
mod agent_timeline;
mod bar;
pub(crate) mod backend;
mod client;
mod color_mapping;
mod context_environment;
mod daemon;
mod daemon_subscriber;
mod dbus_interface;
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
mod structured_output;
mod swarm_watcher;
mod terminal;
mod transcript_watcher;
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
    } else if matches!(command, Commands::Tui | Commands::Monitor) {
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
                Commands::Doctor { fix, report } => cmd_doctor(fix, report).await,
                Commands::Config => cmd_config().await,
                Commands::Smoke { fix } => cmd_smoke(fix).await,
                Commands::Dispatch { target, message } => {
                    cmd_dispatch(target, message).await
                }
                Commands::Window { .. } => unreachable!(),
                Commands::Daemon => unreachable!(),
                Commands::Tui => unreachable!(),
                Commands::Monitor => unreachable!(),
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

// ---------------------------------------------------------------------------
// thc doctor — daemon health checker + diagnostic report
// ---------------------------------------------------------------------------

struct DaemonSpec {
    name: &'static str,
    /// Short name used with runtime::pidfile_path() / runtime::socket_path().
    short_name: &'static str,
    has_pidfile: bool,
    has_socket: bool,
    restart_cmd: Option<&'static [&'static str]>,
}

static DAEMONS: &[DaemonSpec] = &[
    DaemonSpec {
        name: "thermal-conductor",
        short_name: "conductor",
        has_pidfile: false,
        has_socket: true,
        restart_cmd: None,
    },
    DaemonSpec {
        name: "thermal-audio",
        short_name: "audio",
        has_pidfile: true,
        has_socket: true,
        restart_cmd: Some(&["thermal-audio"]),
    },
    DaemonSpec {
        name: "thermal-dispatcher",
        short_name: "dispatcher",
        has_pidfile: true,
        has_socket: false,
        restart_cmd: Some(&["thermal-dispatcher"]),
    },
];

/// Stale artifacts from removed daemons that should be cleaned up on `--fix`.
/// Format: (short_name, has_pidfile, has_socket)
static REMOVED_DAEMON_ARTIFACTS: &[(&str, bool, bool)] = &[
    ("bar", true, false),
    ("hud", true, false),
    ("messages", true, true),
    ("voice", true, true),
    ("notify", true, false),
    ("wallpaper", true, false),
];

#[derive(Debug, Clone, PartialEq)]
enum DaemonHealth {
    Running,
    Dead,
    NotRunning,
}

/// Result of checking a single daemon.
#[derive(Debug, Clone)]
struct DaemonCheckResult {
    name: String,
    health: DaemonHealth,
    pid: Option<u32>,
    pid_status: Option<PidStatus>,
    sock_status: Option<SocketStatus>,
}

#[derive(Debug, Clone, PartialEq)]
enum PidStatus {
    Alive,
    Stale,
    Missing,
}

#[derive(Debug, Clone, PartialEq)]
enum SocketStatus {
    Connectable,
    Stale,
    Missing,
}

/// Status of a socket file under the runtime directory.
#[derive(Debug, Clone)]
struct SocketFileInfo {
    name: String,
    path: std::path::PathBuf,
    status: SocketStatus,
}

/// State directory info.
#[derive(Debug, Clone)]
struct StateDirectoryInfo {
    path: String,
    agent_type: &'static str,
    file_count: usize,
}

/// Full diagnostic report data.
struct DiagnosticReport {
    timestamp: String,
    runtime_dir: std::path::PathBuf,
    daemon_results: Vec<DaemonCheckResult>,
    socket_files: Vec<SocketFileInfo>,
    backend_mode: String,
    session_info: Option<String>,
    log_locations: Vec<(String, String)>,
    gpu_info: Option<String>,
    state_dirs: Vec<StateDirectoryInfo>,
    suggested_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorExecutionPlan {
    report_filename: Option<String>,
    should_run_fix: bool,
    should_print_fix_hint: bool,
}

impl DiagnosticReport {
    /// Format the report as plain text (no ANSI escape codes).
    fn format_plain(&self) -> String {
        let mut out = String::new();

        out.push_str(&format!("Thermal Doctor Report — {}\n", self.timestamp));
        out.push_str(&"=".repeat(60));
        out.push('\n');

        // Section 1: Daemon status
        out.push_str("\n## Daemon Status\n\n");
        for d in &self.daemon_results {
            let tag = match d.health {
                DaemonHealth::Running => "[OK]",
                DaemonHealth::Dead => "[STALE]",
                DaemonHealth::NotRunning => "[MISSING]",
            };
            let pid_info = match (&d.pid_status, d.pid) {
                (Some(PidStatus::Alive), Some(pid)) => format!("  pid {pid} alive"),
                (Some(PidStatus::Stale), Some(pid)) => format!("  pid {pid} STALE"),
                (Some(PidStatus::Alive), None) => "  alive (pid unknown)".to_string(),
                (Some(PidStatus::Stale), None) => "  STALE (pid unknown)".to_string(),
                (Some(PidStatus::Missing), _) => "  no pidfile".to_string(),
                (None, _) => String::new(),
            };
            let sock_info = match &d.sock_status {
                Some(SocketStatus::Connectable) => "  sock OK",
                Some(SocketStatus::Stale) => "  sock STALE",
                Some(SocketStatus::Missing) => "  sock MISSING",
                None => "",
            };
            out.push_str(&format!(
                "  {:<9} {:<24}{}{}\n",
                tag, d.name, pid_info, sock_info
            ));
        }

        // Section 2: Socket paths
        out.push_str("\n## Socket Paths\n\n");
        out.push_str(&format!(
            "  Runtime dir: {}\n\n",
            self.runtime_dir.display()
        ));
        if self.socket_files.is_empty() {
            out.push_str("  No socket files found.\n");
        } else {
            for s in &self.socket_files {
                let tag = match s.status {
                    SocketStatus::Connectable => "[OK]",
                    SocketStatus::Stale => "[STALE]",
                    SocketStatus::Missing => "[MISSING]",
                };
                out.push_str(&format!("  {:<9} {}\n", tag, s.path.display()));
            }
        }

        // Section 3: Backend mode
        out.push_str("\n## Backend Mode\n\n");
        out.push_str(&format!("  {}\n", self.backend_mode));

        // Section 4: Session info
        out.push_str("\n## Session Info\n\n");
        if let Some(ref info) = self.session_info {
            out.push_str(&format!("  {info}\n"));
        } else {
            out.push_str("  No daemon connection — session info unavailable.\n");
        }

        // Section 5: Log locations
        out.push_str("\n## Log Locations\n\n");
        for (component, location) in &self.log_locations {
            out.push_str(&format!("  {:<24} {}\n", component, location));
        }

        // Section 6: GPU info
        out.push_str("\n## GPU / Adapter\n\n");
        if let Some(ref info) = self.gpu_info {
            out.push_str(&format!("  {info}\n"));
        } else {
            out.push_str("  GPU adapter info not available (no wgpu instance).\n");
        }

        // Section 7: Compatibility state readers
        out.push_str("\n## Compatibility State Files\n\n");
        if self.state_dirs.is_empty() {
            out.push_str("  No state directories found.\n");
        } else {
            for sd in &self.state_dirs {
                let files = if sd.file_count == 1 { "file" } else { "files" };
                out.push_str(&format!(
                    "  {:<36} {} ({} {})\n",
                    sd.path, sd.agent_type, sd.file_count, files
                ));
            }
        }

        // Suggested actions
        if !self.suggested_actions.is_empty() {
            out.push_str("\n## Suggested Actions\n\n");
            for action in &self.suggested_actions {
                out.push_str(&format!("  - {action}\n"));
            }
        }

        out.push('\n');
        out
    }

    /// Format with ANSI colors for terminal display.
    fn format_colored(&self) -> String {
        let mut out = String::new();

        out.push_str(&format!(
            "\n\x1b[1mThermal Doctor Report\x1b[0m — {}\n",
            self.timestamp
        ));
        out.push_str(&"\x1b[90m─\x1b[0m".repeat(60));
        out.push('\n');

        // Section 1: Daemon status
        out.push_str("\n\x1b[1m## Daemon Status\x1b[0m\n\n");
        for d in &self.daemon_results {
            let (icon, tag_color) = match d.health {
                DaemonHealth::Running => ("\x1b[32m✓\x1b[0m", "\x1b[32m"),
                DaemonHealth::Dead => ("\x1b[31m✗\x1b[0m", "\x1b[31m"),
                DaemonHealth::NotRunning => ("\x1b[90m-\x1b[0m", "\x1b[90m"),
            };
            let tag = match d.health {
                DaemonHealth::Running => "OK",
                DaemonHealth::Dead => "STALE",
                DaemonHealth::NotRunning => "MISSING",
            };
            let pid_info = match (&d.pid_status, d.pid) {
                (Some(PidStatus::Alive), Some(pid)) => format!("  pid {pid}"),
                (Some(PidStatus::Stale), Some(pid)) => {
                    format!("  \x1b[31mpid {pid} stale\x1b[0m")
                }
                (Some(PidStatus::Alive), None) => "  alive (pid unknown)".to_string(),
                (Some(PidStatus::Stale), None) => {
                    "  \x1b[31mstale (pid unknown)\x1b[0m".to_string()
                }
                (Some(PidStatus::Missing), _) => "  no pidfile".to_string(),
                (None, _) => String::new(),
            };
            let sock_info = match &d.sock_status {
                Some(SocketStatus::Connectable) => "  sock \x1b[32m✓\x1b[0m",
                Some(SocketStatus::Stale) => "  sock \x1b[31m✗ stale\x1b[0m",
                Some(SocketStatus::Missing) => "  sock \x1b[90mmissing\x1b[0m",
                None => "",
            };
            out.push_str(&format!(
                "  {icon} {tag_color}[{tag}]\x1b[0m {:<24}{}{}\n",
                d.name, pid_info, sock_info
            ));
        }

        // Section 2: Socket paths
        out.push_str(&format!(
            "\n\x1b[1m## Socket Paths\x1b[0m  ({})\n\n",
            self.runtime_dir.display()
        ));
        if self.socket_files.is_empty() {
            out.push_str("  \x1b[90mNo socket files found.\x1b[0m\n");
        } else {
            for s in &self.socket_files {
                let (icon, label) = match s.status {
                    SocketStatus::Connectable => ("\x1b[32m✓\x1b[0m", "\x1b[32m[OK]\x1b[0m"),
                    SocketStatus::Stale => ("\x1b[31m✗\x1b[0m", "\x1b[31m[STALE]\x1b[0m"),
                    SocketStatus::Missing => ("\x1b[90m-\x1b[0m", "\x1b[90m[MISSING]\x1b[0m"),
                };
                out.push_str(&format!("  {icon} {label} {}\n", s.name));
            }
        }

        // Section 3: Backend mode
        out.push_str("\n\x1b[1m## Backend Mode\x1b[0m\n\n");
        out.push_str(&format!("  {}\n", self.backend_mode));

        // Section 4: Session info
        out.push_str("\n\x1b[1m## Session Info\x1b[0m\n\n");
        if let Some(ref info) = self.session_info {
            out.push_str(&format!("  {info}\n"));
        } else {
            out.push_str("  \x1b[90mNo daemon connection — session info unavailable.\x1b[0m\n");
        }

        // Section 5: Log locations
        out.push_str("\n\x1b[1m## Log Locations\x1b[0m\n\n");
        for (component, location) in &self.log_locations {
            out.push_str(&format!(
                "  {:<24} \x1b[90m{}\x1b[0m\n",
                component, location
            ));
        }

        // Section 6: GPU info
        out.push_str("\n\x1b[1m## GPU / Adapter\x1b[0m\n\n");
        if let Some(ref info) = self.gpu_info {
            out.push_str(&format!("  {info}\n"));
        } else {
            out.push_str("  \x1b[90mGPU adapter info not available (no wgpu instance).\x1b[0m\n");
        }

        // Section 7: Compatibility state readers
        out.push_str("\n\x1b[1m## Compatibility State Files\x1b[0m\n\n");
        if self.state_dirs.is_empty() {
            out.push_str("  \x1b[90mNo state directories found.\x1b[0m\n");
        } else {
            for sd in &self.state_dirs {
                let files = if sd.file_count == 1 { "file" } else { "files" };
                out.push_str(&format!(
                    "  {:<36} \x1b[36m{}\x1b[0m ({} {})\n",
                    sd.path, sd.agent_type, sd.file_count, files
                ));
            }
        }

        // Suggested actions
        if !self.suggested_actions.is_empty() {
            out.push_str("\n\x1b[1;33m## Suggested Actions\x1b[0m\n\n");
            for action in &self.suggested_actions {
                out.push_str(&format!("  \x1b[33m-\x1b[0m {action}\n"));
            }
        }

        out.push('\n');
        out
    }
}

async fn build_diagnostic_report() -> DiagnosticReport {
    use thermal_core::runtime;

    let run_dir = runtime::runtime_dir();
    let timestamp = chrono_timestamp();

    // 1. Check each daemon
    let mut daemon_results = Vec::new();
    let mut suggested_actions = Vec::new();

    for spec in DAEMONS {
        let result = check_daemon(spec);
        if result.health == DaemonHealth::Dead {
            if spec.restart_cmd.is_some() {
                suggested_actions.push(format!(
                    "Run `thc doctor --fix` to clean stale files and restart {}",
                    spec.name
                ));
            } else {
                suggested_actions.push(format!(
                    "Restart {} manually (stale artifacts detected)",
                    spec.name
                ));
            }
        }
        daemon_results.push(result);
    }

    // 2. Enumerate all socket files under runtime_dir
    let socket_files = enumerate_sockets(&run_dir);

    // 3. Backend mode detection
    let conductor_sock = runtime::socket_path("conductor");
    let backend_mode = if conductor_sock.exists() {
        match runtime::try_connect_read_only("conductor", &conductor_sock) {
            Ok(_stream) => "Daemon mode (conductor socket responding)".to_string(),
            Err(msg) => format!("Standalone / no daemon ({msg})"),
        }
    } else {
        "Standalone PTY (no conductor socket)".to_string()
    };

    // 4. Session info — try to get session count from daemon
    let session_info = gather_session_info().await;

    // 5. Log locations
    let log_locations = gather_log_locations(&run_dir);

    // 6. GPU adapter info (best-effort, synchronous probe)
    let gpu_info = probe_gpu_adapter();

    // 7. Compatibility state directories
    let state_dirs = scan_state_directories();

    DiagnosticReport {
        timestamp,
        runtime_dir: run_dir,
        daemon_results,
        socket_files,
        backend_mode,
        session_info,
        log_locations,
        gpu_info,
        state_dirs,
        suggested_actions,
    }
}

fn doctor_execution_plan(
    fix: bool,
    report: bool,
    diagnostic: &DiagnosticReport,
) -> DoctorExecutionPlan {
    let dead_count = diagnostic
        .daemon_results
        .iter()
        .filter(|d| d.health == DaemonHealth::Dead)
        .count();

    DoctorExecutionPlan {
        report_filename: report.then(|| {
            format!(
                "/tmp/thermal-doctor-{}.txt",
                diagnostic.timestamp.replace([':', ' ', '-'], "")
            )
        }),
        should_run_fix: fix,
        should_print_fix_hint: dead_count > 0 && !fix,
    }
}

// ── thc config ─────────────────────────────────────────────────────────────

/// ANSI helpers for config output.
mod config_colors {
    pub const GREEN: &str = "\x1b[32m"; // env override
    pub const YELLOW: &str = "\x1b[33m"; // toml value
    pub const DIM: &str = "\x1b[2m"; // default
    pub const BOLD: &str = "\x1b[1m";
    pub const RESET: &str = "\x1b[0m";
}

/// Where a setting's effective value came from.
#[derive(Clone, Copy)]
enum ConfigSource {
    Env,
    Toml,
    Default,
}

impl ConfigSource {
    fn label(self) -> &'static str {
        match self {
            ConfigSource::Env => "env",
            ConfigSource::Toml => "toml",
            ConfigSource::Default => "default",
        }
    }

    fn color(self) -> &'static str {
        match self {
            ConfigSource::Env => config_colors::GREEN,
            ConfigSource::Toml => config_colors::YELLOW,
            ConfigSource::Default => config_colors::DIM,
        }
    }
}

/// Print a single config line with colored source annotation.
fn print_setting(key: &str, value: &str, source: ConfigSource) {
    use config_colors::*;
    let color = source.color();
    println!(
        "  {key:<30} = {color}{value:<30}{RESET} {DIM}[source: {}]{RESET}",
        source.label()
    );
}

/// Print a section header.
fn print_section(title: &str) {
    use config_colors::*;
    println!("\n{BOLD}── {title} ──{RESET}");
}

/// Resolve a setting: check env var, then TOML section/key, then default.
fn resolve(
    env_var: &str,
    toml_section: Option<&str>,
    toml_key: Option<&str>,
    toml_table: &toml::Table,
    default: &str,
) -> (String, ConfigSource) {
    // 1. Environment variable wins.
    if let Ok(val) = std::env::var(env_var) {
        return (val, ConfigSource::Env);
    }

    // 2. TOML file value.
    if let (Some(section), Some(key)) = (toml_section, toml_key) {
        if let Some(toml::Value::Table(inner)) = toml_table.get(section) {
            if let Some(v) = inner.get(key) {
                let display = match v {
                    toml::Value::String(s) => s.clone(),
                    toml::Value::Integer(i) => i.to_string(),
                    toml::Value::Float(f) => format!("{f:.2}"),
                    toml::Value::Boolean(b) => b.to_string(),
                    other => other.to_string(),
                };
                return (display, ConfigSource::Toml);
            }
        }
    }

    // 3. Default.
    (default.to_string(), ConfigSource::Default)
}

async fn cmd_config() -> Result<()> {
    use crate::tui::settings::{ensure_settings_file, settings_path};

    // Load TOML once.
    let toml_path = settings_path();
    let _ = ensure_settings_file();
    let toml_table: toml::Table = std::fs::read_to_string(&toml_path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    println!(
        "{}Thermal Desktop — effective configuration{}\n",
        config_colors::BOLD,
        config_colors::RESET
    );
    println!(
        "  {}Settings file:{} {}",
        config_colors::DIM,
        config_colors::RESET,
        toml_path.display()
    );

    // ── Font & Display ─────────────────────────────────────────────────────
    print_section("Font & Display");

    let (val, src) = resolve(
        "THERMAL_FONT_FAMILY",
        None,
        None,
        &toml_table,
        "JetBrainsMono Nerd Font Mono",
    );
    print_setting("font_family", &val, src);

    let (val, src) = resolve("THERMAL_FONT_SIZE", None, None, &toml_table, "17.0");
    print_setting("font_size", &val, src);

    let (val, src) = resolve(
        "THERMAL_FONT_FALLBACK",
        None,
        None,
        &toml_table,
        "Noto Color Emoji",
    );
    print_setting("font_fallback", &val, src);

    let (val, src) = resolve("THERMAL_SCROLLBACK", None, None, &toml_table, "50000");
    print_setting("scrollback_lines", &val, src);

    let (val, src) = resolve("THERMAL_BELL", None, None, &toml_table, "visual");
    print_setting("bell_mode", &val, src);

    // ── Audio & Voice ──────────────────────────────────────────────────────
    print_section("Audio & Voice");

    let (val, src) = resolve(
        "THERMAL_AUDIO_VOICE",
        Some("audio"),
        Some("voice"),
        &toml_table,
        "en-US-GuyNeural",
    );
    print_setting("audio.voice", &val, src);

    let (val, src) = resolve(
        "THERMAL_AUDIO_SPEED",
        Some("audio"),
        Some("speed"),
        &toml_table,
        "1.0",
    );
    print_setting("audio.speed", &val, src);

    let (val, src) = resolve(
        "THERMAL_AUDIO_VOLUME",
        Some("audio"),
        Some("volume"),
        &toml_table,
        "1.0",
    );
    print_setting("audio.volume", &val, src);

    let (val, src) = resolve(
        "THERMAL_VOICE_MODE",
        Some("voice"),
        Some("mode"),
        &toml_table,
        "vad",
    );
    print_setting("voice.mode", &val, src);

    let (val, src) = resolve(
        "THERMAL_VOICE_SENSITIVITY",
        Some("voice"),
        Some("sensitivity"),
        &toml_table,
        "0.6",
    );
    print_setting("voice.sensitivity", &val, src);

    let (val, src) = resolve(
        "THERMAL_VOICE_STT_MODEL",
        Some("voice"),
        Some("stt_model"),
        &toml_table,
        "base.en",
    );
    print_setting("voice.stt_model", &val, src);

    // ── Dispatcher ─────────────────────────────────────────────────────────
    print_section("Dispatcher");

    let (val, src) = resolve(
        "THERMAL_DISPATCHER_BACKEND",
        Some("dispatcher"),
        Some("backend"),
        &toml_table,
        "ollama",
    );
    print_setting("dispatcher.backend", &val, src);

    let (val, src) = resolve(
        "THERMAL_DISPATCHER_MODEL",
        Some("dispatcher"),
        Some("model"),
        &toml_table,
        "qwen3:8b",
    );
    print_setting("dispatcher.model", &val, src);

    // ── Runtime Paths ──────────────────────────────────────────────────────
    print_section("Runtime Paths");

    let runtime = thermal_core::runtime::runtime_dir();
    let xdg_runtime = std::env::var("XDG_RUNTIME_DIR").ok();
    let xdg_config = std::env::var("XDG_CONFIG_HOME").ok();

    if let Some(ref val) = xdg_config {
        print_setting("XDG_CONFIG_HOME", val, ConfigSource::Env);
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "~".into());
        print_setting(
            "XDG_CONFIG_HOME",
            &format!("{home}/.config"),
            ConfigSource::Default,
        );
    }

    if let Some(ref val) = xdg_runtime {
        print_setting("XDG_RUNTIME_DIR", val, ConfigSource::Env);
    } else {
        print_setting(
            "XDG_RUNTIME_DIR",
            "(unset — using /run/user/<uid>)",
            ConfigSource::Default,
        );
    }

    print_setting(
        "runtime_dir",
        &runtime.display().to_string(),
        if xdg_runtime.is_some() {
            ConfigSource::Env
        } else {
            ConfigSource::Default
        },
    );
    print_setting(
        "settings_file",
        &toml_path.display().to_string(),
        if xdg_config.is_some() {
            ConfigSource::Env
        } else {
            ConfigSource::Default
        },
    );

    let socket_names = ["conductor", "audio", "dispatcher"];
    for name in socket_names {
        let sock = thermal_core::runtime::socket_path(name);
        let exists = sock.exists();
        let status = if exists { "exists" } else { "absent" };
        println!(
            "  {:<30} = {}{:<30}{} {}[{}]{}",
            format!("{name}.sock"),
            if exists {
                config_colors::GREEN
            } else {
                config_colors::DIM
            },
            sock.display(),
            config_colors::RESET,
            config_colors::DIM,
            status,
            config_colors::RESET,
        );
    }

    println!();
    Ok(())
}

// ── thc smoke ─────────────────────────────────────────────────────────────

/// A single step result in the smoke test pipeline.
struct SmokeStepResult {
    name: &'static str,
    passed: bool,
    duration: std::time::Duration,
    detail: Option<String>,
}

async fn cmd_smoke(fix: bool) -> Result<()> {
    println!("\n  \x1b[1;36m▸ thc smoke\x1b[0m — headless verification\n");

    let mut results: Vec<SmokeStepResult> = Vec::new();

    // Step 1: cargo check --workspace
    {
        let start = std::time::Instant::now();
        let output = tokio::process::Command::new("cargo")
            .args(["check", "--workspace"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await;
        let duration = start.elapsed();
        let (passed, detail) = match output {
            Ok(o) if o.status.success() => (true, None),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let last_lines: String = stderr
                    .lines()
                    .rev()
                    .take(5)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                (false, Some(last_lines))
            }
            Err(e) => (false, Some(format!("failed to run cargo: {e}"))),
        };
        print_step_progress("cargo check", passed);
        results.push(SmokeStepResult {
            name: "cargo check --workspace",
            passed,
            duration,
            detail,
        });
    }

    // Step 2: cargo test --workspace --lib
    {
        let start = std::time::Instant::now();
        let output = tokio::process::Command::new("cargo")
            .args(["test", "--workspace", "--lib"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await;
        let duration = start.elapsed();
        let (passed, detail) = match output {
            Ok(o) if o.status.success() => (true, None),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let stdout = String::from_utf8_lossy(&o.stdout);
                let combined = format!("{stderr}\n{stdout}");
                let last_lines: String = combined
                    .lines()
                    .rev()
                    .take(8)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                (false, Some(last_lines))
            }
            Err(e) => (false, Some(format!("failed to run cargo: {e}"))),
        };
        print_step_progress("cargo test --lib", passed);
        results.push(SmokeStepResult {
            name: "cargo test --workspace --lib",
            passed,
            duration,
            detail,
        });
    }

    // Step 3: doctor health checks
    {
        let start = std::time::Instant::now();
        let diagnostic = build_diagnostic_report().await;
        let duration = start.elapsed();

        let dead_count = diagnostic
            .daemon_results
            .iter()
            .filter(|d| d.health == DaemonHealth::Dead)
            .count();

        let passed = dead_count == 0;
        let detail = if !passed {
            let dead_names: Vec<String> = diagnostic
                .daemon_results
                .iter()
                .filter(|d| d.health == DaemonHealth::Dead)
                .map(|d| d.name.clone())
                .collect();
            Some(format!("dead daemons: {}", dead_names.join(", ")))
        } else {
            None
        };

        print_step_progress("daemon health", passed);
        results.push(SmokeStepResult {
            name: "daemon health (doctor)",
            passed,
            duration,
            detail,
        });

        // If --fix and there are dead daemons, run fix logic
        if fix && dead_count > 0 {
            println!("    \x1b[33m→ fixing dead daemons...\x1b[0m");
            let run_dir = thermal_core::runtime::runtime_dir();
            for (i, spec) in DAEMONS.iter().enumerate() {
                if diagnostic.daemon_results[i].health == DaemonHealth::Dead {
                    fix_daemon(spec, &run_dir).await;
                }
            }
            cleanup_removed_daemon_artifacts(&run_dir);
        }
    }

    // Summary table
    println!();
    println!(
        "  \x1b[1m{:<32} {:>6}  {:>8}\x1b[0m",
        "Step", "Result", "Duration"
    );
    println!("  {}", "─".repeat(50));

    let mut all_passed = true;
    for r in &results {
        let status = if r.passed {
            "\x1b[32m PASS \x1b[0m"
        } else {
            all_passed = false;
            "\x1b[31m FAIL \x1b[0m"
        };
        let dur = format_duration(r.duration);
        println!("  {:<32} {}  {:>8}", r.name, status, dur);
        if let Some(ref detail) = r.detail {
            for line in detail.lines().take(4) {
                println!("    \x1b[90m{line}\x1b[0m");
            }
        }
    }

    println!();
    if all_passed {
        println!("  \x1b[32;1m✓ All checks passed.\x1b[0m\n");
        Ok(())
    } else {
        println!("  \x1b[31;1m✗ Some checks failed.\x1b[0m\n");
        std::process::exit(1);
    }
}

/// Print a live progress indicator for a smoke step.
fn print_step_progress(name: &str, passed: bool) {
    let icon = if passed {
        "\x1b[32m✓\x1b[0m"
    } else {
        "\x1b[31m✗\x1b[0m"
    };
    println!("  {icon} {name}");
}

/// Format a duration as human-readable (e.g., "1.2s", "340ms").
fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

// ── thc doctor ─────────────────────────────────────────────────────────────

async fn cmd_doctor(fix: bool, report: bool) -> Result<()> {
    let diagnostic = build_diagnostic_report().await;
    let plan = doctor_execution_plan(fix, report, &diagnostic);

    if let Some(filename) = &plan.report_filename {
        let plain = diagnostic.format_plain();
        std::fs::write(filename, &plain)?;
    }

    print!("{}", diagnostic.format_colored());

    if let Some(filename) = &plan.report_filename {
        println!("  Report written to \x1b[1m{filename}\x1b[0m\n");
    }

    if plan.should_run_fix {
        let run_dir = thermal_core::runtime::runtime_dir();
        for (i, spec) in DAEMONS.iter().enumerate() {
            if diagnostic.daemon_results[i].health == DaemonHealth::Dead {
                fix_daemon(spec, &run_dir).await;
            }
        }
        cleanup_removed_daemon_artifacts(&run_dir);
        println!();
    } else if plan.should_print_fix_hint {
        println!(
            "  Run \x1b[1mthc doctor --fix\x1b[0m to clean stale files and restart core daemons.\n"
        );
    }

    Ok(())
}

/// Check a single daemon using thermal_core::runtime helpers.
fn check_daemon(spec: &DaemonSpec) -> DaemonCheckResult {
    use thermal_core::runtime;

    let mut pid_status = None;
    let mut pid_val: Option<u32> = None;
    let mut sock_status = None;

    if spec.has_pidfile {
        let path = runtime::pidfile_path(spec.short_name);
        if !path.exists() {
            pid_status = Some(PidStatus::Missing);
        } else {
            // Read pid manually (validate_pidfile removes stale files, which we
            // don't want during a read-only diagnostic).
            if let Ok(contents) = std::fs::read_to_string(&path) {
                if let Ok(pid) = contents.trim().parse::<u32>() {
                    pid_val = Some(pid);
                    if std::path::Path::new(&format!("/proc/{pid}")).exists() {
                        pid_status = Some(PidStatus::Alive);
                    } else {
                        pid_status = Some(PidStatus::Stale);
                    }
                } else {
                    pid_status = Some(PidStatus::Stale);
                }
            } else {
                pid_status = Some(PidStatus::Missing);
            }
        }
    }

    if spec.has_socket {
        let path = runtime::socket_path(spec.short_name);
        if !path.exists() {
            sock_status = Some(SocketStatus::Missing);
        } else {
            // Non-blocking sync check
            use std::os::unix::net::UnixStream;
            match UnixStream::connect(&path) {
                Ok(_) => sock_status = Some(SocketStatus::Connectable),
                Err(_) => sock_status = Some(SocketStatus::Stale),
            }
        }
    }

    let health = match (&pid_status, &sock_status) {
        (Some(PidStatus::Alive), _) => DaemonHealth::Running,
        (_, Some(SocketStatus::Connectable)) => DaemonHealth::Running,
        (Some(PidStatus::Stale), _) => DaemonHealth::Dead,
        (_, Some(SocketStatus::Stale)) => DaemonHealth::Dead,
        _ => DaemonHealth::NotRunning,
    };

    DaemonCheckResult {
        name: spec.name.to_string(),
        health,
        pid: pid_val,
        pid_status,
        sock_status,
    }
}

/// Enumerate all .sock files under the runtime directory and check their status.
fn enumerate_sockets(run_dir: &std::path::Path) -> Vec<SocketFileInfo> {
    let mut sockets = Vec::new();

    // Check expected sockets from DAEMONS
    for spec in DAEMONS {
        if spec.has_socket {
            let path = run_dir.join(format!("{}.sock", spec.short_name));
            let status = if !path.exists() {
                SocketStatus::Missing
            } else {
                use std::os::unix::net::UnixStream;
                match UnixStream::connect(&path) {
                    Ok(_) => SocketStatus::Connectable,
                    Err(_) => SocketStatus::Stale,
                }
            };
            sockets.push(SocketFileInfo {
                name: format!("{}.sock", spec.short_name),
                path: path.clone(),
                status,
            });
        }
    }

    // Also scan for any unexpected .sock files
    if let Ok(entries) = std::fs::read_dir(run_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("sock") {
                let fname = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                // Skip if already listed
                if sockets.iter().any(|s| s.name == fname) {
                    continue;
                }
                let status = {
                    use std::os::unix::net::UnixStream;
                    match UnixStream::connect(&p) {
                        Ok(_) => SocketStatus::Connectable,
                        Err(_) => SocketStatus::Stale,
                    }
                };
                sockets.push(SocketFileInfo {
                    name: fname,
                    path: p,
                    status,
                });
            }
        }
    }

    sockets
}

/// Try connecting to the daemon and getting session count.
async fn gather_session_info() -> Option<String> {
    use crate::client::DaemonClient;

    let mut client = DaemonClient::connect().await.ok()??;
    let sessions = client.list_sessions().await.ok()?;

    let total = sessions.len();
    let alive = sessions.iter().filter(|s| s.is_alive).count();

    Some(format!(
        "{total} session{} ({alive} alive)",
        if total == 1 { "" } else { "s" }
    ))
}

/// Gather well-known log file locations.
fn gather_log_locations(run_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut locs = Vec::new();

    // Conductor TUI log
    let tui_log = run_dir.join("conductor-tui.log");
    locs.push((
        "conductor (TUI)".to_string(),
        if tui_log.exists() {
            tui_log.display().to_string()
        } else {
            format!("{} (not found)", tui_log.display())
        },
    ));

    // General pattern: RUST_LOG=debug <binary> 2>file.log
    locs.push((
        "all daemons".to_string(),
        "RUST_LOG=debug <daemon> 2>/path/to/file.log".to_string(),
    ));

    locs
}

/// Best-effort wgpu adapter probe.
fn probe_gpu_adapter() -> Option<String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))?;

    let info = adapter.get_info();
    Some(format!(
        "{} ({:?}, {:?})",
        info.name, info.backend, info.device_type,
    ))
}

/// Scan /tmp/*-state/ directories for compatibility state files.
fn scan_state_directories() -> Vec<StateDirectoryInfo> {
    let dirs: &[(&str, &str)] = &[
        ("/tmp/claude-code-state", "claude-code"),
        ("/tmp/codex-state", "codex"),
        ("/tmp/copilot-state", "copilot"),
    ];

    let mut results = Vec::new();

    for (path, agent_type) in dirs {
        let p = std::path::Path::new(path);
        if p.exists() {
            let file_count = std::fs::read_dir(p)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .filter(|e| {
                            e.path().extension().and_then(|ext| ext.to_str()) == Some("json")
                        })
                        .count()
                })
                .unwrap_or(0);
            results.push(StateDirectoryInfo {
                path: path.to_string(),
                agent_type,
                file_count,
            });
        }
    }

    results
}

/// Produce a compact timestamp string.
fn chrono_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple UTC timestamp: YYYY-MM-DD HH:MM:SS
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;
    // Approximate date from epoch days (good enough for a diagnostic timestamp)
    let (year, month, day) = epoch_days_to_date(days);
    format!("{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02} UTC")
}

/// Convert days since Unix epoch to (year, month, day).
fn epoch_days_to_date(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's civil_from_days
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u64, m, d)
}

async fn fix_daemon(spec: &DaemonSpec, run_dir: &std::path::Path) {
    // Clean stale PID file
    if spec.has_pidfile {
        let path = run_dir.join(format!("{}.pid", spec.short_name));
        if path.exists() {
            if let Err(e) = std::fs::remove_file(&path) {
                eprintln!(
                    "    \x1b[33m! could not remove {}: {e}\x1b[0m",
                    path.display()
                );
            } else {
                println!("    \x1b[90mcleaned {}.pid\x1b[0m", spec.short_name);
            }
        }
    }

    // Clean stale socket file
    if spec.has_socket {
        let path = run_dir.join(format!("{}.sock", spec.short_name));
        if path.exists() {
            if let Err(e) = std::fs::remove_file(&path) {
                eprintln!(
                    "    \x1b[33m! could not remove {}: {e}\x1b[0m",
                    path.display()
                );
            } else {
                println!("    \x1b[90mcleaned {}.sock\x1b[0m", spec.short_name);
            }
        }
    }

    // Restart if this is a core service daemon
    if let Some(cmd) = spec.restart_cmd {
        let program = cmd[0];
        let args = &cmd[1..];
        match tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => {
                println!("    \x1b[32m↻ restarted {}\x1b[0m", spec.name);
            }
            Err(e) => {
                eprintln!("    \x1b[31m! failed to restart {}: {e}\x1b[0m", spec.name);
            }
        }
    }
}

/// Remove stale pidfiles and sockets left behind by removed daemons.
///
/// Daemons that no longer exist (thermal-bar, thermal-hud, thermal-messages,
/// thermal-voice, thermal-notify, thermal-wallpaper) may have left runtime
/// artifacts. This function silently removes them on `--fix`.
fn cleanup_removed_daemon_artifacts(run_dir: &std::path::Path) {
    for (short_name, has_pidfile, has_socket) in REMOVED_DAEMON_ARTIFACTS {
        if *has_pidfile {
            let path = run_dir.join(format!("{short_name}.pid"));
            if path.exists() {
                match std::fs::remove_file(&path) {
                    Ok(()) => println!(
                        "    \x1b[90mcleaned stale artifact: {short_name}.pid (removed daemon)\x1b[0m"
                    ),
                    Err(e) => eprintln!(
                        "    \x1b[33m! could not remove {}: {e}\x1b[0m",
                        path.display()
                    ),
                }
            }
        }
        if *has_socket {
            let path = run_dir.join(format!("{short_name}.sock"));
            if path.exists() {
                match std::fs::remove_file(&path) {
                    Ok(()) => println!(
                        "    \x1b[90mcleaned stale artifact: {short_name}.sock (removed daemon)\x1b[0m"
                    ),
                    Err(e) => eprintln!(
                        "    \x1b[33m! could not remove {}: {e}\x1b[0m",
                        path.display()
                    ),
                }
            }
        }
    }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod doctor_tests {
    use super::*;

    #[test]
    fn test_epoch_days_to_date() {
        // 2024-01-01 = 19723 days since epoch
        let (y, m, d) = epoch_days_to_date(19723);
        assert_eq!((y, m, d), (2024, 1, 1));

        // 1970-01-01 = day 0
        let (y, m, d) = epoch_days_to_date(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn test_chrono_timestamp_format() {
        let ts = chrono_timestamp();
        // Should match YYYY-MM-DD HH:MM:SS UTC
        assert!(ts.ends_with(" UTC"), "timestamp should end with UTC: {ts}");
        assert!(ts.len() >= 20, "timestamp too short: {ts}");
    }

    #[test]
    fn test_report_format_plain_has_all_sections() {
        let report = DiagnosticReport {
            timestamp: "2026-04-01 00:00:00 UTC".to_string(),
            runtime_dir: std::path::PathBuf::from("/run/user/1000/thermal"),
            daemon_results: vec![
                DaemonCheckResult {
                    name: "thermal-audio".to_string(),
                    health: DaemonHealth::Dead,
                    pid: Some(9999),
                    pid_status: Some(PidStatus::Stale),
                    sock_status: Some(SocketStatus::Stale),
                },
                DaemonCheckResult {
                    name: "thermal-dispatcher".to_string(),
                    health: DaemonHealth::NotRunning,
                    pid: None,
                    pid_status: Some(PidStatus::Missing),
                    sock_status: None,
                },
            ],
            socket_files: vec![SocketFileInfo {
                name: "conductor.sock".to_string(),
                path: std::path::PathBuf::from("/run/user/1000/thermal/conductor.sock"),
                status: SocketStatus::Connectable,
            }],
            backend_mode: "Standalone PTY (no conductor socket)".to_string(),
            session_info: None,
            log_locations: vec![("conductor (TUI)".to_string(), "/tmp/log".to_string())],
            gpu_info: Some("Test GPU (Vulkan, DiscreteGpu)".to_string()),
            state_dirs: vec![StateDirectoryInfo {
                path: "/tmp/claude-code-state".to_string(),
                agent_type: "claude-code",
                file_count: 2,
            }],
            suggested_actions: vec![
                "Run `thc doctor --fix` to clean stale files and restart thermal-audio".to_string(),
            ],
        };

        let plain = report.format_plain();

        assert!(
            plain.contains("## Daemon Status"),
            "missing Daemon Status section"
        );
        assert!(plain.contains("[OK]"), "missing [OK] tag");
        assert!(plain.contains("[STALE]"), "missing [STALE] tag");
        assert!(plain.contains("[MISSING]"), "missing [MISSING] tag");
        assert!(
            plain.contains("## Socket Paths"),
            "missing Socket Paths section"
        );
        assert!(
            plain.contains("## Backend Mode"),
            "missing Backend Mode section"
        );
        assert!(
            plain.contains("## Session Info"),
            "missing Session Info section"
        );
        assert!(
            plain.contains("## Log Locations"),
            "missing Log Locations section"
        );
        assert!(plain.contains("## GPU / Adapter"), "missing GPU section");
        assert!(
            plain.contains("## Compatibility State Files"),
            "missing state files section"
        );
        assert!(
            plain.contains("## Suggested Actions"),
            "missing suggested actions"
        );
        assert!(plain.contains("claude-code"), "missing agent type");
        assert!(plain.contains("2 files"), "missing file count");
    }

    #[test]
    fn test_report_format_colored_has_ansi() {
        let report = DiagnosticReport {
            timestamp: "2026-04-01 00:00:00 UTC".to_string(),
            runtime_dir: std::path::PathBuf::from("/run/user/1000/thermal"),
            daemon_results: vec![DaemonCheckResult {
                name: "thermal-test".to_string(),
                health: DaemonHealth::Running,
                pid: Some(42),
                pid_status: Some(PidStatus::Alive),
                sock_status: None,
            }],
            socket_files: vec![],
            backend_mode: "test".to_string(),
            session_info: None,
            log_locations: vec![],
            gpu_info: None,
            state_dirs: vec![],
            suggested_actions: vec![],
        };

        let colored = report.format_colored();
        assert!(
            colored.contains("\x1b["),
            "colored output should have ANSI codes"
        );
        assert!(
            colored.contains("\x1b[32m"),
            "should have green for running daemon"
        );
    }

    #[test]
    fn test_report_no_suggested_actions_when_all_healthy() {
        let report = DiagnosticReport {
            timestamp: "2026-04-01 00:00:00 UTC".to_string(),
            runtime_dir: std::path::PathBuf::from("/run/user/1000/thermal"),
            daemon_results: vec![DaemonCheckResult {
                name: "thermal-test".to_string(),
                health: DaemonHealth::Running,
                pid: Some(42),
                pid_status: Some(PidStatus::Alive),
                sock_status: None,
            }],
            socket_files: vec![],
            backend_mode: "test".to_string(),
            session_info: Some("1 session (1 alive)".to_string()),
            log_locations: vec![],
            gpu_info: None,
            state_dirs: vec![],
            suggested_actions: vec![],
        };

        let plain = report.format_plain();
        assert!(
            !plain.contains("## Suggested Actions"),
            "should not have suggestions when all healthy"
        );
    }

    #[test]
    fn test_scan_state_directories_returns_vec() {
        // Just ensure it doesn't panic — the actual directories may or may not exist
        let dirs = scan_state_directories();
        // All entries should have a valid agent type
        for d in &dirs {
            assert!(
                ["claude-code", "codex", "copilot"].contains(&d.agent_type),
                "unexpected agent type: {}",
                d.agent_type
            );
        }
    }

    #[test]
    fn test_doctor_execution_plan_allows_fix_and_report_together() {
        let report = DiagnosticReport {
            timestamp: "2026-04-01 00:00:00 UTC".to_string(),
            runtime_dir: std::path::PathBuf::from("/run/user/1000/thermal"),
            daemon_results: vec![DaemonCheckResult {
                name: "thermal-audio".to_string(),
                health: DaemonHealth::Dead,
                pid: Some(9999),
                pid_status: Some(PidStatus::Stale),
                sock_status: Some(SocketStatus::Stale),
            }],
            socket_files: vec![],
            backend_mode: "test".to_string(),
            session_info: None,
            log_locations: vec![],
            gpu_info: None,
            state_dirs: vec![],
            suggested_actions: vec![],
        };

        let plan = doctor_execution_plan(true, true, &report);
        assert!(plan.should_run_fix, "fix should still run when report=true");
        assert!(
            plan.report_filename.is_some(),
            "report path should still be generated when fix=true"
        );
        assert!(
            !plan.should_print_fix_hint,
            "fix hint should not print when fix=true"
        );
    }
}
