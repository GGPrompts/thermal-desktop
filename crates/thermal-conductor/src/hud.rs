//! Thermal HUD overlay renderer.
//!
//! Produces text lines, scanline helpers, and reticle geometry for the
//! thermal-camera aesthetic overlay drawn on top of pane content.

use thermal_core::ThermalPalette;

/// Input data for a HUD frame.
#[allow(dead_code)]
pub struct HudData {
    /// Current rendering frame rate.
    pub fps: f32,
    /// Total number of panes being tracked.
    pub agent_count: usize,
    /// Number of panes with `Running` or `Thinking` state.
    pub active_agents: usize,
    /// Files that have changed in the watched git repos.
    pub changed_files: Vec<std::path::PathBuf>,
    /// CPU package temperature in Celsius, if available via hwmon.
    pub system_temp_c: Option<f32>,
}

/// Controls which HUD elements are rendered.
#[allow(dead_code)]
pub struct HudRenderer {
    pub show_scanlines: bool,
    pub show_reticle: bool,
    pub show_fps: bool,
}

#[allow(dead_code)]
impl HudRenderer {
    /// Create a `HudRenderer` with all elements enabled.
    pub fn new() -> Self {
        Self {
            show_scanlines: true,
            show_reticle: true,
            show_fps: true,
        }
    }

    /// Produce a list of `(text, x, y, color)` tuples for all active HUD
    /// elements. Callers pass each entry to their text renderer.
    ///
    /// Coordinate system: (0, 0) is top-left, positive y goes down.
    pub fn render_lines(
        &self,
        data: &HudData,
        width: f32,
        height: f32,
    ) -> Vec<(String, f32, f32, [f32; 4])> {
        let mut lines: Vec<(String, f32, f32, [f32; 4])> = Vec::new();

        // ── FPS counter (top-right) ───────────────────────────────────────────
        if self.show_fps {
            let fps_text = format!("{:.0} FPS", data.fps);
            // Right-align: approximate character width at ~8px.
            let x = width - fps_text.len() as f32 * 8.0 - 8.0;
            lines.push((fps_text, x, 8.0, ThermalPalette::WARM));
        }

        // ── Agent count (top-left) ────────────────────────────────────────────
        let agent_text = format!(
            "AGENTS: {}/{} ACTIVE",
            data.active_agents, data.agent_count
        );
        lines.push((agent_text, 8.0, 8.0, ThermalPalette::TEXT));

        // ── System temperature (top-left, second line) ────────────────────────
        if let Some(temp) = data.system_temp_c {
            let color = temp_color(temp);
            let temp_text = format!("CPU {:.1}°C", temp);
            lines.push((temp_text, 8.0, 28.0, color));
        }

        // ── Changed files (bottom-left) ───────────────────────────────────────
        if !data.changed_files.is_empty() {
            let count = data.changed_files.len();
            let file_text = format!("△ {} file{} changed", count, if count == 1 { "" } else { "s" });
            lines.push((file_text, 8.0, height - 28.0, ThermalPalette::HOT));

            // Show up to 3 file names above the count line.
            for (i, path) in data.changed_files.iter().take(3).enumerate() {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                let y = height - 28.0 - (i as f32 + 1.0) * 18.0;
                lines.push((format!("  {}", name), 8.0, y, ThermalPalette::TEXT_MUTED));
            }
        }

        lines
    }

    /// Returns true if pixel row `y` is on a scanline (every 3rd row).
    ///
    /// Callers use this to modulate pixel brightness for the scanline effect.
    pub fn is_scanline(y: u32) -> bool {
        y % 3 == 0
    }

    /// Returns four line segments `(x1, y1, x2, y2)` forming a crosshair
    /// reticle centred on the window with a gap in the middle.
    ///
    /// Segments are: top, right, bottom, left arms.
    pub fn reticle_lines(width: f32, height: f32) -> [(f32, f32, f32, f32); 4] {
        let cx = width / 2.0;
        let cy = height / 2.0;
        let arm = 20.0; // arm length in pixels
        let gap = 8.0;  // gap around centre

        [
            (cx, cy - gap - arm, cx, cy - gap),         // top arm
            (cx + gap, cy, cx + gap + arm, cy),          // right arm
            (cx, cy + gap, cx, cy + gap + arm),          // bottom arm
            (cx - gap - arm, cy, cx - gap, cy),          // left arm
        ]
    }
}

impl Default for HudRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Map CPU temperature to a thermal palette colour.
fn temp_color(temp_c: f32) -> [f32; 4] {
    if temp_c < 50.0 {
        ThermalPalette::COOL
    } else if temp_c < 70.0 {
        ThermalPalette::WARM
    } else if temp_c < 85.0 {
        ThermalPalette::HOT
    } else if temp_c < 95.0 {
        ThermalPalette::HOTTER
    } else {
        ThermalPalette::SEARING
    }
}
