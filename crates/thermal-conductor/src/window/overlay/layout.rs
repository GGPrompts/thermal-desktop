//! Widget positioning and layout calculations.
//!
//! Layout anchoring:
//! - **Bottom-anchored**: passive status widgets (context gauge, thinking indicator)
//! - **Centered**: modal widgets (permission dialog)
//! - **Right-anchored**: tool call cards (stacked vertically from top-right)

use super::widgets::WidgetKind;

/// Pixel-space rectangle for widget placement.
#[derive(Debug, Clone, Copy)]
pub struct WidgetRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WidgetRect {
    /// Convert to normalized device coordinates (NDC) for wgpu.
    /// Input: pixel coordinates with origin at top-left.
    /// Output: NDC with origin at center, x: -1..1, y: -1..1 (y-up).
    pub fn to_ndc(&self, viewport_w: f32, viewport_h: f32) -> [f32; 4] {
        let x0 = (self.x / viewport_w) * 2.0 - 1.0;
        let y0 = 1.0 - (self.y / viewport_h) * 2.0;
        let x1 = ((self.x + self.width) / viewport_w) * 2.0 - 1.0;
        let y1 = 1.0 - ((self.y + self.height) / viewport_h) * 2.0;
        [x0, y1, x1, y0] // [left, bottom, right, top] in NDC
    }
}

/// Fixed layout dimensions (in logical pixels).
const TOOL_CARD_WIDTH: f32 = 320.0;
const TOOL_CARD_HEIGHT: f32 = 60.0;
const TOOL_CARD_MARGIN: f32 = 8.0;

const THINKING_HEIGHT: f32 = 36.0;
const THINKING_MARGIN: f32 = 8.0;

const CONTEXT_GAUGE_WIDTH: f32 = 200.0;
const CONTEXT_GAUGE_HEIGHT: f32 = 24.0;
const CONTEXT_GAUGE_MARGIN: f32 = 8.0;

const PERMISSION_WIDTH: f32 = 420.0;
const PERMISSION_HEIGHT: f32 = 140.0;

const RESULT_CARD_WIDTH: f32 = 320.0;
const RESULT_CARD_HEIGHT: f32 = 50.0;

/// Compute the layout rectangle for a widget given viewport dimensions
/// and its stacking index (for widgets that stack, like tool cards).
pub fn layout_widget(
    kind: &WidgetKind,
    viewport_w: f32,
    viewport_h: f32,
    stack_index: usize,
) -> WidgetRect {
    match kind {
        // Right-anchored, stacked vertically from top.
        WidgetKind::ToolCallCard(_) => {
            let x = viewport_w - TOOL_CARD_WIDTH - TOOL_CARD_MARGIN;
            let y = TOOL_CARD_MARGIN
                + stack_index as f32 * (TOOL_CARD_HEIGHT + TOOL_CARD_MARGIN);
            WidgetRect {
                x,
                y,
                width: TOOL_CARD_WIDTH,
                height: TOOL_CARD_HEIGHT,
            }
        }

        // Bottom-left, above context gauge.
        WidgetKind::ThinkingIndicator(_) => {
            let x = THINKING_MARGIN;
            let y = viewport_h
                - THINKING_HEIGHT
                - THINKING_MARGIN
                - CONTEXT_GAUGE_HEIGHT
                - CONTEXT_GAUGE_MARGIN;
            WidgetRect {
                x,
                y,
                width: viewport_w * 0.5,
                height: THINKING_HEIGHT,
            }
        }

        // Bottom-left corner.
        WidgetKind::ContextGauge(_) => {
            let x = CONTEXT_GAUGE_MARGIN;
            let y = viewport_h - CONTEXT_GAUGE_HEIGHT - CONTEXT_GAUGE_MARGIN;
            WidgetRect {
                x,
                y,
                width: CONTEXT_GAUGE_WIDTH,
                height: CONTEXT_GAUGE_HEIGHT,
            }
        }

        // Centered modal.
        WidgetKind::PermissionDialog(_) => {
            let x = (viewport_w - PERMISSION_WIDTH) / 2.0;
            let y = (viewport_h - PERMISSION_HEIGHT) / 2.0;
            WidgetRect {
                x,
                y,
                width: PERMISSION_WIDTH,
                height: PERMISSION_HEIGHT,
            }
        }

        // Right-anchored, same column as tool cards but from bottom.
        WidgetKind::ResultCard(_) => {
            let x = viewport_w - RESULT_CARD_WIDTH - TOOL_CARD_MARGIN;
            let y = TOOL_CARD_MARGIN
                + stack_index as f32 * (RESULT_CARD_HEIGHT + TOOL_CARD_MARGIN);
            WidgetRect {
                x,
                y,
                width: RESULT_CARD_WIDTH,
                height: RESULT_CARD_HEIGHT,
            }
        }
    }
}
