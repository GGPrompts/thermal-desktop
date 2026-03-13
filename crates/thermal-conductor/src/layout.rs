//! Layout engine for the thermal-conductor pane grid.
//!
//! Computes screen-space `Rect` viewports for each pane given a chosen layout
//! strategy and window dimensions.

use thermal_core::{Layout, ThermalPalette};

use crate::renderer::Rect;

/// Drives the pane layout calculation.
#[allow(dead_code)]
pub struct LayoutEngine {
    pub layout: Layout,
    pub window_width: f32,
    pub window_height: f32,
    pub pane_count: usize,
    pub focused_pane: usize,
}

#[allow(dead_code)]
impl LayoutEngine {
    /// Create a new LayoutEngine.
    pub fn new(layout: Layout, w: f32, h: f32) -> Self {
        Self {
            layout,
            window_width: w,
            window_height: h,
            pane_count: 0,
            focused_pane: 0,
        }
    }

    /// Compute one Rect per pane in display order.
    ///
    /// **Grid**: divides the window into `ceil(sqrt(N))` columns × rows with
    ///   2 px gaps between panes.
    /// **Sidebar**: focused pane takes the left 70 % of width; remaining panes
    ///   share the right 30 % stacked vertically.
    /// **Stack**: only the focused pane is visible; it fills the entire window.
    pub fn compute_rects(&self) -> Vec<Rect> {
        if self.pane_count == 0 {
            return Vec::new();
        }

        match self.layout {
            Layout::Grid => self.grid_rects(),
            Layout::Sidebar => self.sidebar_rects(),
            Layout::Stack => self.stack_rects(),
        }
    }

    /// Return the border colour for pane `i`.
    ///
    /// Focused pane → `ThermalPalette::SEARING` (red-hot).
    /// All others    → `ThermalPalette::COLD` (deep purple).
    pub fn border_color(&self, i: usize) -> [f32; 4] {
        if i == self.focused_pane {
            ThermalPalette::SEARING
        } else {
            ThermalPalette::COLD
        }
    }

    /// Switch focus. If the newly focused pane is not the current one, switch
    /// layout to Sidebar for a prominent view. Clicking the already-focused
    /// pane toggles back to Grid.
    pub fn set_focused(&mut self, pane_idx: usize) {
        if pane_idx == self.focused_pane && self.layout == Layout::Sidebar {
            // Toggle back to grid.
            self.layout = Layout::Grid;
        } else {
            self.focused_pane = pane_idx;
            self.layout = Layout::Sidebar;
        }
    }

    /// Return which pane index contains the point (x, y), if any.
    pub fn pane_at(&self, x: f32, y: f32) -> Option<usize> {
        self.compute_rects()
            .iter()
            .enumerate()
            .find_map(|(i, r)| if r.contains(x, y) { Some(i) } else { None })
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn grid_rects(&self) -> Vec<Rect> {
        let n = self.pane_count;
        let cols = (n as f32).sqrt().ceil() as usize;
        let rows = (n + cols - 1) / cols;

        let gap = 2.0_f32;
        let cell_w = (self.window_width - gap * (cols as f32 + 1.0)) / cols as f32;
        let cell_h = (self.window_height - gap * (rows as f32 + 1.0)) / rows as f32;

        (0..n)
            .map(|i| {
                let col = i % cols;
                let row = i / cols;
                let x = gap + col as f32 * (cell_w + gap);
                let y = gap + row as f32 * (cell_h + gap);
                Rect::new(x, y, cell_w, cell_h)
            })
            .collect()
    }

    fn sidebar_rects(&self) -> Vec<Rect> {
        let n = self.pane_count;
        let main_w = self.window_width * 0.70;
        let side_w = self.window_width - main_w;
        let side_count = if n == 0 { 0 } else { n - 1 };

        let mut rects = Vec::with_capacity(n);

        // Allocate slots: pane 0..focused_pane and focused_pane+1..n go into
        // sidebar slots; focused pane goes into the main area.
        let sidebar_indices: Vec<usize> = (0..n).filter(|&i| i != self.focused_pane).collect();

        let slot_h = if side_count > 0 {
            self.window_height / side_count as f32
        } else {
            self.window_height
        };

        // Build rects in pane order.
        let mut sidebar_slot = 0usize;
        for i in 0..n {
            if i == self.focused_pane {
                rects.push(Rect::new(0.0, 0.0, main_w, self.window_height));
            } else {
                let slot = sidebar_indices.iter().position(|&idx| idx == i).unwrap_or(sidebar_slot);
                sidebar_slot += 1;
                let y = slot as f32 * slot_h;
                rects.push(Rect::new(main_w, y, side_w, slot_h));
            }
        }

        rects
    }

    fn stack_rects(&self) -> Vec<Rect> {
        // Only the focused pane is visible; all others get zero-size rects.
        (0..self.pane_count)
            .map(|i| {
                if i == self.focused_pane {
                    Rect::new(0.0, 0.0, self.window_width, self.window_height)
                } else {
                    Rect::new(0.0, 0.0, 0.0, 0.0)
                }
            })
            .collect()
    }
}
