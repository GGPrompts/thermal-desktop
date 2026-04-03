//! GPU overlay widget system for agent sessions.
//!
//! The [`OverlayManager`] maintains a focus stack of active widgets overlaid
//! on the PTY terminal content. Widgets are either **passive** (informational,
//! never capture input) or **modal** (capture all keyboard input).
//!
//! Widget lifecycle is driven by [`AgentEvent`]s from the structured JSON
//! output parser.

#[allow(dead_code)]
pub mod layout;
#[allow(dead_code)]
pub mod renderer;
#[allow(dead_code)]
pub mod widgets;

use std::time::{Duration, Instant};

use widgets::{
    ContextGauge, ResultCard, ThinkingIndicator, ToolCallCard, ToolStatus, Widget, WidgetId,
    WidgetKind,
};

use crate::structured_output::AgentEvent;

/// Auto-dismiss duration for result cards.
const RESULT_DISMISS_SECS: u64 = 5;

/// Manages overlay widgets with a focus stack.
///
/// The PTY terminal is always the implicit base layer. Modal widgets are
/// pushed onto a focus stack and capture all keyboard input. Passive widgets
/// are rendered but never capture input.
pub struct OverlayManager {
    /// Focus stack of modal widgets. The top widget captures input.
    modal_stack: Vec<Widget>,
    /// Passive (non-capturing) widgets, rendered in insertion order.
    passive_widgets: Vec<Widget>,
    /// Monotonically increasing widget ID counter.
    next_id: WidgetId,
}

impl OverlayManager {
    /// Create a new empty overlay manager.
    pub fn new() -> Self {
        Self {
            modal_stack: Vec::new(),
            passive_widgets: Vec::new(),
            next_id: 1,
        }
    }

    fn alloc_id(&mut self) -> WidgetId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    // ── Modal widget management ──────────────────────────────────────

    /// Push a modal widget onto the focus stack. It will capture all
    /// keyboard input until popped.
    pub fn push_modal(&mut self, kind: WidgetKind) -> WidgetId {
        debug_assert!(kind.is_modal(), "push_modal called with non-modal widget");
        let id = self.alloc_id();
        tracing::debug!(id, label = kind.label(), "overlay: push modal");
        self.modal_stack.push(Widget { id, kind });
        id
    }

    /// Pop the top modal widget from the focus stack.
    /// Returns the removed widget, or `None` if the stack is empty.
    pub fn pop_modal(&mut self) -> Option<Widget> {
        let w = self.modal_stack.pop();
        if let Some(ref w) = w {
            tracing::debug!(id = w.id, label = w.kind.label(), "overlay: pop modal");
        }
        w
    }

    /// The top modal widget, if any. When present, it should receive
    /// all keyboard input instead of the PTY.
    pub fn top(&self) -> Option<&Widget> {
        self.modal_stack.last()
    }

    /// Whether a modal widget is currently capturing input.
    pub fn has_modal(&self) -> bool {
        !self.modal_stack.is_empty()
    }

    // ── Passive widget management ────────────────────────────────────

    /// Add a passive (non-capturing) widget.
    pub fn add_passive(&mut self, kind: WidgetKind) -> WidgetId {
        debug_assert!(
            !kind.is_modal(),
            "add_passive called with modal widget — use push_modal"
        );
        let id = self.alloc_id();
        tracing::debug!(id, label = kind.label(), "overlay: add passive");
        self.passive_widgets.push(Widget { id, kind });
        id
    }

    /// Remove a passive widget by ID. Returns true if found and removed.
    pub fn remove_passive(&mut self, id: WidgetId) -> bool {
        let len_before = self.passive_widgets.len();
        self.passive_widgets.retain(|w| w.id != id);
        let removed = self.passive_widgets.len() < len_before;
        if removed {
            tracing::debug!(id, "overlay: remove passive");
        }
        removed
    }

    /// Find a passive widget by ID.
    pub fn get_passive(&self, id: WidgetId) -> Option<&Widget> {
        self.passive_widgets.iter().find(|w| w.id == id)
    }

    /// Mutably access a passive widget by ID.
    pub fn get_passive_mut(&mut self, id: WidgetId) -> Option<&mut Widget> {
        self.passive_widgets.iter_mut().find(|w| w.id == id)
    }

    /// Find the first passive widget matching a predicate.
    pub fn find_passive<F>(&self, f: F) -> Option<&Widget>
    where
        F: Fn(&Widget) -> bool,
    {
        self.passive_widgets.iter().find(|w| f(w))
    }

