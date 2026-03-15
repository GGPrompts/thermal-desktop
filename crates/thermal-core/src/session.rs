//! Terminal session management via PTY + tmux.
//!
//! Provides [`TerminalSession`] — a persistent PTY connection to a detached
//! tmux session with dedicated reader and writer threads. Output is delivered
//! through a lock-free [`ArcSwap`] sink that can be hot-swapped without
//! restarting the session.
//!
//! [`TerminalManager`] wraps a map of named sessions for multi-pane use.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// TerminalSession
// ---------------------------------------------------------------------------

/// A persistent terminal session backed by a PTY attached to a tmux session.
///
/// The PTY reader and writer threads survive across subscriber changes —
/// callers can [`subscribe_output`](TerminalSession::subscribe_output) and
/// [`send_input`](TerminalSession::send_input) independently.
pub struct TerminalSession {
    /// The tmux session name (e.g. `thermal-main`).
    session_name: String,

    /// Kept alive to own the PTY master fd.
    #[allow(dead_code)]
    pty_master: Box<dyn MasterPty + Send>,

    /// Channel sender to the dedicated writer thread.
    input_tx: std::sync::mpsc::Sender<Vec<u8>>,

    /// Lock-free output sink.  The reader thread loads this on every read
    /// and forwards bytes when `Some`.  Subscribers swap in a new sender
    /// via [`subscribe_output`](TerminalSession::subscribe_output).
    output_sink: Arc<ArcSwap<Option<mpsc::UnboundedSender<Vec<u8>>>>>,

    /// `false` once the reader thread exits (PTY EOF or error).
    reader_alive: Arc<AtomicBool>,

    /// Current terminal dimensions.
    cols: u16,
    rows: u16,
}

