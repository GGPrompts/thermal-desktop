/// Thermal-gradient sparkline rendering for thermal-bar.
///
/// Each sparkline is a rolling history of normalized (0.0–1.0) values rendered
/// as a bar chart. Bar color is mapped through the thermal gradient.
use std::collections::VecDeque;

use thermal_core::thermal_gradient_f32;

// ---------------------------------------------------------------------------
// SparkRect — a colored rectangle for the renderer
// ---------------------------------------------------------------------------

/// A single bar in a sparkline, described in pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct SparkRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [f32; 4],
}

// ---------------------------------------------------------------------------
// Sparkline
// ---------------------------------------------------------------------------

/// A rolling bar-chart sparkline.
///
/// Values are normalized to `[0.0, 1.0]` before being pushed. The bar chart
/// is 20px tall; bar width is `total_width / capacity`.
pub struct Sparkline {
    history: VecDeque<f32>,
    capacity: usize,
}

impl Sparkline {
    /// Create a new sparkline with the given capacity.
    ///
    /// `capacity = 30` gives ~30 seconds of history at 1 Hz polling.
    pub fn new(capacity: usize) -> Self {
        Self {
            history: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Push a normalized value (clamped to `[0.0, 1.0]`).
    pub fn push(&mut self, normalized_value: f32) {
        if self.history.len() == self.capacity {
            self.history.pop_front();
        }
        self.history.push_back(normalized_value.clamp(0.0, 1.0));
    }

    /// Render as a bar-chart into a list of `SparkRect`s.
    ///
    /// - `x`, `y` — top-left origin in pixel coordinates.
    /// - `total_width` — the full pixel width of the sparkline widget.
    /// - Bar height is 20px; each bar is `total_width / capacity` wide.
    /// - Bar color is determined by `thermal_gradient(value)`.
    pub fn render_rects(&self, x: f32, y: f32, total_width: f32) -> Vec<SparkRect> {
        const BAR_HEIGHT: f32 = 20.0;

        if self.capacity == 0 {
            return Vec::new();
        }

        let bar_width = total_width / self.capacity as f32;
        let mut rects = Vec::with_capacity(self.history.len());

        for (i, &value) in self.history.iter().enumerate() {
            let bar_h = (value * BAR_HEIGHT).max(1.0); // at least 1px
            let bx = x + i as f32 * bar_width;
            // Bottom-aligned within the 20px zone.
            let by = y + (BAR_HEIGHT - bar_h);
            let color = thermal_gradient_f32(value);

            rects.push(SparkRect {
                x: bx,
                y: by,
                w: bar_width.max(1.0),
                h: bar_h,
                color,
            });
        }

        rects
    }

    /// Return the most recently pushed value, or 0.0 if empty.
    pub fn latest(&self) -> f32 {
        self.history.back().copied().unwrap_or(0.0)
    }

    /// Number of values currently stored.
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// True if no values have been pushed yet.
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }
}

// ---------------------------------------------------------------------------
// SparklineSet — manages sparklines for the main metrics
// ---------------------------------------------------------------------------

/// A set of sparklines for the standard bar metrics.
pub struct SparklineSet {
    pub cpu_usage: Sparkline,
    pub cpu_temp: Sparkline,
    pub mem_used: Sparkline,
}

impl SparklineSet {
    /// Create a new set with 30-sample capacity.
    pub fn new() -> Self {
        Self {
            cpu_usage: Sparkline::new(30),
            cpu_temp: Sparkline::new(30),
            mem_used: Sparkline::new(30),
        }
    }

    /// Push a new sample from `SystemMetrics`.
    pub fn push_metrics(&mut self, m: &crate::metrics::SystemMetrics) {
        self.cpu_usage.push(m.cpu_usage_pct / 100.0);

        // Normalize CPU temp: 20°C = 0.0, 100°C = 1.0.
        let temp_norm = m.cpu_temp_c.map(|t| (t - 20.0) / 80.0).unwrap_or(0.0);
        self.cpu_temp.push(temp_norm);

        // Normalize memory usage.
        let mem_norm = if m.mem_total_mb > 0 {
            m.mem_used_mb as f32 / m.mem_total_mb as f32
        } else {
            0.0
        };
        self.mem_used.push(mem_norm);
    }