    // ── Housekeeping ─────────────────────────────────────────────────

    /// Remove expired result cards (older than `RESULT_DISMISS_SECS`).
    /// Returns `true` if any widgets were removed (caller should mark dirty).
    pub fn gc_expired(&mut self) -> bool {
        let now = Instant::now();
        let dismiss = Duration::from_secs(RESULT_DISMISS_SECS);
        let before = self.passive_widgets.len();
        self.passive_widgets.retain(|w| {
            if let WidgetKind::ResultCard(ref card) = w.kind {
                now.duration_since(card.received_at) < dismiss
            } else {
                true
            }
        });
        self.passive_widgets.len() < before
    }

    /// Remove all widgets (reset state).
    pub fn clear(&mut self) {
        self.modal_stack.clear();
        self.passive_widgets.clear();
        tracing::debug!("overlay: cleared all widgets");
    }

    /// Whether there are any visible widgets (modal or passive).
    pub fn has_widgets(&self) -> bool {
        !self.modal_stack.is_empty() || !self.passive_widgets.is_empty()
    }

    // ── Rendering ────────────────────────────────────────────────────

    /// Render all visible overlay widgets.
    ///
    /// Passive widgets are rendered first (bottom layer), then modal widgets
    /// on top. Each widget gets a semi-transparent background quad and
    /// (eventually) text content.
    pub fn render(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        viewport_w: u32,
        viewport_h: u32,
    ) {
        let vw = viewport_w as f32;
        let vh = viewport_h as f32;

        // Count tool cards and result cards separately for stacking.
        let mut tool_card_idx: usize = 0;
        let mut result_card_idx: usize = 0;

        for widget in &self.passive_widgets {
            let stack_idx = match &widget.kind {
                WidgetKind::ToolCallCard(_) => {
                    let idx = tool_card_idx;
                    tool_card_idx += 1;
                    idx
                }
                WidgetKind::ResultCard(_) => {
                    let idx = result_card_idx;
                    result_card_idx += 1;
                    // Offset result cards below tool cards.
                    idx + tool_card_idx
                }
                _ => 0,
            };

            let rect = layout::layout_widget(&widget.kind, vw, vh, stack_idx);
            renderer::render_widget_quad(widget, &rect, vw, vh, encoder, view);
        }

        // Modal widgets render on top.
        for widget in &self.modal_stack {
            let rect = layout::layout_widget(&widget.kind, vw, vh, 0);
            renderer::render_widget_quad(widget, &rect, vw, vh, encoder, view);
        }
    }

    // ── AgentEvent handling ──────────────────────────────────────────

    /// Process an `AgentEvent` and update the widget state accordingly.
    /// Returns `true` if the overlay state changed (caller should mark dirty).
    pub fn handle_agent_event(&mut self, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::ToolUse { tool, input } => {
                // Extract file path from input if present.
                let file = input
                    .get("file_path")
                    .or_else(|| input.get("path"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned());

                // Truncate input to a preview string.
                let input_preview = {
                    let s = input.to_string();
                    if s.len() > 80 {
                        format!("{}...", &s[..77])
                    } else {
                        s
                    }
                };

                self.add_passive(WidgetKind::ToolCallCard(ToolCallCard {
                    tool: tool.clone(),
                    status: ToolStatus::Running,
                    file,
                    input_preview,
                }));
                true
            }

            AgentEvent::ToolResult {
                tool,
                output,
                is_error,
            } => {
                // Remove matching ToolCallCard.
                self.passive_widgets.retain(|w| {
                    !matches!(&w.kind, WidgetKind::ToolCallCard(c) if c.tool == *tool)
                });

                // Truncate output to a summary.
                let summary = if output.len() > 120 {
                    format!("{}...", &output[..117])
                } else {
                    output.clone()
                };

                self.add_passive(WidgetKind::ResultCard(ResultCard {
                    tool: tool.clone(),
                    success: !is_error,
                    summary,
                    received_at: Instant::now(),
                }));
                true
            }

            AgentEvent::Progress {
                tool,
                status,
                message,
            } => {
                // Update existing ToolCallCard status if found.
                let mut changed = false;
                for w in &mut self.passive_widgets {
                    if let WidgetKind::ToolCallCard(ref mut card) = w.kind {
                        if card.tool == *tool {
                            let new_status = match status.as_str() {
                                "running" => ToolStatus::Running,
                                "completed" => ToolStatus::Completed,
                                "failed" => ToolStatus::Failed,
                                _ => ToolStatus::Running,
                            };
                            if card.status != new_status {
                                card.status = new_status;
                                changed = true;
                            }
                            if let Some(msg) = message {
                                let preview = if msg.len() > 80 {
                                    format!("{}...", &msg[..77])
                                } else {
                                    msg.clone()
                                };
                                card.input_preview = preview;
                                changed = true;
                            }
                            break;
                        }
                    }
                }
                changed
            }

            AgentEvent::Thinking { content } => {
                // Remove existing thinking indicator (replace, don't stack).
                self.passive_widgets
                    .retain(|w| !matches!(&w.kind, WidgetKind::ThinkingIndicator(_)));

                let preview = if content.len() > 100 {
                    format!("{}...", &content[..97])
                } else {
                    content.clone()
                };

                if !preview.is_empty() {
                    self.add_passive(WidgetKind::ThinkingIndicator(ThinkingIndicator {
                        content_preview: preview,
                        started_at: Instant::now(),
                    }));
                }
                true
            }

            AgentEvent::AssistantMessage { .. } => {
                // Clear thinking indicator when assistant responds.
                let had_thinking = self
                    .passive_widgets
                    .iter()
                    .any(|w| matches!(&w.kind, WidgetKind::ThinkingIndicator(_)));
                self.passive_widgets
                    .retain(|w| !matches!(&w.kind, WidgetKind::ThinkingIndicator(_)));
                had_thinking
            }

            AgentEvent::UserMessage { .. } => {
                // Clear all transient state on new user message — fresh turn.
                let had_widgets = self.has_widgets();
                self.passive_widgets.retain(|w| {
                    // Keep context gauge, remove everything else.
                    matches!(&w.kind, WidgetKind::ContextGauge(_))
                });
                had_widgets
            }
        }
    }

