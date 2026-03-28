//! KittyController — async interface to kitty terminal via `kitty @` remote control.
//!
//! This is the core orchestration layer for managing thermal sessions inside kitty.
//! Sessions are identified by a string ID and mapped to kitty windows via the
//! `thermal-{id}` title convention. A JSON sidecar at
//! `/run/user/$UID/thermal/sessions.json` tracks metadata that kitty itself does not
//! persist (worktree paths, profile names, original cwd, spawn timestamps).
//!
//! All methods are async and shell out to `kitty @` via `tokio::process::Command`.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

// ── Title prefix ────────────────────────────────────────────────────────────

const TITLE_PREFIX: &str = "thermal-";

/// Validate that a session ID contains only safe characters for kitty regex matching.
/// Allows alphanumeric, hyphens, underscores, and dots.
fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("session ID must not be empty");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!(
            "session ID contains invalid characters (allowed: alphanumeric, hyphen, underscore, dot): {id}"
        );
    }
    Ok(())
}

// ── Sidecar types ───────────────────────────────────────────────────────────

/// Metadata for a single thermal session, persisted in the sidecar file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarEntry {
    pub session_id: String,
    pub worktree_path: Option<String>,
    pub profile_name: Option<String>,
    pub original_cwd: String,
    /// Seconds since Unix epoch.
    pub spawn_time: u64,
    /// Short display name derived from the model (e.g. "opus", "sonnet", "gpt5.4mini").
    /// Used for @-mentions in the TUI and message bus. First occurrence gets the bare
    /// name; duplicates get "-2", "-3", etc.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// The full sidecar file — a simple map of session_id -> metadata.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SidecarData {
    pub sessions: Vec<SidecarEntry>,
}

impl SidecarData {
    /// Look up a session by its display name (e.g. "opus", "sonnet-2").
    /// Returns the first entry whose `display_name` matches (case-sensitive).
    pub fn find_by_display_name(&self, name: &str) -> Option<&SidecarEntry> {
        self.sessions
            .iter()
            .find(|e| e.display_name.as_deref() == Some(name))
    }
}

