//! Cross-pane prompt injection via file-based IPC.
//!
//! Allows sending selected text from one thermal-conductor window to all other
//! running instances. Uses a shared directory (`/tmp/thermal-inject/`) as the
//! transport: the sender writes a timestamped `.txt` file, and all other
//! windows pick it up via a `notify` file watcher.
//!
//! Each inject file contains a header line identifying the sender:
//! ```text
//! THERMAL-INJECT-FROM:<session_id>
//! <selected text follows>
//! ```
//!
//! The originating window ignores its own files based on the session ID.
//!
//! When a daemon is available, the injection is sent directly via the daemon
//! client's `send_input` instead of the file-based approach.

use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Directory used for file-based cross-pane injection.
const INJECT_DIR: &str = "/tmp/thermal-inject";

/// Header prefix in inject files to identify the sender.
const HEADER_PREFIX: &str = "THERMAL-INJECT-FROM:";

/// Generate a unique session identifier for this window instance.
///
/// Uses PID + start timestamp to avoid collisions across restarts.
pub fn generate_session_id() -> String {
    let pid = std::process::id();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{pid}-{ts}")
}

/// Write an inject file to `/tmp/thermal-inject/` with the selected text.
///
/// The file is named `<timestamp_nanos>.txt` and contains a header line
/// identifying the sender followed by the selection text.
pub fn write_inject_file(session_id: &str, text: &str) -> std::io::Result<PathBuf> {
    let dir = Path::new(INJECT_DIR);
    std::fs::create_dir_all(dir)?;

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let filename = format!("{ts}.txt");
    let path = dir.join(&filename);

    let content = format!("{HEADER_PREFIX}{session_id}\n{text}");
    std::fs::write(&path, content)?;

    tracing::info!(
        path = %path.display(),
        text_len = text.len(),
        "Wrote inject file"
    );

    Ok(path)
}

/// Watches `/tmp/thermal-inject/` for new inject files from other windows.
///
/// Call [`InjectWatcher::poll`] regularly from the event loop to drain
/// pending injections. Returns text payloads that should be pasted into
/// the local PTY session.
pub struct InjectWatcher {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<NotifyResult<Event>>,
    /// This window's session ID — used to ignore our own inject files.
    session_id: String,
}

impl InjectWatcher {
    /// Create a new watcher on `/tmp/thermal-inject/`.
    ///
    /// Creates the directory if it does not exist and cleans up stale files
    /// (older than 30 seconds) on startup.
    pub fn new(session_id: String) -> NotifyResult<Self> {
        let dir = Path::new(INJECT_DIR);
        std::fs::create_dir_all(dir).ok();

        // Clean up stale inject files on startup.
        cleanup_stale_files(dir);

        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(tx)?;
        watcher.watch(dir, RecursiveMode::NonRecursive)?;

        Ok(Self {
            _watcher: watcher,
            rx,
            session_id,
        })
    }

    /// Drain pending file-watch events and return inject payloads from other
    /// windows.
    ///
    /// Each returned `String` is the text that should be injected into this
    /// window's PTY session. Files are deleted after processing.
    pub fn poll(&self) -> Vec<String> {
        let mut payloads = Vec::new();
        let mut seen_paths = Vec::new();

        while let Ok(result) = self.rx.try_recv() {
            if let Ok(event) = result {
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        for path in &event.paths {
                            if is_inject_file(path) && !seen_paths.contains(path) {
                                seen_paths.push(path.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        for path in &seen_paths {
            match read_inject_file(path, &self.session_id) {
                InjectFileResult::Foreign(text) => {
                    tracing::info!(
                        path = %path.display(),
                        text_len = text.len(),
                        "Received injection from another window"
                    );
                    payloads.push(text);
                    // Delete the file after processing.
                    let _ = std::fs::remove_file(path);
                }
                InjectFileResult::OwnFile => {
                    // This is our own file — ignore but don't delete yet.
                    // The receiver will delete it.
                    tracing::trace!(path = %path.display(), "Ignoring own inject file");
                }
                InjectFileResult::Error => {
                    tracing::debug!(path = %path.display(), "Failed to read inject file");
                }
            }
        }

        payloads
    }
}

/// Result of reading and parsing an inject file.
enum InjectFileResult {
    /// The file is from a different window — contains the text payload.
    Foreign(String),
    /// The file was written by this window instance.
    OwnFile,
    /// Failed to read or parse the file.
    Error,
}

/// Read an inject file and determine if it's from another session.
fn read_inject_file(path: &Path, our_session_id: &str) -> InjectFileResult {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return InjectFileResult::Error,
    };

    // Parse header line.
    let Some(first_newline) = content.find('\n') else {
        return InjectFileResult::Error;
    };
    let header = &content[..first_newline];

    let Some(sender_id) = header.strip_prefix(HEADER_PREFIX) else {
        return InjectFileResult::Error;
    };

    if sender_id == our_session_id {
        return InjectFileResult::OwnFile;
    }

    let text = content[first_newline + 1..].to_string();
    InjectFileResult::Foreign(text)
}

/// Check if a path looks like an inject file (`.txt` extension).
fn is_inject_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "txt")
}

/// Remove inject files older than 30 seconds.
fn cleanup_stale_files(dir: &Path) {
    let cutoff = SystemTime::now() - std::time::Duration::from_secs(30);

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !is_inject_file(&path) {
            continue;
        }
        if let Ok(meta) = path.metadata()
            && let Ok(modified) = meta.modified()
            && modified < cutoff
        {
            tracing::debug!(path = %path.display(), "Removing stale inject file");
            let _ = std::fs::remove_file(&path);
        }
    }
}

// ── Send notification via thermal-notify ─────────────────────────────────────

/// Send a desktop notification about the injection via `notify-send`.
///
/// This is a fire-and-forget call — failures are silently ignored.
pub fn notify_injection(direction: &str, text_len: usize) {
    let summary = format!("Thermal Inject: {direction}");
    let body = format!("{text_len} chars");
    let _ = std::process::Command::new("notify-send")
        .arg("--app-name=thermal-conductor")
        .arg("--expire-time=2000")
        .arg(&summary)
        .arg(&body)
        .spawn();
}
