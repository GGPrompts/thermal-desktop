/// Left/center/right module layout system for the status bar.
use thermal_core::ThermalPalette;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The zone a module belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Left,
    Center,
    Right,
}

/// The rendered output of a single bar module.
#[derive(Debug, Clone)]
pub struct ModuleOutput {
    pub zone: Zone,
    pub text: String,
    /// Foreground color (RGBA f32).
    pub color: [f32; 4],
    /// Optional background color (RGBA f32). None = transparent.
    pub bg_color: Option<[f32; 4]>,
    /// Pixel X position (set by compute_positions).
    pub x: f32,
    /// Allocated pixel width for this module (set by compute_positions).
    pub width: f32,
}

impl ModuleOutput {
    /// Create a new ModuleOutput with x=0, width=0 (positions computed later).
    pub fn new(zone: Zone, text: impl Into<String>, color: [f32; 4]) -> Self {
        Self {
            zone,
            text: text.into(),
            color,
            bg_color: None,
            x: 0.0,
            width: 0.0,
        }
    }

    pub fn with_bg(mut self, bg: [f32; 4]) -> Self {
        self.bg_color = Some(bg);
        self
    }
}

// ---------------------------------------------------------------------------
// BarLayout
// ---------------------------------------------------------------------------

/// The complete bar layout with positioned modules.
pub struct BarLayout {
    pub left: Vec<ModuleOutput>,
    pub center: Vec<ModuleOutput>,
    pub right: Vec<ModuleOutput>,
    pub bar_width: u32,
    pub bar_height: u32,
}

impl BarLayout {
    /// Create a new empty layout with the given bar width (height is always 32).
    pub fn new(bar_width: u32) -> Self {
        Self {
            left: Vec::new(),
            center: Vec::new(),
            right: Vec::new(),
            bar_width,
            bar_height: 32,
        }
    }

    /// Return the X pixel position where left-zone text ends.
    /// Used to position sparklines after the text labels (not on top of them).
    pub fn left_zone_end(&self) -> f32 {
        let char_width: f32 = 10.0;
        let padding: f32 = 16.0;
        let margin: f32 = 8.0;
        let mut x = margin;
        for module in &self.left {
            let w = module.text.chars().count() as f32 * char_width + padding;
            x += w;
        }
        x
    }

    /// Compute pixel X positions for all modules and return a flat list.
    ///
    /// - Left zone: starts at x=8, modules separated by 16px padding.
    /// - Center zone: centered around bar_width/2.
    /// - Right zone: right-aligned ending at bar_width-8.
    ///
    /// Returns positioned `ModuleOutput` items in left->center->right order.
    pub fn compute_positions(&self) -> Vec<ModuleOutput> {
        let char_width: f32 = 10.0; // approximate monospace char width at 16px
        let padding: f32 = 16.0;
        let margin: f32 = 8.0;

        // Estimate text pixel width (char count, not byte length, for unicode).
        let text_px = |m: &ModuleOutput| -> f32 { m.text.chars().count() as f32 * char_width };

        let mut result = Vec::new();

        // --- Left zone ---
        let mut x = margin;
        for module in &self.left {
            let w = text_px(module) + padding;
            let mut m = module.clone();
            m.x = x;
            m.width = w;
            x += w;
            result.push(m);
        }

        // Track where left-zone text ends (used for sparkline positioning).
        let left_end = x;

        // --- Right zone (compute right-to-left) ---
        let mut right_modules: Vec<ModuleOutput> = Vec::new();
        let mut rx = self.bar_width as f32 - margin;
        for module in self.right.iter().rev() {
            let w = text_px(module) + padding;
            rx -= w;
            let mut m = module.clone();
            m.x = rx;
            m.width = w;
            right_modules.push(m);
        }
        right_modules.reverse();
        let right_start = right_modules
            .first()
            .map(|m| m.x)
            .unwrap_or(self.bar_width as f32 - margin);

        // --- Center zone ---
        let total_center_w: f32 = self.center.iter().map(|m| text_px(m) + padding).sum();
        let center_start = (self.bar_width as f32 / 2.0) - (total_center_w / 2.0);
        let center_start = center_start
            .max(left_end + padding)
            .min(right_start - total_center_w - padding);
        let mut cx = center_start;
        for module in &self.center {
            let w = text_px(module) + padding;
            let mut m = module.clone();
            m.x = cx;
            m.width = w;
            cx += w;
            result.push(m);
        }

        // Separator before right zone.
        if !right_modules.is_empty() {
            let sep_x = right_modules[0].x - 1.0;
            result.push(ModuleOutput {
                zone: Zone::Right,
                text: String::new(),
                color: ThermalPalette::COLD,
                bg_color: Some(ThermalPalette::COLD),
                x: sep_x,
                width: 1.0,
            });
        }

        result.extend(right_modules);

        result
    }

    /// Flatten all modules and compute their positions.
    pub fn all_positioned(&self) -> Vec<ModuleOutput> {
        self.compute_positions()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_left(text: &str) -> ModuleOutput {
        ModuleOutput::new(Zone::Left, text, [1.0; 4])
    }

    fn dummy_center(text: &str) -> ModuleOutput {
        ModuleOutput::new(Zone::Center, text, [1.0; 4])
    }

    fn dummy_right(text: &str) -> ModuleOutput {
        ModuleOutput::new(Zone::Right, text, [1.0; 4])
    }

    #[test]
    fn compute_positions_empty_layout_returns_empty() {
        let layout = BarLayout::new(1920);
        assert!(layout.compute_positions().is_empty());
    }

    #[test]
    fn compute_positions_single_left_module_starts_at_margin() {
        let mut layout = BarLayout::new(1920);
        layout.left.push(dummy_left("ABC"));
        let positioned = layout.compute_positions();
        let left_mod = positioned.iter().find(|m| m.zone == Zone::Left).unwrap();
        assert!((left_mod.x - 8.0).abs() < 1e-3, "x={}", left_mod.x);
    }

    #[test]
    fn compute_positions_right_separator_is_inserted() {
        let mut layout = BarLayout::new(1920);
        layout.right.push(dummy_right("CLU 1 tool"));
        let positioned = layout.compute_positions();
        let sep = positioned
            .iter()
            .find(|m| m.zone == Zone::Right && m.text.is_empty());
        assert!(sep.is_some(), "separator module should be present");
        assert!((sep.unwrap().width - 1.0).abs() < 1e-3);
    }

    #[test]
    fn compute_positions_center_module_is_near_middle() {
        let bar_width = 1920u32;
        let mut layout = BarLayout::new(bar_width);
        layout.center.push(dummy_center("CLOCK"));
        let positioned = layout.compute_positions();
        let center_mod = positioned.iter().find(|m| m.zone == Zone::Center).unwrap();
        let mid = bar_width as f32 / 2.0;
        assert!(
            center_mod.x > mid * 0.25 && center_mod.x < mid * 1.75,
            "center x={} not near middle",
            center_mod.x
        );
    }
}
