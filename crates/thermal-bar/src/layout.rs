/// Left/center/right module layout system for thermal-bar.
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
    /// Returns positioned `ModuleOutput` items in left→center→right order.
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

    // -----------------------------------------------------------------------
    // Zone
    // -----------------------------------------------------------------------

    #[test]
    fn zone_variants_are_distinct() {
        assert_ne!(Zone::Left, Zone::Center);
        assert_ne!(Zone::Center, Zone::Right);
        assert_ne!(Zone::Left, Zone::Right);
    }

    #[test]
    fn zone_copy_clone_works() {
        let z = Zone::Left;
        let z2 = z; // Copy
        let z3 = z; // Clone (Copy type)
        assert_eq!(z2, z3);
    }

    // -----------------------------------------------------------------------
    // ModuleOutput construction
    // -----------------------------------------------------------------------

    #[test]
    fn module_output_new_sets_fields() {
        let color = [1.0_f32, 0.0, 0.0, 1.0];
        let m = ModuleOutput::new(Zone::Left, "CPU  42%", color);
        assert_eq!(m.zone, Zone::Left);
        assert_eq!(m.text, "CPU  42%");
        assert_eq!(m.color, color);
        assert!(m.bg_color.is_none());
        assert_eq!(m.x, 0.0);
        assert_eq!(m.width, 0.0);
    }

    #[test]
    fn module_output_new_accepts_string_owned() {
        let text = String::from("hello");
        let m = ModuleOutput::new(Zone::Right, text, [0.0; 4]);
        assert_eq!(m.text, "hello");
    }

    #[test]
    fn module_output_with_bg_sets_bg_color() {
        let bg = [0.1_f32, 0.2, 0.3, 1.0];
        let m = ModuleOutput::new(Zone::Center, "test", [1.0; 4]).with_bg(bg);
        assert_eq!(m.bg_color, Some(bg));
    }

    #[test]
    fn module_output_without_bg_has_none() {
        let m = ModuleOutput::new(Zone::Center, "test", [1.0; 4]);
        assert!(m.bg_color.is_none());
    }

    // -----------------------------------------------------------------------
    // BarLayout construction
    // -----------------------------------------------------------------------

    #[test]
    fn bar_layout_new_has_correct_dimensions() {
        let layout = BarLayout::new(1920);
        assert_eq!(layout.bar_width, 1920);
        assert_eq!(layout.bar_height, 32);
        assert!(layout.left.is_empty());
        assert!(layout.center.is_empty());
        assert!(layout.right.is_empty());
    }

    // -----------------------------------------------------------------------
    // compute_positions — left zone
    // -----------------------------------------------------------------------

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
        // The first left module should be at x = 8.0 (the margin).
        let left_mod = positioned.iter().find(|m| m.zone == Zone::Left).unwrap();
        assert!((left_mod.x - 8.0).abs() < 1e-3, "x={}", left_mod.x);
    }

    #[test]
    fn compute_positions_left_modules_advance_x() {
        let mut layout = BarLayout::new(1920);
        layout.left.push(dummy_left("A")); // 1 char
        layout.left.push(dummy_left("BB")); // 2 chars
        let positioned = layout.compute_positions();
        let lefts: Vec<_> = positioned.iter().filter(|m| m.zone == Zone::Left).collect();
        assert_eq!(lefts.len(), 2);
        // Second module x must be greater than first.
        assert!(lefts[1].x > lefts[0].x, "modules must not overlap");
    }

    #[test]
    fn compute_positions_left_module_width_is_positive() {
        let mut layout = BarLayout::new(1920);
        layout.left.push(dummy_left("CPU  42%"));
        let positioned = layout.compute_positions();
        let m = positioned.iter().find(|m| m.zone == Zone::Left).unwrap();
        assert!(m.width > 0.0);
    }

    #[test]
    fn compute_positions_width_grows_with_text_length() {
        let mut layout1 = BarLayout::new(1920);
        layout1.left.push(dummy_left("A"));
        let mut layout2 = BarLayout::new(1920);
        layout2.left.push(dummy_left("ABCDEFGH"));

        let w1 = layout1.compute_positions()[0].width;
        let w2 = layout2.compute_positions()[0].width;
        assert!(
            w2 > w1,
            "longer text should produce wider module: w1={w1} w2={w2}"
        );
    }

    // -----------------------------------------------------------------------
    // compute_positions — right zone
    // -----------------------------------------------------------------------

    #[test]
    fn compute_positions_right_module_x_is_near_bar_end() {
        let bar_width = 1920u32;
        let mut layout = BarLayout::new(bar_width);
        layout.right.push(dummy_right("12:34:56"));
        let positioned = layout.compute_positions();
        let right_mod = positioned
            .iter()
            .find(|m| m.zone == Zone::Right && !m.text.is_empty())
            .unwrap();
        // The right module should start before bar_width and well into the right half.
        assert!(right_mod.x < bar_width as f32);
        assert!(
            right_mod.x > bar_width as f32 / 2.0,
            "right module x={} should be in the right half",
            right_mod.x
        );
    }

    #[test]
    fn compute_positions_right_modules_end_at_margin() {
        let bar_width = 1920u32;
        let mut layout = BarLayout::new(bar_width);
        layout.right.push(dummy_right("12:34:56"));
        let positioned = layout.compute_positions();
        // The rightmost module's x + width should equal bar_width - margin (8.0).
        let right_mods: Vec<_> = positioned
            .iter()
            .filter(|m| m.zone == Zone::Right && !m.text.is_empty())
            .collect();
        let last = right_mods.last().unwrap();
        let right_edge = last.x + last.width;
        assert!(
            (right_edge - (bar_width as f32 - 8.0)).abs() < 1e-3,
            "right edge={right_edge} expected={}",
            bar_width as f32 - 8.0
        );
    }

    #[test]
    fn compute_positions_right_separator_is_inserted() {
        let mut layout = BarLayout::new(1920);
        layout.right.push(dummy_right("CLU 1 tool"));
        let positioned = layout.compute_positions();
        // The separator is a zero-text 1px-wide module at Zone::Right.
        let sep = positioned
            .iter()
            .find(|m| m.zone == Zone::Right && m.text.is_empty());
        assert!(sep.is_some(), "separator module should be present");
        assert!((sep.unwrap().width - 1.0).abs() < 1e-3);
    }

    // -----------------------------------------------------------------------
    // compute_positions — center zone
    // -----------------------------------------------------------------------

    #[test]
    fn compute_positions_center_module_is_near_middle() {
        let bar_width = 1920u32;
        let mut layout = BarLayout::new(bar_width);
        layout.center.push(dummy_center("CLOCK"));
        let positioned = layout.compute_positions();
        let center_mod = positioned.iter().find(|m| m.zone == Zone::Center).unwrap();
        // Should be within the middle 50% of the bar.
        let mid = bar_width as f32 / 2.0;
        assert!(
            center_mod.x > mid * 0.25 && center_mod.x < mid * 1.75,
            "center x={} not near middle",
            center_mod.x
        );
    }

    // -----------------------------------------------------------------------
    // all_positioned delegates to compute_positions
    // -----------------------------------------------------------------------

    #[test]
    fn all_positioned_matches_compute_positions() {
        let mut layout = BarLayout::new(1920);
        layout.left.push(dummy_left("CPU 50%"));
        layout.right.push(dummy_right("12:00:00"));
        let a = layout.all_positioned();
        let b = layout.compute_positions();
        assert_eq!(a.len(), b.len());
        for (ma, mb) in a.iter().zip(b.iter()) {
            assert_eq!(ma.text, mb.text);
            assert!((ma.x - mb.x).abs() < 1e-6);
        }
    }

    // -----------------------------------------------------------------------
    // Unicode char-counting (width estimation uses .chars().count())
    // -----------------------------------------------------------------------

    #[test]
    fn compute_positions_unicode_text_does_not_panic() {
        // Hotkeys use Unicode codepoints like ▸ ⌘ ⏎ etc.
        let mut layout = BarLayout::new(1920);
        layout.center.push(dummy_center("▸ ⌘⏎ term"));
        layout.center.push(dummy_center("✕ ⌘Q close"));
        let positioned = layout.compute_positions();
        assert_eq!(
            positioned.iter().filter(|m| m.zone == Zone::Center).count(),
            2
        );
    }
}
