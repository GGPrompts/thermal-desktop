/// System metrics bar modules.
///
/// Reads CPU, memory, network, and GPU data from `SystemMetrics` and
/// returns styled `ModuleOutput` items for the left zone.
use thermal_core::{thermal_gradient_f32, ThermalPalette};

use crate::layout::{ModuleOutput, Zone};
use crate::metrics::SystemMetrics;

pub struct MetricsModule;

impl MetricsModule {
    pub fn new() -> Self {
        Self
    }

    /// Poll metrics and produce left-zone module outputs.
    pub fn render(&self) -> Vec<ModuleOutput> {
        let m = SystemMetrics::poll_full();
        let mut modules = Vec::new();

        // CPU usage.
        let cpu_heat = (m.cpu_usage_pct / 100.0).clamp(0.0, 1.0);
        let cpu_color = thermal_gradient_f32(cpu_heat);
        modules.push(ModuleOutput::new(
            Zone::Left,
            format!("CPU {:>3.0}%", m.cpu_usage_pct),
            cpu_color,
        ));

        // CPU temp (if available).
        if let Some(temp) = m.cpu_temp_c {
            // Normalize: 0°C = 0.0, 100°C = 1.0.
            let heat = ((temp - 20.0) / 80.0).clamp(0.0, 1.0);
            let color = thermal_gradient_f32(heat);
            modules.push(ModuleOutput::new(
                Zone::Left,
                format!("{:>3.0}°C", temp),
                color,
            ));
        }

        // Memory.
        let mem_pct = if m.mem_total_mb > 0 {
            m.mem_used_mb as f32 / m.mem_total_mb as f32
        } else {
            0.0
        };
        let mem_heat = mem_pct.clamp(0.0, 1.0);
        let mem_color = thermal_gradient_f32(mem_heat);
        modules.push(ModuleOutput::new(
            Zone::Left,
            format!("MEM {:>4}MB", m.mem_used_mb),
            mem_color,
        ));

        // Network.
        if m.net_rx_kbps > 0.1 || m.net_tx_kbps > 0.1 {
            let net_str = format!(
                "↓{:.0}K ↑{:.0}K",
                m.net_rx_kbps, m.net_tx_kbps
            );
            modules.push(ModuleOutput::new(
                Zone::Left,
                net_str,
                ThermalPalette::ACCENT_NEUTRAL,
            ));
        }

        // GPU temp + usage (if available).
        if let (Some(gpu_temp), Some(gpu_pct)) = (m.gpu_temp_c, m.gpu_usage_pct) {
            let gpu_heat = ((gpu_temp - 20.0) / 80.0).clamp(0.0, 1.0);
            let gpu_color = thermal_gradient_f32(gpu_heat);
            modules.push(ModuleOutput::new(
                Zone::Left,
                format!("GPU {:>3.0}% {:>3.0}°C", gpu_pct, gpu_temp),
                gpu_color,
            ));
        }

        modules
    }
}

impl Default for MetricsModule {
    fn default() -> Self {
        Self::new()
    }
}
