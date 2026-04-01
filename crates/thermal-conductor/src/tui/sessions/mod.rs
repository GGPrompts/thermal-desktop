//! Sessions page — absorbed from thermal-monitor.
//!
//! Shows all Claude sessions with subagent nesting, context %, mouse scroll,
//! kitty attach, and a history popup overlay.

mod chat_panel;
mod display;
mod format;
mod input;
mod preview;
mod render;
mod workspace;

use chat_panel::*;
use display::*;
use format::*;
use preview::*;
use workspace::*;

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Command;
use std::time::Instant;

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::TableState,
};

// Re-export for tests.
#[cfg(test)]
use thermal_core::message::AgentId;

use thermal_core::{ClaudeSessionState, ClaudeStatePoller, ClaudeStatus, palette::ThermalPalette};

use crate::agent_timeline::{AgentTimeline, ToolCategory};
use crate::backend::BackendPreference;
use crate::profiles_config::{Profile, load_profiles, save_profiles};

use super::TuiPage;







// ---------------------------------------------------------------------------
// Panel focus
// ---------------------------------------------------------------------------

/// Which of the 3 session panels currently has keyboard/mouse focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FocusedPanel {
    AgentList,
    Preview,
    Chat,
}

impl FocusedPanel {
    /// Cycle forward: AgentList → Preview → Chat → AgentList.
    fn next(self) -> Self {
        match self {
            Self::AgentList => Self::Preview,
            Self::Preview => Self::Chat,
            Self::Chat => Self::AgentList,
        }
    }

    /// Cycle backward: AgentList → Chat → Preview → AgentList.
    fn prev(self) -> Self {
        match self {
            Self::AgentList => Self::Chat,
            Self::Preview => Self::AgentList,
            Self::Chat => Self::Preview,
        }
    }
}

// ---------------------------------------------------------------------------
// Sessions page state
// ---------------------------------------------------------------------------

pub(in crate::tui) struct SessionsPage {
    pub(super) sessions: Vec<ClaudeSessionState>,
    pub(super) display_rows: Vec<DisplayRow>,
    pub(super) table_state: TableState,
    pub(super) prev_state: HashMap<String, (ClaudeStatus, Option<String>)>,
    pub(super) cached_context_pct: HashMap<String, f64>,
    pub(super) history: HashMap<String, VecDeque<HistoryEntry>>,
    pub(super) history_popup: Option<String>,
    /// working_dir -> Hyprland workspace ID cache.
    pub(super) workspace_map: HashMap<String, i64>,
    pub(super) last_workspace_refresh: Instant,
    /// Per-session tool activity timelines, keyed by session_id.
    pub(super) timelines: HashMap<String, AgentTimeline>,
    /// Tracks when each session first disappeared from the active list.
    pub(super) stale_since: HashMap<String, Instant>,

    // -- Multi-select --
    pub(super) selected_set: HashSet<usize>,

    // -- Inline chat input --
    pub(super) chat_input: String,
    pub(super) chat_cursor: usize,
    pub(super) focused_panel: FocusedPanel,
    pub(super) chat_messages: VecDeque<ChatEntry>,
    pub(super) chat_status: Option<(String, bool, Instant)>,
    pub(super) chat_history: VecDeque<String>,
    pub(super) chat_history_index: Option<usize>,
    pub(super) chat_saved_input: String,

    // -- Preview pane --
    pub(super) preview_content: Vec<Line<'static>>,
    pub(super) preview_scroll: usize,
    pub(super) preview_pinned: bool,
    pub(super) last_preview_session: Option<String>,
    pub(super) last_preview_update: Option<Instant>,
    pub(super) kitty_window_map: HashMap<String, (String, i64)>,
    pub(super) last_kitty_ls: Option<Instant>,
    pub(super) backend_pref: BackendPreference,
    pub(super) daemon_session_map: HashMap<String, String>,
    pub(super) last_daemon_ls: Option<Instant>,

