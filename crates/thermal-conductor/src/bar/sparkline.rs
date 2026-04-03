/// Thermal-gradient sparkline rendering for the status bar.
///
/// Each sparkline is a rolling history of normalized (0.0-1.0) values rendered
/// as a bar chart. Bar color is mapped through the thermal gradient.
use std::collections::VecDeque;

use thermal_core::thermal_gradient_f32;

use super::metrics::SystemMetrics;

// ---------------------------------------------------------------------------
// SparkRect -- a colored rectangle for the renderer
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
pub struct Sparkline {
    history: VecDeque<f32>,
    capacity: usize,
}

impl Sparkline {
    pub fn new(capacity: usize) -> Self {
        Self {
            history: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, normalized_value: f32) {
        if self.history.len() == self.capacity {
            self.history.pop_front();
        }
        self.history.push_back(normalized_value.clamp(0.0, 1.0));
    }

    pub fn render_rects(&self, x: f32, y: f32, total_width: f32) -> Vec<SparkRect> {
        const BAR_HEIGHT: f32 = 20.0;

        if self.capacity == 0 {
            return Vec::new();
        }

        let bar_width = total_width / self.capacity as f32;
        let mut rects = Vec::with_capacity(self.history.len());

        for (i, &value) in self.history.iter().enumerate() {
            let bar_h = (value * BAR_HEIGHT).max(1.0);
            let bx = x + i as f32 * bar_width;
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

    #[allow(dead_code)]
    pub fn latest(&self) -> f32 {
        self.history.back().copied().unwrap_or(0.0)
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.history.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }
}

// ---------------------------------------------------------------------------
// SparklineSet
// ---------------------------------------------------------------------------

/// A set of sparklines for the standard bar metrics.
pub struct SparklineSet {
    pub cpu_usage: Sparkline,
    pub cpu_temp: Sparkline,
    pub mem_used: Sparkline,
}

impl SparklineSet {
    pub fn new() -> Self {
        Self {
            cpu_usage: Sparkline::new(30),
            cpu_temp: Sparkline::new(30),
            mem_used: Sparkline::new(30),
        }
    }

    pub fn push_metrics(&mut self, m: &SystemMetrics) {
        self.cpu_usage.push(m.cpu_usage_pct / 100.0);
        let temp_norm = m.cpu_temp_c.map(|t| (t - 20.0) / 80.0).unwrap_or(0.0);
        self.cpu_temp.push(temp_norm);
        let mem_norm = if m.mem_total_mb > 0 {
            m.mem_used_mb as f32 / m.mem_total_mb as f32
        } else {
            0.0
        };
        self.mem_used.push(mem_norm);
    }

    pub fn render_all(&self, start_x: f32, y: f32) -> Vec<SparkRect> {
        let spark_width: f32 = 60.0;
        let gap: f32 = 8.0;
        let mut all = Vec::new();

        all.extend(self.cpu_usage.render_rects(start_x, y, spark_width));
        all.extend(
            self.cpu_temp
                .render_rects(start_x + spark_width + gap, y, spark_width),
        );
        all.extend(
            self.mem_used
                .render_rects(start_x + (spark_width + gap) * 2.0, y, spark_width),
        );

        all
    }
}

impl Default for SparklineSet {
    fn default() -> Self {
        Self::new()
    }
}