    /// Render all sparklines at fixed positions in the left zone (after text labels).
    ///
    /// Each sparkline is 60px wide. Starting at `start_x`.
    pub fn render_all(&self, start_x: f32, y: f32) -> Vec<SparkRect> {
        let spark_width: f32 = 60.0;
        let gap: f32 = 8.0;
        let mut all = Vec::new();

        all.extend(self.cpu_usage.render_rects(start_x, y, spark_width));
        all.extend(self.cpu_temp.render_rects(start_x + spark_width + gap, y, spark_width));
        all.extend(self.mem_used.render_rects(start_x + (spark_width + gap) * 2.0, y, spark_width));

        all
    }
}

impl Default for SparklineSet {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::SystemMetrics;

    // -----------------------------------------------------------------------
    // Sparkline basic operations
    // -----------------------------------------------------------------------

    #[test]
    fn sparkline_new_is_empty() {
        let s = Sparkline::new(30);
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.latest(), 0.0);
    }

    #[test]
    fn sparkline_push_increments_len() {
        let mut s = Sparkline::new(10);
        s.push(0.5);
        assert_eq!(s.len(), 1);
        s.push(0.3);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn sparkline_latest_returns_most_recent_value() {
        let mut s = Sparkline::new(10);
        s.push(0.2);
        s.push(0.8);
        assert!((s.latest() - 0.8).abs() < 1e-6);
    }

    #[test]
    fn sparkline_push_clamps_above_one() {
        let mut s = Sparkline::new(5);
        s.push(1.5);
        assert!((s.latest() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn sparkline_push_clamps_below_zero() {
        let mut s = Sparkline::new(5);
        s.push(-0.5);
        assert!((s.latest() - 0.0).abs() < 1e-6);
    }

    #[test]
    fn sparkline_evicts_oldest_when_full() {
        let mut s = Sparkline::new(3);
        s.push(0.1);
        s.push(0.2);
        s.push(0.3);
        assert_eq!(s.len(), 3);
        // Push one more — oldest (0.1) should be dropped.
        s.push(0.4);
        assert_eq!(s.len(), 3);
        assert!((s.latest() - 0.4).abs() < 1e-6);
    }

    #[test]
    fn sparkline_is_empty_true_before_push() {
        let s = Sparkline::new(30);
        assert!(s.is_empty());
    }

    #[test]
    fn sparkline_is_empty_false_after_push() {
        let mut s = Sparkline::new(30);
        s.push(0.5);
        assert!(!s.is_empty());
    }

    #[test]
    fn sparkline_capacity_one_keeps_last_value() {
        let mut s = Sparkline::new(1);
        s.push(0.3);
        s.push(0.7);
        assert_eq!(s.len(), 1);
        assert!((s.latest() - 0.7).abs() < 1e-6);
    }

    // -----------------------------------------------------------------------
    // Sparkline::render_rects
    // -----------------------------------------------------------------------

    #[test]
    fn render_rects_empty_sparkline_returns_empty() {
        let s = Sparkline::new(30);
        assert!(s.render_rects(0.0, 0.0, 60.0).is_empty());
    }

    #[test]
    fn render_rects_zero_capacity_returns_empty() {
        let s = Sparkline::new(0);
        assert!(s.render_rects(0.0, 0.0, 60.0).is_empty());
    }

    #[test]
    fn render_rects_count_matches_history_len() {
        let mut s = Sparkline::new(10);
        s.push(0.1);
        s.push(0.5);
        s.push(0.9);
        let rects = s.render_rects(0.0, 0.0, 60.0);
        assert_eq!(rects.len(), 3);
    }

    #[test]
    fn render_rects_x_positions_advance_left_to_right() {
        let mut s = Sparkline::new(5);
        for v in [0.1, 0.3, 0.5, 0.7, 0.9] {
            s.push(v);
        }
        let rects = s.render_rects(0.0, 0.0, 50.0);
        for i in 1..rects.len() {
            assert!(rects[i].x > rects[i - 1].x,
                "rect[{}].x={} should be > rect[{}].x={}", i, rects[i].x, i-1, rects[i-1].x);
        }
    }

    #[test]
    fn render_rects_bar_height_at_max_value() {
        let mut s = Sparkline::new(5);
        s.push(1.0); // max value → bar_h = 20.0
        let rects = s.render_rects(0.0, 0.0, 50.0);
        assert!((rects[0].h - 20.0).abs() < 1e-3, "h={}", rects[0].h);
    }

    #[test]
    fn render_rects_bar_height_minimum_one_px() {
        let mut s = Sparkline::new(5);
        s.push(0.0); // zero value → bar_h clamped to 1.0
        let rects = s.render_rects(0.0, 0.0, 50.0);
        assert!((rects[0].h - 1.0).abs() < 1e-3, "h={}", rects[0].h);
    }

    #[test]
    fn render_rects_origin_offset_applied() {
        let mut s = Sparkline::new(5);
        s.push(1.0);
        let rects_origin = s.render_rects(0.0, 0.0, 50.0);
        let rects_offset = s.render_rects(100.0, 200.0, 50.0);
        assert!((rects_offset[0].x - rects_origin[0].x - 100.0).abs() < 1e-3);
        // y for full-height bar (h=20) at y=0: by = 0 + (20 - 20) = 0
        // y for full-height bar at y=200: by = 200 + (20 - 20) = 200
        assert!((rects_offset[0].y - rects_origin[0].y - 200.0).abs() < 1e-3);
    }

    #[test]
    fn render_rects_bar_width_minimum_one_px() {
        // With total_width < capacity the computed bar_width could be < 1px;
        // the max(1.0) guard should prevent zero-width rects.
        let mut s = Sparkline::new(100);
        s.push(0.5);
        let rects = s.render_rects(0.0, 0.0, 10.0); // 10px wide for 100-slot sparkline
        assert!(rects[0].w >= 1.0, "w={}", rects[0].w);
    }

    #[test]
    fn render_rects_colors_are_valid_rgba() {
        let mut s = Sparkline::new(5);
        for v in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            s.push(v);
        }
        let rects = s.render_rects(0.0, 0.0, 50.0);
        for r in &rects {
            for &ch in &r.color {
                assert!(ch >= 0.0 && ch <= 1.0, "color channel out of range: {ch}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // SparklineSet
    // -----------------------------------------------------------------------

    #[test]
    fn sparkline_set_new_has_empty_sparklines() {
        let ss = SparklineSet::new();
        assert!(ss.cpu_usage.is_empty());
        assert!(ss.cpu_temp.is_empty());
        assert!(ss.mem_used.is_empty());
    }

    #[test]
    fn sparkline_set_default_equivalent_to_new() {
        let ss_new = SparklineSet::new();
        let ss_def = SparklineSet::default();
        assert_eq!(ss_new.cpu_usage.len(), ss_def.cpu_usage.len());
    }

    #[test]
    fn push_metrics_increments_all_sparklines() {
        let mut ss = SparklineSet::new();
        let m = SystemMetrics {
            cpu_usage_pct: 60.0,
            cpu_temp_c: Some(50.0),
            mem_used_mb: 8000,
            mem_total_mb: 16000,
            net_rx_kbps: 0.0,
            net_tx_kbps: 0.0,
            gpu_temp_c: None,
            gpu_usage_pct: None,
        };
        ss.push_metrics(&m);
        assert_eq!(ss.cpu_usage.len(), 1);
        assert_eq!(ss.cpu_temp.len(), 1);
        assert_eq!(ss.mem_used.len(), 1);
    }

    #[test]
    fn push_metrics_normalizes_cpu_usage() {
        let mut ss = SparklineSet::new();
        let m = SystemMetrics {
            cpu_usage_pct: 100.0,
            cpu_temp_c: None,
            mem_used_mb: 0,
            mem_total_mb: 1,
            net_rx_kbps: 0.0,
            net_tx_kbps: 0.0,
            gpu_temp_c: None,
            gpu_usage_pct: None,
        };
        ss.push_metrics(&m);
        // 100% CPU → normalized value = 1.0
        assert!((ss.cpu_usage.latest() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn push_metrics_normalizes_cpu_temp_range() {
        let mut ss = SparklineSet::new();
        // 20°C = 0.0 normalized, 100°C = 1.0 normalized
        let m_cold = SystemMetrics {
            cpu_usage_pct: 0.0,
            cpu_temp_c: Some(20.0),
            mem_used_mb: 0,
            mem_total_mb: 1,
            net_rx_kbps: 0.0,
            net_tx_kbps: 0.0,
            gpu_temp_c: None,
            gpu_usage_pct: None,
        };
        ss.push_metrics(&m_cold);
        assert!((ss.cpu_temp.latest() - 0.0).abs() < 1e-6, "at 20°C");

        let m_hot = SystemMetrics {
            cpu_usage_pct: 0.0,
            cpu_temp_c: Some(100.0),
            mem_used_mb: 0,
            mem_total_mb: 1,
            net_rx_kbps: 0.0,
            net_tx_kbps: 0.0,
            gpu_temp_c: None,
            gpu_usage_pct: None,
        };
        ss.push_metrics(&m_hot);
        assert!((ss.cpu_temp.latest() - 1.0).abs() < 1e-6, "at 100°C");
    }

    #[test]
    fn push_metrics_no_temp_pushes_zero_norm() {
        let mut ss = SparklineSet::new();
        let m = SystemMetrics {
            cpu_usage_pct: 0.0,
            cpu_temp_c: None,
            mem_used_mb: 0,
            mem_total_mb: 1,
            net_rx_kbps: 0.0,
            net_tx_kbps: 0.0,
            gpu_temp_c: None,
            gpu_usage_pct: None,
        };
        ss.push_metrics(&m);
        assert_eq!(ss.cpu_temp.latest(), 0.0);
    }

    #[test]
    fn push_metrics_normalizes_memory_usage() {
        let mut ss = SparklineSet::new();
        let m = SystemMetrics {
            cpu_usage_pct: 0.0,
            cpu_temp_c: None,
            mem_used_mb: 8000,
            mem_total_mb: 16000,
            net_rx_kbps: 0.0,
            net_tx_kbps: 0.0,
            gpu_temp_c: None,
            gpu_usage_pct: None,
        };
        ss.push_metrics(&m);
        // 8000 / 16000 = 0.5
        assert!((ss.mem_used.latest() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn push_metrics_mem_zero_total_pushes_zero() {
        let mut ss = SparklineSet::new();
        let m = SystemMetrics {
            cpu_usage_pct: 0.0,
            cpu_temp_c: None,
            mem_used_mb: 1000,
            mem_total_mb: 0,  // avoid divide-by-zero
            net_rx_kbps: 0.0,
            net_tx_kbps: 0.0,
            gpu_temp_c: None,
            gpu_usage_pct: None,
        };
        ss.push_metrics(&m);
        assert_eq!(ss.mem_used.latest(), 0.0);
    }

    #[test]
    fn render_all_returns_rects_when_data_present() {
        let mut ss = SparklineSet::new();
        let m = SystemMetrics {
            cpu_usage_pct: 50.0,
            cpu_temp_c: Some(60.0),
            mem_used_mb: 4000,
            mem_total_mb: 8000,
            net_rx_kbps: 0.0,
            net_tx_kbps: 0.0,
            gpu_temp_c: None,
            gpu_usage_pct: None,
        };
        ss.push_metrics(&m);
        let rects = ss.render_all(0.0, 6.0);
        // One sample in each of the three sparklines → 3 rects total.
        assert_eq!(rects.len(), 3);
    }

    #[test]
    fn render_all_empty_returns_empty() {
        let ss = SparklineSet::new();
        let rects = ss.render_all(0.0, 6.0);
        assert!(rects.is_empty());
    }
}