    // -- Focus throttle --
    pub(super) last_focus_time: Option<Instant>,

    // -- Autocomplete --
    pub(super) autocomplete_items: Vec<String>,
    pub(super) autocomplete_index: usize,
    pub(super) autocomplete_active: bool,

    // -- Panel rects for mouse hit-testing --
    pub(super) panel_rect_agent: Rect,
    pub(super) panel_rect_preview: Rect,
    pub(super) panel_rect_chat: Rect,

    // -- Bus subscriber --
    pub(super) bus_connection: Option<BusConnection>,
    pub(super) last_bus_seq: u64,
    pub(super) last_bus_connect_attempt: Option<Instant>,

    // -- Daemon semantic subscription --
    pub(super) daemon_sub_rx: Option<tokio::sync::watch::Receiver<Vec<ClaudeSessionState>>>,

    // -- Preview broadcast subscription --
    pub(super) preview_subscriber: Option<PreviewSubscriber>,
    pub(super) preview_attached_daemon_id: Option<String>,
}

impl SessionsPage {
    pub fn new(backend_pref: BackendPreference) -> Self {
        // When using the Daemon backend, try to subscribe to semantic events
        // for real-time session state (avoids file-watching overhead).
        // Sessions via daemon have source: "daemon" or "daemon:external".
        // Fallback to ClaudeStatePoller: source not tagged (file-derived).
        let daemon_sub_rx = if matches!(backend_pref, BackendPreference::Daemon) {
            crate::daemon_subscriber::try_spawn_subscriber()
        } else {
            None
        };

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
            stale_since: HashMap::new(),
            selected_set: HashSet::new(),
            chat_input: String::new(),
            chat_cursor: 0,
            focused_panel: FocusedPanel::AgentList,
            chat_messages: VecDeque::new(),
            chat_status: None,
            chat_history: VecDeque::new(),
            chat_history_index: None,
            chat_saved_input: String::new(),
            preview_content: Vec::new(),
            preview_scroll: 0,
            preview_pinned: false,
            last_preview_session: None,
            last_preview_update: None,
            kitty_window_map: HashMap::new(),
            last_kitty_ls: None,
            backend_pref,
            daemon_session_map: HashMap::new(),
            last_daemon_ls: None,
            last_focus_time: None,
            autocomplete_items: Vec::new(),
            autocomplete_index: 0,
            autocomplete_active: false,
            panel_rect_agent: Rect::default(),
            panel_rect_preview: Rect::default(),
            panel_rect_chat: Rect::default(),
            bus_connection: None,
            last_bus_seq: 0,
            last_bus_connect_attempt: None,
            daemon_sub_rx,
            preview_subscriber: None,
            preview_attached_daemon_id: None,
        }
    }

    fn update_from_poller(&mut self, poller: &mut ClaudeStatePoller) {
        // When a daemon subscription is active, use it instead of file polling.
        if let Some(ref rx) = self.daemon_sub_rx {
            let sessions = rx.borrow().clone();
            if !sessions.is_empty() || self.sessions.is_empty() {
                self.sessions = sessions;
            }
        } else {
            let updated = poller.poll();
            if !updated.is_empty() {
                self.sessions = updated;
            }
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
        // Record idle for sessions that have disappeared, and track staleness.
        let stale_ids: Vec<String> = self
            .timelines
            .keys()
            .filter(|id| !active_ids.contains(id.as_str()))
            .cloned()
            .collect();
        let now = Instant::now();
        for id in &stale_ids {
            if let Some(tl) = self.timelines.get_mut(id) {
                tl.record_idle();
            }
            // Mark when this session first went stale.
            self.stale_since.entry(id.clone()).or_insert(now);
        }
        // Sessions that reappeared are no longer stale.
        self.stale_since.retain(|id, _| !active_ids.contains(id.as_str()));

        // Garbage-collect sessions that have been stale for >30s and have no
        // corresponding state file on disk.
        const STALE_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
        let expired: Vec<String> = self
            .stale_since
            .iter()
            .filter(|(_, since)| now.duration_since(**since) >= STALE_GRACE)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            // Compatibility fallback: check /tmp state file existence as a
            // last-resort liveness signal. This direct file read is intentional
            // for stale-session GC — it catches sessions that disappeared from
            // the daemon event stream but still have a state file on disk
            // (e.g. unmanaged/external sessions). See claude_state.rs header
            // for the full state authority boundary documentation.
            let state_path = format!("/tmp/claude-code-state/{}.json", id);
            if std::path::Path::new(&state_path).exists() {
                // State file still present — keep the entry, reset the timer
                // so we re-check after another grace period.
                self.stale_since.insert(id.clone(), now);
                continue;
            }
            self.timelines.remove(id);
            self.stale_since.remove(id);
            self.cached_context_pct.remove(id);
            self.prev_state.remove(id);
            self.history.remove(id);
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
                self.chat_status = Some(("No session selected".into(), true, Instant::now()));
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
        let len = self.display_rows.len();
        let start = self.table_state.selected().unwrap_or(0);
        // Skip subagent rows — land on the next parent session.
        for offset in 1..=len {
            let idx = (start + offset) % len;
            if !self.display_rows[idx].is_subagent {
                self.table_state.select(Some(idx));
                return;
            }
        }
    }

    pub fn nav_up(&mut self) {
        if self.display_rows.is_empty() {
            return;
        }
        let len = self.display_rows.len();
        let start = self.table_state.selected().unwrap_or(0);
        for offset in 1..=len {
            let idx = (start + len - offset) % len;
            if !self.display_rows[idx].is_subagent {
                self.table_state.select(Some(idx));
                return;
            }
        }
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

        let cwd = self
            .table_state
            .selected()
            .and_then(|i| self.display_rows.get(i))
            .and_then(|row| row.session.working_dir.clone());

        if let Some(cwd) = cwd {
            self.last_focus_time = Some(Instant::now());
            if let Some((socket, wid)) = self.resolve_kitty_window(&cwd) {
                let match_arg = format!("id:{wid}");
                let _ = Command::new("kitty")
                    .args(["@", "--to", &socket, "focus-window", "--match", &match_arg])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
        }
    }

    pub fn attach_selected(&self) {
        if let Some(i) = self.table_state.selected()
            && let Some(row) = self.display_rows.get(i)
        {
            // Resolve workspace ID for the selected session.
            let ws_id = row.session.workspace.or_else(|| {
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
        self.render_sessions(f, area);
    }

    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        poller: &mut ClaudeStatePoller,
    ) -> bool {
        self.handle_key_sessions(key, poller)
    }

    fn has_text_focus(&self) -> bool {
        self.focused_panel == FocusedPanel::Chat
    }

    fn handle_mouse(
        &mut self,
        event: crossterm::event::MouseEvent,
        poller: &mut ClaudeStatePoller,
    ) {
        self.handle_mouse_sessions(event, poller);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
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
            stale_since: HashMap::new(),
            selected_set: HashSet::new(),
            chat_input: String::new(),
            chat_cursor: 0,
            focused_panel: FocusedPanel::AgentList,
            chat_messages: VecDeque::new(),
            chat_status: None,
            chat_history: VecDeque::new(),
            chat_history_index: None,
            chat_saved_input: String::new(),
            autocomplete_items: Vec::new(),
            autocomplete_index: 0,
            autocomplete_active: false,
            panel_rect_agent: Rect::default(),
            panel_rect_preview: Rect::default(),
            panel_rect_chat: Rect::default(),
            bus_connection: None,
            last_bus_seq: 0,
            last_bus_connect_attempt: None,
            preview_content: Vec::new(),
            preview_scroll: 0,
            preview_pinned: false,
            last_preview_session: None,
            last_preview_update: None,
            kitty_window_map: HashMap::new(),
            last_kitty_ls: None,
            backend_pref: BackendPreference::Auto,
            daemon_session_map: HashMap::new(),
            last_daemon_ls: None,
            last_focus_time: None,
            daemon_sub_rx: None,
            preview_subscriber: None,
            preview_attached_daemon_id: None,
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
        page.focused_panel = FocusedPanel::Chat;
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
        page.focused_panel = FocusedPanel::Chat;
        assert!(page.has_text_focus());
    }

    #[test]
    fn tab_cycles_panel_focus_forward() {
        let mut page = page_with_sessions(vec![]);
        assert_eq!(page.focused_panel, FocusedPanel::AgentList);
        page.focused_panel = page.focused_panel.next();
        assert_eq!(page.focused_panel, FocusedPanel::Preview);
        page.focused_panel = page.focused_panel.next();
        assert_eq!(page.focused_panel, FocusedPanel::Chat);
        page.focused_panel = page.focused_panel.next();
        assert_eq!(page.focused_panel, FocusedPanel::AgentList);
    }

    #[test]
    fn shift_tab_cycles_panel_focus_backward() {
        let mut page = page_with_sessions(vec![]);
        assert_eq!(page.focused_panel, FocusedPanel::AgentList);
        page.focused_panel = page.focused_panel.prev();
        assert_eq!(page.focused_panel, FocusedPanel::Chat);
        page.focused_panel = page.focused_panel.prev();
        assert_eq!(page.focused_panel, FocusedPanel::Preview);
        page.focused_panel = page.focused_panel.prev();
        assert_eq!(page.focused_panel, FocusedPanel::AgentList);
    }

    // ── Agent badges ────────────────────────────────────────────────────────

    #[test]
    fn agent_badge_uses_emoji() {
        let claude = ClaudeSessionState::default();
        let (badge, _) = agent_type_badge(&claude);
        assert!(
            badge.contains('\u{1F916}'),
            "badge should contain robot emoji: {badge}"
        );
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

    /// Test-only wrapper: calls parse_at_mention_with_sessions with no sessions.
    fn parse_at_mention(text: &str) -> (Option<AgentId>, String) {
        parse_at_mention_with_sessions(text, &[])
    }

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

    #[test]
    fn parse_at_mention_display_name_resolves_to_session() {
        let sessions = vec![ClaudeSessionState {
            session_id: "abc123".into(),
            model: Some("claude-opus-4-6".into()),
            agent_type: Some("claude".into()),
            ..Default::default()
        }];
        let (target, content) = parse_at_mention_with_sessions("@opus fix the build", &sessions);
        let target = target.unwrap();
        assert_eq!(target.agent_type, "claude");
        assert_eq!(target.key, "abc123");
        assert_eq!(content, "fix the build");
    }

    #[test]
    fn parse_at_mention_display_name_not_found_falls_through() {
        let sessions = vec![ClaudeSessionState {
            session_id: "abc123".into(),
            model: Some("claude-opus-4-6".into()),
            agent_type: Some("claude".into()),
            ..Default::default()
        }];
        let (target, _) = parse_at_mention_with_sessions("@gemini hello", &sessions);
        assert!(target.is_none());
    }

    #[test]
    fn parse_at_mention_static_takes_priority_over_display_name() {
        // If someone names a session "claude", the static target wins
        let sessions = vec![ClaudeSessionState {
            session_id: "abc123".into(),
            model: Some("claude-opus-4-6".into()),
            agent_type: Some("claude".into()),
            ..Default::default()
        }];
        let (target, _) = parse_at_mention_with_sessions("@claude hello", &sessions);
        let target = target.unwrap();
        assert_eq!(target.agent_type, "claude");
        assert_eq!(target.key, "default"); // static target, not session key
    }
}
