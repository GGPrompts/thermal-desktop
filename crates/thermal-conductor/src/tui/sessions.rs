//! Sessions page — absorbed from thermal-monitor.
//!
//! Shows all Claude sessions with subagent nesting, context %, mouse scroll,
//! kitty attach, and a history popup overlay.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState, Wrap},
};

use thermal_core::message::{AgentId, Message, MessageType};

use thermal_core::{ClaudeSessionState, ClaudeStatePoller, ClaudeStatus, palette::ThermalPalette};

use crate::agent_timeline::{AgentTimeline, ToolCategory};
use crate::profiles_config::{Profile, load_profiles, save_profiles};

use super::TuiPage;

// ---------------------------------------------------------------------------
// Palette helpers (same as thermal-monitor)
// ---------------------------------------------------------------------------

const fn pal(c: [f32; 4]) -> Color {
    Color::Rgb(
        (c[0] * 255.0) as u8,
        (c[1] * 255.0) as u8,
        (c[2] * 255.0) as u8,
    )
}

const BG: Color = pal(ThermalPalette::BG);
const BG_SURFACE: Color = pal(ThermalPalette::BG_SURFACE);
const TEXT: Color = pal(ThermalPalette::TEXT);
const TEXT_BRIGHT: Color = pal(ThermalPalette::TEXT_BRIGHT);
const TEXT_MUTED: Color = pal(ThermalPalette::TEXT_MUTED);
const COLD: Color = pal(ThermalPalette::COLD);
const ACCENT_COLD: Color = pal(ThermalPalette::ACCENT_COLD);

/// Map a ToolCategory to a ratatui Color using thermal palette colors.
fn tool_category_color(cat: ToolCategory) -> Color {
    match cat {
        ToolCategory::Read => pal(ThermalPalette::COLD),
        ToolCategory::Write => pal(ThermalPalette::HOT),
        ToolCategory::Execute => pal(ThermalPalette::HOTTER),
        ToolCategory::Thinking => pal(ThermalPalette::MILD),
        ToolCategory::Idle => pal(ThermalPalette::FREEZING),
    }
}

fn status_color(status: &ClaudeStatus) -> Color {
    match status {
        ClaudeStatus::Idle => pal(ThermalPalette::COLD),
        ClaudeStatus::Processing => pal(ThermalPalette::WARM),
        ClaudeStatus::ToolUse => pal(ThermalPalette::HOT),
        ClaudeStatus::AwaitingInput => pal(ThermalPalette::SEARING),
    }
}

/// Emoji badge and color for the agent type column.
/// Subagent count renders next to the emoji: `\u{1F916}x3`.
fn agent_type_badge(session: &ClaudeSessionState) -> (String, Color) {
    let (emoji, color) = match session.agent_type.as_deref() {
        Some("copilot") => ("\u{1F916}", pal(ThermalPalette::ACCENT_HOT)),
        Some("codex") => ("\u{1F916}", pal(ThermalPalette::ACCENT_COOL)),
        _ => ("\u{1F916}", pal(ThermalPalette::ACCENT_WARM)),
    };

    let label = if let Some(n) = session.subagent_count
        && n > 0
    {
        format!("{}x{}", emoji, n)
    } else {
        emoji.to_string()
    };

    (label, color)
}

fn status_label(status: &ClaudeStatus) -> &'static str {
    match status {
        ClaudeStatus::Idle => "IDLE",
        ClaudeStatus::Processing => "RUNNING",
        ClaudeStatus::ToolUse => "TOOL USE",
        ClaudeStatus::AwaitingInput => "AWAITING",
    }
}

/// Color for context percentage thresholds.
fn ctx_color(pct: f32) -> Color {
    if pct < 50.0 {
        Color::Green
    } else if pct < 75.0 {
        Color::Yellow
    } else if pct < 90.0 {
        Color::Rgb(249, 115, 22) // orange
    } else {
        Color::Red
    }
}

// ---------------------------------------------------------------------------
// Hyprland workspace lookup
// ---------------------------------------------------------------------------

/// Query hyprctl for all client windows and return a PID → workspace map.
fn query_hyprland_workspaces() -> HashMap<u32, i64> {
    let output = match Command::new("hyprctl").args(["clients", "-j"]).output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return HashMap::new(),
    };

    #[derive(serde::Deserialize)]
    struct HyprClient {
        pid: u32,
        workspace: HyprWorkspace,
    }
    #[derive(serde::Deserialize)]
    struct HyprWorkspace {
        id: i64,
    }

    let clients: Vec<HyprClient> = match serde_json::from_slice(&output) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    clients
        .into_iter()
        .map(|c| (c.pid, c.workspace.id))
        .collect()
}

/// Walk up the process tree from `pid` until we find a PID in `window_pids`.
/// Returns the workspace ID if found.
fn find_workspace_for_pid(pid: u32, window_pids: &HashMap<u32, i64>) -> Option<i64> {
    let mut current = pid;
    // Walk up to 10 levels to avoid infinite loops.
    for _ in 0..10 {
        if let Some(&ws) = window_pids.get(&current) {
            return Some(ws);
        }
        // Read parent PID from /proc.
        let stat = match std::fs::read_to_string(format!("/proc/{current}/stat")) {
            Ok(s) => s,
            Err(_) => return None,
        };
        // Format: "pid (comm) state ppid ..."
        // Find the closing ')' then split to get ppid.
        let after_comm = match stat.rfind(')') {
            Some(pos) => &stat[pos + 2..],
            None => return None,
        };
        let ppid: u32 = match after_comm.split_whitespace().nth(1) {
            Some(s) => match s.parse() {
                Ok(p) => p,
                Err(_) => return None,
            },
            None => return None,
        };
        if ppid <= 1 {
            return None;
        }
        current = ppid;
    }
    None
}

// ---------------------------------------------------------------------------
// Activity formatting
// ---------------------------------------------------------------------------

/// Extract just the filename from a path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Build the activity string from session state.
fn format_activity(s: &ClaudeSessionState) -> String {
    if s.status == ClaudeStatus::Idle || s.status == ClaudeStatus::AwaitingInput {
        return "\u{2705} Ready".into();
    }

    let tool_name = s.current_tool.as_deref().unwrap_or("");
    if tool_name.is_empty() {
        return "\u{26A1} Processing".into();
    }

    let trunc = |s: &str, n: usize| -> String {
        if s.chars().count() > n {
            format!("{}...", s.chars().take(n).collect::<String>())
        } else {
            s.to_string()
        }
    };
    let detail = s
        .details
        .as_ref()
        .and_then(|d| d.args.as_ref())
        .map(|a| {
            if let Some(fp) = &a.file_path {
                basename(fp).to_string()
            } else if let Some(cmd) = &a.command {
                trunc(cmd, 20)
            } else if let Some(pat) = &a.pattern {
                pat.clone()
            } else if let Some(desc) = &a.description {
                trunc(desc, 20)
            } else {
                String::new()
            }
        })
        .unwrap_or_default();

    let (emoji, label) = match tool_name {
        "Read" => ("\u{1F4D6}", "Read"),
        "Write" => ("\u{1F4DD}", "Write"),
        "Edit" => ("\u{270F}\u{FE0F}", "Edit"),
        "Bash" => ("\u{1F53A}", "Bash"),
        "Glob" => ("\u{1F50D}", "Glob"),
        "Grep" => ("\u{1F50E}", "Grep"),
        "Task" | "Agent" => ("\u{1F916}", "Task"),
        "WebFetch" => ("\u{1F310}", "Fetch"),
        "WebSearch" => ("\u{1F50D}", "Search"),
        other => ("", other),
    };

    if emoji.is_empty() {
        label.to_string()
    } else if detail.is_empty() {
        format!("{} {}", emoji, label)
    } else {
        format!("{} {}: {}", emoji, label, detail)
    }
}

// ---------------------------------------------------------------------------
// Relative timestamps
// ---------------------------------------------------------------------------

fn relative_time(iso: &str) -> String {
    parse_secs_ago(iso)
        .map(|s| {
            let s = s.max(0);
            if s < 60 {
                format!("{}s", s)
            } else if s < 3600 {
                format!("{}m", s / 60)
            } else {
                format!("{}h", s / 3600)
            }
        })
        .unwrap_or_else(|| "-".into())
}

fn parse_secs_ago(iso: &str) -> Option<i64> {
    let s = iso.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, mo, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let time = time.split('.').next()?;
    let time = time.split('+').next()?;
    let mut t = time.split(':');
    let (h, mi, sc): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next().and_then(|s| s.parse().ok()).unwrap_or(0),
    );
    let (mut yr, mut mn) = (y, mo);
    if mn <= 2 {
        yr -= 1;
        mn += 12;
    }
    let days = 365 * yr + yr / 4 - yr / 100 + yr / 400 + (153 * (mn - 3) + 2) / 5 + day - 719469;
    let ts = days * 86400 + h * 3600 + mi * 60 + sc;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(now - ts)
}

// ---------------------------------------------------------------------------
// History tracking
// ---------------------------------------------------------------------------

const MAX_HISTORY: usize = 12;

struct HistoryEntry {
    text: String,
    timestamp: Instant,
}

// ---------------------------------------------------------------------------
// Display ordering -- parents first, subagents nested underneath
// ---------------------------------------------------------------------------

struct DisplayRow {
    session: ClaudeSessionState,
    is_subagent: bool,
    is_last_child: bool,
}

fn build_display_order(sessions: &[ClaudeSessionState]) -> Vec<DisplayRow> {
    let mut parents: Vec<&ClaudeSessionState> = sessions
        .iter()
        .filter(|s| s.parent_session_id.is_none())
        .collect();
    parents.sort_by(|a, b| a.session_id.cmp(&b.session_id));

    let mut rows = Vec::with_capacity(sessions.len());

    for parent in &parents {
        rows.push(DisplayRow {
            session: (*parent).clone(),
            is_subagent: false,
            is_last_child: false,
        });

        let mut children: Vec<&ClaudeSessionState> = sessions
            .iter()
            .filter(|s| s.parent_session_id.as_deref() == Some(&parent.session_id))
            .collect();
        children.sort_by(|a, b| a.session_id.cmp(&b.session_id));

        let child_count = children.len();
        for (i, child) in children.into_iter().enumerate() {
            rows.push(DisplayRow {
                session: child.clone(),
                is_subagent: true,
                is_last_child: i == child_count - 1,
            });
        }
    }

    // Orphan subagents
    for s in sessions {
        if s.parent_session_id.is_some()
            && !parents
                .iter()
                .any(|p| Some(p.session_id.as_str()) == s.parent_session_id.as_deref())
        {
            rows.push(DisplayRow {
                session: s.clone(),
                is_subagent: true,
                is_last_child: true,
            });
        }
    }

    rows
}

// ---------------------------------------------------------------------------
// Messages socket path
// ---------------------------------------------------------------------------

fn messages_socket_path() -> std::path::PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    std::path::PathBuf::from(format!("/run/user/{uid}/thermal/messages.sock"))
}

// ---------------------------------------------------------------------------
// Bus connection (subscriber) — mirrors chat.rs BusConnection
// ---------------------------------------------------------------------------

/// Non-blocking connection to the messages daemon.
struct BusConnection {
    reader: BufReader<UnixStream>,
    /// Buffer for partial line reads.
    line_buf: String,
}