/// Derive a unique display name from a base model name, avoiding collisions with
/// names already present in `existing` entries.
///
/// - If `base` is not taken, returns it as-is (e.g. "opus").
/// - If taken, appends a suffix: "opus-2", "opus-3", etc.
/// - Skips the entry with `exclude_session_id` (so re-upserts don't collide with
///   the session's own prior name).
pub fn assign_display_name(base: &str, existing: &[SidecarEntry], exclude_session_id: Option<&str>) -> String {
    let is_taken = |candidate: &str| -> bool {
        existing.iter().any(|e| {
            if let Some(exc) = exclude_session_id {
                if e.session_id == exc {
                    return false;
                }
            }
            e.display_name.as_deref() == Some(candidate)
        })
    };

    if !is_taken(base) {
        return base.to_string();
    }

    let mut suffix = 2u32;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !is_taken(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

// ── kitty @ ls JSON structures ──────────────────────────────────────────────

/// Top-level entry from `kitty @ ls` — one per OS window.
#[derive(Debug, Deserialize)]
struct KittyOsWindow {
    tabs: Vec<KittyTab>,
}

/// A tab within an OS window.
#[derive(Debug, Deserialize)]
struct KittyTab {
    windows: Vec<KittyWindow>,
}

/// A single kitty window (pane).
#[derive(Debug, Deserialize)]
struct KittyWindow {
    id: i64,
    title: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    is_focused: bool,
    #[serde(default)]
    foreground_processes: Vec<KittyProcess>,
}

#[derive(Debug, Deserialize)]
struct KittyProcess {
    #[serde(default)]
    #[allow(dead_code)]
    pid: i64,
    #[serde(default)]
    cmdline: Vec<String>,
}

// ── Public result type ──────────────────────────────────────────────────────

/// Information about a thermal session window, combining kitty state with sidecar
/// metadata. Field names parallel `protocol::SessionInfo` where applicable.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// The thermal session ID (the part after `thermal-` in the title).
    pub session_id: String,
    /// Kitty's internal window ID.
    pub kitty_window_id: i64,
    /// Current working directory as reported by kitty.
    pub cwd: String,
    /// Window title (full, including `thermal-` prefix).
    pub title: String,
    /// Whether this window is currently focused.
    pub is_focused: bool,
    /// Foreground process command line, if available.
    pub foreground_command: Option<String>,
    /// From sidecar: git worktree path, if the session uses one.
    pub worktree_path: Option<String>,
    /// From sidecar: profile name used to spawn.
    pub profile_name: Option<String>,
    /// From sidecar: the directory the session was originally spawned in.
    pub original_cwd: Option<String>,
    /// From sidecar: seconds since epoch when spawned.
    pub spawn_time: Option<u64>,
}

// ── KittyController ─────────────────────────────────────────────────────────

/// Async controller for managing thermal sessions inside kitty via `kitty @`.
pub struct KittyController {
    /// Cached availability result (set once on first check).
    available: OnceLock<bool>,
}

impl KittyController {
    /// Create a new controller. Does not perform any I/O — availability is
    /// checked lazily on first call to `is_available()`.
    pub fn new() -> Self {
        Self {
            available: OnceLock::new(),
        }
    }

    // ── Availability ────────────────────────────────────────────────────────

    /// Check whether kitty remote control is reachable.
    ///
    /// Runs `kitty @ ls` and caches the result. Subsequent calls return the
    /// cached value without spawning a process.
    pub async fn is_available(&self) -> bool {
        if let Some(&cached) = self.available.get() {
            return cached;
        }

        let result = Command::new("kitty")
            .args(["@", "ls"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);

        // Only cache positive results — if kitty is not available now it may
        // become available later (user starts kitty, enables remote control).
        if result {
            let _ = self.available.set(true);
        }
        result
    }

    // ── Spawn ───────────────────────────────────────────────────────────────

    /// Spawn a new kitty window running `command` in directory `cwd`.
    ///
    /// The window title is set to `thermal-{id}` for later matching. Metadata
    /// is written to the sessions sidecar.
    ///
    /// Optional `profile_name` and `worktree_path` are stored in the sidecar
    /// for downstream consumers. If `model_display_base` is provided (e.g.
    /// from `ClaudeSessionState::model_display_name()`), a unique display name
    /// is assigned with dedup numbering.
    pub async fn spawn(
        &self,
        id: &str,
        command: &str,
        cwd: &str,
        profile_name: Option<&str>,
        worktree_path: Option<&str>,
    ) -> Result<()> {
        self.spawn_with_model(id, command, cwd, profile_name, worktree_path, None)
            .await
    }

    /// Like [`spawn`](Self::spawn), but also assigns a display name from
    /// the given model base name.
    pub async fn spawn_with_model(
        &self,
        id: &str,
        command: &str,
        cwd: &str,
        profile_name: Option<&str>,
        worktree_path: Option<&str>,
        model_display_base: Option<&str>,
    ) -> Result<()> {
        let title = format!("{TITLE_PREFIX}{id}");

        let output = Command::new("kitty")
            .args([
                "@",
                "launch",
                "--type=window",
                &format!("--title={title}"),
                &format!("--cwd={cwd}"),
                "--",
            ])
            .arg(command)
            .output()
            .await
            .context("failed to run kitty @ launch")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("kitty @ launch failed: {stderr}");
        }

        // Record in sidecar (display_name assigned inside the locked update).
        let id_owned = id.to_string();
        let worktree = worktree_path.map(String::from);
        let profile = profile_name.map(String::from);
        let cwd_owned = cwd.to_string();
        let model_base = model_display_base.map(String::from);

        sidecar_locked_update(move |data| {
            // Remove any prior entry for this session.
            data.sessions.retain(|e| e.session_id != id_owned);

            let display_name = model_base
                .as_deref()
                .map(|base| assign_display_name(base, &data.sessions, None));

            data.sessions.push(SidecarEntry {
                session_id: id_owned,
                worktree_path: worktree,
                profile_name: profile,
                original_cwd: cwd_owned,
                spawn_time: now_epoch(),
                display_name,
            });
        })
        .await?;

        tracing::info!(id, cwd, "spawned kitty window");
        Ok(())
    }

    // ── List ────────────────────────────────────────────────────────────────

    /// List all kitty windows whose title starts with `thermal-`, merged with
    /// sidecar metadata.
    pub async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let raw = run_kitty_ls().await?;
        let os_windows: Vec<KittyOsWindow> =
            serde_json::from_str(&raw).context("failed to parse kitty @ ls JSON")?;

        let sidecar = sidecar_read().await;
        let mut results = Vec::new();

        for os_win in &os_windows {
            for tab in &os_win.tabs {
                for win in &tab.windows {
                    if let Some(session_id) = win.title.strip_prefix(TITLE_PREFIX) {
                        let side = sidecar.sessions.iter().find(|e| e.session_id == session_id);

                        let foreground_command = win
                            .foreground_processes
                            .first()
                            .map(|p| p.cmdline.join(" "))
                            .filter(|s| !s.is_empty());

                        results.push(WindowInfo {
                            session_id: session_id.to_string(),
                            kitty_window_id: win.id,
                            cwd: win.cwd.clone(),
                            title: win.title.clone(),
                            is_focused: win.is_focused,
                            foreground_command,
                            worktree_path: side.and_then(|s| s.worktree_path.clone()),
                            profile_name: side.and_then(|s| s.profile_name.clone()),
                            original_cwd: side.map(|s| s.original_cwd.clone()),
                            spawn_time: side.map(|s| s.spawn_time),
                        });
                    }
                }
            }
        }

        Ok(results)
    }

    // ── Close ───────────────────────────────────────────────────────────────

    /// Close the kitty window for session `id` and remove its sidecar entry.
    pub async fn close_window(&self, id: &str) -> Result<()> {
        validate_session_id(id)?;
        let match_arg = format!("title:^{TITLE_PREFIX}{id}$");

        let output = Command::new("kitty")
            .args(["@", "close-window", "--match", &match_arg])
            .output()
            .await
            .context("failed to run kitty @ close-window")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("kitty @ close-window failed: {stderr}");
        }

        sidecar_remove(id).await?;
        tracing::info!(id, "closed kitty window");
        Ok(())
    }

    // ── Send text ───────────────────────────────────────────────────────────

    /// Send text to the kitty window for session `id`.
    pub async fn send_text(&self, id: &str, text: &str) -> Result<()> {
        validate_session_id(id)?;
        let match_arg = format!("title:^{TITLE_PREFIX}{id}$");

        let output = Command::new("kitty")
            .args(["@", "send-text", "--match", &match_arg, "--"])
            .arg(text)
            .output()
            .await
            .context("failed to run kitty @ send-text")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("kitty @ send-text failed: {stderr}");
        }

        Ok(())
    }

    // ── Focus ───────────────────────────────────────────────────────────────

    /// Focus the kitty window for session `id`.
    #[allow(dead_code)]
    pub async fn focus_window(&self, id: &str) -> Result<()> {
        validate_session_id(id)?;
        let match_arg = format!("title:^{TITLE_PREFIX}{id}$");

        let output = Command::new("kitty")
            .args(["@", "focus-window", "--match", &match_arg])
            .output()
            .await
            .context("failed to run kitty @ focus-window")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("kitty @ focus-window failed: {stderr}");
        }

        Ok(())
    }
}