impl TerminalSession {
    /// Spawn a new terminal session.
    ///
    /// Creates (or reuses) a detached tmux session named `session_name`,
    /// opens a PTY that attaches to it, and starts persistent reader/writer
    /// threads.
    ///
    /// * `session_name` — tmux session name (must be unique).
    /// * `command` — optional shell command to run inside the tmux session
    ///   (only used when creating a *new* tmux session).
    /// * `cwd` — optional working directory for the tmux session.
    /// * `cols` / `rows` — initial terminal dimensions.
    pub fn spawn(
        session_name: &str,
        command: Option<&str>,
        cwd: Option<&Path>,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        let tmux_name = format!("thermal-{session_name}");
        info!(session = %tmux_name, cols, rows, "Spawning terminal session");

        // Check if the tmux session already exists.
        let tmux_exists = Command::new("tmux")
            .args(["has-session", "-t", &tmux_name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if tmux_exists {
            info!(session = %tmux_name, "Reattaching to existing tmux session");
            // Detach stale clients before we attach our PTY.
            let _ = Command::new("tmux")
                .args(["detach-client", "-s", &tmux_name])
                .output();
            // Enter copy-mode to absorb phantom bytes that tmux injects
            // during attach-session.
            let _ = Command::new("tmux")
                .args(["copy-mode", "-t", &tmux_name])
                .output();
        } else {
            let cols_str = cols.to_string();
            let rows_str = rows.to_string();
            let cwd_str = cwd.map(|p| p.to_string_lossy().into_owned());

            let mut args = vec![
                "new-session",
                "-d",
                "-s",
                &tmux_name,
                "-x",
                &cols_str,
                "-y",
                &rows_str,
            ];

            // Working directory.
            if let Some(ref dir) = cwd_str {
                args.push("-c");
                args.push(dir);
            }

            // Optional initial command (appended as the shell command).
            if let Some(cmd) = command {
                args.push(cmd);
            }

            let output = Command::new("tmux")
                .args(&args)
                .output()
                .context("Failed to run tmux new-session")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow!("tmux new-session failed: {stderr}"));
            }

            // Disable tmux status bar — the GPU renderer provides its own.
            let _ = Command::new("tmux")
                .args(["set-option", "-t", &tmux_name, "status", "off"])
                .output();
        }

        // -----------------------------------------------------------------
        // Open a PTY and attach to the tmux session.
        // -----------------------------------------------------------------
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to open PTY")?;

        let mut cmd = CommandBuilder::new("tmux");
        cmd.args(["attach-session", "-t", &tmux_name]);
        cmd.env("TERM", "xterm-256color");
        cmd.env("LANG", "en_US.UTF-8");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("FORCE_COLOR", "1");
        cmd.env("COLORFGBG", "15;0");
        cmd.env("NCURSES_NO_UTF8_ACS", "1");
        cmd.env_remove("TMUX");

        let _child = pair
            .slave
            .spawn_command(cmd)
            .context("Failed to spawn tmux attach in PTY")?;

        let reader = pair
            .master
            .try_clone_reader()
            .context("Failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("Failed to take PTY writer")?;

        // -----------------------------------------------------------------
        // Output sink (lock-free).
        // -----------------------------------------------------------------
        let output_sink: Arc<ArcSwap<Option<mpsc::UnboundedSender<Vec<u8>>>>> =
            Arc::new(ArcSwap::from_pointee(None));
        let reader_alive = Arc::new(AtomicBool::new(true));

        // -----------------------------------------------------------------
        // Persistent reader thread — outlives any individual subscriber.
        // -----------------------------------------------------------------
        {
            let sink = output_sink.clone();
            let alive = reader_alive.clone();
            let name = tmux_name.clone();
            std::thread::Builder::new()
                .name(format!("pty-reader-{session_name}"))
                .spawn(move || {
                    let mut reader = reader;
                    let mut buf = [0u8; 4096];
                    loop {
                        match std::io::Read::read(&mut reader, &mut buf) {
                            Ok(0) => {
                                info!(session = %name, "PTY reader EOF");
                                alive.store(false, Ordering::SeqCst);
                                let guard = sink.load();
                                if let Some(ref tx) = **guard {
                                    let _ = tx.send(Vec::new());
                                }
                                break;
                            }
                            Ok(n) => {
                                let guard = sink.load();
                                if let Some(ref tx) = **guard {
                                    let _ = tx.send(buf[..n].to_vec());
                                }
                            }
                            Err(e) => {
                                error!(session = %name, error = %e, "PTY read error");
                                alive.store(false, Ordering::SeqCst);
                                let guard = sink.load();
                                if let Some(ref tx) = **guard {
                                    let _ = tx.send(Vec::new());
                                }
                                break;
                            }
                        }
                    }
                })
                .context("Failed to spawn PTY reader thread")?;
        }

        // -----------------------------------------------------------------
        // Dedicated writer thread — drains the input channel.
        // -----------------------------------------------------------------
        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        {
            let name = tmux_name.clone();
            let mut writer = writer;
            std::thread::Builder::new()
                .name(format!("pty-writer-{session_name}"))
                .spawn(move || {
                    for data in input_rx {
                        if let Err(e) = writer.write_all(&data) {
                            warn!(session = %name, error = %e, "PTY write error");
                            break;
                        }
                    }
                })
                .context("Failed to spawn PTY writer thread")?;
        }

        // Exit copy-mode after attach has settled (only for reattach).
        if tmux_exists {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = Command::new("tmux")
                .args(["send-keys", "-t", &tmux_name, "-X", "cancel"])
                .output();
        }

        info!(session = %tmux_name, "Terminal session ready");

        Ok(Self {
            session_name: tmux_name,
            pty_master: pair.master,
            input_tx,
            output_sink,
            reader_alive,
            cols,
            rows,
        })
    }

    /// Send raw bytes to the terminal's stdin via the writer thread.
    pub fn send_input(&self, data: &[u8]) {
        let _ = self.input_tx.send(data.to_vec());
    }

    /// Subscribe to the terminal's stdout.
    ///
    /// Replaces any previous subscriber (only one at a time).  An empty
    /// `Vec<u8>` on the receiver signals EOF.
    ///
    /// Also performs a resize bump so tmux redraws the full screen for
    /// the new subscriber.
    pub fn subscribe_output(&self) -> mpsc::UnboundedReceiver<Vec<u8>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.output_sink.store(Arc::new(Some(tx)));

        // Resize bump forces tmux to redraw for the new subscriber.
        if self.rows > 1 {
            let _ = self.pty_master.resize(PtySize {
                rows: self.rows - 1,
                cols: self.cols,
                pixel_width: 0,
                pixel_height: 0,
            });
            let _ = self.pty_master.resize(PtySize {
                rows: self.rows,
                cols: self.cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }

        rx
    }

    /// Disconnect the current output subscriber without destroying the session.
    pub fn disconnect_output(&self) {
        self.output_sink.store(Arc::new(None));
    }

    /// Whether the reader thread is still running.
    pub fn is_alive(&self) -> bool {
        self.reader_alive.load(Ordering::SeqCst)
    }

    /// Resize the PTY.  tmux receives SIGWINCH and adapts automatically.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.pty_master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to resize PTY")?;
        self.cols = cols;
        self.rows = rows;
        info!(session = %self.session_name, cols, rows, "Terminal resized");
        Ok(())
    }

    /// The tmux session name (e.g. `thermal-main`).
    pub fn name(&self) -> &str {
        &self.session_name
    }

    /// Current column count.
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Current row count.
    pub fn rows(&self) -> u16 {
        self.rows
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.output_sink.store(Arc::new(None));
        info!(session = %self.session_name, "Session dropped (tmux session preserved)");
    }
}

// ---------------------------------------------------------------------------
// TerminalManager
// ---------------------------------------------------------------------------

/// Manages multiple named [`TerminalSession`]s.
pub struct TerminalManager {
    sessions: Mutex<HashMap<String, TerminalSession>>,
}

impl TerminalManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Spawn a new session (or return an error if the name is taken and alive).
    pub fn spawn(
        &self,
        name: &str,
        command: Option<&str>,
        cwd: Option<&Path>,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        let mut sessions = self.sessions.lock().map_err(|e| anyhow!("Lock poisoned: {e}"))?;

        // Remove stale sessions with the same name.
        if let Some(existing) = sessions.get(name) {
            if existing.is_alive() {
                return Err(anyhow!("Session '{name}' already exists and is alive"));
            }
            sessions.remove(name);
        }

        let session = TerminalSession::spawn(name, command, cwd, cols, rows)?;
        sessions.insert(name.to_string(), session);
        Ok(())
    }