impl BusConnection {
    /// Attempt to connect and send a Subscribe message.
    fn connect(since_seq: u64) -> Option<Self> {
        let path = messages_socket_path();
        let stream = UnixStream::connect(&path).ok()?;
        stream.set_nonblocking(true).ok()?;

        // Build subscribe message.
        let subscribe = Message {
            seq: 0,
            ts: 0,
            from: AgentId::new("user", "tui"),
            to: AgentId::new("daemon", "bus"),
            context_id: None,
            project: None,
            content: String::new(),
            msg_type: MessageType::Subscribe {
                since_seq: if since_seq > 0 { Some(since_seq) } else { None },
            },
            metadata: Default::default(),
        };

        let mut json = serde_json::to_string(&subscribe).ok()?;
        json.push('\n');

        // Briefly set blocking for the subscribe write.
        stream.set_nonblocking(false).ok()?;
        let mut write_stream = stream.try_clone().ok()?;
        write_stream.write_all(json.as_bytes()).ok()?;
        stream.set_nonblocking(true).ok()?;

        Some(Self {
            reader: BufReader::new(stream),
            line_buf: String::new(),
        })
    }

    /// Try to read any available messages (non-blocking).
    fn poll(&mut self) -> Vec<Message> {
        let mut msgs = Vec::new();
        loop {
            self.line_buf.clear();
            match self.reader.read_line(&mut self.line_buf) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = self.line_buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(msg) = serde_json::from_str::<Message>(trimmed) {
                        msgs.push(msg);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break, // Connection lost
            }
        }
        msgs
    }
}

// ---------------------------------------------------------------------------
// @-mention parsing
// ---------------------------------------------------------------------------

/// Valid target agent types for @-mentions.
const VALID_MENTION_TARGETS: &[&str] = &[
    "system", "claude", "codex", "planner", "user", "dispatcher",
];

/// Parse an @-mention from chat input text.
///
/// If the text starts with or contains `@target` (where target is a valid agent
/// type), extracts the target and returns the remaining content with the
/// @mention stripped.
///
/// Returns `(Option<AgentId>, cleaned_content)`.
fn parse_at_mention(text: &str) -> (Option<AgentId>, String) {
    let trimmed = text.trim();
    // Look for @word at the beginning of the text.
    if let Some(rest) = trimmed.strip_prefix('@') {
        // Extract the mention word (up to first whitespace).
        let (mention, content) = match rest.find(char::is_whitespace) {
            Some(pos) => (&rest[..pos], rest[pos..].trim_start()),
            None => (rest, ""),
        };
        let mention_lower = mention.to_lowercase();
        if VALID_MENTION_TARGETS.contains(&mention_lower.as_str()) {
            return (
                Some(AgentId::new(mention_lower, "default")),
                content.to_string(),
            );
        }
    }
    // No valid @-mention found.
    (None, trimmed.to_string())
}