// ── Sidecar helpers ─────────────────────────────────────────────────────────

/// Return the sidecar file path: `/run/user/<uid>/thermal/sessions.json`.
fn sidecar_path() -> PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    PathBuf::from(format!("/run/user/{uid}/thermal/sessions.json"))
}

/// Read the sidecar, returning default if missing or unparseable.
async fn sidecar_read() -> SidecarData {
    let path = sidecar_path();
    match tokio::fs::read_to_string(&path).await {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => SidecarData::default(),
    }
}

/// Acquire an exclusive flock on the sidecar lockfile, perform a read-modify-write,
/// then release. This prevents concurrent thc invocations from clobbering each other.
async fn sidecar_locked_update(f: impl FnOnce(&mut SidecarData) + Send + 'static) -> Result<()> {
    // Run the locked operation in a blocking task to avoid holding the lock
    // across an async suspension point.
    tokio::task::spawn_blocking(move || {
        let lock_path = sidecar_path().with_extension("json.lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .context("failed to open sidecar lock file")?;

        // Acquire exclusive lock (blocking).
        let _lock = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusive)
            .map_err(|(_, e)| anyhow::anyhow!("flock on sidecar lock file failed: {e}"))?;

        // Read-modify-write under lock.
        let path = sidecar_path();
        let mut data: SidecarData = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        f(&mut data);

        let json = serde_json::to_string_pretty(&data).context("failed to serialize sidecar")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes()).context("failed to write sidecar temp file")?;
        std::fs::rename(&tmp, &path).context("failed to rename sidecar temp file")?;

        // Lock is released when lock_file is dropped.
        Ok(())
    })
    .await
    .context("sidecar update task panicked")?
}

/// Add an entry to the sidecar (locked read-modify-write).
/// If the entry has no `display_name` and a `model_display_base` is provided,
/// a unique display name will be assigned. Use [`sidecar_upsert_display_name`]
/// to assign/update display names on existing entries.
#[allow(dead_code)]
async fn sidecar_add(entry: SidecarEntry) -> Result<()> {
    sidecar_locked_update(move |data| {
        data.sessions.retain(|e| e.session_id != entry.session_id);
        data.sessions.push(entry);
    })
    .await
}

