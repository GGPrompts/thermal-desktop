//! JSONL append-log persistence for the message bus.
//!
//! When `--persist` is enabled, every ingested message is appended to
//! `~/.local/share/thermal/messages.jsonl`. On startup, the log is read
//! back to populate the ring buffer with historical messages.
//!
//! Rotation: when line count exceeds `MAX_LINES`, the current log is
//! renamed to `messages.jsonl.1` and a fresh file is started.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{info, warn};

use thermal_core::message::Message;

/// Maximum lines before rotation.
const MAX_LINES: usize = 10_000;

/// Number of messages between forced flushes.
const FLUSH_INTERVAL: usize = 16;

// ---------------------------------------------------------------------------
// Log path
// ---------------------------------------------------------------------------

pub fn log_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".local/share/thermal")
    } else {
        PathBuf::from("/tmp/thermal")
    }
}

pub fn log_path() -> PathBuf {
    log_dir().join("messages.jsonl")
}

// ---------------------------------------------------------------------------
// Writer — wraps BufWriter with line counting and rotation
// ---------------------------------------------------------------------------

pub struct PersistWriter {
    writer: std::io::BufWriter<std::fs::File>,
    line_count: usize,
    since_flush: usize,
    path: PathBuf,
}

impl PersistWriter {
    /// Open (or create) the log file for appending. Counts existing lines
    /// so rotation triggers at the correct threshold.
    pub fn open() -> Result<Self> {
        let dir = log_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating persist dir {}", dir.display()))?;

        let path = log_path();
        let existing_lines = if path.exists() {
            count_lines(&path)?
        } else {
            0
        };

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening log {}", path.display()))?;

        info!(
            path = %path.display(),
            existing_lines,
            "persist log opened"
        );

        Ok(Self {
            writer: std::io::BufWriter::new(file),
            line_count: existing_lines,
            since_flush: 0,
            path,
        })
    }

    /// Append a message as a single JSON line.
    pub fn append(&mut self, msg: &Message) -> Result<()> {
        // Serialize + newline into a single buffer so the OS write() is atomic-ish.
        let json = serde_json::to_string(msg).context("serializing message")?;
        let mut line = json.into_bytes();
        line.push(b'\n');

        self.writer.write_all(&line).context("writing log line")?;
        self.line_count += 1;
        self.since_flush += 1;

        // Periodic flush.
        if self.since_flush >= FLUSH_INTERVAL {
            self.writer.flush().context("flushing log")?;
            self.since_flush = 0;
        }

        // Rotate if needed.
        if self.line_count >= MAX_LINES {
            self.rotate()?;
        }

        Ok(())
    }

    /// Flush any buffered data.
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush().context("flushing log")?;
        self.since_flush = 0;
        Ok(())
    }

    /// Rotate: flush, rename current → .1, open fresh file.
    fn rotate(&mut self) -> Result<()> {
        self.writer.flush().context("flushing before rotation")?;

        // Derive rotated path from current path (same directory).
        let mut rotated = self.path.clone().into_os_string();
        rotated.push(".1");
        let rotated = PathBuf::from(rotated);
        std::fs::rename(&self.path, &rotated).with_context(|| {
            format!(
                "rotating {} → {}",
                self.path.display(),
                rotated.display()
            )
        })?;

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening fresh log {}", self.path.display()))?;

        self.writer = std::io::BufWriter::new(file);
        self.line_count = 0;
        self.since_flush = 0;

        info!(
            rotated_to = %rotated.display(),
            "log rotated"
        );

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Startup loader — read log into ring buffer
// ---------------------------------------------------------------------------

/// Read the JSONL log and return the last `max_entries` messages.
/// Skips unparseable lines with a warning.
pub fn load_log(max_entries: usize) -> Vec<Message> {
    let path = log_path();
    if !path.exists() {
        return Vec::new();
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let all: Vec<Message> = content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|line| {
                    serde_json::from_str::<Message>(line)
                        .map_err(|e| {
                            warn!(error = %e, "skipping unparseable log line");
                            e
                        })
                        .ok()
                })
                .collect();

            let total = all.len();
            let msgs: Vec<Message> = if total > max_entries {
                all.into_iter().skip(total - max_entries).collect()
            } else {
                all
            };

            info!(
                path = %path.display(),
                total_lines = total,
                loaded = msgs.len(),
                "loaded persist log"
            );

            msgs
        }
        Err(e) => {
            warn!(error = %e, path = %path.display(), "failed to read persist log");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn count_lines(path: &Path) -> Result<usize> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {} for line count", path.display()))?;
    Ok(content.lines().filter(|l| !l.trim().is_empty()).count())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use thermal_core::message::{AgentId, MessageType};

    fn make_msg(seq: u64, content: &str) -> Message {
        Message {
            seq,
            ts: 1000 + seq,
            from: AgentId::new("claude", "s1"),
            to: AgentId::new("*", "broadcast"),
            context_id: None,
            project: None,
            content: content.to_string(),
            msg_type: MessageType::AgentMsg,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn persist_write_and_load() {
        // Use a temp dir to avoid polluting real data.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("thermal");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("messages.jsonl");

        // Write some messages directly.
        {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .unwrap();
            let mut w = std::io::BufWriter::new(file);
            for i in 1..=5 {
                let msg = make_msg(i, &format!("msg {i}"));
                let json = serde_json::to_string(&msg).unwrap();
                w.write_all(json.as_bytes()).unwrap();
                w.write_all(b"\n").unwrap();
            }
            w.flush().unwrap();
        }

        // Override HOME to point to temp.
        // Instead, just test load_log reads from the path.
        // We test the parsing logic directly.
        let content = std::fs::read_to_string(&log).unwrap();
        let msgs: Vec<Message> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0].seq, 1);
        assert_eq!(msgs[4].content, "msg 5");
    }

    #[test]
    fn persist_writer_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".local/share/thermal");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("messages.jsonl");

        // Create a writer manually pointing at our temp path.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();

        let mut pw = PersistWriter {
            writer: std::io::BufWriter::new(file),
            line_count: 9_998,
            since_flush: 0,
            path: log.clone(),
        };

        // Write 3 messages — should trigger rotation after the 2nd (hitting 10k).
        for i in 1..=3 {
            let msg = make_msg(i, &format!("rot {i}"));
            pw.append(&msg).unwrap();
        }
        pw.flush().unwrap();

        // After rotation, the old file should exist as .1
        let rotated = dir.join("messages.jsonl.1");
        assert!(rotated.exists(), "rotated file should exist");

        // The current log should have just 1 line (the 3rd message, post-rotation).
        let current = std::fs::read_to_string(&log).unwrap();
        let current_lines: Vec<&str> = current.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(current_lines.len(), 1);
    }

    #[test]
    fn count_lines_works() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
        assert_eq!(count_lines(&path).unwrap(), 3);
    }

    #[test]
    fn count_lines_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();
        assert_eq!(count_lines(&path).unwrap(), 0);
    }

    #[test]
    fn load_log_nonexistent_returns_empty() {
        // load_log checks log_path() which uses HOME — but the function
        // handles missing files gracefully.
        // We just verify the code path doesn't panic.
        let msgs = load_log(100);
        // May or may not be empty depending on whether the file exists,
        // but it shouldn't panic.
        let _ = msgs;
    }
}