/// Send text to a kitty window matching the given PID via `kitty @ send-text`.
/// This is a synchronous helper for use in the TUI event loop.
fn kitty_send_text(pid: u32, text: &str) -> bool {
    let match_arg = format!("pid:{pid}");
    Command::new("kitty")
        .args(["@", "send-text", "--match", &match_arg, "--"])
        .arg(text)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Send a message to the message bus via messages.sock.
///
/// If `target` is provided, routes to that specific agent. Otherwise sends as
/// broadcast to `("*", "broadcast")`.
fn send_to_message_bus(text: &str, target: Option<&AgentId>) -> bool {
    let path = messages_socket_path();
    let stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_nonblocking(false);

    // Send subscribe first, then the actual message.
    let subscribe = Message {
        seq: 0,
        ts: 0,
        from: AgentId::new("user", "tui"),
        to: AgentId::new("daemon", "bus"),
        context_id: None,
        project: None,
        content: String::new(),
        msg_type: MessageType::Subscribe { since_seq: None },
        metadata: Default::default(),
    };
    let mut json = match serde_json::to_string(&subscribe) {
        Ok(j) => j,
        Err(_) => return false,
    };
    json.push('\n');
    let mut writer = stream.try_clone().unwrap_or_else(|_| {
        // Fallback — just reuse the same stream (write + read on same fd).
        UnixStream::connect(&path).unwrap()
    });
    if writer.write_all(json.as_bytes()).is_err() {
        return false;
    }

    let to = match target {
        Some(id) => id.clone(),
        None => AgentId::new("*", "broadcast"),
    };

    let msg = Message {
        seq: 0,
        ts: 0,
        from: AgentId::new("user", "tui"),
        to,
        context_id: None,
        project: None,
        content: text.to_string(),
        msg_type: MessageType::AgentMsg,
        metadata: Default::default(),
    };
    let mut json = match serde_json::to_string(&msg) {
        Ok(j) => j,
        Err(_) => return false,
    };
    json.push('\n');
    writer.write_all(json.as_bytes()).is_ok()
}

// ---------------------------------------------------------------------------
// Sessions page state
// ---------------------------------------------------------------------------

/// Maximum recent messages to display in the chat area.
const MAX_RECENT_MESSAGES: usize = 50;

/// A recent message displayed in the inline chat area.
struct ChatEntry {
    from_label: String,
    content: String,
    timestamp: Instant,
}

pub struct SessionsPage {
    sessions: Vec<ClaudeSessionState>,
    display_rows: Vec<DisplayRow>,
    table_state: TableState,
    prev_state: HashMap<String, (ClaudeStatus, Option<String>)>,
    cached_context_pct: HashMap<String, f32>,
    history: HashMap<String, VecDeque<HistoryEntry>>,
    history_popup: Option<String>,
    /// working_dir → Hyprland workspace ID cache.
    workspace_map: HashMap<String, i64>,
    last_workspace_refresh: Instant,
    /// Per-session tool activity timelines, keyed by session_id.
    timelines: HashMap<String, AgentTimeline>,

    // -- Multi-select --
    /// Set of selected display_row indices.
    selected_set: HashSet<usize>,

    // -- Inline chat input --
    /// Text input buffer.
    chat_input: String,
    /// Cursor position in the input string (byte offset).
    chat_cursor: usize,
    /// Whether the chat input is focused.
    chat_focused: bool,
    /// Recent chat messages for display.
    chat_messages: VecDeque<ChatEntry>,
    /// Status message with error flag and timestamp.
    chat_status: Option<(String, bool, Instant)>,
    /// Command history for the chat input (most recent at front).
    chat_history: VecDeque<String>,
    /// Current position in chat_history (None = not browsing history).
    chat_history_index: Option<usize>,
    /// Saves the in-progress input when entering history browsing mode.
    chat_saved_input: String,

    // -- Preview pane --
    /// Cached lines from last `kitty @ get-text` call.
    preview_content: Vec<String>,
    /// Scroll offset for PgUp/PgDn within the preview pane.
    preview_scroll: usize,
    /// Session ID of the session currently being previewed (to detect selection changes).
    last_preview_session: Option<String>,
    /// Throttle preview refreshes (only every 500ms).
    last_preview_update: Option<Instant>,

    // -- Focus throttle --
    /// Last time we issued a kitty focus-window for single-select navigation.
    last_focus_time: Option<Instant>,

    // -- Autocomplete --
    /// Current filtered list of @-mention completions.
    autocomplete_items: Vec<String>,
    /// Selected item index in the autocomplete popup.
    autocomplete_index: usize,
    /// Whether the autocomplete popup is visible.
    autocomplete_active: bool,

    // -- Bus subscriber --
    /// Connection to messages.sock for receiving messages.
    bus_connection: Option<BusConnection>,
    /// Highest seen sequence number (for replay on reconnect).
    last_bus_seq: u64,
    /// Last connection attempt time (for retry throttling).
    last_bus_connect_attempt: Option<Instant>,
}

impl SessionsPage {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            display_rows: Vec::new(),
            table_state: TableState::default(),
            prev_state: HashMap::new(),
            cached_context_pct: HashMap::new(),
            history: HashMap::new(),
            history_popup: None,
            workspace_map: HashMap::new(),
            last_workspace_refresh: Instant::now() - std::time::Duration::from_secs(10),
            timelines: HashMap::new(),
            selected_set: HashSet::new(),
            chat_input: String::new(),
            chat_cursor: 0,
            chat_focused: false,
            chat_messages: VecDeque::new(),
            chat_status: None,
            chat_history: VecDeque::new(),
            chat_history_index: None,
            chat_saved_input: String::new(),
            preview_content: Vec::new(),
            preview_scroll: 0,
            last_preview_session: None,
            last_preview_update: None,
            last_focus_time: None,
            autocomplete_items: Vec::new(),
            autocomplete_index: 0,
            autocomplete_active: false,
            bus_connection: None,
            last_bus_seq: 0,
            last_bus_connect_attempt: None,
        }
    }

    fn update_from_poller(&mut self, poller: &mut ClaudeStatePoller) {
        let updated = poller.poll();
        if !updated.is_empty() {
            self.sessions = updated;
        }
        // Cache context_percent
        for s in &mut self.sessions {
            if let Some(pct) = s.context_percent {
                self.cached_context_pct.insert(s.session_id.clone(), pct);
            } else if let Some(&cached) = self.cached_context_pct.get(&s.session_id) {
                s.context_percent = Some(cached);
            }
        }

        // Feed per-session tool activity timelines.
        let active_ids: std::collections::HashSet<String> =
            self.sessions.iter().map(|s| s.session_id.clone()).collect();
        for s in &self.sessions {
            let tl = self
                .timelines
                .entry(s.session_id.clone())
                .or_insert_with(AgentTimeline::new);
            if s.status == ClaudeStatus::Idle {
                tl.record_idle();
            } else {
                tl.record_tool_change(s.current_tool.as_deref());
            }
        }
        // Record idle for sessions that have disappeared.
        let stale_ids: Vec<String> = self
            .timelines
            .keys()
            .filter(|id| !active_ids.contains(id.as_str()))
            .cloned()
            .collect();
        for id in stale_ids {
            if let Some(tl) = self.timelines.get_mut(&id) {
                tl.record_idle();
            }
        }

        self.display_rows = build_display_order(&self.sessions);
        self.clamp_selection();
        self.update_history();
    }

    fn update_history(&mut self) {
        let now = Instant::now();
        for s in &self.sessions {
            if s.parent_session_id.is_some() {
                continue;
            }
            let current = (s.status.clone(), s.current_tool.clone());
            let changed = match self.prev_state.get(&s.session_id) {
                Some(prev) => *prev != current,
                None => true,
            };
            if changed {
                let activity = format_activity(s);
                let entries = self.history.entry(s.session_id.clone()).or_default();
                entries.push_back(HistoryEntry {
                    text: activity,
                    timestamp: now,
                });
                while entries.len() > MAX_HISTORY {
                    entries.pop_front();
                }
                self.prev_state.insert(s.session_id.clone(), current);
            }
        }
    }

    fn force_refresh(&mut self, poller: &mut ClaudeStatePoller) {
        self.sessions = poller.get_all();
        self.display_rows = build_display_order(&self.sessions);
        self.clamp_selection();
    }

    /// Save the currently selected session as a new spawn profile.
    fn save_session_as_profile(&mut self) {
        let row = match self
            .table_state
            .selected()
            .and_then(|i| self.display_rows.get(i))
        {
            Some(r) => r,
            None => {
                self.chat_status = Some((
                    "No session selected".into(),
                    true,
                    Instant::now(),
                ));
                return;
            }
        };

        let session = &row.session;

        // Extract cwd — required to make a useful profile.
        let cwd = match session.working_dir.as_deref() {
            Some(d) if !d.is_empty() => d,
            _ => {
                self.chat_status = Some((
                    "Session has no working directory".into(),
                    true,
                    Instant::now(),
                ));
                return;
            }
        };

        // Build profile name: model_display_name + project dir basename.
        // e.g. "opus-thermal-desktop"
        let model = session.model_display_name();
        let project = std::path::Path::new(cwd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project");
        let profile_name = format!("{}-{}", model, project);

        // Load existing profiles and check for duplicates.
        let (default_cwd, mut profiles) = load_profiles();
        if profiles.iter().any(|p| p.name == profile_name) {
            self.chat_status = Some((
                format!("Profile already exists: {}", profile_name),
                true,
                Instant::now(),
            ));
            return;
        }

        // Build the new profile.
        let new_profile = Profile {
            name: profile_name.clone(),
            command: None,
            cwd: Some(cwd.to_string()),
            icon: None,
            count: 1,
            git_worktree: false,
        };
        profiles.push(new_profile);

        // Save.
        match save_profiles(default_cwd.as_deref(), &profiles) {
            Ok(()) => {
                self.chat_status = Some((
                    format!("Saved profile: {}", profile_name),
                    false,
                    Instant::now(),
                ));
            }
            Err(e) => {
                self.chat_status = Some((
                    format!("Failed to save profile: {}", e),
                    true,
                    Instant::now(),
                ));
            }
        }
    }

    fn clamp_selection(&mut self) {
        if self.display_rows.is_empty() {
            self.table_state.select(None);
        } else if let Some(i) = self.table_state.selected()
            && i >= self.display_rows.len()
        {
            self.table_state.select(Some(self.display_rows.len() - 1));
        }
    }

    pub fn nav_down(&mut self) {
        if self.display_rows.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let next = if i >= self.display_rows.len() - 1 {
            0
        } else {
            i + 1
        };
        self.table_state.select(Some(next));
    }

    pub fn nav_up(&mut self) {
        if self.display_rows.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let prev = if i == 0 {
            self.display_rows.len() - 1
        } else {
            i - 1
        };
        self.table_state.select(Some(prev));
    }

    /// Focus the kitty window for the currently highlighted session.
    ///
    /// Only fires when not in multi-select mode (selected_set is empty) and
    /// throttled to at most once every 200ms to avoid rapid workspace switching
    /// during fast scrolling.
    fn focus_selected_window(&mut self) {
        // Don't focus if multi-select is active.
        if !self.selected_set.is_empty() {
            return;
        }

        // Throttle: skip if last focus was less than 200ms ago.
        if let Some(last) = self.last_focus_time {
            if last.elapsed() < std::time::Duration::from_millis(200) {
                return;
            }
        }

        let pid = self
            .table_state
            .selected()
            .and_then(|i| self.display_rows.get(i))
            .and_then(|row| row.session.pid);

        if let Some(pid) = pid {
            self.last_focus_time = Some(Instant::now());
            let match_arg = format!("pid:{}", pid);
            // Fire-and-forget: spawn the process without waiting for output.
            let _ = Command::new("kitty")
                .args([
                    "@",
                    "focus-window",
                    "--to",
                    "unix:/tmp/kitty-thc",
                    "--match",
                    &match_arg,
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }

    pub fn attach_selected(&self) {
        if let Some(i) = self.table_state.selected()
            && let Some(row) = self.display_rows.get(i)
        {
            // Resolve workspace ID for the selected session.
            let ws_id = row
                .session
                .workspace
                .or_else(|| {
                    row.session
                        .working_dir
                        .as_deref()
                        .and_then(|wd| self.workspace_map.get(wd).copied())
                });

            // Switch to the correct Hyprland workspace first.
            if let Some(ws) = ws_id {
                let _ = Command::new("hyprctl")
                    .args(["dispatch", "workspace", &ws.to_string()])
                    .status();
            }

            let target = if let Some(ref parent) = row.session.parent_session_id {
                parent.as_str()
            } else {
                &row.session.session_id
            };
            let kitty_ok = Command::new("kitty")
                .args([
                    "@",
                    "focus-window",
                    "--match",
                    &format!("pid:{}", row.session.pid.unwrap_or(0)),
                ])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

            if !kitty_ok {
                // kitty focus failed (window gone or PID stale) — try tmux fallback
                let _ = Command::new("tmux")
                    .args(["switch-client", "-t", target])
                    .status();
            }
        }
    }

    pub fn toggle_history(&mut self) {
        if self.history_popup.is_some() {
            self.history_popup = None;
        } else if let Some(i) = self.table_state.selected()
            && let Some(row) = self.display_rows.get(i)
        {
            let target = row
                .session
                .parent_session_id
                .as_ref()
                .unwrap_or(&row.session.session_id)
                .clone();
            self.history_popup = Some(target);
        }
    }

    pub fn dismiss_history(&mut self) {
        self.history_popup = None;
    }

    /// Returns true if the history popup overlay is currently visible.
    #[allow(dead_code)]
    pub fn has_history_popup(&self) -> bool {
        self.history_popup.is_some()
    }

    /// Build a compact 1-line timeline bar from a session's AgentTimeline.
    ///
    /// Each character represents a time slice, colored by ToolCategory.
    /// The bar shows the most recent `width` slices, newest on the right.
    fn build_timeline_line(&self, session_id: &str, width: usize) -> Line<'static> {
        let timeline = match self.timelines.get(session_id) {
            Some(tl) if !tl.entries.is_empty() => tl,
            _ => {
                // No timeline data — return a dim placeholder.
                return Line::from(Span::styled(
                    "\u{2500}".repeat(width),
                    Style::default().fg(pal(ThermalPalette::FREEZING)),
                ));
            }
        };

        let now = Instant::now();
        let entries = &timeline.entries;

        // Determine the time window: last N seconds, where N = width (1 char = 1 second).
        let window_secs = width as f64;
        let window_start = now - std::time::Duration::from_secs_f64(window_secs);

        let mut spans: Vec<Span<'static>> = Vec::with_capacity(width);

        for i in 0..width {
            let slot_time = window_start + std::time::Duration::from_secs(i as u64);
            let slot_end = slot_time + std::time::Duration::from_secs(1);

            // Find which entry covers this time slot (latest entry that started before slot_end).
            let mut matched_cat = None;
            for entry in entries.iter().rev() {
                let entry_end = entry.end_time.unwrap_or(now);
                if entry.start_time < slot_end && entry_end > slot_time {
                    matched_cat = Some(entry.category);
                    break;
                }
            }

            let (ch, color) = match matched_cat {
                Some(cat) => {
                    let c = tool_category_color(cat);
                    let block = match cat {
                        ToolCategory::Read => "\u{2584}",     // lower half block
                        ToolCategory::Write => "\u{2588}",    // full block
                        ToolCategory::Execute => "\u{2593}",  // dark shade
                        ToolCategory::Thinking => "\u{2591}", // light shade
                        ToolCategory::Idle => "\u{2500}",     // horizontal line
                    };
                    (block, c)
                }
                None => ("\u{2500}", pal(ThermalPalette::FREEZING)),
            };

            spans.push(Span::styled(ch.to_string(), Style::default().fg(color)));
        }

        Line::from(spans)
    }

    // -- Multi-select --

    fn toggle_select_current(&mut self) {
        if let Some(i) = self.table_state.selected() {
            if self.selected_set.contains(&i) {
                self.selected_set.remove(&i);
            } else {
                self.selected_set.insert(i);
            }
        }
    }

    fn select_all(&mut self) {
        for i in 0..self.display_rows.len() {
            self.selected_set.insert(i);
        }
    }

    fn deselect_all(&mut self) {
        self.selected_set.clear();
    }

    // -- Bus subscriber --

    /// Attempt to connect to the message bus (throttled to every 3 seconds).
    fn try_bus_connect(&mut self) {
        if let Some(last) = self.last_bus_connect_attempt {
            if last.elapsed().as_secs() < 3 {
                return;
            }
        }
        self.last_bus_connect_attempt = Some(Instant::now());

        if let Some(conn) = BusConnection::connect(self.last_bus_seq) {
            self.bus_connection = Some(conn);
        }
    }

    /// Poll the bus connection for incoming messages and add them to chat_messages.
    fn poll_bus_messages(&mut self) {
        let msgs = if let Some(ref mut conn) = self.bus_connection {
            let msgs = conn.poll();
            if msgs.is_empty() {
                return;
            }
            msgs
        } else {
            return;
        };

        for msg in msgs {
            if msg.seq > self.last_bus_seq {
                self.last_bus_seq = msg.seq;
            }
            // Skip subscribe/ack control messages.
            match &msg.msg_type {
                MessageType::Subscribe { .. } | MessageType::Ack { .. } => continue,
                _ => {}
            }
            // Filter out our own messages (user/tui) to avoid echo.
            if msg.from.agent_type == "user" && msg.from.key == "tui" {
                continue;
            }
            let entry = ChatEntry {
                from_label: format!("{}/{}", msg.from.agent_type, msg.from.key),
                content: msg.content.clone(),
                timestamp: Instant::now(),
            };
            self.chat_messages.push_back(entry);
        }

        // Trim chat messages.
        while self.chat_messages.len() > MAX_RECENT_MESSAGES {
            self.chat_messages.pop_front();
        }
    }

    // -- Preview pane --

    /// Fetch terminal content from the selected session's kitty window via
    /// `kitty @ get-text`. Throttled to at most once every 500ms.
    fn fetch_preview(&mut self) {
        // Determine the currently selected session's PID.
        let selected = self.table_state.selected().and_then(|i| {
            self.display_rows.get(i).and_then(|row| {
                row.session.pid.map(|pid| (row.session.session_id.clone(), pid))
            })
        });

        let (session_id, pid) = match selected {
            Some(pair) => pair,
            None => {
                self.preview_content.clear();
                self.last_preview_session = None;
                return;
            }
        };

        // If the selection changed, reset scroll and force an immediate refresh.
        let selection_changed = self.last_preview_session.as_deref() != Some(&session_id);
        if selection_changed {
            self.last_preview_session = Some(session_id.clone());
            self.preview_scroll = 0;
            // Force refresh by clearing the throttle timestamp.
            self.last_preview_update = None;
        }

        // Throttle: only refresh every 500ms.
        if let Some(last) = self.last_preview_update {
            if last.elapsed() < std::time::Duration::from_millis(500) {
                return;
            }
        }
        self.last_preview_update = Some(Instant::now());

        // Run kitty @ get-text synchronously (called from tick()).
        let match_arg = format!("pid:{pid}");
        let result = Command::new("kitty")
            .args([
                "@",
                "get-text",
                "--extent=screen",
                "--match",
                &match_arg,
            ])
            .output();

        match result {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                self.preview_content = text.lines().map(|l| l.to_string()).collect();
                // Scroll to bottom (latest content).
                let content_len = self.preview_content.len();
                self.preview_scroll = content_len;
            }
            _ => {
                self.preview_content = vec!["(preview unavailable)".to_string()];
                self.preview_scroll = 0;
            }
        }
    }

    // -- Autocomplete --

    /// Build the full list of @-mention completion candidates.
    fn build_autocomplete(&self) -> Vec<String> {
        let mut items: Vec<String> = VALID_MENTION_TARGETS
            .iter()
            .map(|s| s.to_string())
            .collect();

        // Add live session model display names (deduplicated).
        let mut seen = std::collections::HashSet::new();
        for row in &self.display_rows {
            let name = row.session.model_display_name();
            if seen.insert(name.clone()) {
                items.push(name);
            }
        }

        items.sort();
        items.dedup();
        items
    }

    /// Update autocomplete state based on current chat_input and cursor position.
    fn update_autocomplete(&mut self) {
        // Find the last '@' before or at the cursor position.
        let before_cursor = &self.chat_input[..self.chat_cursor];
        if let Some(at_pos) = before_cursor.rfind('@') {
            let after_at = &before_cursor[at_pos + 1..];
            // If there's a space in the partial, the @-word is complete — dismiss.
            if after_at.contains(' ') {
                self.autocomplete_active = false;
                return;
            }
            let prefix = after_at.to_lowercase();
            let candidates = self.build_autocomplete();
            let filtered: Vec<String> = candidates
                .into_iter()
                .filter(|item| item.to_lowercase().starts_with(&prefix))
                .collect();

            if filtered.is_empty() {
                self.autocomplete_active = false;
            } else {
                self.autocomplete_items = filtered;
                self.autocomplete_index = self.autocomplete_index.min(
                    self.autocomplete_items.len().saturating_sub(1),
                );
                self.autocomplete_active = true;
            }
        } else {
            self.autocomplete_active = false;
        }
    }

    /// Accept the currently selected autocomplete item, replacing the @partial.
    fn accept_autocomplete(&mut self) {
        if !self.autocomplete_active || self.autocomplete_items.is_empty() {
            return;
        }
        let completion = self.autocomplete_items[self.autocomplete_index].clone();

        // Find the last '@' before cursor and replace from there to cursor.
        let before_cursor = &self.chat_input[..self.chat_cursor];
        if let Some(at_pos) = before_cursor.rfind('@') {
            let after_cursor = &self.chat_input[self.chat_cursor..];
            let new_input = format!(
                "{}@{} {}",
                &self.chat_input[..at_pos],
                completion,
                after_cursor,
            );
            // Place cursor right after the inserted completion + space.
            self.chat_cursor = at_pos + 1 + completion.len() + 1;
            self.chat_input = new_input;
        }

        self.autocomplete_active = false;
    }

    // -- Chat input --

    fn send_chat_input(&mut self) {
        let text = self.chat_input.trim().to_string();
        if text.is_empty() {
            return;
        }

        let selected_pids: Vec<(u32, String)> = if !self.selected_set.is_empty() {
            // Multi-select: send to all selected sessions.
            self.selected_set
                .iter()
                .filter_map(|&i| {
                    self.display_rows.get(i).and_then(|row| {
                        row.session.pid.map(|pid| {
                            let label = row.session.session_id.clone();
                            (pid, label)
                        })
                    })
                })
                .collect()
        } else if let Some(i) = self.table_state.selected() {
            // Single selection: send to the highlighted session.
            self.display_rows
                .get(i)
                .and_then(|row| {
                    row.session.pid.map(|pid| {
                        vec![(pid, row.session.session_id.clone())]
                    })
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        if selected_pids.is_empty() {
            // No session selected — send to message bus.
            // Parse @-mention for targeted routing.
            let (mention_target, cleaned_content) = parse_at_mention(&text);
            let (from_label, ok) = if let Some(ref target) = mention_target {
                let label = format!("you \u{2192} @{}", target);
                let ok = send_to_message_bus(&cleaned_content, Some(target));
                (label, ok)
            } else {
                let ok = send_to_message_bus(&text, None);
                ("you \u{2192} bus".to_string(), ok)
            };
            let display_content = if mention_target.is_some() {
                cleaned_content.clone()
            } else {
                text.clone()
            };
            let entry = ChatEntry {
                from_label,
                content: display_content,
                timestamp: Instant::now(),
            };
            self.chat_messages.push_back(entry);
            if !ok {
                self.chat_status = Some((
                    "Failed to send to message bus".into(),
                    true,
                    Instant::now(),
                ));
            }
        } else {
            // Send to kitty terminal(s) via send-text.
            let text_with_enter = format!("{}\r", text);
            let mut success_count = 0;
            let mut target_label = String::new();
            for (pid, label) in &selected_pids {
                if kitty_send_text(*pid, &text_with_enter) {
                    success_count += 1;
                }
                if target_label.is_empty() {
                    target_label = label.clone();
                }
            }
            if selected_pids.len() > 1 {
                target_label = format!("{} sessions", selected_pids.len());
            }
            let entry = ChatEntry {
                from_label: format!("you \u{2192} {}", target_label),
                content: text.clone(),
                timestamp: Instant::now(),
            };
            self.chat_messages.push_back(entry);
            if success_count < selected_pids.len() {
                self.chat_status = Some((
                    format!(
                        "Sent to {}/{} sessions",
                        success_count,
                        selected_pids.len()
                    ),
                    true,
                    Instant::now(),
                ));
            }
        }

        // Trim chat messages.
        while self.chat_messages.len() > MAX_RECENT_MESSAGES {
            self.chat_messages.pop_front();
        }

        // Push to command history.
        self.chat_history.push_front(text);
        if self.chat_history.len() > 100 {
            self.chat_history.pop_back();
        }
        self.chat_history_index = None;

        self.chat_input.clear();
        self.chat_cursor = 0;
    }

    fn chat_handle_char(&mut self, ch: char) {
        self.chat_input.insert(self.chat_cursor, ch);
        self.chat_cursor += ch.len_utf8();
    }

    fn chat_handle_backspace(&mut self) {
        if self.chat_cursor > 0 {
            let prev = self.chat_input[..self.chat_cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.chat_input.replace_range(prev..self.chat_cursor, "");
            self.chat_cursor = prev;
        }
    }
}

impl TuiPage for SessionsPage {
    fn title(&self) -> &str {
        "Sessions"
    }

    fn tick(&mut self, poller: &mut ClaudeStatePoller) {
        self.update_from_poller(poller);

        // Refresh workspace map every 3s (runs hyprctl + reads /proc).
        if self.last_workspace_refresh.elapsed() >= std::time::Duration::from_secs(3) {
            let window_pids = query_hyprland_workspaces();
            self.workspace_map.clear();
            // Scan live "claude" processes, read their cwd, walk to a window PID.
            if let Ok(output) = Command::new("pgrep").arg("-x").arg("claude").output() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if let Ok(pid) = line.trim().parse::<u32>()
                        && let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd"))
                        && let Some(cwd_str) = cwd.to_str()
                        && let Some(ws) = find_workspace_for_pid(pid, &window_pids)
                    {
                        self.workspace_map.insert(cwd_str.to_string(), ws);
                    }
                }
            }
            self.last_workspace_refresh = Instant::now();
        }

        // Refresh preview pane from kitty terminal content.
        self.fetch_preview();

        // Poll bus for incoming messages.
        if self.bus_connection.is_none() {
            self.try_bus_connect();
        }
        self.poll_bus_messages();

        // Clear chat status after 4 seconds.
        if let Some((_, _, when)) = &self.chat_status {
            if when.elapsed().as_secs() >= 4 {
                self.chat_status = None;
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chat_msg_height = if self.chat_messages.is_empty() { 0 } else {
            (self.chat_messages.len() as u16).min(5)
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(40),             // table
                Constraint::Length(1),                   // timeline bar for selected session
                Constraint::Percentage(35),             // preview pane
                Constraint::Length(chat_msg_height),     // recent chat messages
                Constraint::Length(3),                   // chat input bar
                Constraint::Length(1),                   // footer
            ])
            .split(area);

        // Background
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);

        // -- Session table header info --
        let parent_count = self.display_rows.iter().filter(|r| !r.is_subagent).count();
        let active = self
            .display_rows
            .iter()
            .filter(|r| !r.is_subagent && r.session.status != ClaudeStatus::Idle)
            .count();
        let subagent_count = self.display_rows.iter().filter(|r| r.is_subagent).count();
        let block_title = if subagent_count > 0 {
            format!(
                " Sessions [{} active / {}, {} subagents] ",
                active, parent_count, subagent_count
            )
        } else {
            format!(" Sessions [{} active / {}] ", active, parent_count)
        };

        let header_cells = [
            "\u{2610}", "Name", "Agent", "Status", "Activity", "Ctx%", "Project", "WS", "Updated",
        ]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            )
        });
        let header_row = Row::new(header_cells).height(1);

        let rows: Vec<Row> = self
            .display_rows
            .iter()
            .enumerate()
            .map(|(row_idx, row)| {
                let s = &row.session;
                let color = status_color(&s.status);
                let label = status_label(&s.status);
                let activity = format_activity(s);
                let (agent_badge, agent_color) = agent_type_badge(s);

                let checkbox = if self.selected_set.contains(&row_idx) {
                    "\u{2611}"  // checked box
                } else {
                    "\u{2610}"  // unchecked box
                };
                let check_color = if self.selected_set.contains(&row_idx) {
                    pal(ThermalPalette::WARM)
                } else {
                    TEXT_MUTED
                };

                let (ctx_str, ctx_c) = match s.context_percent {
                    Some(pct) => (format!("{:.0}%", pct), ctx_color(pct)),
                    None => ("-".into(), TEXT_MUTED),
                };

                let project = s
                    .working_dir
                    .as_deref()
                    .and_then(|d| std::path::Path::new(d).file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("-")
                    .to_string();

                let ws_str = s
                    .workspace
                    .map(|ws| ws.to_string())
                    .or_else(|| {
                        s.working_dir
                            .as_deref()
                            .and_then(|wd| self.workspace_map.get(wd))
                            .map(|ws| ws.to_string())
                    })
                    .unwrap_or_else(|| "-".into());

                let updated = s
                    .last_updated
                    .as_deref()
                    .map(relative_time)
                    .unwrap_or_else(|| "-".into());

                if row.is_subagent {
                    let tree = if row.is_last_child {
                        "\u{2514}\u{2500}"
                    } else {
                        "\u{251C}\u{2500}"
                    };
                    let agent_label = s
                        .agent_id
                        .as_deref()
                        .map(|id| if id.len() > 8 { &id[..8] } else { id })
                        .unwrap_or("agent");
                    let id_str = format!("{} {}", tree, agent_label);

                    Row::new(vec![
                        Cell::from(checkbox).style(Style::default().fg(check_color)),
                        Cell::from(id_str).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(agent_badge).style(Style::default().fg(agent_color)),
                        Cell::from(label).style(Style::default().fg(color)),
                        Cell::from(activity).style(Style::default().fg(TEXT)),
                        Cell::from(ctx_str).style(Style::default().fg(ctx_c)),
                        Cell::from(project.clone()).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(ws_str).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(updated).style(Style::default().fg(TEXT_MUTED)),
                    ])
                } else {
                    let display_name = s.model_display_name();
                    let name_label = if display_name.len() > 14 {
                        format!("{}..", &display_name[..12])
                    } else {
                        display_name
                    };

                    Row::new(vec![
                        Cell::from(checkbox).style(Style::default().fg(check_color)),
                        Cell::from(name_label).style(Style::default().fg(TEXT)),
                        Cell::from(agent_badge).style(Style::default().fg(agent_color)),
                        Cell::from(label).style(Style::default().fg(color)),
                        Cell::from(activity).style(Style::default().fg(TEXT_BRIGHT)),
                        Cell::from(ctx_str).style(Style::default().fg(ctx_c)),
                        Cell::from(project.clone()).style(Style::default().fg(TEXT_MUTED)),
                        Cell::from(ws_str).style(Style::default().fg(ACCENT_COLD)),
                        Cell::from(updated).style(Style::default().fg(TEXT_MUTED)),
                    ])
                }
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(3),  // checkbox
                Constraint::Length(14), // session id
                Constraint::Length(5),  // Agent emoji + count
                Constraint::Length(10), // status
                Constraint::Length(28), // activity
                Constraint::Length(6),  // ctx%
                Constraint::Min(14),   // project
                Constraint::Length(4),  // WS
                Constraint::Length(8),  // updated
            ],
        )
        .header(header_row)
        .block(
            Block::default()
                .title(block_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(COLD))
                .style(Style::default().bg(BG)),
        )
        .row_highlight_style(Style::default().bg(BG_SURFACE).add_modifier(Modifier::BOLD));

        f.render_stateful_widget(table, chunks[0], &mut self.table_state);

        // -- Timeline bar for selected session --
        {
            let tl_area = chunks[1];
            let bar_width = tl_area.width.saturating_sub(2) as usize; // leave 1 char padding each side
            let timeline_line = if let Some(idx) = self.table_state.selected() {
                if let Some(row) = self.display_rows.get(idx) {
                    let sid = &row.session.session_id;
                    let label_span =
                        Span::styled(" \u{2502} ", Style::default().fg(pal(ThermalPalette::COLD)));
                    let bar = self.build_timeline_line(sid, bar_width.saturating_sub(3));
                    let mut spans = vec![label_span];
                    spans.extend(bar.spans);
                    Line::from(spans)
                } else {
                    Line::from(Span::styled(
                        " no session selected",
                        Style::default().fg(TEXT_MUTED),
                    ))
                }
            } else {
                Line::from(Span::styled(
                    " no session selected",
                    Style::default().fg(TEXT_MUTED),
                ))
            };
            let tl_widget = Paragraph::new(timeline_line).style(Style::default().bg(BG));
            f.render_widget(tl_widget, tl_area);
        }

        // -- Preview pane --
        {
            let preview_area = chunks[2];
            let inner_height = preview_area.height.saturating_sub(2) as usize; // borders top+bottom

            let preview_widget = if self.preview_content.is_empty() {
                Paragraph::new(Line::from(Span::styled(
                    "(no session selected)",
                    Style::default().fg(TEXT_MUTED),
                )))
                .alignment(Alignment::Center)
                .block(
                    Block::default()
                        .title(" Preview ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(COLD))
                        .style(Style::default().bg(BG)),
                )
            } else {
                // Apply scroll: show lines ending at preview_scroll (bottom-anchored).
                // preview_scroll is the index of the line just past the visible window bottom.
                let total = self.preview_content.len();
                let end = self.preview_scroll.min(total);
                let start = end.saturating_sub(inner_height);
                let visible_lines: Vec<Line> = self.preview_content[start..end]
                    .iter()
                    .map(|l| Line::from(Span::styled(l.as_str(), Style::default().fg(TEXT_MUTED))))
                    .collect();

                let scroll_indicator = if total > inner_height {
                    let pct = if total == 0 {
                        100
                    } else {
                        (end * 100) / total
                    };
                    format!(" Preview [{}/{} {}%] ", end, total, pct)
                } else {
                    " Preview ".to_string()
                };

                Paragraph::new(visible_lines).block(
                    Block::default()
                        .title(scroll_indicator)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(COLD))
                        .style(Style::default().bg(BG)),
                )
            };
            f.render_widget(preview_widget, preview_area);
        }

        // -- Recent chat messages --
        if !self.chat_messages.is_empty() {
            let now = Instant::now();
            let msg_lines: Vec<Line> = self
                .chat_messages
                .iter()
                .rev()
                .take(chat_msg_height as usize)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|e| {
                    let ago = now.duration_since(e.timestamp).as_secs();
                    let rel = if ago < 60 {
                        format!("{}s", ago)
                    } else {
                        format!("{}m", ago / 60)
                    };
                    Line::from(vec![
                        Span::styled(
                            format!("[{rel}] "),
                            Style::default().fg(TEXT_MUTED),
                        ),
                        Span::styled(
                            format!("{}: ", e.from_label),
                            Style::default()
                                .fg(pal(ThermalPalette::WARM))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            e.content.clone(),
                            Style::default().fg(TEXT_BRIGHT),
                        ),
                    ])
                })
                .collect();
            let msgs_widget = Paragraph::new(msg_lines).style(Style::default().bg(BG));
            f.render_widget(msgs_widget, chunks[3]);
        }

        // -- Chat input bar --
        {
            let selected_count = self.selected_set.len();
            let target_hint = if selected_count > 1 {
                format!(" [{} selected] ", selected_count)
            } else if selected_count == 1 {
                let idx = *self.selected_set.iter().next().unwrap();
                let label = self
                    .display_rows
                    .get(idx)
                    .map(|r| r.session.model_display_name())
                    .unwrap_or_else(|| "-".into());
                format!(" [{}] ", label)
            } else if let Some(i) = self.table_state.selected() {
                let label = self
                    .display_rows
                    .get(i)
                    .map(|r| r.session.model_display_name())
                    .unwrap_or_else(|| "-".into());
                format!(" \u{2192} {} ", label)
            } else {
                " \u{2192} bus ".into()
            };

            let input_border_color = if self.chat_focused {
                pal(ThermalPalette::WARM)
            } else {
                COLD
            };
            let input_title = if self.chat_focused {
                format!("{}Enter=send, Esc=cancel ", target_hint)
            } else {
                format!("{}/ to type ", target_hint)
            };

            let (before_cursor, after_cursor) = self.chat_input.split_at(self.chat_cursor);
            let input_line = if self.chat_focused {
                Line::from(vec![
                    Span::styled(before_cursor, Style::default().fg(TEXT_BRIGHT)),
                    Span::styled(
                        if after_cursor.is_empty() {
                            " "
                        } else {
                            &after_cursor[..1]
                        },
                        Style::default().fg(BG).bg(TEXT_BRIGHT),
                    ),
                    Span::styled(
                        if after_cursor.len() > 1 {
                            &after_cursor[1..]
                        } else {
                            ""
                        },
                        Style::default().fg(TEXT_BRIGHT),
                    ),
                ])
            } else if self.chat_input.is_empty() {
                Line::from(Span::styled(
                    "Press / to start typing...",
                    Style::default().fg(TEXT_MUTED),
                ))
            } else {
                Line::from(Span::styled(
                    &self.chat_input,
                    Style::default().fg(TEXT),
                ))
            };

            let input_widget = Paragraph::new(input_line).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(input_border_color))
                    .title(input_title)
                    .title_style(Style::default().fg(if self.chat_focused {
                        pal(ThermalPalette::WARM)
                    } else {
                        TEXT_MUTED
                    }))
                    .style(Style::default().bg(BG)),
            );
            f.render_widget(input_widget, chunks[4]);
        }

        // -- Footer --
        let sel_count = self.selected_set.len();
        let sel_hint = if sel_count > 0 {
            vec![
                Span::styled(
                    format!(" {} selected ", sel_count),
                    Style::default()
                        .fg(pal(ThermalPalette::WARM))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(TEXT_MUTED)),
            ]
        } else {
            vec![]
        };

        let mut footer_spans = sel_hint;
        footer_spans.extend(vec![
            Span::styled(
                "j/k",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": nav  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "Space",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": select  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "Enter",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": attach  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "/",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": chat  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "h",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": history", Style::default().fg(TEXT_MUTED)),
        ]);

        let footer = if let Some((ref msg, is_error, _)) = self.chat_status {
            let color = if is_error {
                pal(ThermalPalette::SEARING)
            } else {
                pal(ThermalPalette::WARM)
            };
            Paragraph::new(Line::from(Span::styled(
                msg.as_str(),
                Style::default().fg(color),
            )))
            .alignment(Alignment::Center)
        } else {
            Paragraph::new(Line::from(footer_spans))
        }
        .style(Style::default().bg(BG));
        f.render_widget(footer, chunks[5]);

        // -- Autocomplete popup overlay --
        if self.autocomplete_active && !self.autocomplete_items.is_empty() {
            let chat_area = chunks[4];
            let item_count = self.autocomplete_items.len().min(8);
            let popup_h = item_count as u16 + 2; // +2 for border
            let max_item_len = self.autocomplete_items.iter().map(|s| s.len()).max().unwrap_or(0);
            let popup_w = (max_item_len as u16 + 4).min(chat_area.width); // +4 for border + padding

            // Position: above the chat input, aligned to the left of the input area.
            // Calculate X offset based on cursor position within the input.
            let before_cursor = &self.chat_input[..self.chat_cursor];
            let at_offset = before_cursor.rfind('@').unwrap_or(0);
            // +1 for the left border of the chat input block
            let popup_x = chat_area.x + 1 + at_offset as u16;
            let popup_x = popup_x.min(area.x + area.width.saturating_sub(popup_w));
            let popup_y = chat_area.y.saturating_sub(popup_h);

            let popup_rect = Rect::new(popup_x, popup_y, popup_w, popup_h);

            f.render_widget(Clear, popup_rect);

            let items: Vec<ListItem> = self
                .autocomplete_items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let style = if i == self.autocomplete_index {
                        Style::default().fg(BG).bg(ACCENT_COLD).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(TEXT_BRIGHT)
                    };
                    ListItem::new(format!(" @{} ", item)).style(style)
                })
                .collect();

            let list = List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(COLD))
                    .style(Style::default().bg(BG_SURFACE)),
            );
            f.render_widget(list, popup_rect);
        }

        // -- History popup overlay --
        if let Some(ref sid) = self.history_popup {
            self.render_history_popup(f, sid);
        }
    }

    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        poller: &mut ClaudeStatePoller,
    ) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // History popup intercepts all keys.
        if self.history_popup.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('h') => self.dismiss_history(),
                _ => {}
            }
            return false;
        }

        // Chat input mode.
        if self.chat_focused {
            // Autocomplete navigation intercepts certain keys.
            if self.autocomplete_active && !self.autocomplete_items.is_empty() {
                match key.code {
                    KeyCode::Tab | KeyCode::Down => {
                        self.autocomplete_index =
                            (self.autocomplete_index + 1) % self.autocomplete_items.len();
                        return false;
                    }
                    KeyCode::Up => {
                        self.autocomplete_index = if self.autocomplete_index == 0 {
                            self.autocomplete_items.len() - 1
                        } else {
                            self.autocomplete_index - 1
                        };
                        return false;
                    }
                    KeyCode::Enter => {
                        self.accept_autocomplete();
                        return false;
                    }
                    KeyCode::Esc => {
                        self.autocomplete_active = false;
                        return false;
                    }
                    // All other keys fall through to normal input handling.
                    _ => {}
                }
            }

            match key.code {
                KeyCode::Esc => {
                    self.chat_focused = false;
                    self.autocomplete_active = false;
                }
                KeyCode::Enter => {
                    self.send_chat_input();
                    self.autocomplete_active = false;
                }
                KeyCode::Backspace => {
                    self.chat_handle_backspace();
                    self.update_autocomplete();
                }
                KeyCode::Left => {
                    if self.chat_cursor > 0 {
                        self.chat_cursor = self.chat_input[..self.chat_cursor]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                    }
                    self.update_autocomplete();
                }
                KeyCode::Right => {
                    if self.chat_cursor < self.chat_input.len() {
                        self.chat_cursor = self.chat_input[self.chat_cursor..]
                            .char_indices()
                            .nth(1)
                            .map(|(i, _)| self.chat_cursor + i)
                            .unwrap_or(self.chat_input.len());
                    }
                    self.update_autocomplete();
                }
                KeyCode::Up => {
                    if !self.chat_history.is_empty() {
                        match self.chat_history_index {
                            None => {
                                self.chat_saved_input = self.chat_input.clone();
                                self.chat_history_index = Some(0);
                                self.chat_input = self.chat_history[0].clone();
                            }
                            Some(idx) if idx < self.chat_history.len() - 1 => {
                                let new_idx = idx + 1;
                                self.chat_history_index = Some(new_idx);
                                self.chat_input = self.chat_history[new_idx].clone();
                            }
                            _ => {} // Already at oldest entry
                        }
                        self.chat_cursor = self.chat_input.len();
                    }
                    self.autocomplete_active = false;
                }
                KeyCode::Down => {
                    match self.chat_history_index {
                        Some(0) => {
                            self.chat_history_index = None;
                            self.chat_input = self.chat_saved_input.clone();
                        }
                        Some(idx) if idx > 0 => {
                            let new_idx = idx - 1;
                            self.chat_history_index = Some(new_idx);
                            self.chat_input = self.chat_history[new_idx].clone();
                        }
                        _ => {} // Not browsing history
                    }
                    self.chat_cursor = self.chat_input.len();
                    self.autocomplete_active = false;
                }
                KeyCode::Home => self.chat_cursor = 0,
                KeyCode::End => self.chat_cursor = self.chat_input.len(),
                KeyCode::Char(ch) => {
                    self.chat_handle_char(ch);
                    self.update_autocomplete();
                }
                _ => {}
            }
            return false;
        }

        // Normal navigation mode.

        // Ctrl+A selects all, Ctrl+D deselects all.
        if ctrl && key.code == KeyCode::Char('a') {
            self.select_all();
            return false;
        }
        if ctrl && key.code == KeyCode::Char('d') {
            self.deselect_all();
            return false;
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.nav_down();
                self.focus_selected_window();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.nav_up();
                self.focus_selected_window();
            }
            KeyCode::Char(' ') => self.toggle_select_current(),
            KeyCode::Enter => self.attach_selected(),
            KeyCode::Char('/') => {
                self.chat_focused = true;
            }
            KeyCode::Char('h') => self.toggle_history(),
            KeyCode::Char('r') => self.force_refresh(poller),
            KeyCode::Char('s') => self.save_session_as_profile(),
            KeyCode::PageUp => {
                // Scroll preview up by ~10 lines.
                self.preview_scroll = self.preview_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                // Scroll preview down by ~10 lines.
                self.preview_scroll = (self.preview_scroll + 10).min(self.preview_content.len());
            }
            KeyCode::Home => {
                // Jump to top of preview.
                self.preview_scroll = 0;
            }
            KeyCode::End => {
                // Jump to bottom of preview.
                self.preview_scroll = self.preview_content.len();
            }
            _ => {}
        }
        false
    }

    fn has_text_focus(&self) -> bool {
        self.chat_focused
    }

    fn handle_mouse(
        &mut self,
        event: crossterm::event::MouseEvent,
        _poller: &mut ClaudeStatePoller,
    ) {
        use crossterm::event::{MouseButton, MouseEventKind};
        match event.kind {
            MouseEventKind::ScrollDown => self.nav_down(),
            MouseEventKind::ScrollUp => self.nav_up(),
            MouseEventKind::Down(MouseButton::Left) => {
                // The sessions table is rendered in chunks[0] which starts at
                // the page area's top. The table has a Block with Borders::ALL
                // (1 row top border) + 1 header row + 1 bottom_margin (not used
                // here but header height=1). So data rows start at relative row 2
                // (border + header).
                // Mouse coordinates are absolute, and the page area starts at row 3
                // (below the 3-row tab bar). So absolute data row 0 = row 3+1+1 = 5.
                let data_start = 3 + 1 + 1; // tab_bar(3) + table border(1) + header(1)
                if event.row >= data_start {
                    let clicked_row = (event.row - data_start) as usize;
                    if clicked_row < self.display_rows.len() {
                        self.table_state.select(Some(clicked_row));
                    }
                }
            }
            _ => {}
        }
    }

}

