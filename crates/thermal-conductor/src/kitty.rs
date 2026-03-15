//! KittyController — interface to kitty terminal via remote control protocol.
//!
//! All communication goes through `kitty @` CLI commands over a unix socket.

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Controller for interacting with a kitty terminal instance via remote control.
pub struct KittyController {
    /// Path to the kitty unix socket (e.g. `/tmp/kitty-{pid}`).
    socket: String,
}

impl KittyController {
    /// Create a new KittyController.
    ///
    /// Discovers the kitty socket by checking `KITTY_LISTEN_ON` env var first,
    /// then scanning `/tmp/kitty-*` for unix sockets.
    pub fn new() -> Result<Self> {
        let socket = Self::find_socket()?;
        tracing::debug!(socket = %socket, "kitty socket found");
        Ok(Self { socket })
    }

    /// Find the kitty remote control socket.
    fn find_socket() -> Result<String> {
        // 1. Check KITTY_LISTEN_ON env var
        if let Ok(listen_on) = std::env::var("KITTY_LISTEN_ON") {
            if !listen_on.is_empty() {
                return Ok(listen_on);
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
                        return Ok(socket_path);
                    }
                }
            }
        }

        bail!("no kitty socket found — is kitty running with allow_remote_control enabled?")
    }

    /// Build the base `kitty @` command with the socket target.
    fn base_cmd(&self) -> Command {
        let mut cmd = Command::new("kitty");
        cmd.arg("@").arg("--to").arg(&self.socket);
        cmd
    }

    /// Spawn a new kitty OS window running the given command.
    /// Returns the kitty window id.
    pub async fn spawn(&self, title: &str, command: &[&str]) -> Result<u64> {
        let mut cmd = self.base_cmd();
        cmd.arg("launch")
            .arg("--type=os-window")
            .arg(format!("--title={title}"));

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

    /// List all kitty windows as JSON.
    pub async fn list_windows(&self) -> Result<serde_json::Value> {
        let mut cmd = self.base_cmd();
        cmd.arg("ls");

        let output = cmd
            .output()
            .await
            .context("failed to run kitty @ ls")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("kitty @ ls failed: {stderr}");
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse kitty @ ls JSON")?;

        Ok(json)
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