    /// Get a reference to a session by name (locks briefly).
    ///
    /// The callback `f` runs while the sessions map lock is held — keep it
    /// short.  For long-lived access, clone the `input_tx` or call
    /// `subscribe_output` inside the callback.
    pub fn with_session<F, R>(&self, name: &str, f: F) -> Result<R>
    where
        F: FnOnce(&TerminalSession) -> R,
    {
        let sessions = self.sessions.lock().map_err(|e| anyhow!("Lock poisoned: {e}"))?;
        let session = sessions
            .get(name)
            .ok_or_else(|| anyhow!("No session named '{name}'"))?;
        Ok(f(session))
    }

    /// Mutable access to a session (e.g. for resize).
    pub fn with_session_mut<F, R>(&self, name: &str, f: F) -> Result<R>
    where
        F: FnOnce(&mut TerminalSession) -> R,
    {
        let mut sessions = self.sessions.lock().map_err(|e| anyhow!("Lock poisoned: {e}"))?;
        let session = sessions
            .get_mut(name)
            .ok_or_else(|| anyhow!("No session named '{name}'"))?;
        Ok(f(session))
    }

    /// Remove a session.  The tmux session is *not* killed — only the PTY
    /// attachment is dropped.
    pub fn remove(&self, name: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().map_err(|e| anyhow!("Lock poisoned: {e}"))?;
        sessions
            .remove(name)
            .ok_or_else(|| anyhow!("No session named '{name}'"))?;
        Ok(())
    }

    /// Remove a session *and* kill the underlying tmux session.
    pub fn kill(&self, name: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().map_err(|e| anyhow!("Lock poisoned: {e}"))?;
        let session = sessions
            .remove(name)
            .ok_or_else(|| anyhow!("No session named '{name}'"))?;

        let tmux_name = session.name().to_string();
        drop(session);

        let output = Command::new("tmux")
            .args(["kill-session", "-t", &tmux_name])
            .output();

        match output {
            Ok(o) if !o.status.success() => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                warn!(session = %tmux_name, stderr = %stderr, "tmux kill-session failed");
            }
            Err(e) => {
                error!(session = %tmux_name, error = %e, "Failed to run tmux kill-session");
            }
            _ => {
                info!(session = %tmux_name, "tmux session killed");
            }
        }

        Ok(())
    }

    /// List active session names.
    pub fn list(&self) -> Result<Vec<String>> {
        let sessions = self.sessions.lock().map_err(|e| anyhow!("Lock poisoned: {e}"))?;
        Ok(sessions.keys().cloned().collect())
    }
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}
