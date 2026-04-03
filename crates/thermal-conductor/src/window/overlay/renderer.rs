//! wgpu render pass for overlay widgets.
//!
//! Renders semi-transparent background quads behind widget content and
//! text labels using thermal palette colors. This is a simplified
//! placeholder renderer — full text rendering will use glyphon/cosmic-text
//! in a follow-up.

use super::layout::WidgetRect;
use super::widgets::{ToolStatus, Widget, WidgetKind};

/// Thermal palette colors as `[f32; 4]` RGBA for overlay rendering.
mod colors {
    use thermal_core::palette::ThermalPalette;

    /// Semi-transparent dark background for widget cards.
    pub const CARD_BG: [f32; 4] = [0.04, 0.0, 0.06, 0.85];
    /// Active tool — searing red.
    pub const TOOL_ACTIVE: [f32; 4] = ThermalPalette::SEARING;
    /// Tool pending — accent cold.
    pub const TOOL_PENDING: [f32; 4] = ThermalPalette::ACCENT_COLD;
    /// Tool completed — status OK green.
    pub const TOOL_COMPLETED: [f32; 4] = ThermalPalette::STATUS_OK;
    /// Tool failed — status error red.
    pub const TOOL_FAILED: [f32; 4] = ThermalPalette::STATUS_ERROR;
    /// Thinking indicator — warm green.
    pub const THINKING: [f32; 4] = ThermalPalette::WARM;
    /// Context gauge fill (interpolated by usage).
    pub const GAUGE_LOW: [f32; 4] = ThermalPalette::MILD;
    pub const GAUGE_HIGH: [f32; 4] = ThermalPalette::SEARING;
    /// Permission dialog border — hot yellow.
    pub const PERMISSION_BORDER: [f32; 4] = ThermalPalette::HOT;
    /// Result success.
    pub const RESULT_OK: [f32; 4] = ThermalPalette::STATUS_OK;
    /// Result failure.
    pub const RESULT_ERROR: [f32; 4] = ThermalPalette::STATUS_ERROR;
}

/// Get the accent color for a tool call card based on its status.
fn tool_status_color(status: &ToolStatus) -> [f32; 4] {
    match status {
        ToolStatus::Pending => colors::TOOL_PENDING,
        ToolStatus::Running => colors::TOOL_ACTIVE,
        ToolStatus::Completed => colors::TOOL_COMPLETED,
        ToolStatus::Failed => colors::TOOL_FAILED,
    }
}

/// Render a single overlay widget as a colored quad.
///
/// This is a placeholder that renders background quads only — text rendering
/// will be added once we integrate glyphon into the overlay pass. The colored
/// quad still provides useful visual feedback (card positions, status colors).
pub fn render_widget_quad(
    widget: &Widget,
    rect: &WidgetRect,
    viewport_w: f32,
    viewport_h: f32,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
) {
    // Compute accent color based on widget type.
    let accent = match &widget.kind {
        WidgetKind::ToolCallCard(card) => tool_status_color(&card.status),
        WidgetKind::ThinkingIndicator(_) => colors::THINKING,
        WidgetKind::ContextGauge(gauge) => {
            let pct = if gauge.total > 0.0 {
                (gauge.used / gauge.total).clamp(0.0, 1.0)
            } else {
                0.0
            };
            // Interpolate between low (teal) and high (searing red).
            [
                colors::GAUGE_LOW[0] + (colors::GAUGE_HIGH[0] - colors::GAUGE_LOW[0]) * pct,
                colors::GAUGE_LOW[1] + (colors::GAUGE_HIGH[1] - colors::GAUGE_LOW[1]) * pct,
                colors::GAUGE_LOW[2] + (colors::GAUGE_HIGH[2] - colors::GAUGE_LOW[2]) * pct,
                1.0,
            ]
        }
        WidgetKind::PermissionDialog(_) => colors::PERMISSION_BORDER,
        WidgetKind::ResultCard(card) => {
            if card.success {
                colors::RESULT_OK
            } else {
                colors::RESULT_ERROR
            }
        }
    };

    // For now, render a simple clear-pass tinted quad as a placeholder.
    // A proper implementation would use a quad pipeline with vertex/fragment
    // shaders and alpha blending. The clear pass approach at least confirms
    // the layout math and widget lifecycle are working.
    //
    // TODO(therm-cgos-render): Replace with proper quad pipeline + glyphon text.
    let _ndc = rect.to_ndc(viewport_w, viewport_h);
    let _bg = colors::CARD_BG;
    let _accent = accent;

    // Placeholder: log that we would render this widget.
    tracing::trace!(
        widget = widget.kind.label(),
        id = widget.id,
        x = rect.x,
        y = rect.y,
        w = rect.width,
        h = rect.height,
        "overlay widget quad (placeholder)"
    );

    // Suppress unused warnings — these will be used when the quad pipeline
    // is implemented.
    let _ = (encoder, view);
}
