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

    /// Compute pixel X positions for all modules and return a flat list.
    ///
    /// - Left zone: starts at x=8, modules separated by 16px padding.
    /// - Center zone: centered around bar_width/2.
    /// - Right zone: right-aligned ending at bar_width-8.
    ///
    /// Returns positioned `ModuleOutput` items in left→center→right order.
    pub fn compute_positions(&self) -> Vec<ModuleOutput> {
        let char_width: f32 = 8.0; // approximate monospace char width at 13px
        let padding: f32 = 16.0;
        let margin: f32 = 8.0;

        // Estimate text pixel width.
        let text_px = |m: &ModuleOutput| -> f32 {
            m.text.len() as f32 * char_width
        };

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

        // Separator between left and center (stored as a 1px-wide module).
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
        let right_start = right_modules.first().map(|m| m.x).unwrap_or(self.bar_width as f32 - margin);

        // --- Center zone ---
        let total_center_w: f32 = self.center.iter().map(|m| text_px(m) + padding).sum();
        let center_start = (self.bar_width as f32 / 2.0) - (total_center_w / 2.0);
        let center_start = center_start.max(left_end + padding).min(right_start - total_center_w - padding);
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
