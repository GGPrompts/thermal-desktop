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


/// Fixed layout dimensions (in logical pixels).
const TOOL_CARD_WIDTH: f32 = 320.0;
const TOOL_CARD_HEIGHT: f32 = 60.0;
const TOOL_CARD_MARGIN: f32 = 8.0;

const THINKING_HEIGHT: f32 = 36.0;
const THINKING_MARGIN: f32 = 8.0;

const CONTEXT_GAUGE_WIDTH: f32 = 200.0;
const CONTEXT_GAUGE_HEIGHT: f32 = 24.0;
const CONTEXT_GAUGE_MARGIN: f32 = 8.0;

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
            let y = TOOL_CARD_MARGIN + stack_index as f32 * (TOOL_CARD_HEIGHT + TOOL_CARD_MARGIN);
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

        // Right-anchored, same column as tool cards but from bottom.
        WidgetKind::ResultCard(_) => {
            let x = viewport_w - RESULT_CARD_WIDTH - TOOL_CARD_MARGIN;
            let y = TOOL_CARD_MARGIN + stack_index as f32 * (RESULT_CARD_HEIGHT + TOOL_CARD_MARGIN);
            WidgetRect {
                x,
                y,
                width: RESULT_CARD_WIDTH,
                height: RESULT_CARD_HEIGHT,
            }
        }
    }
}