/// Assign or update the display name for an existing sidecar entry.
///
/// Reads the current sidecar under lock, derives a unique display name from
/// `model_display_base`, and writes it back. No-op if the session is not found.
pub async fn sidecar_upsert_display_name(session_id: &str, model_display_base: &str) -> Result<()> {
    let sid = session_id.to_string();
    let base = model_display_base.to_string();
    sidecar_locked_update(move |data| {
        let name = assign_display_name(&base, &data.sessions, Some(&sid));
        if let Some(entry) = data.sessions.iter_mut().find(|e| e.session_id == sid) {
            entry.display_name = Some(name);
        }
    })
    .await
}

/// Remove an entry from the sidecar by session ID (locked read-modify-write).
async fn sidecar_remove(id: &str) -> Result<()> {
    let id = id.to_string();
    sidecar_locked_update(move |data| {
        data.sessions.retain(|e| e.session_id != id);
    })
    .await
}

// ── Kitty ls helper ─────────────────────────────────────────────────────────

/// Run `kitty @ ls` and return the raw JSON string.
async fn run_kitty_ls() -> Result<String> {
    let output = Command::new("kitty")
        .args(["@", "ls"])
        .output()
        .await
        .context("failed to run kitty @ ls")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitty @ ls failed: {stderr}");
    }

    String::from_utf8(output.stdout).context("kitty @ ls output is not valid UTF-8")
}

