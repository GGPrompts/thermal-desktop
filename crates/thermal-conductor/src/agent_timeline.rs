//! Agent timeline — tracks tool usage over time and renders a GPU timeline bar.
//!
//! When the Claude state poller reports a tool change, we close the previous
//! entry and start a new one. The timeline bar renders at the bottom of the
//! window as a series of colored horizontal segments, one per tool invocation.

use std::collections::VecDeque;
use std::time::Instant;

/// Maximum number of timeline entries to keep.
const MAX_ENTRIES: usize = 500;

/// Height of the timeline bar in pixels.
pub const TIMELINE_BAR_HEIGHT: u32 = 64;

/// How a tool entry is categorized for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    /// Read, Glob, Grep, Search — blue/cool tones
    Read,
    /// Edit, Write — warm yellow tones
    Write,
    /// Bash, shell execution — hot orange
    Execute,
    /// Thinking, processing (no specific tool) — mild green
    Thinking,
    /// Idle / waiting for input — dim/transparent
    Idle,
}

impl ToolCategory {
    /// Classify a tool name into a category.
    pub fn from_tool_name(name: Option<&str>) -> Self {
        match name {
            None => Self::Thinking,
            Some(tool) => {
                let lower = tool.to_lowercase();
                if lower.contains("read")
                    || lower.contains("glob")
                    || lower.contains("grep")
                    || lower.contains("search")
                    || lower.contains("list")
                {
                    Self::Read
                } else if lower.contains("edit")
                    || lower.contains("write")
                    || lower.contains("notebook")
                {
                    Self::Write
                } else if lower.contains("bash")
                    || lower.contains("exec")
                    || lower.contains("shell")
                {
                    Self::Execute
                } else {
                    Self::Thinking
                }
            }
        }
    }
}

/// A single tool usage entry in the timeline.
#[derive(Debug, Clone)]
pub struct TimelineEntry {
    /// Name of the tool (or "Thinking" / "Idle").
    pub tool_name: String,
    /// When this tool invocation started.
    pub start_time: Instant,
    /// When this tool invocation ended. None if still active.
    pub end_time: Option<Instant>,
    /// Category for coloring.
    pub category: ToolCategory,
}

/// Agent timeline state — tracks tool transitions and visibility.
pub struct AgentTimeline {
    /// Chronological tool entries.
    pub entries: VecDeque<TimelineEntry>,
    /// Whether the timeline bar is visible.
    pub visible: bool,
    /// Horizontal scroll offset in seconds (0 = newest on right edge).
    pub scroll_offset: f64,
    /// The last tool name we recorded, used to detect transitions.
    last_tool: Option<String>,
    /// Whether we are currently tracking (have a Claude session).
    tracking: bool,
}

impl AgentTimeline {
    /// Create a new empty timeline.
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_ENTRIES),
            visible: false,
            scroll_offset: 0.0,
            last_tool: None,
            tracking: false,
        }
    }

    /// Record a tool change from the Claude state poller.
    ///
    /// If the tool name differs from the previous one, close the previous
    /// entry and start a new one.
    pub fn record_tool_change(&mut self, tool_name: Option<&str>) {
        let now = Instant::now();

        // Check if the tool actually changed.
        let new_name = tool_name.map(|s| s.to_string());
        if new_name == self.last_tool && self.tracking {
            return;
        }

        // Close the previous entry.
        if let Some(entry) = self.entries.back_mut()
            && entry.end_time.is_none()
        {
            entry.end_time = Some(now);
        }

        // Build a display name.
        let display_name = tool_name.unwrap_or("Thinking").to_string();
        let category = ToolCategory::from_tool_name(tool_name);

        // Create a new entry.
        let entry = TimelineEntry {
            tool_name: display_name,
            start_time: now,
            end_time: None,
            category,
        };
        self.entries.push_back(entry);

        // Trim to MAX_ENTRIES.
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }

        self.last_tool = new_name;
        self.tracking = true;
    }

    /// Record an idle state (no Claude session matched).
    pub fn record_idle(&mut self) {
        if !self.tracking {
            return;
        }

        let now = Instant::now();

        // Close the previous entry.
        if let Some(entry) = self.entries.back_mut()
            && entry.end_time.is_none()
        {
            entry.end_time = Some(now);
        }

        // Start an idle entry.
        let entry = TimelineEntry {
            tool_name: "Idle".to_string(),
            start_time: now,
            end_time: None,
            category: ToolCategory::Idle,
        };
        self.entries.push_back(entry);

        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }

        self.last_tool = None;
        self.tracking = false;
    }

    /// Toggle visibility.
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        tracing::info!(visible = self.visible, "Agent timeline toggled");
    }
}