impl SessionsPage {
    fn render_history_popup(&self, f: &mut Frame, session_id: &str) {
        let area = f.area();
        let popup_w = (area.width as u32 * 60 / 100).min(72) as u16;
        let popup_h = 16u16.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(popup_w)) / 2;
        let y = (area.height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect::new(x, y, popup_w, popup_h);

        f.render_widget(Clear, popup_area);

        let short_id = if session_id.len() > 16 {
            &session_id[..16]
        } else {
            session_id
        };
        let title = format!(" History: {} ", short_id);

        let now = Instant::now();
        let lines: Vec<Line> = self
            .history
            .get(session_id)
            .map(|entries| {
                entries
                    .iter()
                    .rev()
                    .map(|e| {
                        let ago = now.duration_since(e.timestamp).as_secs();
                        let rel = if ago < 60 {
                            format!("{}s ago", ago)
                        } else if ago < 3600 {
                            format!("{}m ago", ago / 60)
                        } else {
                            format!("{}h ago", ago / 3600)
                        };
                        Line::from(vec![
                            Span::styled(format!("{:>7}  ", rel), Style::default().fg(TEXT_MUTED)),
                            Span::styled(&e.text, Style::default().fg(TEXT_BRIGHT)),
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default();

        let content = if lines.is_empty() {
            Paragraph::new("  No history yet.").style(Style::default().fg(TEXT_MUTED).bg(BG))
        } else {
            Paragraph::new(lines)
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: true })
        };

        let popup = content.block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT_COLD))
                .style(Style::default().bg(BG)),
        );

        f.render_widget(popup, popup_area);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use thermal_core::{ClaudeSessionState, ClaudeStatus};

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_session(id: &str, parent: Option<&str>) -> ClaudeSessionState {
        ClaudeSessionState {
            session_id: id.to_string(),
            parent_session_id: parent.map(String::from),
            ..ClaudeSessionState::default()
        }
    }

    fn make_session_with_status(id: &str, status: ClaudeStatus) -> ClaudeSessionState {
        ClaudeSessionState {
            session_id: id.to_string(),
            status,
            ..ClaudeSessionState::default()
        }
    }

    fn make_session_with_tool(
        id: &str,
        status: ClaudeStatus,
        tool: Option<&str>,
    ) -> ClaudeSessionState {
        ClaudeSessionState {
            session_id: id.to_string(),
            status,
            current_tool: tool.map(String::from),
            ..ClaudeSessionState::default()
        }
    }

    // ── build_display_order: ordering ─────────────────────────────────────────

    #[test]
    fn display_order_empty_input_produces_empty_output() {
        let rows = build_display_order(&[]);
        assert!(rows.is_empty());
    }

    #[test]
    fn display_order_single_parent() {
        let sessions = vec![make_session("parent-1", None)];
        let rows = build_display_order(&sessions);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session.session_id, "parent-1");
        assert!(!rows[0].is_subagent);
    }

    #[test]
    fn display_order_parent_before_child() {
        let sessions = vec![
            make_session("child-1", Some("parent-1")),
            make_session("parent-1", None),
        ];
        let rows = build_display_order(&sessions);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session.session_id, "parent-1");
        assert!(!rows[0].is_subagent);
        assert_eq!(rows[1].session.session_id, "child-1");
        assert!(rows[1].is_subagent);
    }