    /// Update the context gauge. If no gauge exists, creates one.
    /// Returns true if the value changed.
    pub fn update_context_gauge(&mut self, used: f32, total: f32) -> bool {
        // Look for existing gauge.
        for w in &mut self.passive_widgets {
            if let WidgetKind::ContextGauge(ref mut gauge) = w.kind {
                if (gauge.used - used).abs() > 0.001 || (gauge.total - total).abs() > 0.001 {
                    gauge.used = used;
                    gauge.total = total;
                    return true;
                }
                return false;
            }
        }
        // No existing gauge — create one.
        self.add_passive(WidgetKind::ContextGauge(ContextGauge { used, total }));
        true
    }
}

impl Default for OverlayManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use widgets::PermissionDialog;

    #[test]
    fn modal_push_pop() {
        let mut mgr = OverlayManager::new();
        assert!(!mgr.has_modal());
        assert!(mgr.top().is_none());

        let id = mgr.push_modal(WidgetKind::PermissionDialog(PermissionDialog {
            tool: "Bash".into(),
            message: "Run rm -rf?".into(),
        }));
        assert!(mgr.has_modal());
        assert_eq!(mgr.top().unwrap().id, id);

        let popped = mgr.pop_modal().unwrap();
        assert_eq!(popped.id, id);
        assert!(!mgr.has_modal());
    }

    #[test]
    fn passive_add_remove() {
        let mut mgr = OverlayManager::new();
        let id = mgr.add_passive(WidgetKind::ContextGauge(ContextGauge {
            used: 0.5,
            total: 1.0,
        }));
        assert!(mgr.has_widgets());
        assert!(mgr.get_passive(id).is_some());

        assert!(mgr.remove_passive(id));
        assert!(!mgr.has_widgets());
        assert!(!mgr.remove_passive(id)); // already removed
    }

    #[test]
    fn handle_tool_use_creates_card() {
        let mut mgr = OverlayManager::new();
        let event = AgentEvent::ToolUse {
            tool: "Read".into(),
            input: serde_json::json!({"file_path": "/tmp/test.rs"}),
        };
        assert!(mgr.handle_agent_event(&event));
        assert_eq!(mgr.passive_widgets.len(), 1);
        match &mgr.passive_widgets[0].kind {
            WidgetKind::ToolCallCard(card) => {
                assert_eq!(card.tool, "Read");
                assert_eq!(card.file.as_deref(), Some("/tmp/test.rs"));
                assert_eq!(card.status, ToolStatus::Running);
            }
            other => panic!("expected ToolCallCard, got {:?}", other),
        }
    }

    #[test]
    fn handle_tool_result_replaces_card() {
        let mut mgr = OverlayManager::new();
        // First, create a tool card.
        mgr.handle_agent_event(&AgentEvent::ToolUse {
            tool: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        });
        assert_eq!(mgr.passive_widgets.len(), 1);

        // Then, deliver a result.
        mgr.handle_agent_event(&AgentEvent::ToolResult {
            tool: "Bash".into(),
            output: "file1.txt\nfile2.txt".into(),
            is_error: false,
        });

        // Should have replaced tool card with result card.
        assert_eq!(mgr.passive_widgets.len(), 1);
        match &mgr.passive_widgets[0].kind {
            WidgetKind::ResultCard(card) => {
                assert_eq!(card.tool, "Bash");
                assert!(card.success);
            }
            other => panic!("expected ResultCard, got {:?}", other),
        }
    }

    #[test]
    fn handle_thinking_replaces_previous() {
        let mut mgr = OverlayManager::new();
        mgr.handle_agent_event(&AgentEvent::Thinking {
            content: "first thought".into(),
        });
        mgr.handle_agent_event(&AgentEvent::Thinking {
            content: "second thought".into(),
        });
        // Should only have one thinking indicator.
        let thinking_count = mgr
            .passive_widgets
            .iter()
            .filter(|w| matches!(&w.kind, WidgetKind::ThinkingIndicator(_)))
            .count();
        assert_eq!(thinking_count, 1);
    }

    #[test]
    fn assistant_message_clears_thinking() {
        let mut mgr = OverlayManager::new();
        mgr.handle_agent_event(&AgentEvent::Thinking {
            content: "analyzing...".into(),
        });
        assert!(
            mgr.passive_widgets
                .iter()
                .any(|w| matches!(&w.kind, WidgetKind::ThinkingIndicator(_)))
        );

        mgr.handle_agent_event(&AgentEvent::AssistantMessage {
            content: "Here's the result".into(),
        });
        assert!(
            !mgr.passive_widgets
                .iter()
                .any(|w| matches!(&w.kind, WidgetKind::ThinkingIndicator(_)))
        );
    }

    #[test]
    fn context_gauge_update() {
        let mut mgr = OverlayManager::new();
        assert!(mgr.update_context_gauge(0.3, 1.0));
        // Same value should return false.
        assert!(!mgr.update_context_gauge(0.3, 1.0));
        // Different value should return true.
        assert!(mgr.update_context_gauge(0.5, 1.0));
    }

    #[test]
    fn gc_expired_removes_old_results() {
        let mut mgr = OverlayManager::new();
        // Manually insert a result card with a past timestamp.
        let id = mgr.alloc_id();
        mgr.passive_widgets.push(Widget {
            id,
            kind: WidgetKind::ResultCard(ResultCard {
                tool: "Test".into(),
                success: true,
                summary: "ok".into(),
                received_at: Instant::now() - Duration::from_secs(10),
            }),
        });
        assert!(mgr.gc_expired());
        assert!(mgr.passive_widgets.is_empty());
    }

    #[test]
    fn progress_updates_tool_card_status() {
        let mut mgr = OverlayManager::new();
        mgr.handle_agent_event(&AgentEvent::ToolUse {
            tool: "Bash".into(),
            input: serde_json::json!({"command": "cargo build"}),
        });

        let changed = mgr.handle_agent_event(&AgentEvent::Progress {
            tool: "Bash".into(),
            status: "completed".into(),
            message: Some("Build succeeded".into()),
        });
        assert!(changed);

        match &mgr.passive_widgets[0].kind {
            WidgetKind::ToolCallCard(card) => {
                assert_eq!(card.status, ToolStatus::Completed);
                assert!(card.input_preview.contains("Build succeeded"));
            }
            other => panic!("expected ToolCallCard, got {:?}", other),
        }
    }

    #[test]
    fn user_message_clears_transient_widgets() {
        let mut mgr = OverlayManager::new();
        mgr.update_context_gauge(0.5, 1.0);
        mgr.handle_agent_event(&AgentEvent::Thinking {
            content: "hmm".into(),
        });
        mgr.handle_agent_event(&AgentEvent::ToolUse {
            tool: "Read".into(),
            input: serde_json::json!({}),
        });
        assert_eq!(mgr.passive_widgets.len(), 3);

        mgr.handle_agent_event(&AgentEvent::UserMessage {
            content: "next question".into(),
        });
        // Only context gauge should remain.
        assert_eq!(mgr.passive_widgets.len(), 1);
        assert!(matches!(
            &mgr.passive_widgets[0].kind,
            WidgetKind::ContextGauge(_)
        ));
    }
}
