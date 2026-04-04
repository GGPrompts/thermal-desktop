//! Widget type definitions for agent overlay widgets.
//!
//! Each widget is a self-contained data object describing what to render.
//! Widgets are either **passive** (informational, never capture input) or
//! **modal** (capture all keyboard input until dismissed).

use std::time::Instant;

/// Unique identifier for a widget instance.
pub type WidgetId = u64;

/// Status of an active tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    /// Tool is currently executing.
    Running,
    /// Tool completed successfully.
    Completed,
    /// Tool failed with an error.
    Failed,
}

/// A card showing an active tool invocation.
#[derive(Debug, Clone)]
pub struct ToolCallCard {
    pub tool: String,
    pub status: ToolStatus,
    /// Optional file path associated with the tool (e.g., Read, Edit targets).
    pub file: Option<String>,
    /// Brief summary of the tool input (truncated).
    pub input_preview: String,
    /// Unique per-invocation identity from the agent (e.g., Claude Code `tool_use_id`).
    /// Used to disambiguate concurrent calls to the same tool.
    pub tool_use_id: Option<String>,
}

/// Animated thinking/reasoning indicator.
#[derive(Debug, Clone)]
pub struct ThinkingIndicator {
    /// Truncated preview of the thinking content.
    pub content_preview: String,
}

/// Context window usage gauge bar.
#[derive(Debug, Clone)]
pub struct ContextGauge {
    /// Tokens used (or approximate percentage as fraction 0.0-1.0).
    pub used: f32,
    /// Total context window capacity (1.0 = 100%).
    pub total: f32,
}

/// Tool result card — shown briefly after tool completion.
#[derive(Debug, Clone)]
pub struct ResultCard {
    pub tool: String,
    pub success: bool,
    pub summary: String,
    /// When the result was received (for auto-dismiss timing).
    pub received_at: Instant,
}

/// Union of all widget types.
#[derive(Debug, Clone)]
pub enum WidgetKind {
    ToolCallCard(ToolCallCard),
    ThinkingIndicator(ThinkingIndicator),
    ContextGauge(ContextGauge),
    ResultCard(ResultCard),
}

impl WidgetKind {
    /// Human-readable label for logging/debugging.
    pub fn label(&self) -> &'static str {
        match self {
            WidgetKind::ToolCallCard(_) => "ToolCallCard",
            WidgetKind::ThinkingIndicator(_) => "ThinkingIndicator",
            WidgetKind::ContextGauge(_) => "ContextGauge",
            WidgetKind::ResultCard(_) => "ResultCard",
        }
    }
}

/// A positioned widget instance with a unique ID.
#[derive(Debug, Clone)]
pub struct Widget {
    pub id: WidgetId,
    pub kind: WidgetKind,
}