    #[test]
    fn display_order_multiple_parents_sorted_by_id() {
        let sessions = vec![
            make_session("beta", None),
            make_session("alpha", None),
            make_session("gamma", None),
        ];
        let rows = build_display_order(&sessions);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].session.session_id, "alpha");
        assert_eq!(rows[1].session.session_id, "beta");
        assert_eq!(rows[2].session.session_id, "gamma");
    }

    #[test]
    fn display_order_children_sorted_under_parent() {
        let sessions = vec![
            make_session("child-z", Some("parent")),
            make_session("parent", None),
            make_session("child-a", Some("parent")),
        ];
        let rows = build_display_order(&sessions);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].session.session_id, "parent");
        assert_eq!(rows[1].session.session_id, "child-a");
        assert_eq!(rows[2].session.session_id, "child-z");
        assert!(rows[1].is_subagent);
        assert!(rows[2].is_subagent);
    }

    #[test]
    fn display_order_last_child_flag() {
        let sessions = vec![
            make_session("parent", None),
            make_session("child-a", Some("parent")),
            make_session("child-b", Some("parent")),
        ];
        let rows = build_display_order(&sessions);
        // child-a is NOT the last child
        let child_a = rows
            .iter()
            .find(|r| r.session.session_id == "child-a")
            .unwrap();
        assert!(!child_a.is_last_child);
        // child-b IS the last child (alphabetically last)
        let child_b = rows
            .iter()
            .find(|r| r.session.session_id == "child-b")
            .unwrap();
        assert!(child_b.is_last_child);
    }

    #[test]
    fn display_order_single_child_is_last_child() {
        let sessions = vec![
            make_session("parent", None),
            make_session("child-only", Some("parent")),
        ];
        let rows = build_display_order(&sessions);
        let child = rows
            .iter()
            .find(|r| r.session.session_id == "child-only")
            .unwrap();
        assert!(child.is_last_child);
    }

    #[test]
    fn display_order_parent_is_never_last_child() {
        let sessions = vec![make_session("sole-parent", None)];
        let rows = build_display_order(&sessions);
        assert!(!rows[0].is_last_child);
    }

    #[test]
    fn display_order_orphan_subagent_appended_as_subagent() {
        // A session with a parent_session_id that doesn't correspond to any
        // known parent goes to the orphan section.
        let sessions = vec![make_session("orphan", Some("missing-parent"))];
        let rows = build_display_order(&sessions);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session.session_id, "orphan");
        assert!(rows[0].is_subagent);
        assert!(rows[0].is_last_child); // orphans are always is_last_child = true
    }

    #[test]
    fn display_order_mixed_parents_and_subagents() {
        let sessions = vec![
            make_session("p1", None),
            make_session("p2", None),
            make_session("p1-child1", Some("p1")),
            make_session("p2-child1", Some("p2")),
            make_session("p1-child2", Some("p1")),
        ];
        let rows = build_display_order(&sessions);
        // 2 parents + 2 children of p1 + 1 child of p2 = 5
        assert_eq!(rows.len(), 5);
        // p1 comes before p2 alphabetically
        assert_eq!(rows[0].session.session_id, "p1");
        assert!(!rows[0].is_subagent);
        assert_eq!(rows[1].session.session_id, "p1-child1");
        assert!(rows[1].is_subagent);
        assert_eq!(rows[2].session.session_id, "p1-child2");
        assert!(rows[2].is_subagent);
        assert!(rows[2].is_last_child); // last child of p1
        assert_eq!(rows[3].session.session_id, "p2");
        assert!(!rows[3].is_subagent);
        assert_eq!(rows[4].session.session_id, "p2-child1");
        assert!(rows[4].is_subagent);
    }

    // ── ctx_color thresholds ──────────────────────────────────────────────────

    #[test]
    fn ctx_color_below_50_is_green() {
        assert_eq!(ctx_color(0.0), Color::Green);
        assert_eq!(ctx_color(49.9), Color::Green);
    }

    #[test]
    fn ctx_color_50_to_74_is_yellow() {
        assert_eq!(ctx_color(50.0), Color::Yellow);
        assert_eq!(ctx_color(74.9), Color::Yellow);
    }

    #[test]
    fn ctx_color_75_to_89_is_orange() {
        assert_eq!(ctx_color(75.0), Color::Rgb(249, 115, 22));
        assert_eq!(ctx_color(89.9), Color::Rgb(249, 115, 22));
    }

    #[test]
    fn ctx_color_90_and_above_is_red() {
        assert_eq!(ctx_color(90.0), Color::Red);
        assert_eq!(ctx_color(100.0), Color::Red);
    }

    // ── format_activity ───────────────────────────────────────────────────────

    #[test]
    fn format_activity_idle_returns_ready() {
        let s = make_session_with_status("s", ClaudeStatus::Idle);
        assert_eq!(format_activity(&s), "✅ Ready");
    }

    #[test]
    fn format_activity_awaiting_returns_ready() {
        let s = make_session_with_status("s", ClaudeStatus::AwaitingInput);
        assert_eq!(format_activity(&s), "✅ Ready");
    }

    #[test]
    fn format_activity_processing_no_tool_returns_processing() {
        let s = ClaudeSessionState {
            status: ClaudeStatus::Processing,
            current_tool: None,
            ..ClaudeSessionState::default()
        };
        assert_eq!(format_activity(&s), "⚡ Processing");
    }

    #[test]
    fn format_activity_tool_use_empty_tool_returns_processing() {
        let s = ClaudeSessionState {
            status: ClaudeStatus::ToolUse,
            current_tool: Some(String::new()),
            ..ClaudeSessionState::default()
        };
        assert_eq!(format_activity(&s), "⚡ Processing");
    }

    #[test]
    fn format_activity_read_tool_no_detail() {
        let s = make_session_with_tool("s", ClaudeStatus::ToolUse, Some("Read"));
        let result = format_activity(&s);
        assert_eq!(result, "📖 Read");
    }

    #[test]
    fn format_activity_write_tool_no_detail() {
        let s = make_session_with_tool("s", ClaudeStatus::ToolUse, Some("Write"));
        let result = format_activity(&s);
        assert_eq!(result, "📝 Write");
    }

    #[test]
    fn format_activity_edit_tool_no_detail() {
        let s = make_session_with_tool("s", ClaudeStatus::ToolUse, Some("Edit"));
        let result = format_activity(&s);
        assert_eq!(result, "✏️ Edit");
    }

    #[test]
    fn format_activity_bash_tool_no_detail() {
        let s = make_session_with_tool("s", ClaudeStatus::ToolUse, Some("Bash"));
        let result = format_activity(&s);
        assert_eq!(result, "🔺 Bash");
    }

    #[test]
    fn format_activity_glob_tool_no_detail() {
        let s = make_session_with_tool("s", ClaudeStatus::ToolUse, Some("Glob"));
        let result = format_activity(&s);
        assert_eq!(result, "🔍 Glob");
    }

    #[test]
    fn format_activity_grep_tool_no_detail() {
        let s = make_session_with_tool("s", ClaudeStatus::ToolUse, Some("Grep"));
        let result = format_activity(&s);
        assert_eq!(result, "🔎 Grep");
    }

    #[test]
    fn format_activity_task_tool() {
        let s = make_session_with_tool("s", ClaudeStatus::ToolUse, Some("Task"));
        let result = format_activity(&s);
        assert_eq!(result, "🤖 Task");
    }

    #[test]
    fn format_activity_agent_tool() {
        let s = make_session_with_tool("s", ClaudeStatus::ToolUse, Some("Agent"));
        let result = format_activity(&s);
        assert_eq!(result, "🤖 Task");
    }

    #[test]
    fn format_activity_webfetch_tool() {
        let s = make_session_with_tool("s", ClaudeStatus::ToolUse, Some("WebFetch"));
        let result = format_activity(&s);
        assert_eq!(result, "🌐 Fetch");
    }

    #[test]
    fn format_activity_websearch_tool() {
        let s = make_session_with_tool("s", ClaudeStatus::ToolUse, Some("WebSearch"));
        let result = format_activity(&s);
        assert_eq!(result, "🔍 Search");
    }

    #[test]
    fn format_activity_unknown_tool_uses_name_as_label() {
        let s = make_session_with_tool("s", ClaudeStatus::ToolUse, Some("MyCustomTool"));
        let result = format_activity(&s);
        // No emoji prefix for unknown tools; label is the tool name.
        assert_eq!(result, "MyCustomTool");
    }

    #[test]
    fn format_activity_read_with_file_path_detail() {
        let s = ClaudeSessionState {
            status: ClaudeStatus::ToolUse,
            current_tool: Some("Read".into()),
            details: Some(thermal_core::ToolDetails {
                args: Some(thermal_core::ToolArgs {
                    file_path: Some("/home/builder/projects/foo/src/main.rs".into()),
                    ..thermal_core::ToolArgs::default()
                }),
                ..thermal_core::ToolDetails::default()
            }),
            ..ClaudeSessionState::default()
        };
        let result = format_activity(&s);
        // basename extraction — only the filename portion
        assert_eq!(result, "📖 Read: main.rs");
    }

    #[test]
    fn format_activity_bash_with_command_truncated() {
        let s = ClaudeSessionState {
            status: ClaudeStatus::ToolUse,
            current_tool: Some("Bash".into()),
            details: Some(thermal_core::ToolDetails {
                args: Some(thermal_core::ToolArgs {
                    command: Some("cargo test --workspace -- --nocapture 2>&1".into()),
                    ..thermal_core::ToolArgs::default()
                }),
                ..thermal_core::ToolDetails::default()
            }),
            ..ClaudeSessionState::default()
        };
        let result = format_activity(&s);
        // command > 20 chars → truncated with "..."
        assert!(result.starts_with("🔺 Bash: "));
        let detail = result.trim_start_matches("🔺 Bash: ");
        assert!(
            detail.ends_with("..."),
            "long command should be truncated: {detail}"
        );
        assert!(
            detail.chars().count() <= 23,
            "truncated detail should be at most 23 chars: {detail}"
        );
    }

    #[test]
    fn format_activity_bash_with_short_command_not_truncated() {
        let s = ClaudeSessionState {
            status: ClaudeStatus::ToolUse,
            current_tool: Some("Bash".into()),
            details: Some(thermal_core::ToolDetails {
                args: Some(thermal_core::ToolArgs {
                    command: Some("ls".into()),
                    ..thermal_core::ToolArgs::default()
                }),
                ..thermal_core::ToolDetails::default()
            }),
            ..ClaudeSessionState::default()
        };
        let result = format_activity(&s);
        assert_eq!(result, "🔺 Bash: ls");
    }

    #[test]
    fn agent_badge_with_subagent_count() {
        let s = ClaudeSessionState {
            subagent_count: Some(3),
            ..ClaudeSessionState::default()
        };
        let (badge, _color) = agent_type_badge(&s);
        assert!(
            badge.contains("x3"),
            "badge should contain subagent count: {badge}"
        );
    }

    #[test]
    fn agent_badge_zero_subagents_no_count() {
        let s = ClaudeSessionState {
            subagent_count: Some(0),
            ..ClaudeSessionState::default()
        };
        let (badge, _color) = agent_type_badge(&s);
        assert!(
            !badge.contains('x'),
            "zero subagents should not show count: {badge}"
        );
    }

    #[test]
    fn agent_badge_none_subagents_no_count() {
        let s = ClaudeSessionState {
            subagent_count: None,
            ..ClaudeSessionState::default()
        };
        let (badge, _color) = agent_type_badge(&s);
        assert!(
            !badge.contains('x'),
            "None subagent_count should not show count: {badge}"
        );
    }

    #[test]
    fn format_activity_no_longer_includes_subagent_indicator() {
        // Subagent count now lives in the badge, not the activity string.
        let s = ClaudeSessionState {
            status: ClaudeStatus::ToolUse,
            current_tool: Some("Task".into()),
            subagent_count: Some(3),
            ..ClaudeSessionState::default()
        };
        let result = format_activity(&s);
        assert!(
            !result.contains('\u{00D7}'),
            "activity should not contain subagent indicator: {result}"
        );
    }

    #[test]
    fn format_activity_grep_with_pattern_detail() {
        let s = ClaudeSessionState {
            status: ClaudeStatus::ToolUse,
            current_tool: Some("Grep".into()),
            details: Some(thermal_core::ToolDetails {
                args: Some(thermal_core::ToolArgs {
                    pattern: Some("fn main".into()),
                    ..thermal_core::ToolArgs::default()
                }),
                ..thermal_core::ToolDetails::default()
            }),
            ..ClaudeSessionState::default()
        };
        let result = format_activity(&s);
        assert_eq!(result, "🔎 Grep: fn main");
    }

    #[test]
    fn format_activity_unknown_tool_with_description_truncated() {
        let s = ClaudeSessionState {
            status: ClaudeStatus::ToolUse,
            current_tool: Some("MyTool".into()),
            details: Some(thermal_core::ToolDetails {
                args: Some(thermal_core::ToolArgs {
                    description: Some(
                        "A very long description that exceeds the twenty char limit".into(),
                    ),
                    ..thermal_core::ToolArgs::default()
                }),
                ..thermal_core::ToolDetails::default()
            }),
            ..ClaudeSessionState::default()
        };
        let result = format_activity(&s);
        // unknown tool: no emoji, so format is "label: detail" but label == tool name
        assert!(
            result.contains("MyTool"),
            "should contain tool name: {result}"
        );
    }

    // ── relative_time / parse_secs_ago ────────────────────────────────────────

    /// Returns an ISO 8601 timestamp for `seconds_ago` seconds in the past.
    fn iso_ago(seconds_ago: u64) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ts = now.saturating_sub(seconds_ago);
        // Convert epoch → broken-down time (no-dep algorithm).
        let days = (ts / 86400) as i64;
        let secs_of_day = ts % 86400;
        let h = secs_of_day / 3600;
        let m = (secs_of_day % 3600) / 60;
        let s = secs_of_day % 60;
        // Civil date from day count (days since 1970-01-01).
        // Using the same approach as the source's parse_secs_ago inverse.
        let z = days + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let mo = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if mo <= 2 { y + 1 } else { y };
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, m, s)
    }

    #[test]
    fn relative_time_seconds_ago() {
        let iso = iso_ago(30);
        let result = relative_time(&iso);
        // Should end with 's' and be a small number
        assert!(result.ends_with('s'), "expected Xs format, got: {result}");
        let n: i64 = result.trim_end_matches('s').parse().unwrap();
        // Allow ±3 s for test execution timing
        assert!((25..=35).contains(&n), "expected ~30s, got {n}");
    }

    #[test]
    fn relative_time_minutes_ago() {
        let iso = iso_ago(125); // 2m5s
        let result = relative_time(&iso);
        assert!(result.ends_with('m'), "expected Xm format, got: {result}");
        let n: i64 = result.trim_end_matches('m').parse().unwrap();
        assert_eq!(n, 2, "expected 2m, got {n}");
    }

    #[test]
    fn relative_time_hours_ago() {
        let iso = iso_ago(7200); // exactly 2h
        let result = relative_time(&iso);
        assert!(result.ends_with('h'), "expected Xh format, got: {result}");
        let n: i64 = result.trim_end_matches('h').parse().unwrap();
        assert_eq!(n, 2, "expected 2h, got {n}");
    }

    #[test]
    fn relative_time_invalid_iso_returns_dash() {
        let result = relative_time("not-a-timestamp");
        assert_eq!(result, "-");
    }

    #[test]
    fn relative_time_empty_string_returns_dash() {
        let result = relative_time("");
        assert_eq!(result, "-");
    }

    #[test]
    fn parse_secs_ago_with_milliseconds_and_z() {
        // Format: "2026-03-19T12:00:00.123Z"
        let iso = iso_ago(60);
        // Append fractional seconds to simulate real Claude timestamps.
        let with_ms = iso.replace('Z', ".999Z");
        let result = relative_time(&with_ms);
        assert!(
            result.ends_with('m') || result.ends_with('s'),
            "should parse ms-bearing timestamp: {result}"
        );
    }

    #[test]
    fn parse_secs_ago_with_offset() {
        // Timezone offset suffix "+00:00" should be stripped at the '+' split.
        let iso = iso_ago(45);
        let with_offset = iso.replace('Z', "+00:00");
        let result = relative_time(&with_offset);
        assert!(
            result.ends_with('s'),
            "should handle +offset timestamps: {result}"
        );
    }

    // ── SessionsPage: navigation ──────────────────────────────────────────────

    fn page_with_sessions(sessions: Vec<ClaudeSessionState>) -> SessionsPage {
        let display_rows = build_display_order(&sessions);
        SessionsPage {
            sessions,
            display_rows,
            table_state: ratatui::widgets::TableState::default(),
            prev_state: HashMap::new(),
            cached_context_pct: HashMap::new(),
            history: HashMap::new(),
            history_popup: None,
            workspace_map: HashMap::new(),
            last_workspace_refresh: Instant::now(),
            timelines: HashMap::new(),
            selected_set: HashSet::new(),
            chat_input: String::new(),
            chat_cursor: 0,
            chat_focused: false,
            chat_messages: VecDeque::new(),
            chat_status: None,
            chat_history: VecDeque::new(),
            chat_history_index: None,
            chat_saved_input: String::new(),
            autocomplete_items: Vec::new(),
            autocomplete_index: 0,
            autocomplete_active: false,
            bus_connection: None,
            last_bus_seq: 0,
            last_bus_connect_attempt: None,
            preview_content: Vec::new(),
            preview_scroll: 0,
            last_preview_session: None,
            last_preview_update: None,
            last_focus_time: None,
        }
    }

    #[test]
    fn nav_down_wraps_to_zero_at_end() {
        let mut page = page_with_sessions(vec![make_session("a", None), make_session("b", None)]);
        page.table_state.select(Some(1)); // last row
        page.nav_down();
        assert_eq!(page.table_state.selected(), Some(0));
    }

    #[test]
    fn nav_up_wraps_to_last_at_start() {
        let mut page = page_with_sessions(vec![make_session("a", None), make_session("b", None)]);
        page.table_state.select(Some(0)); // first row
        page.nav_up();
        assert_eq!(page.table_state.selected(), Some(1));
    }

    #[test]
    fn nav_down_no_op_when_empty() {
        let mut page = page_with_sessions(vec![]);
        page.nav_down(); // should not panic
        assert_eq!(page.table_state.selected(), None);
    }

    #[test]
    fn nav_up_no_op_when_empty() {
        let mut page = page_with_sessions(vec![]);
        page.nav_up(); // should not panic
        assert_eq!(page.table_state.selected(), None);
    }

    #[test]
    fn clamp_selection_removes_out_of_bounds_selection() {
        let mut page = page_with_sessions(vec![make_session("only", None)]);
        page.table_state.select(Some(99)); // out of bounds
        page.clamp_selection();
        assert_eq!(page.table_state.selected(), Some(0));
    }

    #[test]
    fn clamp_selection_sets_none_when_empty() {
        let mut page = page_with_sessions(vec![]);
        page.table_state.select(Some(0));
        page.clamp_selection();
        assert_eq!(page.table_state.selected(), None);
    }

    #[test]
    fn toggle_history_sets_popup_for_selected_session() {
        let mut page = page_with_sessions(vec![make_session("sess-1", None)]);
        page.table_state.select(Some(0));
        page.toggle_history();
        assert_eq!(page.history_popup.as_deref(), Some("sess-1"));
    }

    #[test]
    fn toggle_history_clears_popup_when_already_shown() {
        let mut page = page_with_sessions(vec![make_session("sess-1", None)]);
        page.table_state.select(Some(0));
        page.toggle_history();
        assert!(page.has_history_popup());
        page.toggle_history();
        assert!(!page.has_history_popup());
    }

    #[test]
    fn dismiss_history_clears_popup() {
        let mut page = page_with_sessions(vec![make_session("s", None)]);
        page.history_popup = Some("s".into());
        page.dismiss_history();
        assert!(!page.has_history_popup());
    }

    #[test]
    fn history_popup_for_subagent_uses_parent_id() {
        let sessions = vec![
            make_session("parent", None),
            make_session("child", Some("parent")),
        ];
        let mut page = page_with_sessions(sessions);
        // Find the index of the child row in display_rows.
        let child_idx = page
            .display_rows
            .iter()
            .position(|r| r.session.session_id == "child")
            .unwrap();
        page.table_state.select(Some(child_idx));
        page.toggle_history();
        // History popup should track the *parent* session, not the child.
        assert_eq!(page.history_popup.as_deref(), Some("parent"));
    }

    // ── status_label / status_color ───────────────────────────────────────────

    #[test]
    fn status_label_all_variants() {
        assert_eq!(status_label(&ClaudeStatus::Idle), "IDLE");
        assert_eq!(status_label(&ClaudeStatus::Processing), "RUNNING");
        assert_eq!(status_label(&ClaudeStatus::ToolUse), "TOOL USE");
        assert_eq!(status_label(&ClaudeStatus::AwaitingInput), "AWAITING");
    }

    #[test]
    fn status_color_returns_distinct_colors() {
        let idle = status_color(&ClaudeStatus::Idle);
        let processing = status_color(&ClaudeStatus::Processing);
        let tool_use = status_color(&ClaudeStatus::ToolUse);
        let awaiting = status_color(&ClaudeStatus::AwaitingInput);
        // Each status maps to a different color.
        assert_ne!(idle, processing);
        assert_ne!(processing, tool_use);
        assert_ne!(tool_use, awaiting);
    }

    // ── Multi-select ────────────────────────────────────────────────────────

    #[test]
    fn toggle_select_current_adds_and_removes() {
        let mut page = page_with_sessions(vec![make_session("a", None), make_session("b", None)]);
        page.table_state.select(Some(0));
        page.toggle_select_current();
        assert!(page.selected_set.contains(&0));
        assert_eq!(page.selected_set.len(), 1);

        page.toggle_select_current();
        assert!(!page.selected_set.contains(&0));
        assert!(page.selected_set.is_empty());
    }

    #[test]
    fn select_all_selects_every_row() {
        let mut page = page_with_sessions(vec![
            make_session("a", None),
            make_session("b", None),
            make_session("c", None),
        ]);
        page.select_all();
        assert_eq!(page.selected_set.len(), 3);
    }

    #[test]
    fn deselect_all_clears_selection() {
        let mut page = page_with_sessions(vec![make_session("a", None), make_session("b", None)]);
        page.select_all();
        assert_eq!(page.selected_set.len(), 2);
        page.deselect_all();
        assert!(page.selected_set.is_empty());
    }

    // ── Chat input ──────────────────────────────────────────────────────────

    #[test]
    fn chat_input_char_and_backspace() {
        let mut page = page_with_sessions(vec![]);
        page.chat_focused = true;
        page.chat_handle_char('h');
        page.chat_handle_char('i');
        assert_eq!(page.chat_input, "hi");
        assert_eq!(page.chat_cursor, 2);

        page.chat_handle_backspace();
        assert_eq!(page.chat_input, "h");
        assert_eq!(page.chat_cursor, 1);
    }

    #[test]
    fn has_text_focus_when_chat_focused() {
        let mut page = page_with_sessions(vec![]);
        assert!(!page.has_text_focus());
        page.chat_focused = true;
        assert!(page.has_text_focus());
    }

    // ── Agent badges ────────────────────────────────────────────────────────

    #[test]
    fn agent_badge_uses_emoji() {
        let claude = ClaudeSessionState::default();
        let (badge, _) = agent_type_badge(&claude);
        assert!(badge.contains('\u{1F916}'), "badge should contain robot emoji: {badge}");
    }

    #[test]
    fn agent_badge_colors_differ_by_type() {
        let claude = ClaudeSessionState::default();
        let codex = ClaudeSessionState {
            agent_type: Some("codex".into()),
            ..ClaudeSessionState::default()
        };
        let copilot = ClaudeSessionState {
            agent_type: Some("copilot".into()),
            ..ClaudeSessionState::default()
        };
        let (_, c1) = agent_type_badge(&claude);
        let (_, c2) = agent_type_badge(&codex);
        let (_, c3) = agent_type_badge(&copilot);
        assert_ne!(c1, c2);
        assert_ne!(c2, c3);
    }

    // ── parse_at_mention ──────────────────────────────────────────────────────

    #[test]
    fn parse_at_mention_dispatcher_with_content() {
        let (target, content) = parse_at_mention("@dispatcher make a workspace");
        let target = target.unwrap();
        assert_eq!(target.agent_type, "dispatcher");
        assert_eq!(target.key, "default");
        assert_eq!(content, "make a workspace");
    }

    #[test]
    fn parse_at_mention_claude_target() {
        let (target, content) = parse_at_mention("@claude hello world");
        let target = target.unwrap();
        assert_eq!(target.agent_type, "claude");
        assert_eq!(content, "hello world");
    }

    #[test]
    fn parse_at_mention_case_insensitive() {
        let (target, content) = parse_at_mention("@Dispatcher do something");
        let target = target.unwrap();
        assert_eq!(target.agent_type, "dispatcher");
        assert_eq!(content, "do something");
    }

    #[test]
    fn parse_at_mention_no_mention_returns_none() {
        let (target, content) = parse_at_mention("hello world");
        assert!(target.is_none());
        assert_eq!(content, "hello world");
    }

    #[test]
    fn parse_at_mention_invalid_target_returns_none() {
        let (target, content) = parse_at_mention("@foobar do stuff");
        assert!(target.is_none());
        assert_eq!(content, "@foobar do stuff");
    }

    #[test]
    fn parse_at_mention_only_target_no_content() {
        let (target, content) = parse_at_mention("@system");
        let target = target.unwrap();
        assert_eq!(target.agent_type, "system");
        assert_eq!(content, "");
    }

    #[test]
    fn parse_at_mention_all_valid_targets() {
        for name in VALID_MENTION_TARGETS {
            let input = format!("@{} test", name);
            let (target, content) = parse_at_mention(&input);
            assert!(target.is_some(), "should parse @{name}");
            assert_eq!(target.unwrap().agent_type, *name);
            assert_eq!(content, "test");
        }
    }

    #[test]
    fn parse_at_mention_with_leading_whitespace() {
        let (target, content) = parse_at_mention("  @planner plan the feature");
        let target = target.unwrap();
        assert_eq!(target.agent_type, "planner");
        assert_eq!(content, "plan the feature");
    }
}