// ── Utilities ───────────────────────────────────────────────────────────────

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_round_trip() {
        let data = SidecarData {
            sessions: vec![
                SidecarEntry {
                    session_id: "abc".into(),
                    worktree_path: Some("/tmp/wt-abc".into()),
                    profile_name: Some("dev".into()),
                    original_cwd: "/home/builder/projects/foo".into(),
                    spawn_time: 1_700_000_000,
                    display_name: Some("opus".into()),
                },
                SidecarEntry {
                    session_id: "def".into(),
                    worktree_path: None,
                    profile_name: None,
                    original_cwd: "/tmp".into(),
                    spawn_time: 1_700_000_001,
                    display_name: None,
                },
            ],
        };

        let json = serde_json::to_string(&data).expect("serialize");
        let decoded: SidecarData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.sessions.len(), 2);
        assert_eq!(decoded.sessions[0].session_id, "abc");
        assert_eq!(
            decoded.sessions[0].worktree_path.as_deref(),
            Some("/tmp/wt-abc")
        );
        assert_eq!(decoded.sessions[0].display_name.as_deref(), Some("opus"));
        assert_eq!(decoded.sessions[1].profile_name, None);
        assert_eq!(decoded.sessions[1].display_name, None);
    }

    #[test]
    fn sidecar_deserializes_without_display_name() {
        // Old sidecar files won't have display_name — serde(default) handles it.
        let json = r#"{"sessions":[{"session_id":"old","original_cwd":"/tmp","spawn_time":1700000000}]}"#;
        let data: SidecarData = serde_json::from_str(json).expect("deserialize");
        assert_eq!(data.sessions[0].session_id, "old");
        assert_eq!(data.sessions[0].display_name, None);
    }

    #[test]
    fn title_prefix_format() {
        let id = "my-session";
        let title = format!("{TITLE_PREFIX}{id}");
        assert_eq!(title, "thermal-my-session");
        assert_eq!(title.strip_prefix(TITLE_PREFIX), Some("my-session"));
    }

    #[test]
    fn parse_kitty_ls_json() {
        let json = r#"[
            {
                "tabs": [
                    {
                        "windows": [
                            {
                                "id": 1,
                                "title": "thermal-agent-1",
                                "cwd": "/home/builder/projects/foo",
                                "is_focused": true,
                                "foreground_processes": [
                                    {"pid": 1234, "cmdline": ["claude", "--model", "opus"]}
                                ]
                            },
                            {
                                "id": 2,
                                "title": "plain-shell",
                                "cwd": "/tmp",
                                "is_focused": false,
                                "foreground_processes": []
                            }
                        ]
                    }
                ]
            }
        ]"#;

        let os_windows: Vec<KittyOsWindow> = serde_json::from_str(json).expect("parse");
        assert_eq!(os_windows.len(), 1);
        assert_eq!(os_windows[0].tabs.len(), 1);
        assert_eq!(os_windows[0].tabs[0].windows.len(), 2);

        // Only the first window has a thermal- prefix.
        let win = &os_windows[0].tabs[0].windows[0];
        assert_eq!(win.title.strip_prefix(TITLE_PREFIX), Some("agent-1"));
        assert_eq!(win.id, 1);
        assert!(win.is_focused);
        assert_eq!(
            win.foreground_processes[0].cmdline.join(" "),
            "claude --model opus"
        );

        // Second window should not match.
        let win2 = &os_windows[0].tabs[0].windows[1];
        assert!(win2.title.strip_prefix(TITLE_PREFIX).is_none());
    }

    #[test]
    fn now_epoch_is_reasonable() {
        let epoch = now_epoch();
        // Should be after 2024-01-01 (1_704_067_200).
        assert!(epoch > 1_704_067_200);
    }

    #[test]
    fn controller_new_does_not_panic() {
        let _ctrl = KittyController::new();
    }

    // ── assign_display_name tests ──────────────────────────────────────────

    fn make_entry(session_id: &str, display_name: Option<&str>) -> SidecarEntry {
        SidecarEntry {
            session_id: session_id.into(),
            worktree_path: None,
            profile_name: None,
            original_cwd: "/tmp".into(),
            spawn_time: 1_700_000_000,
            display_name: display_name.map(String::from),
        }
    }

    #[test]
    fn assign_display_name_first_gets_bare_name() {
        let entries = vec![];
        assert_eq!(assign_display_name("opus", &entries, None), "opus");
    }

    #[test]
    fn assign_display_name_dedup_second() {
        let entries = vec![make_entry("s1", Some("opus"))];
        assert_eq!(assign_display_name("opus", &entries, None), "opus-2");
    }

    #[test]
    fn assign_display_name_dedup_third() {
        let entries = vec![
            make_entry("s1", Some("opus")),
            make_entry("s2", Some("opus-2")),
        ];
        assert_eq!(assign_display_name("opus", &entries, None), "opus-3");
    }

    #[test]
    fn assign_display_name_different_bases_no_conflict() {
        let entries = vec![make_entry("s1", Some("opus"))];
        assert_eq!(assign_display_name("sonnet", &entries, None), "sonnet");
    }

    #[test]
    fn assign_display_name_excludes_own_session() {
        // Re-upserting s1 should not conflict with s1's own display_name.
        let entries = vec![make_entry("s1", Some("opus"))];
        assert_eq!(
            assign_display_name("opus", &entries, Some("s1")),
            "opus"
        );
    }

    #[test]
    fn assign_display_name_excludes_own_but_conflicts_with_others() {
        let entries = vec![
            make_entry("s1", Some("opus")),
            make_entry("s2", Some("opus-2")),
        ];
        // Re-upserting s1 — "opus" is s1's own (excluded), but "opus-2" is s2's.
        assert_eq!(
            assign_display_name("opus", &entries, Some("s1")),
            "opus"
        );
    }

    #[test]
    fn assign_display_name_gap_in_numbering_fills_first_available() {
        // "opus" and "opus-3" taken (but not "opus-2").
        let entries = vec![
            make_entry("s1", Some("opus")),
            make_entry("s3", Some("opus-3")),
        ];
        assert_eq!(assign_display_name("opus", &entries, None), "opus-2");
    }

    #[test]
    fn assign_display_name_entries_without_display_name_ignored() {
        // Entries with None display_name should not conflict.
        let entries = vec![make_entry("s1", None)];
        assert_eq!(assign_display_name("opus", &entries, None), "opus");
    }

    // ── find_by_display_name tests ─────────────────────────────────────────

    #[test]
    fn find_by_display_name_found() {
        let data = SidecarData {
            sessions: vec![
                make_entry("s1", Some("opus")),
                make_entry("s2", Some("sonnet")),
            ],
        };
        let found = data.find_by_display_name("sonnet");
        assert!(found.is_some());
        assert_eq!(found.unwrap().session_id, "s2");
    }

    #[test]
    fn find_by_display_name_not_found() {
        let data = SidecarData {
            sessions: vec![make_entry("s1", Some("opus"))],
        };
        assert!(data.find_by_display_name("haiku").is_none());
    }

    #[test]
    fn find_by_display_name_none_entries_skipped() {
        let data = SidecarData {
            sessions: vec![make_entry("s1", None)],
        };
        assert!(data.find_by_display_name("opus").is_none());
    }

    #[test]
    fn find_by_display_name_numbered_variant() {
        let data = SidecarData {
            sessions: vec![
                make_entry("s1", Some("opus")),
                make_entry("s2", Some("opus-2")),
            ],
        };
        let found = data.find_by_display_name("opus-2");
        assert!(found.is_some());
        assert_eq!(found.unwrap().session_id, "s2");
    }
}
