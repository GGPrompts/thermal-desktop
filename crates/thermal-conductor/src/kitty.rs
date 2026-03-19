//! KittyController — interface to kitty terminal via remote control protocol.
//!
//! All communication goes through `kitty @` CLI commands over unix sockets.
//! Supports multiple kitty instances by scanning all `/tmp/kitty-*` sockets.
//!
//! DEPRECATED: This module is no longer used by the CLI commands.
//! CLI subcommands now talk to the session daemon via `DaemonClient` (client.rs).
//! This module is kept for reference and will be removed in a future cleanup.

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Controller for interacting with kitty terminal instances via remote control.
pub struct KittyController {
    /// All discovered kitty unix sockets.
    sockets: Vec<String>,
}

#[allow(dead_code)]
impl KittyController {
    /// Create a new KittyController.
    ///
    /// Discovers kitty sockets by checking `KITTY_LISTEN_ON` env var first,
    /// then scanning `/tmp/kitty-*` for all unix sockets.
    pub fn new() -> Result<Self> {
        let sockets = Self::find_sockets()?;
        tracing::debug!(count = sockets.len(), "kitty sockets found");
        Ok(Self { sockets })
    }

    /// Find all kitty remote control sockets.
    fn find_sockets() -> Result<Vec<String>> {
        let mut sockets = Vec::new();

        // 1. Check KITTY_LISTEN_ON env var
        if let Ok(listen_on) = std::env::var("KITTY_LISTEN_ON") {
            if !listen_on.is_empty() {
                sockets.push(listen_on);
            }
        }

        // 2. Scan /tmp/kitty-* for unix sockets
        let tmp = std::path::Path::new("/tmp");
        if let Ok(entries) = std::fs::read_dir(tmp) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("kitty-") {
                        let socket_path = format!("unix:{}", path.display());
                        if !sockets.contains(&socket_path) {
                            sockets.push(socket_path);
                        }
                    }
                }
            }
        }

        if sockets.is_empty() {
            bail!("no kitty socket found — is kitty running with allow_remote_control enabled?");
        }

        Ok(sockets)
    }

    /// Get the first socket (used for spawn, send, kill — targeted operations).
    fn primary_socket(&self) -> &str {
        &self.sockets[0]
    }

    /// Build a `kitty @` command targeting a specific socket.
    fn cmd_for(&self, socket: &str) -> Command {
        let mut cmd = Command::new("kitty");
        cmd.arg("@").arg("--to").arg(socket);
        cmd
    }

    /// Build the base `kitty @` command with the primary socket.
    fn base_cmd(&self) -> Command {
        self.cmd_for(self.primary_socket())
    }

    /// Spawn a new kitty OS window running the given command.
    /// Returns the kitty window id.
    pub async fn spawn(&self, title: &str, command: &[&str], cwd: Option<&str>) -> Result<u64> {
        let mut cmd = self.base_cmd();
        cmd.arg("launch")
            .arg("--type=os-window")
            .arg(format!("--title={title}"));

        if let Some(dir) = cwd {
            cmd.arg(format!("--cwd={dir}"));
        }

        // Add -- separator then the command args
        cmd.arg("--");
        for arg in command {
            cmd.arg(arg);
        }

        let output = cmd
            .output()
            .await
            .context("failed to run kitty @ launch")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("kitty @ launch failed: {stderr}");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let window_id: u64 = stdout
            .trim()
            .parse()
            .context("failed to parse window id from kitty @ launch output")?;

        Ok(window_id)
    }

    /// Get all text content from a kitty window.
    pub async fn get_text(&self, match_arg: &str) -> Result<String> {
        let mut cmd = self.base_cmd();
        cmd.arg("get-text")
            .arg("--match")
            .arg(match_arg)
            .arg("--extent")
            .arg("all");

        let output = cmd
            .output()
            .await
            .context("failed to run kitty @ get-text")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("kitty @ get-text failed: {stderr}");
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Send text input to a kitty window.
    pub async fn send_text(&self, match_arg: &str, text: &str) -> Result<()> {
        let mut cmd = self.base_cmd();
        cmd.arg("send-text")
            .arg("--match")
            .arg(match_arg)
            .arg(text);

        let output = cmd
            .output()
            .await
            .context("failed to run kitty @ send-text")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("kitty @ send-text failed: {stderr}");
        }

        Ok(())
    }

    /// List all kitty windows across ALL sockets, merged into one JSON array.
    pub async fn list_windows(&self) -> Result<serde_json::Value> {
        let mut all_windows = Vec::new();

        for socket in &self.sockets {
            let mut cmd = self.cmd_for(socket);
            cmd.arg("ls");

            let output = cmd.output().await;

            match output {
                Ok(out) if out.status.success() => {
                    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                        if let Some(arr) = json.as_array() {
                            all_windows.extend(arr.clone());
                        }
                    }
                }
                _ => {
                    tracing::debug!(socket = %socket, "failed to query kitty socket, skipping");
                }
            }
        }

        Ok(serde_json::Value::Array(all_windows))
    }

    /// Close a kitty window.
    pub async fn close_window(&self, match_arg: &str) -> Result<()> {
        let mut cmd = self.base_cmd();
        cmd.arg("close-window")
            .arg("--match")
            .arg(match_arg);

        let output = cmd
            .output()
            .await
            .context("failed to run kitty @ close-window")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("kitty @ close-window failed: {stderr}");
        }

        Ok(())
    }
}
