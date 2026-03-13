//! thermal-ctl — CLI for controlling thermal-conductor
//!
//! Usage:
//!   thermal-ctl create [command]    Create a new pane
//!   thermal-ctl send <pane> <keys>  Send keys to a pane
//!   thermal-ctl focus <pane>        Focus a pane
//!   thermal-ctl list                List all panes
//!   thermal-ctl state <pane>        Get agent state
//!   thermal-ctl layout <layout>     Set layout (grid/sidebar/stack)
//!   thermal-ctl capture <pane>      Capture pane content
//!   thermal-ctl kill <pane>         Kill a pane

use std::process::Command;

use clap::{Parser, Subcommand};

const SESSION: &str = "thermal-conductor";

#[derive(Parser)]
#[command(name = "thermal-ctl")]
#[command(about = "Control the Thermal Conductor agent dashboard")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new agent pane
    Create {
        /// Command to run in the pane
        #[arg(default_value = "zsh")]
        command: String,
    },
    /// Send keys to a pane
    Send {
        /// Pane ID (e.g., "pane-0" or "%0")
        pane: String,
        /// Keys to send
        keys: Vec<String>,
    },
    /// Focus a pane
    Focus {
        /// Pane ID
        pane: String,
    },
    /// List all panes and their states
    List,
    /// Get agent state for a pane
    State {
        /// Pane ID
        pane: String,
    },
    /// Set the layout
    Layout {
        /// Layout type: grid, sidebar, stack
        layout: String,
    },
    /// Capture pane content
    Capture {
        /// Pane ID
        pane: String,
        /// Number of scrollback lines
        #[arg(short, long, default_value = "50")]
        lines: i32,
    },
    /// Kill a pane
    Kill {
        /// Pane ID
        pane: String,
    },
}

// ── tmux helpers ──────────────────────────────────────────────────────────────

/// Run tmux with the given args. Returns stdout on success or an error string.
fn tmux(args: &[&str]) -> Result<String, String> {
    let output = Command::new("tmux").args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "tmux not found — install tmux first".to_string()
        } else {
            format!("io error: {e}")
        }
    })?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if msg.is_empty() {
            format!("tmux exited with status {}", output.status)
        } else {
            msg
        })
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        // ── Create ────────────────────────────────────────────────────────────
        Commands::Create { command } => {
            println!("◉ Creating pane with command: {}", command);
            println!("  (D-Bus pending — use tmux directly for now)");
        }

        // ── Send ──────────────────────────────────────────────────────────────
        Commands::Send { pane, keys } => {
            let keys_str = keys.join(" ");
            match tmux(&["send-keys", "-t", &pane, &keys_str]) {
                Ok(_) => {
                    // Also send Enter so the command runs.
                    match tmux(&["send-keys", "-t", &pane, "Enter"]) {
                        Ok(_) => println!("◉ Sent to {}: {}", pane, keys_str),
                        Err(e) => eprintln!("✗ send Enter failed: {e}"),
                    }
                }
                Err(e) => eprintln!("✗ send-keys failed: {e}"),
            }
        }

        // ── Focus ─────────────────────────────────────────────────────────────
        Commands::Focus { pane } => {
            println!("◉ Focus {}: (D-Bus pending)", pane);
        }

        // ── List ──────────────────────────────────────────────────────────────
        Commands::List => {
            let target = format!("{SESSION}:");
            match tmux(&[
                "list-panes",
                "-t",
                &target,
                "-F",
                "#{pane_id} #{pane_current_command} #{pane_active}",
            ]) {
                Ok(output) => {
                    println!("◉ THERMAL CONDUCTOR — Pane Status");
                    println!("  {:<12}  {:<24}  {}", "ID", "COMMAND", "STATUS");
                    println!("  {}", "─".repeat(50));
                    for line in output.lines().filter(|l| !l.is_empty()) {
                        let parts: Vec<&str> = line.splitn(3, ' ').collect();
                        if parts.len() == 3 {
                            let active = parts[2].trim() == "1";
                            let icon = if active { "◉" } else { "○" };
                            let status = if active { "ACTIVE" } else { "idle" };
                            println!(
                                "  {icon} {:<10}  {:<24}  {}",
                                parts[0], parts[1], status
                            );
                        }
                    }
                }
                Err(e) => eprintln!("✗ list-panes failed: {e}"),
            }
        }

        // ── State ─────────────────────────────────────────────────────────────
        Commands::State { pane } => {
            let start = format!("-{}", 20);
            match tmux(&["capture-pane", "-t", &pane, "-p", "-e", "-S", &start]) {
                Ok(content) => {
                    // Simple heuristic: look at the last non-empty line.
                    let last = content
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("");
                    // Strip ANSI escapes naively.
                    let mut clean = String::new();
                    let mut in_escape = false;
                    for ch in last.chars() {
                        if ch == '\x1b' {
                            in_escape = true;
                        } else if in_escape && ch.is_alphabetic() {
                            in_escape = false;
                        } else if !in_escape {
                            clean.push(ch);
                        }
                    }
                    let clean = clean.trim();
                    let state = if clean.ends_with("$ ")
                        || clean.ends_with("❯ ")
                        || clean.ends_with("% ")
                        || clean.ends_with("# ")
                    {
                        "IDLE"
                    } else {
                        "RUNNING"
                    };
                    println!("◉ State for {}: {}", pane, state);
                }
                Err(e) => eprintln!("✗ capture-pane failed: {e}"),
            }
        }

        // ── Layout ────────────────────────────────────────────────────────────
        Commands::Layout { layout } => {
            println!("◉ Layout {}: (D-Bus pending)", layout);
        }

        // ── Capture ───────────────────────────────────────────────────────────
        Commands::Capture { pane, lines } => {
            let start = format!("-{}", lines.abs());
            match tmux(&["capture-pane", "-t", &pane, "-p", "-e", "-S", &start]) {
                Ok(content) => print!("{content}"),
                Err(e) => eprintln!("✗ capture-pane failed: {e}"),
            }
        }

        // ── Kill ──────────────────────────────────────────────────────────────
        Commands::Kill { pane } => {
            match tmux(&["kill-pane", "-t", &pane]) {
                Ok(_) => println!("◉ Killed pane {}", pane),
                Err(e) => eprintln!("✗ kill-pane failed: {e}"),
            }
        }
    }
}
