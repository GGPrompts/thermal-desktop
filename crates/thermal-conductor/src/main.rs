//! Thermal Conductor — Native GPU-rendered agent dashboard.
//!
//! A wall of terminal panes, each running a Claude agent session.
//! Thermal state indicators, PipeWire audio cues, git diff awareness.

use thermal_core::ThermalPalette;

fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!(
        "◉ THERMAL CONDUCTOR — Initializing..."
    );

    // TODO: Initialize Wayland connection
    // TODO: Create wgpu device and surface
    // TODO: Set up PTY manager
    // TODO: Set up D-Bus service
    // TODO: Enter event loop

    println!("thermal-conductor v{}", env!("CARGO_PKG_VERSION"));
    println!("Palette BG: {:?}", ThermalPalette::BG);
}
